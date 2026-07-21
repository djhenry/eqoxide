//! The depth-tested 3D nav diagnostics overlay (#608) — the HUMAN consumer of
//! `eqoxide_nav::diagnostics::NavDebugSnapshot`.
//!
//! # Publish, don't recompute — enforced by signature
//!
//! [`overlay_vertices`] is a **pure function of the snapshot**. It takes `&NavDebugSnapshot` and
//! nothing else — no `Collision`, no planner, no floor queries — so a re-derivation ("let me just
//! check that edge against the geometry before drawing it") is not merely discouraged, it is
//! inexpressible here. What nav published is what gets drawn, verbatim, including a verdict that
//! disagrees with the world: if the planner is wrong, the overlay shows the planner being wrong,
//! which is precisely what a diagnostic of the planner must do. (The old egui overlay re-raycast
//! the grid itself and could disagree with the planner — the #608 problem class.)
//!
//! **Absence means unevaluated.** Only edges present in the snapshot's trace are drawn. Nothing is
//! synthesized for cells the planner never touched — an overlay that fills in gaps to look
//! complete is the same lie class in pixels.
//!
//! # Depth correctness — the point of the rewrite
//!
//! The old overlay was a screen-space egui painter: world points projected to 2D and drawn OVER
//! everything, so a floor dot behind a wall was indistinguishable from one in front of it —
//! exactly the discrimination #423 and the goal-Z family need. This pass draws real world-space
//! line geometry through the normal depth test (`LessEqual`, write off — the same contract as the
//! weather pass), so overlay geometry behind a wall is occluded by it. Lines are lifted `Z_LIFT`
//! above the floor so they don't z-fight the ground they annotate.

use eqoxide_nav::diagnostics::{
    EdgeKind, EdgeVerdict, NavDebugSnapshot, PadKnowledge, RejectReason,
};

/// One overlay vertex: world position + straight-through RGBA color.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct OverlayVertex {
    pub pos:   [f32; 3],
    pub color: [f32; 4],
}

/// Vertical lift applied to floor-level lines so they render just above the surface they annotate
/// instead of z-fighting it. Purely visual; the underlying data is untouched.
pub const Z_LIFT: f32 = 0.25;

// ── The color vocabulary. One verdict, one color — the mapping the consumer tests pin. ──────────
pub const COL_ACCEPT_WALK:    [f32; 4] = [0.15, 0.85, 0.30, 0.85];
pub const COL_ACCEPT_SWIM:    [f32; 4] = [0.15, 0.70, 0.95, 0.85];
pub const COL_ACCEPT_AIR:     [f32; 4] = [1.00, 0.62, 0.10, 0.90]; // jump / controlled fall
pub const COL_ACCEPT_PAD:     [f32; 4] = [0.75, 0.35, 1.00, 0.95];
pub const COL_REJECT_STEP:    [f32; 4] = [0.95, 0.15, 0.15, 0.90];
pub const COL_REJECT_GRADE:   [f32; 4] = [1.00, 0.42, 0.05, 0.90];
pub const COL_REJECT_CLEAR:   [f32; 4] = [1.00, 0.15, 0.65, 0.90];
pub const COL_REJECT_NOFLOOR: [f32; 4] = [0.55, 0.08, 0.08, 0.90];
pub const COL_REJECT_WATER:   [f32; 4] = [0.45, 0.30, 0.95, 0.90];
pub const COL_COARSE_ROUTE:   [f32; 4] = [1.00, 0.90, 0.15, 1.00];
pub const COL_FINE_ROUTE:     [f32; 4] = [0.25, 0.90, 1.00, 1.00];
pub const COL_GOAL:           [f32; 4] = [1.00, 1.00, 0.20, 1.00];
pub const COL_PLAYER:         [f32; 4] = [1.00, 1.00, 1.00, 1.00];
pub const COL_RING_OK:        [f32; 4] = [0.20, 0.80, 0.40, 0.80];
pub const COL_RING_BLOCKED:   [f32; 4] = [1.00, 0.20, 0.20, 0.95];

/// The color for an ACCEPTED edge of the given kind.
pub fn accept_color(kind: EdgeKind) -> [f32; 4] {
    match kind {
        EdgeKind::Walk => COL_ACCEPT_WALK,
        EdgeKind::Jump | EdgeKind::Fall => COL_ACCEPT_AIR,
        EdgeKind::Pad => COL_ACCEPT_PAD,
        EdgeKind::SwimSurface | EdgeKind::SwimInterior | EdgeKind::SwimVertical
        | EdgeKind::WaterEntry | EdgeKind::WaterDescent | EdgeKind::HaulOut => COL_ACCEPT_SWIM,
    }
}

/// The color for a REJECTED edge with the given reason.
pub fn reject_color(reason: RejectReason) -> [f32; 4] {
    match reason {
        RejectReason::StepUp | RejectReason::StepDown => COL_REJECT_STEP,
        RejectReason::Grade => COL_REJECT_GRADE,
        RejectReason::Clearance => COL_REJECT_CLEAR,
        RejectReason::NoFloor => COL_REJECT_NOFLOOR,
        RejectReason::Water | RejectReason::HaulOutTooHigh => COL_REJECT_WATER,
    }
}

fn lift(p: [f32; 3]) -> [f32; 3] { [p[0], p[1], p[2] + Z_LIFT] }

fn push_line(v: &mut Vec<OverlayVertex>, a: [f32; 3], b: [f32; 3], color: [f32; 4]) {
    v.push(OverlayVertex { pos: a, color });
    v.push(OverlayVertex { pos: b, color });
}

/// A small 3-axis cross marker (3 line segments).
fn push_cross(v: &mut Vec<OverlayVertex>, p: [f32; 3], half: f32, color: [f32; 4]) {
    push_line(v, [p[0] - half, p[1], p[2]], [p[0] + half, p[1], p[2]], color);
    push_line(v, [p[0], p[1] - half, p[2]], [p[0], p[1] + half, p[2]], color);
    push_line(v, [p[0], p[1], p[2] - half], [p[0], p[1], p[2] + half], color);
}

/// Encode the snapshot as world-space LINE-LIST vertices (pairs). Pure: the snapshot in, the
/// vertices out — see the module docs for why this signature is the anti-drift property itself.
///
/// (The GPU path splits this into [`trace_vertices`] + [`live_vertices`] so the potentially-large
/// plan trace is only re-encoded when the PLAN changes, not on every per-tick publish — the
/// "diagnostic must not perturb what it observes" budget. This function stays the single
/// definition the property tests pin: it is exactly the concatenation of the two.)
pub fn overlay_vertices(snap: &NavDebugSnapshot) -> Vec<OverlayVertex> {
    let mut v = trace_vertices(snap);
    v.extend(live_vertices(snap));
    v
}

/// The plan-trace part: the evaluated edges of the calls whose outcome was RETURNED
/// (`outcome_calls`) — the answer the walker acted on, not a retry that lost. Each edge drawn
/// exactly as recorded; absent edges (unevaluated) draw NOTHING. Changes only when a new plan
/// lands, so the GPU path caches it keyed on the plan's `gen`.
pub fn trace_vertices(snap: &NavDebugSnapshot) -> Vec<OverlayVertex> {
    let mut v: Vec<OverlayVertex> = Vec::new();
    if let Some(plan) = &snap.plan {
        let (o0, o1) = plan.trace.outcome_calls;
        for call in plan.trace.calls.get(o0..o1).unwrap_or(&[]) {
            for e in &call.edges {
                let color = match e.verdict {
                    EdgeVerdict::Accepted { kind } => accept_color(kind),
                    EdgeVerdict::Rejected { reason } => reject_color(reason),
                };
                push_line(&mut v, lift(e.from), lift(e.to), color);
            }
        }
    }
    v
}

/// The small per-tick part: committed routes, goal, player, clearance sample, pads.
pub fn live_vertices(snap: &NavDebugSnapshot) -> Vec<OverlayVertex> {
    let mut v: Vec<OverlayVertex> = Vec::new();

    // The COMMITTED coarse route (#246: the walker's actual plan, verbatim) + the fine plan.
    for w in snap.committed_coarse.windows(2) {
        push_line(&mut v, lift(w[0]), lift(w[1]), COL_COARSE_ROUTE);
    }
    for w in snap.committed_fine.windows(2) {
        push_line(&mut v, lift(w[0]), lift(w[1]), COL_FINE_ROUTE);
    }

    // Goal beacon: a tall vertical line + a cross, so the destination is findable at range.
    if let Some(g) = snap.goal {
        push_line(&mut v, g, [g[0], g[1], g[2] + 30.0], COL_GOAL);
        push_cross(&mut v, lift(g), 2.0, COL_GOAL);
    }

    // Player marker.
    push_cross(&mut v, lift(snap.player), 1.5, COL_PLAYER);

    // The live clearance sample, drawn AT its own recorded position (`at` — where the probe was
    //    really taken, which may lag the player by a few ticks): wall spokes shaded by distance
    //    (red = wall at touch range, green = roomy), and the footprint ring per direction.
    if let Some(c) = &snap.clearance {
        let n = c.wall_spokes.len().max(1);
        for (i, &d) in c.wall_spokes.iter().enumerate() {
            let a = (i as f32) / (n as f32) * std::f32::consts::TAU;
            let t = (d / c.cap).clamp(0.0, 1.0);
            let color = [1.0 - t * 0.8, t, 0.15, 0.85];
            let from = lift([c.at[0], c.at[1], c.at[2] + 1.0]);
            let to = lift([c.at[0] + a.cos() * d, c.at[1] + a.sin() * d, c.at[2] + 1.0]);
            push_line(&mut v, from, to, color);
        }
        let rn = c.footprint_ok.len().max(1);
        for (i, &ok) in c.footprint_ok.iter().enumerate() {
            let a0 = (i as f32) / (rn as f32) * std::f32::consts::TAU;
            let a1 = ((i as f32) + 1.0) / (rn as f32) * std::f32::consts::TAU;
            let color = if ok { COL_RING_OK } else { COL_RING_BLOCKED };
            let z = c.at[2] + 0.6;
            push_line(&mut v,
                [c.at[0] + a0.cos() * c.footprint_radius, c.at[1] + a0.sin() * c.footprint_radius, z + Z_LIFT],
                [c.at[0] + a1.cos() * c.footprint_radius, c.at[1] + a1.sin() * c.footprint_radius, z + Z_LIFT],
                color);
        }
    }

    // Pads with a usable advertised destination: source → dest link + markers. Pads whose state
    //    is Unknown/AdvertisedUnusable carry no drawable geometry (their positions are unknown or
    //    refused) — honesty by omission, matching the endpoint's full report.
    for pad in &snap.pads {
        if let PadKnowledge::AdvertisedUsable { source, dest } = pad.knowledge {
            push_line(&mut v, lift(source), lift(dest), COL_ACCEPT_PAD);
            push_cross(&mut v, lift(source), 1.5, COL_ACCEPT_PAD);
            push_cross(&mut v, lift(dest), 1.5, COL_ACCEPT_PAD);
        }
    }

    v
}

// ─────────────────────────────── GPU side ───────────────────────────────

/// One growable line-list vertex buffer + the cache key of its current contents.
#[derive(Default)]
pub struct OverlayBuf {
    pub vbuf:     Option<wgpu::Buffer>,
    pub capacity: usize, // vertices the buffer can hold
    pub count:    u32,   // vertices to draw this frame
    pub key:      u64,   // cache key of the encoded contents (0 = empty/stale)
}

impl OverlayBuf {
    /// Upload `verts` if `key` differs from what the buffer holds, growing it as needed.
    fn refresh(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, label: &str,
               key: u64, verts: &[OverlayVertex]) {
        if self.key == key { return; }
        self.key = key;
        self.count = verts.len() as u32;
        if verts.is_empty() { return; }
        if self.vbuf.is_none() || self.capacity < verts.len() {
            let capacity = verts.len().next_power_of_two().max(1024);
            self.vbuf = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: (capacity * std::mem::size_of::<OverlayVertex>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            self.capacity = capacity;
        }
        queue.write_buffer(self.vbuf.as_ref().unwrap(), 0, bytemuck::cast_slice(verts));
    }
}

/// The overlay's GPU state, split so the DIAGNOSTIC never perturbs what it observes (#608's
/// frame-rate regression requirement):
/// * `trace`: the plan's evaluated edges — potentially tens of thousands of lines, re-encoded
///   only when a NEW PLAN lands (keyed on the plan's `gen`), not on every per-tick publish;
/// * `live`: routes/goal/player/clearance/pads — a few hundred vertices, re-encoded per publish
///   (keyed on the snapshot's `seq`).
#[derive(Default)]
pub struct NavOverlayGpu {
    pub trace: OverlayBuf,
    pub live:  OverlayBuf,
}

/// Encode the overlay pass. No-op when the scene carries no snapshot (overlay toggled off, or
/// nothing published). Depth-tested against the scene's depth buffer (`LessEqual`, write off) —
/// overlay geometry behind a wall is hidden by it, which is the whole point (#608).
pub fn encode_nav_overlay_pass(
    r:       &mut crate::renderer::EqRenderer,
    encoder: &mut wgpu::CommandEncoder,
    view:    &wgpu::TextureView,
    scene:   &crate::scene::SceneState,
) {
    let Some(snap) = &scene.nav_debug else {
        // Toggled off: draw nothing and force a re-encode when toggled back on.
        r.nav_overlay.trace.key = 0;
        r.nav_overlay.trace.count = 0;
        r.nav_overlay.live.key = 0;
        r.nav_overlay.live.count = 0;
        return;
    };
    // The trace buffer is keyed on the PLAN's generation (0 = no plan): the expensive encode runs
    // once per plan. The live buffer is keyed on the publish seq (never 0: seq starts at 1).
    let plan_key = snap.plan.as_ref().map(|p| p.gen.max(1)).unwrap_or(0);
    if plan_key == 0 {
        r.nav_overlay.trace.key = 0;
        r.nav_overlay.trace.count = 0;
    } else {
        let (device, queue) = (&r.device, &r.queue);
        r.nav_overlay.trace.refresh(device, queue, "nav_overlay_trace", plan_key,
            &trace_vertices(snap));
    }
    {
        let (device, queue) = (&r.device, &r.queue);
        r.nav_overlay.live.refresh(device, queue, "nav_overlay_live", snap.seq.max(1),
            &live_vertices(snap));
    }
    if r.nav_overlay.trace.count == 0 && r.nav_overlay.live.count == 0 { return; }

    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("nav_overlay"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view, resolve_target: None,
            ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
        })],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: &r.depth_view,
            depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store }),
            stencil_ops: None,
        }),
        timestamp_writes: None, occlusion_query_set: None,
    });
    pass.set_pipeline(&r.pipelines.nav_debug);
    pass.set_bind_group(0, &r.camera_uniform.bind_group, &[]);
    for buf in [&r.nav_overlay.trace, &r.nav_overlay.live] {
        if let (Some(vbuf), count) = (&buf.vbuf, buf.count) {
            if count > 0 {
                pass.set_vertex_buffer(0, vbuf.slice(..));
                pass.draw(0..count, 0..1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eqoxide_nav::diagnostics::*;
    use std::sync::Arc;

    fn snap_with(trace: SearchTrace) -> NavDebugSnapshot {
        NavDebugSnapshot {
            seq: 1,
            zone_model_loaded: true,
            nav_state: "navigating".into(),
            nav_reason: None,
            player: [0.0, 0.0, 0.0],
            goal: None,
            committed_coarse: vec![],
            committed_fine: vec![],
            plan: Some(Arc::new(PlanDebug {
                gen: 1, start: [0.0; 3], goal: [0.0; 3],
                outcome: "route".into(), reason: "route".into(), route_len: 0,
                plan_ms: 0, tight: false, goal_snapped: false, trace,
            })),
            pads: vec![],
            clearance: None,
            water: None,
        }
    }

    fn segments_of(color: [f32; 4], verts: &[OverlayVertex]) -> usize {
        assert_eq!(verts.len() % 2, 0, "line list = vertex pairs");
        verts.chunks(2).filter(|c| c[0].color == color && c[1].color == color).count()
    }

    /// **THE #608 CONSUMER PROPERTY, pinned.** Every edge drawn "accepted" is one the snapshot
    /// says the planner accepted; every "rejected reason" line is one it rejected for that reason;
    /// and the counts match 1:1 — nothing invented, nothing dropped, nothing re-judged. The
    /// fabricated trace here is deliberately geometry-free (there IS no geometry): the encoder has
    /// no collision access by signature, so it cannot "correct" a verdict, and this test goes RED
    /// if a second derivation (or a color-map swap — the mutation check) is ever introduced.
    #[test]
    fn drawn_verdicts_are_exactly_the_published_verdicts() {
        let mut trace = SearchTrace::with_budget(64);
        trace.begin_call(2.0, 8.0, true);
        trace.edge([0.0; 3], [8.0, 0.0, 0.0], EdgeVerdict::Accepted { kind: EdgeKind::Walk });
        trace.edge([0.0; 3], [0.0, 8.0, 0.0], EdgeVerdict::Accepted { kind: EdgeKind::Walk });
        trace.edge([0.0; 3], [8.0, 8.0, 0.0], EdgeVerdict::Rejected { reason: RejectReason::Grade });
        trace.edge([0.0; 3], [-8.0, 0.0, 0.0], EdgeVerdict::Rejected { reason: RejectReason::Clearance });
        trace.outcome_calls = (0, 1);
        let verts = overlay_vertices(&snap_with(trace));

        assert_eq!(segments_of(COL_ACCEPT_WALK, &verts), 2,
            "exactly the two published accepted-walk edges may be drawn accepted");
        assert_eq!(segments_of(COL_REJECT_GRADE, &verts), 1);
        assert_eq!(segments_of(COL_REJECT_CLEAR, &verts), 1);
        // And the drawn segments are the published endpoints (lifted by the documented Z offset).
        let walk: Vec<_> = verts.chunks(2).filter(|c| c[0].color == COL_ACCEPT_WALK).collect();
        assert_eq!(walk[0][0].pos, [0.0, 0.0, Z_LIFT]);
        assert_eq!(walk[0][1].pos, [8.0, 0.0, Z_LIFT]);
    }

    /// **Absence means unevaluated — nothing is drawn for it.** An empty trace draws no edge
    /// segments at all (only the player marker), and calls OUTSIDE `outcome_calls` (a generous
    /// pass that lost, a ring retry) are not drawn as the answer.
    #[test]
    fn unevaluated_cells_draw_nothing_and_losing_calls_are_not_the_answer() {
        // Empty trace → only the player cross (3 segments).
        let verts = overlay_vertices(&snap_with(SearchTrace::with_budget(8)));
        assert_eq!(verts.len(), 6, "an empty trace draws only the player marker — no invented coverage");

        // An edge recorded in a call OUTSIDE the outcome range must not be drawn.
        let mut trace = SearchTrace::with_budget(8);
        trace.begin_call(2.0, 8.0, true); // the generous pass that lost
        trace.edge([0.0; 3], [8.0, 0.0, 0.0], EdgeVerdict::Rejected { reason: RejectReason::Clearance });
        trace.begin_call(1.0, 8.0, true); // the minimum pass that answered
        trace.edge([0.0; 3], [8.0, 0.0, 0.0], EdgeVerdict::Accepted { kind: EdgeKind::Walk });
        trace.outcome_calls = (1, 2);
        let verts = overlay_vertices(&snap_with(trace));
        assert_eq!(segments_of(COL_ACCEPT_WALK, &verts), 1);
        assert_eq!(segments_of(COL_REJECT_CLEAR, &verts), 0,
            "a losing call's edges are not part of the answer being drawn");
    }

    /// The committed routes are drawn verbatim from the snapshot — the #246 property on the
    /// consumer side. A fabricated route through nowhere is still drawn: the walker's actual
    /// committed plan is the truth being displayed, not this module's opinion of it.
    #[test]
    fn committed_routes_are_drawn_verbatim() {
        let mut snap = snap_with(SearchTrace::with_budget(8));
        snap.plan = None;
        snap.committed_coarse = vec![[0.0, 0.0, 0.0], [8.0, 0.0, 0.0], [16.0, 0.0, 0.0]];
        snap.committed_fine = vec![[0.0, 0.0, 0.0], [2.0, 0.0, 0.0]];
        snap.goal = Some([16.0, 0.0, 0.0]);
        let verts = overlay_vertices(&snap);
        assert_eq!(segments_of(COL_COARSE_ROUTE, &verts), 2, "2 segments for 3 committed waypoints");
        assert_eq!(segments_of(COL_FINE_ROUTE, &verts), 1);
        assert!(segments_of(COL_GOAL, &verts) >= 1, "the goal beacon is drawn");
    }

    /// The vertex type is what the GPU pipeline expects: 28 bytes, pos then color.
    #[test]
    fn overlay_vertex_layout_is_pos3_color4() {
        assert_eq!(std::mem::size_of::<OverlayVertex>(), 28);
        let v = OverlayVertex { pos: [1.0, 2.0, 3.0], color: [0.5; 4] };
        let bytes: &[u8] = bytemuck::bytes_of(&v);
        assert_eq!(bytes.len(), 28);
        assert_eq!(&bytes[0..4], &1.0f32.to_le_bytes());
    }
}
