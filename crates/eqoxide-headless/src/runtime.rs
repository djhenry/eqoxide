//! CPU host for the normal login/world/zone session and the shared physical controller.
use crate::{
    control::{AdmittedCommand, CommandStatus as Status, Control, Operation},
    profile::Profile,
};
use eqoxide_command::CommandState;
use eqoxide_core::{game_state::GameState, region_map::RegionMap};
use eqoxide_ipc as ipc;
use eqoxide_net::{action_loop::ActionLoopSlots, gameplay::GameplayLifecycle};
use eqoxide_physics::{
    movement::{manual_wish, CharacterController, MoveIntent},
    zone_in::ZoneIn,
};
use eqoxide_zone_geometry::{
    collision::{Collision, SharedCollision},
    native_map::{NativeMap, NativeMapLimits},
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::Read,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

struct Wiring {
    command: CommandState,
    combat: ipc::CombatSlots,
    controller: ipc::ControllerSlots,
    snapshot: ipc::GameStateSnapshot,
    health: ipc::NetHealthShared,
    shutdown: Arc<AtomicBool>,
    slots: ActionLoopSlots,
    lifecycle: GameplayLifecycle,
    collision: SharedCollision,
}
impl Wiring {
    fn new(maps_dir: std::path::PathBuf) -> Self {
        let combat = ipc::CombatSlots::default();
        let merchant = ipc::MerchantSlots::default();
        let inventory = ipc::InventorySlots::default();
        let interact = ipc::InteractSlots::default();
        let quest = ipc::QuestSlots::default();
        let group = ipc::GroupSlots::default();
        let guild = ipc::GuildSlots::default();
        let trainer = ipc::TrainerSlots::default();
        let social = ipc::SocialSlots::default();
        let chat = ipc::ChatSlots::default();
        let life = ipc::LifecycleSlots::default();
        let controller = ipc::ControllerSlots::default();
        let command = CommandState::new(
            combat.clone(),
            merchant.clone(),
            inventory.clone(),
            interact.clone(),
            quest.clone(),
            group.clone(),
            guild.clone(),
            trainer,
            social.clone(),
            chat.clone(),
            life.clone(),
        );
        let snapshot = Arc::new(arc_swap::ArcSwap::from_pointee(GameState::new()));
        let health = Arc::new(Mutex::new(ipc::NetHealth::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let collision = Arc::new(std::sync::RwLock::new(None));
        let slots = ActionLoopSlots {
            world: Default::default(),
            quest,
            group_slots: group,
            command: command.clone(),
            social,
            merchant_slots: merchant,
            inventory_slots: inventory,
            interact,
            chat,
            controller: controller.clone(),
            guild_slots: guild,
            collision: collision.clone(),
            maps_dir,
        };
        let lifecycle = GameplayLifecycle {
            shutdown: shutdown.clone(),
            camp: life.camp,
            camp_until: life.camp_until,
            respawn: life.respawn,
            game_state_snapshot: snapshot.clone(),
            net_health: health.clone(),
        };
        Self {
            command,
            combat,
            controller,
            snapshot,
            health,
            shutdown,
            slots,
            lifecycle,
            collision,
        }
    }
}

/// Deadline lives in the physics owner. No HTTP retry, poll, or slow transaction renews it.
struct Motion {
    id: String,
    wish: [f32; 2],
    until: Instant,
}
struct Host {
    control: Control,
    command: CommandState,
    combat: ipc::CombatSlots,
    controller_slots: ipc::ControllerSlots,
    body: Option<CharacterController>,
    zone_in: ZoneIn,
    epoch: u64,
    heading: f32,
    motion: Option<Motion>,
    attack_until: Option<Instant>,
    action_ids: HashMap<u64, (String, bool)>,
    transaction_uncertain: Arc<AtomicBool>,
    inventory_revision: u64,
    merchant_revision: u64,
    inventory_value: Value,
    merchant_value: Value,
    last_event: u64,
    last_echo: u64,
}
impl Host {
    fn outcome(&self, id: &str, status: Status, reason: Option<&str>, evidence: Option<Value>) {
        let _ = self
            .control
            .publish_outcome(id, status, reason.map(str::to_owned), evidence);
    }
    fn halt(&mut self, reason: &str) {
        if let Some(motion) = self.motion.take() {
            self.control
                .publish_event(json!({"kind":"motion_stopped","id":motion.id,"reason":reason}));
        }
        self.attack_until = None;
        // LAST-WINS: safety off must replace an attack still waiting in the net mailbox.
        *self.combat.attack.lock().unwrap() = Some(false);
    }
    fn step(
        &mut self,
        gs: &GameState,
        col: Option<&Collision>,
        ready: bool,
        now: Instant,
        dt: f32,
    ) {
        if !ready {
            self.halt("not_ready");
            return;
        }
        if self.motion.as_ref().is_some_and(|m| now >= m.until) {
            let motion = self.motion.take().unwrap();
            self.control
                .publish_event(json!({"kind":"motion_expired","id":motion.id}));
        }
        if self.attack_until.is_some_and(|until| now >= until) {
            self.attack_until = None;
            *self.combat.attack.lock().unwrap() = Some(false); // LAST-WINS: lease expiry
            self.control
                .publish_event(json!({"kind":"attack_lease_expired"}));
        }
        let Some(body) = self.body.as_mut() else {
            return;
        };
        if let Some(correction) = self.controller_slots.pos_correction.lock().unwrap().take() {
            body.teleport(correction);
        }
        self.zone_in.on_frame(
            body,
            &gs.world.zone_name,
            false,
            true,
            col,
            gs.world.zone_underworld,
        );
        let stepped = col.is_some();
        if let Some(col) = col {
            let wish = self.motion.as_ref().map(|m| m.wish).unwrap_or([0.0, 0.0]);
            body.set_underworld(gs.world.zone_underworld);
            body.set_levitating(gs.player_levitating());
            body.step(
                MoveIntent {
                    wish_dir: wish,
                    speed: eqoxide_physics::movement::DEFAULT_MOVE_SPEED,
                    ..Default::default()
                },
                dt,
                col,
            );
        } else {
            body.clear_hold();
        }
        let mut view = self.controller_slots.controller_view.lock().unwrap();
        // Network zone entry invalidates this view under the same lock. Never republish an old epoch.
        if view.zone_epoch.is_some_and(|epoch| epoch != self.epoch) {
            return;
        }
        view.zone_epoch = Some(self.epoch);
        if stepped {
            view.step_seq = view.step_seq.wrapping_add(1);
        }
        view.pos = body.pos;
        view.heading = self.heading;
        view.initialized = true;
        view.publish_disclosures(body.disclosures());
        if view.landed_fall_height.is_none() {
            view.landed_fall_height = body.take_landed_fall_height();
        }
        if view.relocated.is_none() {
            view.relocated = body.take_relocation();
        }
    }
    fn update_knowledge(&mut self, gs: &GameState) {
        let inventory = json!({"known":gs.inventory_received,"items":gs.inventory,"coin":gs.coin,"coin_confirmed":gs.coin_confirmed});
        if inventory != self.inventory_value {
            self.inventory_revision += 1;
            self.inventory_value = inventory;
        }
        let merchant = json!({"open":gs.merchant_open,"items":gs.merchant_items});
        if merchant != self.merchant_value {
            self.merchant_revision += 1;
            self.merchant_value = merchant;
        }
        for event in gs.chat_events.iter().filter(|e| e.id > self.last_event) {
            self.control.publish_event(json!({"kind":"received_game_event","source":if event.category=="chat" {"received_packet"} else {"client_game_event"},"event_id":event.id,
                "category":event.category,"event_kind":event.kind,"from":event.from,"directed":event.directed,"text":event.text}));
        }
        if let Some(event) = gs.chat_events.back() {
            self.last_event = event.id;
        }
        for echo in gs
            .received_self_echoes
            .iter()
            .filter(|echo| echo.id > self.last_echo)
        {
            self.control.publish_event(json!({"kind":"received_self_echo","source":"received_packet",
                "channel":echo.channel,"sender":echo.sender,"text":echo.text,"truncated":echo.truncated,
                "recipient_delivery_confirmed":false}));
        }
        if let Some(echo) = gs.received_self_echoes.back() {
            self.last_echo = echo.id;
        }
        for event in self.command.actions().take_events() {
            if let Some((id, transaction)) = self.action_ids.remove(&event.id) {
                let status = match event.result.as_str() {
                    "confirmed" => Status::Confirmed,
                    "refused" => Status::Refused,
                    _ => Status::Unknown,
                };
                if transaction {
                    self.transaction_uncertain
                        .store(status == Status::Unknown, Ordering::Release);
                }
                let evidence = (status==Status::Confirmed).then(||json!({"kind":"server_action_receipt","action":event.kind,"detail":event.reason}));
                self.outcome(&id, status, Some(&event.reason), evidence);
            }
        }
    }
    async fn dispatch(
        &mut self,
        command: AdmittedCommand,
        gs: &GameState,
        ready: bool,
        now: Instant,
    ) {
        let id = command.id;
        let req = command.request;
        if req.zone_epoch != self.epoch || (!ready && !req.operation.is_priority()) {
            self.outcome(
                &id,
                Status::Refused,
                Some("session_or_zone_not_ready"),
                None,
            );
            return;
        }
        // Claim under the journal lock before any side effect. A priority command may have
        // superseded this drained entry while the host was waiting to dispatch it.
        if self.control.mark_dispatched(&id).is_err() {
            return;
        }
        let operation = req.operation;
        if matches!(
            operation,
            Operation::Buy { .. } | Operation::Sell { .. } | Operation::InventoryMove { .. }
        ) {
            if self.transaction_uncertain.load(Ordering::Acquire) {
                self.outcome(
                    &id,
                    Status::Refused,
                    Some("transaction_outcome_unknown_relog_required"),
                    None,
                );
                return;
            }
            if req.inventory_revision != Some(self.inventory_revision) || !gs.inventory_received {
                self.outcome(
                    &id,
                    Status::Refused,
                    Some("inventory_revision_required_or_stale"),
                    None,
                );
                return;
            }
            if !matches!(operation, Operation::InventoryMove { .. })
                && req.merchant_revision != Some(self.merchant_revision)
            {
                self.outcome(
                    &id,
                    Status::Refused,
                    Some("merchant_revision_required_or_stale"),
                    None,
                );
                return;
            }
        }
        match operation {
            Operation::MoveFor {
                east,
                north,
                duration_ms,
            } => {
                if gs.sitting || self.body.is_none() {
                    self.outcome(&id, Status::Refused, Some("stand_before_moving"), None);
                    return;
                }
                let (wish, heading) = manual_wish([east, north]);
                if let Some(heading) = heading {
                    self.heading = heading;
                }
                self.motion = Some(Motion {
                    id: id.clone(),
                    wish,
                    until: now + Duration::from_millis(duration_ms),
                });
                self.outcome(&id,Status::AppliedLocal,None,Some(json!({"kind":"controller_intent","duration_ms":duration_ms,"server_position_confirmed":false})));
            }
            Operation::Stop => {
                self.motion = None;
                self.outcome(
                    &id,
                    Status::AppliedLocal,
                    None,
                    Some(json!({"kind":"controller_stopped"})),
                );
            }
            Operation::AttackOff => {
                self.attack_until = None;
                *self.combat.attack.lock().unwrap() = Some(false); // LAST-WINS: safety request
                self.outcome(
                    &id,
                    Status::AppliedLocal,
                    None,
                    Some(json!({"kind":"attack_off_queued","server_confirmed":false})),
                );
            }
            Operation::MerchantOpen { entity_id } => {
                if !observed_entity(gs, entity_id as u32, 30.0) {
                    self.outcome(
                        &id,
                        Status::Refused,
                        Some("merchant_not_received_or_out_of_range"),
                        None,
                    );
                    return;
                }
                let (tx, rx) = tokio::sync::oneshot::channel();
                let context = self
                    .command
                    .actions()
                    .begin_for_zone("merchant_open", self.epoch);
                if !context
                    .scope(async { self.command.request_open_await(entity_id as u32, tx) })
                    .await
                {
                    self.outcome(&id, Status::Refused, Some("network_mailbox_busy"), None);
                    return;
                }
                let control = self.control.clone();
                tokio::spawn(async move {
                    complete_await(control, id, rx, "server_merchant_open", None).await;
                });
            }
            Operation::Buy { slot, quantity: _ } => {
                let Some(merchant) = gs.merchant_open else {
                    self.outcome(&id, Status::Refused, Some("merchant_closed"), None);
                    return;
                };
                if !gs
                    .merchant_items
                    .iter()
                    .any(|item| item.merchant_slot == slot)
                {
                    self.outcome(
                        &id,
                        Status::Refused,
                        Some("catalog_slot_not_received"),
                        None,
                    );
                    return;
                }
                let (tx, rx) = tokio::sync::oneshot::channel();
                let context = self.command.actions().begin_for_zone("buy", self.epoch);
                if !context
                    .scope(async { self.command.request_buy_await(merchant, slot, tx) })
                    .await
                {
                    self.outcome(&id, Status::Refused, Some("network_mailbox_busy"), None);
                    return;
                }
                let control = self.control.clone();
                let uncertain = self.transaction_uncertain.clone();
                uncertain.store(true, Ordering::Release);
                tokio::spawn(async move {
                    complete_await(control, id, rx, "server_buy_receipt", Some(uncertain)).await;
                });
            }
            operation => {
                let transaction = matches!(
                    operation,
                    Operation::Sell { .. } | Operation::InventoryMove { .. }
                );
                let tracker = self.command.actions();
                let context = tracker.begin_for_zone("headless_command", self.epoch);
                let accepted = context
                    .scope(async {
                        match operation {
                            Operation::Loot { entity_id } => {
                                gs.world.entities.get(&(entity_id as u32)).is_some_and(|e| {
                                    e.dead
                                        && e.is_npc
                                        && e.dist_to(gs.player_x, gs.player_y, gs.player_z) <= 20.0
                                }) && self.command.request_loot(entity_id as u32)
                            }
                            Operation::Target { entity_id } => {
                                observed_entity(gs, entity_id as u32, 500.0)
                                    && self.command.request_target(entity_id as u32)
                            }
                            Operation::Attack { lease_ms } => {
                                if gs
                                    .target_id
                                    .and_then(|id| gs.world.entities.get(&id))
                                    .is_none_or(|e| {
                                        e.dead
                                            || !e.is_npc
                                            || e.dist_to(gs.player_x, gs.player_y, gs.player_z)
                                                > 30.0
                                    })
                                {
                                    false
                                } else {
                                    let accepted = self.command.request_attack(true);
                                    if accepted {
                                        self.attack_until =
                                            Some(now + Duration::from_millis(lease_ms));
                                    }
                                    accepted
                                }
                            }
                            Operation::Sit => {
                                self.motion = None;
                                self.attack_until = None;
                                *self.combat.attack.lock().unwrap() = Some(false);
                                self.command.request_sit(true)
                            }
                            Operation::Stand => self.command.request_sit(false),
                            Operation::Say { text } => self.command.request_say(text),
                            Operation::Tell { recipient, text } => {
                                self.command.request_chat_send(ipc::ChatSend {
                                    chan: 7,
                                    to: recipient,
                                    text,
                                });
                                true
                            }
                            Operation::MerchantClose => {
                                self.command.request_merchant_trade(ipc::TradeCmd::Close)
                            }
                            Operation::Sell { slot, quantity } => {
                                if let Some(merchant) = gs.merchant_open {
                                    gs.inventory.iter().any(|item| {
                                        item.slot == slot as i32
                                            && quantity <= item.charges.max(1) as u32
                                    }) && self
                                        .command
                                        .request_merchant_sell(merchant, slot, quantity)
                                } else {
                                    false
                                }
                            }
                            Operation::InventoryMove {
                                from_slot,
                                to_slot,
                                quantity,
                            } => {
                                quantity == 1
                                    && from_slot <= 32
                                    && to_slot <= 32
                                    && gs
                                        .inventory
                                        .iter()
                                        .any(|item| item.slot == from_slot as i32)
                                    && self.command.request_inventory_move(from_slot, to_slot)
                            }
                            Operation::Camp => {
                                self.halt("camp");
                                self.command
                                    .request_camp(ipc::CampCmd::StartInZone(self.epoch));
                                true
                            }
                            _ => false,
                        }
                    })
                    .await;
                if accepted && context.accepted() {
                    if transaction {
                        self.transaction_uncertain.store(true, Ordering::Release);
                    }
                    self.action_ids.insert(context.id(), (id, transaction));
                } else {
                    tracker.discard(context.id());
                    self.outcome(
                        &id,
                        if accepted {
                            Status::Unknown
                        } else {
                            Status::Refused
                        },
                        Some(if accepted {
                            "queued_without_server_receipt"
                        } else {
                            "precondition_failed_or_network_mailbox_busy"
                        }),
                        None,
                    );
                }
            }
        }
    }
}
/// Milliseconds on the host's monotonic clock, which starts when `run` starts. State carries the
/// current reading as `clock_ms` so a script can compare any `*_at_ms` field with it.
fn clock_ms(started: Instant, at: Instant) -> u64 {
    at.saturating_duration_since(started).as_millis() as u64
}
/// The last self-position update put on the wire. The datagram is unreliable: this proves the
/// attempt and its content, never that the server received it.
fn position_send_json(gs: &GameState, started: Instant, now: Instant) -> Value {
    gs.last_position_send.map_or(Value::Null, |sent| {
        json!({"sequence":sent.seq,"position":sent.pos,"delta":sent.delta,"heading":sent.heading,
            "animation":sent.animation,"reason":sent.reason.as_str(),"sent_at_ms":clock_ms(started,sent.at),
            "age_ms":now.saturating_duration_since(sent.at).as_millis() as u64,"delivery_confirmed":false})
    })
}
fn observed_entity(gs: &GameState, id: u32, radius: f32) -> bool {
    gs.world.entities.get(&id).is_some_and(|entity| {
        !entity.dead && entity.dist_to(gs.player_x, gs.player_y, gs.player_z) <= radius
    })
}
async fn complete_await<T: serde::Serialize>(
    control: Control,
    id: String,
    rx: tokio::sync::oneshot::Receiver<ipc::CommandResult<T>>,
    kind: &str,
    uncertain: Option<Arc<AtomicBool>>,
) {
    let (status, reason, evidence) = match tokio::time::timeout(Duration::from_secs(8), rx).await {
        Ok(Ok(ipc::CommandResult::Resolved(receipt))) => (
            Status::Confirmed,
            None,
            Some(json!({"kind":kind,"receipt":receipt})),
        ),
        Ok(Ok(ipc::CommandResult::Refused(reason))) => (Status::Refused, Some(reason), None),
        _ => (
            Status::Unknown,
            Some("no_correlated_server_receipt_relog_before_another_transaction".into()),
            None,
        ),
    };
    if let Some(uncertain) = uncertain {
        // Keep admission closed while a packet is outstanding and after an unknown outcome.
        uncertain.store(status == Status::Unknown, Ordering::Release);
    }
    let _ = control.publish_outcome(&id, status, reason, evidence);
}

fn load_collision(dir: &std::path::Path, zone: &str) -> Result<(Arc<Collision>, Value), String> {
    if zone.is_empty() || !zone.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
        return Err("invalid received zone name".into());
    }
    let path = dir.join(format!("{zone}.map"));
    let limits = NativeMapLimits::default();
    let map = NativeMap::load(&path, &limits).map_err(|e| e.to_string())?;
    let stats = map.stats().clone();
    let mut bytes = Vec::new();
    std::fs::File::open(&path)
        .map_err(|e| e.to_string())?
        .take((limits.max_file_bytes + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > limits.max_file_bytes {
        return Err("map changed beyond file limit".into());
    }
    let digest = format!("sha256:{:x}", Sha256::digest(&bytes));
    let mut col = map.into_collision(32.0).map_err(|e| e.to_string())?;
    let water = RegionMap::try_load(dir, zone).map(Arc::new);
    let region = match &water {
        Ok(map) => {
            json!({"status":"ready","nodes":map.node_count(),"zone_line_indices":map.zone_line_indices()})
        }
        Err(e) => json!({"status":"unavailable","reason":format!("{e:?}")}),
    };
    col.set_region_data(water);
    Ok((
        Arc::new(col),
        json!({"status":"ready","source":"eqemu-map-v2","digest":digest,"triangles":stats.triangles,
        "models":stats.models,"placements":stats.placements,"region":region,"dynamic_doors":"unsupported"}),
    ))
}

pub async fn run(profile: Profile) -> Result<(), String> {
    // Bind before opening any gameplay connection: a busy control port fails without logging in.
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, profile.api_port))
        .await
        .map_err(|e| format!("control bind failed: {e}"))?;
    let session = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "system clock before epoch")?
            .as_nanos()
    );
    let started = Instant::now();
    let control = Control::new(session, profile.control_token.clone(), 4096, 2048)?;
    let router = control.router();
    let api = tokio::spawn(async move { axum::serve(listener, router).await });
    println!("API_PORT={}", profile.api_port);
    let wiring = Wiring::new(profile.maps_dir.clone());
    let mut host = Host {
        control: control.clone(),
        command: wiring.command,
        combat: wiring.combat,
        controller_slots: wiring.controller,
        body: None,
        zone_in: Default::default(),
        epoch: 0,
        heading: 0.0,
        motion: None,
        attack_until: None,
        action_ids: HashMap::new(),
        transaction_uncertain: Arc::new(AtomicBool::new(false)),
        inventory_revision: 0,
        merchant_revision: 0,
        inventory_value: Value::Null,
        merchant_value: Value::Null,
        last_event: 0,
        last_echo: 0,
    };
    let (net_tx, mut net) = tokio::sync::oneshot::channel();
    let config = profile.login_config();
    let expected = profile.expected();
    std::thread::Builder::new()
        .name("headless-net".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| "network runtime initialization failed".to_string())?
                    .block_on(eqoxide_net::login::run_login_flow_selected(
                        config,
                        1,
                        wiring.slots,
                        wiring.lifecycle,
                        expected,
                    ))
            }))
            .unwrap_or_else(|_| Err("network task panicked".into()));
            let _ = net_tx.send(result);
        })
        .map_err(|_| "cannot spawn network owner".to_string())?;
    let shutdown = wiring.shutdown.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        shutdown.store(true, Ordering::Release);
    });
    let shutdown = wiring.shutdown.clone();
    tokio::spawn(async move {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
            shutdown.store(true, Ordering::Release);
        }
    });
    let mut interval = tokio::time::interval(Duration::from_millis(20));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut previous = Instant::now();
    let mut geometry = json!({"status":"unavailable","reason":"waiting_for_real_zone_entry"});
    let mut col: Option<Arc<Collision>> = None;
    let mut loaded_zone = String::new();
    let mut publish_at = Instant::now();
    let mut terminal: Option<String> = None;
    let mut shutdown_started: Option<Instant> = None;
    loop {
        interval.tick().await;
        let now = Instant::now();
        let dt = (now - previous).as_secs_f32().min(0.05);
        previous = now;
        if terminal.is_none() {
            match net.try_recv() {
                Ok(result) => {
                    terminal = Some(result.err().unwrap_or_else(|| "session_ended".into()))
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    terminal = Some("network_task_failed".into())
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
            }
            if terminal.is_some() {
                host.halt("network_ended");
                control.set_ready(false);
                control.publish_event(json!({"kind":"session_ended","reason":terminal}));
            }
        }
        let gs = wiring.snapshot.load_full();
        if gs.zone_epoch != host.epoch {
            host.halt("zone_entry");
            control.set_ready(false);
            host.body = None;
            host.epoch = gs.zone_epoch;
            host.zone_in = ZoneIn::default();
            col = None;
            loaded_zone.clear();
            *wiring.collision.write().unwrap() = None;
            let mut view = host.controller_slots.controller_view.lock().unwrap();
            view.initialized = false;
            view.zone_epoch = Some(gs.zone_epoch);
            view.landed_fall_height = None;
            view.relocated = None;
        }
        if gs.player_id != 0
            && gs.player_pos_known
            && !gs.world.zone_name.is_empty()
            && loaded_zone != gs.world.zone_name
        {
            control.set_ready(false);
            host.halt("loading_geometry");
            loaded_zone = gs.world.zone_name.clone();
            match load_collision(&profile.maps_dir, &loaded_zone) {
                Ok((collision, metadata)) => {
                    *wiring.collision.write().unwrap() = Some(collision.clone());
                    col = Some(collision);
                    geometry = metadata;
                }
                Err(reason) => {
                    geometry = json!({"status":"unavailable","reason":reason});
                }
            }
        }
        if host.body.is_none() && gs.player_id != 0 && gs.player_pos_known && col.is_some() {
            let pos = [gs.player_x, gs.player_y, gs.player_z];
            if pos.iter().all(|v| v.is_finite()) {
                host.body = Some(CharacterController::new(pos));
                host.heading = gs.player_heading;
            }
        }
        let health = wiring.health.lock().unwrap();
        let tick_age = health.last_tick.elapsed().as_millis() as u64;
        let inbound_age = health.last_packet.elapsed().as_millis() as u64;
        let link_age = health.last_datagram.elapsed().as_millis() as u64;
        let dropped = health.session_drop.is_some();
        drop(health);
        let ready = terminal.is_none()
            && !wiring.shutdown.load(Ordering::Acquire)
            && gs.player_id != 0
            && gs.player_pos_known
            && !gs.world.zone_in_failed
            && host.body.is_some()
            && col.is_some()
            && tick_age < 500
            && link_age < 15000
            && !dropped
            && gs.player_dead_since.is_none()
            && !(gs.max_hp > 0 && gs.cur_hp <= 0);
        host.step(&gs, col.as_deref(), ready, now, dt);
        host.update_knowledge(&gs);
        if now >= publish_at {
            let position = host.body.as_ref().map(|body| body.pos);
            let mut nearby: Vec<_> = gs
                .world
                .entities
                .values()
                .filter(|entity| {
                    gs.player_pos_known
                        && entity.dist_to(gs.player_x, gs.player_y, gs.player_z) <= 500.0
                })
                .collect();
            nearby.sort_by_key(|entity| entity.spawn_id);
            let truncated = nearby.len() > 256;
            nearby.truncate(256);
            let entities:Vec<_>=nearby.iter().map(|entity|json!({"id":entity.spawn_id,"class_id":entity.class_id,"name":entity.name,"npc":entity.is_npc,"dead":entity.dead,
                "position":[entity.x,entity.y,entity.z],"hp_pct":entity.hp_pct,"level":entity.level,
                "los":position.zip(col.as_ref()).map(|(p,c)|c.line_clear([p[0],p[1],p[2]+5.0],[entity.x,entity.y,entity.z+5.0],0.0)),"los_source":"local_native_static_geometry"})).collect();
            control.publish_state(host.epoch,json!({"clock_ms":clock_ms(started,now),"phase":if terminal.is_some(){"failed"}else if ready{"ready"}else{"not_ready"},"failure":terminal,
                "freshness":{"network_tick_age_ms":tick_age,"inbound_age_ms":inbound_age,"link_age_ms":link_age},
                "zone":{"id":gs.world.zone_id,"name":gs.world.zone_name,"instance_id":Value::Null},"geometry":geometry,
                "self":{"id":gs.player_id,"name":gs.player_name,"position":position,"position_source":"local_prediction_from_received_self_spawn",
                    "heading":host.heading,"on_ground":host.body.as_ref().map(|b|b.on_ground),"in_water":host.body.as_ref().map(|b|b.in_water),
                    "hp":{"value":gs.cur_hp,"max":gs.max_hp,"known":gs.max_hp>0,"confirmed":gs.hp_confirmed},"sitting":gs.sitting,
                    "last_position_send":position_send_json(&gs,started,now),
                    "target_id":gs.target_id,"attack_on":gs.auto_attack,"motion_active":host.motion.is_some(),"hold":gs.player_hold.map(|hold|json!({"reason":hold.reason.as_str(),"seconds":hold.secs}))},
                "nearby":{"available":gs.player_pos_known,"complete":false,"scope":"received_roster_within_radius","radius":500,"limit":256,"truncated":truncated,"entities":entities},
                "inventory":{"known":gs.inventory_received,"revision":host.inventory_revision,"items":gs.inventory,"source":"received_with_local_prediction","coin":gs.coin,"coin_confirmed":gs.coin_confirmed},
                "merchant":{"known":gs.merchant_open.is_some(),"catalog_complete":false,"revision":host.merchant_revision,"open":gs.merchant_open,"items":gs.merchant_items},
                "policy":{"loot":"explicit_only","automatic_zone_crossing":false},
                "transactions_blocked_until_relog":host.transaction_uncertain.load(Ordering::Acquire)
            }))?;
            publish_at = now + Duration::from_millis(100);
        }
        control.set_ready(ready);
        for command in control.drain_actions() {
            host.dispatch(command, &gs, ready, now).await;
        }
        if wiring.shutdown.load(Ordering::Acquire) {
            let started = shutdown_started.get_or_insert(now);
            if terminal.is_some() {
                break;
            }
            if now.duration_since(*started) >= Duration::from_secs(8) {
                api.abort();
                return Err("logout completion deadline exceeded; session outcome unknown".into());
            }
        }
        if api.is_finished() {
            host.halt("http_server_ended");
            wiring.shutdown.store(true, Ordering::Release);
            return Err("control HTTP server ended".into());
        }
    }
    api.abort();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use eqoxide_zone_geometry::collision::ServerTriangle;
    pub(super) fn host() -> Host {
        let wiring = Wiring::new(std::path::PathBuf::from("/unused"));
        Host {
            control: Control::new("test-session".into(), "test-credential".into(), 16, 32).unwrap(),
            command: wiring.command,
            combat: wiring.combat,
            controller_slots: wiring.controller,
            body: Some(CharacterController::new([0.0, 0.0, 0.0])),
            zone_in: Default::default(),
            epoch: 1,
            heading: 0.0,
            motion: None,
            attack_until: None,
            action_ids: HashMap::new(),
            transaction_uncertain: Arc::new(AtomicBool::new(false)),
            inventory_revision: 0,
            merchant_revision: 0,
            inventory_value: Value::Null,
            merchant_value: Value::Null,
            last_event: 0,
            last_echo: 0,
        }
    }
    fn floor() -> Collision {
        Collision::from_server_triangles(
            vec![
                ServerTriangle([
                    [-100.0, -100.0, 0.0],
                    [100.0, -100.0, 0.0],
                    [100.0, 100.0, 0.0],
                ]),
                ServerTriangle([
                    [-100.0, -100.0, 0.0],
                    [100.0, 100.0, 0.0],
                    [-100.0, 100.0, 0.0],
                ]),
            ],
            32.0,
        )
        .unwrap()
    }
    #[test]
    fn movement_and_attack_expire_without_a_caller() {
        let mut host = host();
        let col = floor();
        let start = Instant::now();
        let mut gs = GameState::new();
        gs.zone_epoch = 1;
        gs.world.zone_name = "synthetic".into();
        host.motion = Some(Motion {
            id: "one".into(),
            wish: [1.0, 0.0],
            until: start + Duration::from_millis(200),
        });
        host.attack_until = Some(start + Duration::from_millis(200));
        host.step(
            &gs,
            Some(&col),
            true,
            start + Duration::from_millis(100),
            0.1,
        );
        let moving = host.body.as_ref().unwrap().pos;
        assert!(moving[0] > 0.0);
        for i in 2..20 {
            host.step(
                &gs,
                Some(&col),
                true,
                start + Duration::from_millis(i * 100),
                0.1,
            );
        }
        assert_eq!(host.body.as_ref().unwrap().pos, moving);
        assert!(host.motion.is_none());
        assert!(host.attack_until.is_none());
        assert_eq!(*host.combat.attack.lock().unwrap(), Some(false));
    }
    #[test]
    fn network_loss_clears_wish_and_does_not_initialize_a_body() {
        let mut host = host();
        host.body = None;
        let now = Instant::now();
        host.motion = Some(Motion {
            id: "move".into(),
            wish: [1.0, 0.0],
            until: now + Duration::from_secs(2),
        });
        host.attack_until = Some(now + Duration::from_secs(5));
        host.step(&GameState::new(), None, false, now, 0.02);
        assert!(host.body.is_none());
        assert!(host.motion.is_none());
        assert!(host.attack_until.is_none());
        assert!(
            !host
                .controller_slots
                .controller_view
                .lock()
                .unwrap()
                .initialized
        );
    }
    /// central-vgwu: the step count tells the net streamer a physics step happened; a tick with
    /// no collision does not step the body and must not count.
    #[test]
    fn controller_view_counts_only_real_physics_steps() {
        let mut host = host();
        let col = floor();
        let mut gs = GameState::new();
        gs.zone_epoch = 1;
        fn host_step_seq(host: &Host) -> u64 {
            host.controller_slots.controller_view.lock().unwrap().step_seq
        }
        host.step(&gs, Some(&col), true, Instant::now(), 0.02);
        host.step(&gs, Some(&col), true, Instant::now(), 0.02);
        assert_eq!(host_step_seq(&host), 2);
        host.step(&gs, None, true, Instant::now(), 0.02);
        assert_eq!(host_step_seq(&host), 2, "an unstepped republish is not a step");
    }
    #[test]
    fn state_publishes_the_last_position_send() {
        use eqoxide_core::game_state::{PositionSend, PositionSendReason};
        let started = Instant::now();
        let mut gs = GameState::new();
        assert_eq!(position_send_json(&gs, started, started), Value::Null);
        let at = started + Duration::from_millis(1500);
        gs.last_position_send = Some(PositionSend {
            seq: 41, pos: [1.0, 2.0, 3.0], delta: [0.0; 3], heading: 90.0, animation: 0,
            reason: PositionSendReason::Rest, at,
        });
        assert_eq!(
            position_send_json(&gs, started, at + Duration::from_millis(20)),
            json!({"sequence":41,"position":[1.0,2.0,3.0],"delta":[0.0,0.0,0.0],"heading":90.0,
                "animation":0,"reason":"rest","sent_at_ms":1500,"age_ms":20,"delivery_confirmed":false})
        );
    }
    #[test]
    fn fenced_controller_cannot_stream_previous_zone() {
        let mut host = host();
        let col = floor();
        let mut gs = GameState::new();
        gs.zone_epoch = 1;
        host.controller_slots
            .controller_view
            .lock()
            .unwrap()
            .zone_epoch = Some(2);
        host.step(&gs, Some(&col), true, Instant::now(), 0.02);
        assert!(
            !host
                .controller_slots
                .controller_view
                .lock()
                .unwrap()
                .initialized
        );
    }
}

#[cfg(test)]
mod transaction_admission_tests {
    use super::*;
    #[tokio::test]
    async fn refused_transaction_does_not_poison_later_work() {
        let wiring = Wiring::new("/unused".into());
        let mut host = Host {
            control: Control::new("s".into(), "credential".into(), 8, 16).unwrap(),
            command: wiring.command,
            combat: wiring.combat,
            controller_slots: wiring.controller,
            body: None,
            zone_in: Default::default(),
            epoch: 1,
            heading: 0.0,
            motion: None,
            attack_until: None,
            action_ids: HashMap::new(),
            transaction_uncertain: Arc::new(AtomicBool::new(false)),
            inventory_revision: 0,
            merchant_revision: 0,
            inventory_value: Value::Null,
            merchant_value: Value::Null,
            last_event: 0,
            last_echo: 0,
        };
        let mut gs = GameState::new();
        gs.inventory_received = true;
        host.control
            .publish_state(1, json!({"phase":"ready"}))
            .unwrap();
        host.control.set_ready(true);
        for (index, operation) in [
            Operation::Sell {
                slot: 23,
                quantity: 1,
            },
            Operation::InventoryMove {
                from_slot: 23,
                to_slot: 24,
                quantity: 1,
            },
        ]
        .into_iter()
        .enumerate()
        {
            use tower::ServiceExt;
            let id = format!("rejected-{index}");
            let request = crate::control::CommandRequest {
                session: "s".into(),
                zone_epoch: 1,
                inventory_revision: Some(0),
                merchant_revision: Some(0),
                operation: operation.clone(),
            };
            host.control
                .router()
                .oneshot(
                    axum::http::Request::builder()
                        .method("PUT")
                        .uri(format!("/v2/player/commands/{id}"))
                        .header("authorization", "Bearer credential")
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(
                            serde_json::to_vec(&request).unwrap(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            let admitted = host
                .control
                .drain_actions()
                .pop()
                .expect("admitted test transaction");
            host.dispatch(admitted, &gs, true, Instant::now()).await;
            assert!(!host.transaction_uncertain.load(Ordering::Acquire));
        }
        // A command accepted into a mailbox can still be definitively rejected before send.
        let tracker = host.command.actions();
        let context = tracker.begin_for_zone("inventory", 1);
        assert!(
            context
                .scope(async { host.command.request_inventory_move(23, 24) })
                .await
        );
        host.action_ids
            .insert(context.id(), ("queued-tx".into(), true));
        host.transaction_uncertain.store(true, Ordering::Release);
        tracker.set_zone_epoch(2);
        assert!(host.command.take_inventory_move().is_none());
        host.update_knowledge(&gs);
        assert!(!host.transaction_uncertain.load(Ordering::Acquire));
    }
}

#[cfg(test)]
mod execution_claim_tests {
    use super::*;
    use tower::ServiceExt;
    async fn put(control: &Control, id: &str, operation: Value) {
        control
            .router()
            .oneshot(
                axum::http::Request::builder()
                    .method("PUT")
                    .uri(format!("/v2/player/commands/{id}"))
                    .header("authorization", "Bearer test-credential")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        json!({
                "session":"test-session","zone_epoch":1,"operation":operation})
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn superseded_drained_attack_never_enters_network_mailbox() {
        let mut host = super::tests::host();
        host.control.publish_state(1, json!({})).unwrap();
        host.control.set_ready(true);
        put(
            &host.control,
            "attack",
            json!({"kind":"attack","lease_ms":1000}),
        )
        .await;
        let attack = host.control.drain_actions().pop().unwrap();
        put(&host.control, "off", json!({"kind":"attack_off"})).await;
        let mut gs = GameState::new();
        gs.target_id = Some(7);
        gs.upsert_entity(eqoxide_core::game_state::make_entity(
            7, "rat", 0.0, 0.0, 0.0, true,
        ));
        host.dispatch(attack, &gs, true, Instant::now()).await;
        assert!(host.combat.attack.lock().unwrap().is_none());
        assert_eq!(
            host.control.command("attack").unwrap().status,
            Status::Refused
        );
    }
    #[tokio::test]
    async fn stale_awaited_buy_returns_definite_refusal_and_releases_transaction_latch() {
        let host = super::tests::host();
        host.control.publish_state(1, json!({})).unwrap();
        host.control.set_ready(true);
        put(
            &host.control,
            "buy",
            json!({"kind":"buy","slot":1,"quantity":1}),
        )
        .await;
        host.control.drain_actions();
        host.control.mark_dispatched("buy").unwrap();
        let tracker = host.command.actions();
        let context = tracker.begin_for_zone("buy", 1);
        let (tx, rx) = tokio::sync::oneshot::channel();
        assert!(
            context
                .scope(async { host.command.request_buy_await(7, 1, tx) })
                .await
        );
        host.transaction_uncertain.store(true, Ordering::Release);
        tracker.set_zone_epoch(2);
        assert!(host.command.take_buy_await().is_none());
        complete_await(
            host.control.clone(),
            "buy".into(),
            rx,
            "server_buy_receipt",
            Some(host.transaction_uncertain.clone()),
        )
        .await;
        assert!(!host.transaction_uncertain.load(Ordering::Acquire));
        assert_eq!(host.control.command("buy").unwrap().status, Status::Refused);
    }
}
