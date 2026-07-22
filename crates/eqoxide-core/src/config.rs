//! Application and EQ connection configuration, loaded from YAML files.

use std::path::{Path, PathBuf};

// Test-only override for the directory [`AppConfig::load`]'s `./config.yaml`
// fallback searches (see [`AppConfig::load_fallback_dir`]). `thread_local`, not a
// process-global: each test thread gets its own cell, so a test can drive the
// real `load()` call site end-to-end without `std::env::set_current_dir` — which
// mutates every relative-path lookup in the whole process — and without any risk
// of leaking into another test running concurrently on a different thread (#604
// F1, review of PR #611).
#[cfg(test)]
thread_local! {
    static LOAD_FALLBACK_DIR_OVERRIDE: std::cell::RefCell<Option<PathBuf>> =
        std::cell::RefCell::new(None);
}

/// Directory where eqoxide stores its config and cached per-character login
/// credentials: `~/.config/eqoxide/` (honoring `XDG_CONFIG_HOME` via the `dirs`
/// crate). Created on demand; on failure we fall back to the working directory.
pub fn config_dir() -> PathBuf {
    let dir = dirs::config_dir()
        .map(|c| c.join("eqoxide"))
        .unwrap_or_else(|| PathBuf::from("."));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("config: could not create {} ({e}), using cwd", dir.display());
        return PathBuf::from(".");
    }
    dir
}

/// Keys understood inside a `renderer:` block. Anything else is reported at
/// startup rather than silently dropped (#597) — a config value that is quietly
/// discarded is exactly the defect this module was fixed for.
pub const KNOWN_RENDERER_KEYS: &[&str] =
    &["assets_path", "models_path", "asset_server_url", "eq_ui_dir"];

/// Where an effective setting came from: the label of the config layer that
/// supplied it, or `None` when nothing did and the built-in default is in force.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// Supplied by a config file (label = its path, or `<inline>` in tests).
    File(String),
    /// No config file set this key; the compiled-in default is in force.
    Default,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::File(p) => write!(f, "{p}"),
            Source::Default => write!(f, "<built-in default>"),
        }
    }
}

/// Renderer / HTTP server settings.
///
/// # Precedence (#597)
///
/// Values are merged **key by key** from up to two layers, later wins:
///
/// 1. the global `~/.config/eqoxide/config.yaml` (falling back to `./config.yaml`);
/// 2. the per-character file selected by `--config <name|path>`, when that
///    resolves to a *different* file than (1).
///
/// So a per-character file that sets only `renderer.asset_server_url` still
/// inherits `assets_path`/`models_path`/`eq_ui_dir`/`http_port` from the global
/// file. With no `--config` there is only layer (1) and behavior is unchanged.
///
/// [`AppConfig::sources`] records which file supplied each effective value and
/// [`AppConfig::disclose`] logs that at startup, so a wrong value is *visible*
/// rather than inferred later from an empty world.
pub struct AppConfig {
    pub assets_path: PathBuf,
    pub models_path: PathBuf,
    pub http_port: u16,
    pub asset_server_url: String,
    /// Directory holding the native client's UI atlases (`uifiles/default`),
    /// for item/spell icons in the window system. Optional — UI falls back to
    /// text when unset and the default location is absent (#162).
    pub eq_ui_dir: Option<String>,
    /// Provenance of each effective value: `(field name, source)`. Logged by
    /// [`AppConfig::disclose`].
    pub sources: Vec<(&'static str, Source)>,
    /// Problems found while merging (unreadable file, unknown `renderer:` key,
    /// misplaced `http_port`). Emitted by [`AppConfig::disclose`]; collected
    /// rather than logged inline so tests can assert on them.
    pub warnings: Vec<String>,
}

impl AppConfig {
    /// Load renderer/HTTP settings, honoring `--config`.
    ///
    /// `config_path` is `Some(path)` **only when the user actually passed
    /// `--config`** (the path being what [`LoginConfig::resolve_path`] resolved it
    /// to); that file is layered on top of the global `config.yaml`. `None` means
    /// no `--config` was given: only the global file is read, with no second layer
    /// and no warning about one — the same merge/warning behavior as pre-#597,
    /// including the `./config.yaml` fallback. One cosmetic difference from
    /// pre-#604: the fallback's disclosed source string is now `./config.yaml`
    /// rather than bare `config.yaml` (see [`load_fallback_dir`](Self::load_fallback_dir));
    /// that string's only consumer is the startup disclosure log line, and
    /// [`same_file`] is unaffected because it canonicalizes both sides before
    /// comparing.
    pub fn load(config_path: Option<&Path>) -> Self {
        Self::load_with_fallback_dir(config_path, &Self::load_fallback_dir())
    }

    /// The directory [`load`](Self::load) searches for the `./config.yaml`
    /// back-compat fallback: always `.` (the real process cwd) in production.
    ///
    /// `load_with_fallback_dir`'s own tests (below) exercise the merge/fallback
    /// logic thoroughly by calling it directly — but none of them called `load()`
    /// itself, so a mutation of *its* hardcoded fallback argument went completely
    /// undetected (#604 F1, review of PR #611: reverting this to a hardcoded `.`
    /// literal left the whole `eqoxide-core` suite green, 113/113). This method is
    /// the fix: the ONLY thing that changes between production and test is this one
    /// call, via a `thread_local` override (`LOAD_FALLBACK_DIR_OVERRIDE`) rather
    /// than `std::env::set_current_dir` — so `load_wires_the_fallback_dir_through_
    /// to_load_with_fallback_dir` below can drive the real `load()` call site
    /// end-to-end without mutating the process's actual working directory.
    #[cfg(test)]
    fn load_fallback_dir() -> PathBuf {
        LOAD_FALLBACK_DIR_OVERRIDE
            .with(|o| o.borrow().clone())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// Non-test build: always the real process cwd. See the `#[cfg(test)]` overload
    /// above for why this indirection exists.
    #[cfg(not(test))]
    fn load_fallback_dir() -> PathBuf {
        PathBuf::from(".")
    }

    /// Test-only: run `f` with [`load`](Self::load)'s fallback directory overridden
    /// to `dir`, on this thread only (see [`load_fallback_dir`](Self::load_fallback_dir)
    /// and the `LOAD_FALLBACK_DIR_OVERRIDE` thread_local above it).
    #[cfg(test)]
    fn with_load_fallback_dir_for_test<R>(dir: &Path, f: impl FnOnce() -> R) -> R {
        LOAD_FALLBACK_DIR_OVERRIDE.with(|o| *o.borrow_mut() = Some(dir.to_path_buf()));
        let out = f();
        LOAD_FALLBACK_DIR_OVERRIDE.with(|o| *o.borrow_mut() = None);
        out
    }

    /// Same as [`load`](Self::load), but takes the `./config.yaml` fallback
    /// directory as an explicit parameter instead of resolving it via
    /// [`load_fallback_dir`](Self::load_fallback_dir). Behaviourally identical to
    /// `load()` for the merge/warning logic; tests use this directly to exercise
    /// that logic with an isolated temp dir standing in for the fallback
    /// directory, without touching the real process cwd.
    fn load_with_fallback_dir(config_path: Option<&Path>, fallback_dir: &Path) -> Self {
        let mut layers: Vec<(String, String)> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();

        // Layer 1: the global config. Prefer ~/.config/eqoxide/config.yaml; fall
        // back to <fallback_dir>/config.yaml for back-compat (`.` in production).
        let primary = config_dir().join("config.yaml");
        let fallback = fallback_dir.join("config.yaml");
        let mut global_path: Option<PathBuf> = None;
        if let Ok(t) = std::fs::read_to_string(&primary) {
            global_path = Some(primary.clone());
            layers.push((primary.display().to_string(), t));
        } else if let Ok(t) = std::fs::read_to_string(&fallback) {
            global_path = Some(fallback.clone());
            layers.push((fallback.display().to_string(), t));
        }

        // Layer 2: the per-character file from --config. Skipped entirely when
        // --config was absent (`None`), and when it resolved to the very file that
        // already supplied layer 1 (otherwise we'd read the same file twice).
        if let Some(config_path) = config_path {
            let is_global = global_path.as_deref().is_some_and(|g| same_file(g, config_path));
            if !is_global {
                match std::fs::read_to_string(config_path) {
                    Ok(t) => layers.push((config_path.display().to_string(), t)),
                    Err(e) => warnings.push(format!(
                        "config: --config named {} but it could not be read ({e}) — renderer \
                         settings come from the global config only",
                        config_path.display()
                    )),
                }
            }
        }

        if layers.is_empty() {
            warnings.push(format!(
                "config: no config.yaml in {} or cwd — using built-in renderer defaults",
                primary.display()
            ));
        }

        let mut cfg = Self::from_layers(&layers);
        // Load-level warnings come first, then per-layer parse warnings.
        warnings.append(&mut cfg.warnings);
        cfg.warnings = warnings;
        cfg
    }

    /// Parse a single YAML document as the only layer. Used by tests and by
    /// callers that already hold the text.
    pub fn from_yaml_str(cfg_text: &str) -> Self {
        Self::from_layers(&[("<inline>".to_string(), cfg_text.to_string())])
    }

    /// Merge layers key by key; the LAST layer that supplies a key wins.
    pub fn from_layers(layers: &[(String, String)]) -> Self {
        let mut warnings = Vec::new();
        let parsed: Vec<(&str, serde_yaml::Value)> = layers
            .iter()
            .map(|(label, text)| {
                let v = match serde_yaml::from_str::<serde_yaml::Value>(text) {
                    Ok(v) => v,
                    Err(e) => {
                        warnings.push(format!("config {label}: YAML parse error ({e}) — file ignored"));
                        serde_yaml::Value::Null
                    }
                };
                (label.as_str(), v)
            })
            .collect();

        // Report MISPLACED and UNKNOWN keys instead of dropping them. Four shapes,
        // all of which used to (or would) vanish without a trace:
        //   renderer.<unknown>   — a typo'd or unsupported renderer key
        //   renderer.http_port   — a top-level key nested one level too deep
        //   <renderer key> at top level — the pre-#597 docs showed exactly this
        //                          layout, so configs written against them have it
        //   renderer: <not a map> — e.g. `renderer: [1, 2]`. `renderer:` null and
        //                          `renderer: {}` are legitimate no-ops (nothing to
        //                          merge) and stay silent; any OTHER shape is a
        //                          config author's mistake and warns (#604).
        for (label, cfg) in &parsed {
            match cfg.get("renderer") {
                Some(serde_yaml::Value::Mapping(m)) => {
                    for k in m.keys().filter_map(|k| k.as_str()) {
                        if KNOWN_RENDERER_KEYS.contains(&k) {
                            continue;
                        }
                        if k == "http_port" {
                            warnings.push(format!(
                                "config {label}: 'http_port' must be a TOP-LEVEL key, not under 'renderer:' \
                                 — this one is IGNORED"
                            ));
                        } else {
                            warnings.push(format!(
                                "config {label}: unknown key 'renderer.{k}' is IGNORED (known keys: {})",
                                KNOWN_RENDERER_KEYS.join(", ")
                            ));
                        }
                    }
                }
                Some(serde_yaml::Value::Null) | None => {}
                Some(other) => {
                    warnings.push(format!(
                        "config {label}: 'renderer:' is present but is {}, not a map — IGNORED \
                         (no renderer settings come from this layer; known keys: {})",
                        yaml_type(other),
                        KNOWN_RENDERER_KEYS.join(", ")
                    ));
                }
            }
            if let serde_yaml::Value::Mapping(m) = cfg {
                for k in m.keys().filter_map(|k| k.as_str()) {
                    if KNOWN_RENDERER_KEYS.contains(&k) {
                        warnings.push(format!(
                            "config {label}: '{k}' must be nested under 'renderer:', not at the top \
                             level — this one is IGNORED"
                        ));
                    }
                }
            }
        }

        // `pick` walks the layers in order and keeps the last hit, so a later
        // (per-character) layer overrides an earlier (global) one key by key.
        //
        // A key that is PRESENT but UNUSABLE (wrong YAML type, out of range) is
        // deliberately NOT a hit: it warns and the previous layer stands. That
        // keeps the disclosed source honest — the value we print and the file we
        // attribute it to always come from the same layer (#597 F1: recording the
        // hit before parsing let `disclose()` blame a file for a value it did not
        // contain).
        fn pick<T>(
            parsed: &[(&str, serde_yaml::Value)],
            warnings: &mut Vec<String>,
            get: impl Fn(&serde_yaml::Value, &str, &mut Vec<String>) -> Option<T>,
        ) -> Option<(T, Source)> {
            let mut found = None;
            for (label, cfg) in parsed {
                if let Some(v) = get(cfg, label, warnings) {
                    found = Some((v, Source::File((*label).to_string())));
                }
            }
            found
        }

        // A `renderer.<key>` that exists but is not a string (null, a number, a
        // nested map) is a silent drop waiting to happen — warn and skip the layer.
        // An explicitly EMPTY string is a real value: it overrides and is disclosed.
        fn renderer_str(
            key: &'static str,
        ) -> impl Fn(&serde_yaml::Value, &str, &mut Vec<String>) -> Option<String> {
            move |cfg, label, warns| {
                let v = cfg.get("renderer")?.get(key)?;
                match v.as_str() {
                    Some(s) => Some(s.to_string()),
                    None => {
                        warns.push(format!(
                            "config {label}: 'renderer.{key}' is present but is {}, not a string \
                             — IGNORED (the previous layer or the built-in default stands)",
                            yaml_type(v)
                        ));
                        None
                    }
                }
            }
        }

        let mut sources: Vec<(&'static str, Source)> = Vec::new();
        let mut src = |field: &'static str, s: Option<Source>| {
            sources.push((field, s.unwrap_or(Source::Default)));
        };

        let assets_path_hit = pick(&parsed, &mut warnings, renderer_str("assets_path"));
        let assets_path = assets_path_hit
            .as_ref()
            .map(|(p, _)| PathBuf::from(shellexpand::tilde(p).into_owned()))
            .unwrap_or_else(|| PathBuf::from("eq_assets"));
        src("assets_path", assets_path_hit.map(|(_, s)| s));

        let models_path_hit = pick(&parsed, &mut warnings, renderer_str("models_path"));
        let models_path = models_path_hit
            .as_ref()
            .map(|(p, _)| PathBuf::from(shellexpand::tilde(p).into_owned()))
            .unwrap_or_else(|| PathBuf::from("eqoxide/assets/models"));
        src("models_path", models_path_hit.map(|(_, s)| s));

        // `http_port` is a TOP-LEVEL key (not under `renderer:`), and is only the
        // BASE port: the HTTP server still scans upward for a free port and prints
        // `API_PORT=<bound>`, and `--api-port N` still overrides it exactly.
        // Parsed to u16 INSIDE the picker so an out-of-range or non-integer value
        // is not a hit (see the note on `pick`).
        let http_port_hit = pick(&parsed, &mut warnings, |cfg, label, warns| {
            let v = cfg.get("http_port")?;
            match v.as_u64() {
                Some(n) if (1..=u16::MAX as u64).contains(&n) => Some(n as u16),
                Some(n) => {
                    warns.push(format!(
                        "config {label}: 'http_port: {n}' is out of range (1..=65535) — IGNORED \
                         (the previous layer or the built-in default stands)"
                    ));
                    None
                }
                None => {
                    warns.push(format!(
                        "config {label}: 'http_port' is present but is {}, not an integer — IGNORED \
                         (the previous layer or the built-in default stands)",
                        yaml_type(v)
                    ));
                    None
                }
            }
        });
        let http_port = http_port_hit.as_ref().map(|(v, _)| *v).unwrap_or(8765);
        src("http_port", http_port_hit.map(|(_, s)| s));

        let url_hit = pick(&parsed, &mut warnings, renderer_str("asset_server_url"));
        let asset_server_url = url_hit
            .as_ref()
            .map(|(v, _)| v.clone())
            .unwrap_or_else(|| "http://localhost:8088".to_string());
        src("asset_server_url", url_hit.map(|(_, s)| s));

        let ui_hit = pick(&parsed, &mut warnings, renderer_str("eq_ui_dir"));
        let eq_ui_dir = ui_hit.as_ref().map(|(v, _)| v.clone());
        src("eq_ui_dir", ui_hit.map(|(_, s)| s));

        AppConfig {
            assets_path,
            models_path,
            http_port,
            asset_server_url,
            eq_ui_dir,
            sources,
            warnings,
        }
    }

    /// Look up where an effective field came from (for logging/tests).
    pub fn source_of(&self, field: &str) -> Source {
        self.sources
            .iter()
            .find(|(f, _)| *f == field)
            .map(|(_, s)| s.clone())
            .unwrap_or(Source::Default)
    }

    /// The exact disclosure lines [`disclose`](Self::disclose) logs, in order.
    /// Split out so the disclosure itself is testable: a snapshot over this makes
    /// a swapped/mislabelled field a RED test rather than a silent lie in a log.
    pub fn disclose_lines(&self) -> Vec<String> {
        // `eq_ui_dir` is the one key with a consumer that can override it after the
        // fact: `eqoxide-ui`'s icon loader prefers $EQ_UI_DIR (and $EQ_SPELL_ICONS_DIR
        // when nothing else is set) over the config value, and falls back to a default
        // atlas dir when all are unset. Say so rather than let the line imply the
        // config value is necessarily what the UI uses (#597 F5). The atlas dir
        // actually chosen is logged by that loader as `ui icons: using atlas dir …`.
        let ui_note = match (std::env::var("EQ_UI_DIR"), self.eq_ui_dir.is_some()) {
            (Ok(v), _) => format!(" — OVERRIDDEN by $EQ_UI_DIR={v}; the UI uses that"),
            (Err(_), true) => String::new(),
            (Err(_), false) => match std::env::var("EQ_SPELL_ICONS_DIR") {
                Ok(v) => format!(" — $EQ_SPELL_ICONS_DIR={v} is in force instead"),
                Err(_) => " — the UI may still fall back to a default atlas dir; \
                            see the 'ui icons:' line"
                    .to_string(),
            },
        };
        vec![
            format!(
                "config: effective asset_server_url={} (from {})",
                self.asset_server_url,
                self.source_of("asset_server_url")
            ),
            format!(
                "config: effective http_port={} (from {}) — base port; actual bound port is logged as API_PORT=",
                self.http_port,
                self.source_of("http_port")
            ),
            format!(
                "config: effective assets_path={} (from {})",
                self.assets_path.display(),
                self.source_of("assets_path")
            ),
            format!(
                "config: effective models_path={} (from {})",
                self.models_path.display(),
                self.source_of("models_path")
            ),
            format!(
                "config: effective eq_ui_dir={} (from {}){ui_note}",
                self.eq_ui_dir.as_deref().unwrap_or("<unset>"),
                self.source_of("eq_ui_dir")
            ),
        ]
    }

    /// Log the effective renderer/HTTP settings and the file each came from, plus
    /// any merge warnings. Called once at startup: a wrong `asset_server_url` must
    /// be readable in the log, never inferred later from a world with no geometry.
    pub fn disclose(&self) {
        for w in &self.warnings {
            tracing::warn!("{w}");
        }
        for line in self.disclose_lines() {
            tracing::info!("{line}");
        }
    }
}

/// Human name for a YAML value's type, for "present but wrong type" warnings.
fn yaml_type(v: &serde_yaml::Value) -> &'static str {
    match v {
        serde_yaml::Value::Null => "null",
        serde_yaml::Value::Bool(_) => "a boolean",
        serde_yaml::Value::Number(_) => "a number",
        serde_yaml::Value::String(_) => "a string",
        serde_yaml::Value::Sequence(_) => "a list",
        serde_yaml::Value::Mapping(_) => "a map",
        _ => "an unsupported value",
    }
}

/// True when both paths denote the same existing file (falls back to a literal
/// comparison when either cannot be canonicalized).
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// EQ login credentials and server addresses, loaded from a per-character config
/// file in `~/.config/eqoxide/`. Selected via the `--config <value>` CLI flag (see
/// [`LoginConfig::resolve_path`]); defaults to `~/.config/eqoxide/config.yaml`.
pub struct LoginConfig {
    pub login_host:     String,
    pub login_port:     u16,
    pub world_port:     u16,
    pub username:       String,
    pub password:       String,
    pub character_name: String,
    /// When set and `character_name` is not already on the account's
    /// char-select list, the client creates the character via the normal
    /// OP_ApproveName → OP_CharacterCreate handshake before entering world.
    pub create:         Option<CharacterCreate>,
}

/// Appearance + stat allocation for creating a new character. Mirrors the
/// fields the native Titanium character-creation screen sends in
/// CharCreate_Struct. Stats must satisfy the server's per-class/race floors
/// and total; cosmetic fields default to 0.
#[derive(Clone, Debug)]
pub struct CharacterCreate {
    pub race:       u32,
    pub class:      u32,
    pub gender:     u32, // 0=male, 1=female
    pub deity:      u32,
    pub start_zone: u32, // start-city ZONE_ID, NOT a StartZoneIndex. RoF2 validates this against
                         // char_create_combinations.start_zone (a zone_id) via CheckCharCreateInfoSoF,
                         // so it must be the chosen start city's zoneidnumber valid for this
                         // race/class/deity (e.g. 42 = neriakc or 394 = crescent for a Dark Elf
                         // Necromancer). A Titanium StartZoneIndex (0..13) is rejected. See eqoxide#5.
    pub str_:       u32,
    pub sta:        u32,
    pub agi:        u32,
    pub dex:        u32,
    pub wis:        u32,
    pub int_:       u32,
    pub cha:        u32,
    pub face:       u32,
    pub hairstyle:  u32,
    pub haircolor:  u32,
    pub beard:      u32,
    pub beardcolor: u32,
    pub eyecolor1:  u32,
    pub eyecolor2:  u32,
}

impl CharacterCreate {
    fn from_yaml(cfg: &serde_yaml::Value) -> Option<Self> {
        let c = cfg.get("character_create")?;
        let u = |k: &str, d: u32| c.get(k).and_then(|x| x.as_u64()).map(|n| n as u32).unwrap_or(d);
        Some(CharacterCreate {
            race:       u("race", 0),
            class:      u("class", 0),
            gender:     u("gender", 0),
            deity:      u("deity", 0),
            start_zone: u("start_zone", 0),
            str_:       u("str", 0),
            sta:        u("sta", 0),
            agi:        u("agi", 0),
            dex:        u("dex", 0),
            wis:        u("wis", 0),
            int_:       u("int", 0),
            cha:        u("cha", 0),
            face:       u("face", 0),
            hairstyle:  u("hairstyle", 0),
            haircolor:  u("haircolor", 0),
            beard:      u("beard", 0),
            beardcolor: u("beardcolor", 0),
            eyecolor1:  u("eyecolor1", 0),
            eyecolor2:  u("eyecolor2", 0),
        })
    }
}

impl LoginConfig {
    /// Resolve the `--config <value>` argument to a config-file path:
    /// - `None` → `~/.config/eqoxide/config.yaml`
    /// - a value containing a path separator (or `~`) → used as a literal path
    /// - a bare filename ending in `.yaml`/`.yml` → looked up in `~/.config/eqoxide/`
    /// - any other bare word (a profile name) → `~/.config/eqoxide/config-<name>.yaml`
    pub fn resolve_path(arg: Option<&str>) -> PathBuf {
        let Some(v) = arg else { return config_dir().join("config.yaml"); };
        let expanded = shellexpand::tilde(v).into_owned();
        if expanded.contains('/') {
            PathBuf::from(expanded)
        } else if expanded.ends_with(".yaml") || expanded.ends_with(".yml") {
            config_dir().join(expanded)
        } else {
            config_dir().join(format!("config-{expanded}.yaml"))
        }
    }

    pub fn load(path: &Path) -> Self {
        let cfg_text = std::fs::read_to_string(path).unwrap_or_default();
        let cfg: serde_yaml::Value =
            serde_yaml::from_str(&cfg_text).unwrap_or(serde_yaml::Value::Null);

        LoginConfig {
            login_host: cfg
                .get("server").and_then(|s| s.get("login_host")).and_then(|v| v.as_str())
                .unwrap_or("127.0.0.1").to_string(),
            login_port: cfg
                .get("server").and_then(|s| s.get("login_port")).and_then(|v| v.as_u64())
                // 5999 = EQEmu loginserver's SoD/RoF2 listener. eqoxide is a RoF2 client, so it
                // speaks the SoD login protocol, not the legacy Titanium listener on 5998 (#404).
                .unwrap_or(5999) as u16,
            world_port: cfg
                .get("server").and_then(|s| s.get("world_port")).and_then(|v| v.as_u64())
                .unwrap_or(9000) as u16,
            username: cfg
                .get("account").and_then(|a| a.get("username")).and_then(|v| v.as_str())
                .unwrap_or("testuser").to_string(),
            password: cfg
                .get("account").and_then(|a| a.get("password")).and_then(|v| v.as_str())
                .unwrap_or("REDACTED").to_string(),
            character_name: cfg
                .get("account").and_then(|a| a.get("character_name")).and_then(|v| v.as_str())
                .unwrap_or("Aiquestbot").to_string(),
            create: CharacterCreate::from_yaml(&cfg),
        }
    }
}

#[cfg(test)]
mod b1_config_tests {
    use super::*;

    #[test]
    fn asset_server_url_defaults_and_overrides() {
        let yaml_default = "renderer:\n  assets_path: /x\n";
        let cfg = AppConfig::from_yaml_str(yaml_default);
        assert_eq!(cfg.asset_server_url, "http://localhost:8088");

        let yaml_set = "renderer:\n  asset_server_url: http://host:9999\n";
        let cfg = AppConfig::from_yaml_str(yaml_set);
        assert_eq!(cfg.asset_server_url, "http://host:9999");
    }
}

/// #597: `--config` must select the renderer/HTTP settings too. Before the fix the
/// per-character `renderer:` block was read from disk by nobody — the client accepted
/// the config, reported no error, and used the global file's asset server instead.
#[cfg(test)]
mod config_precedence_tests_597 {
    use super::*;

    fn layers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(l, t)| (l.to_string(), t.to_string())).collect()
    }

    const GLOBAL: &str = "\
renderer:
  assets_path: /global/assets
  models_path: /global/models
  asset_server_url: http://localhost:8088
  eq_ui_dir: /global/ui
http_port: 8765
";

    #[test]
    fn per_character_layer_overrides_global_key_by_key() {
        let cfg = AppConfig::from_layers(&layers(&[
            ("global.yaml", GLOBAL),
            ("config-x.yaml", "renderer:\n  asset_server_url: http://prod-assets:8088\n"),
        ]));
        // Overridden key comes from the per-character file...
        assert_eq!(cfg.asset_server_url, "http://prod-assets:8088");
        assert_eq!(cfg.source_of("asset_server_url"), Source::File("config-x.yaml".into()));
        // ...and every key it does NOT mention still comes from the global file.
        assert_eq!(cfg.assets_path, PathBuf::from("/global/assets"));
        assert_eq!(cfg.source_of("assets_path"), Source::File("global.yaml".into()));
        assert_eq!(cfg.models_path, PathBuf::from("/global/models"));
        assert_eq!(cfg.eq_ui_dir.as_deref(), Some("/global/ui"));
        assert_eq!(cfg.http_port, 8765);
        assert_eq!(cfg.source_of("http_port"), Source::File("global.yaml".into()));
    }

    #[test]
    fn every_key_is_independently_overridable() {
        let over = "\
renderer:
  assets_path: /char/assets
  models_path: /char/models
  asset_server_url: http://char:1234
  eq_ui_dir: /char/ui
http_port: 8795
";
        let cfg = AppConfig::from_layers(&layers(&[("global.yaml", GLOBAL), ("char.yaml", over)]));
        assert_eq!(cfg.assets_path, PathBuf::from("/char/assets"));
        assert_eq!(cfg.models_path, PathBuf::from("/char/models"));
        assert_eq!(cfg.asset_server_url, "http://char:1234");
        assert_eq!(cfg.eq_ui_dir.as_deref(), Some("/char/ui"));
        assert_eq!(cfg.http_port, 8795);
        for f in ["assets_path", "models_path", "asset_server_url", "eq_ui_dir", "http_port"] {
            assert_eq!(cfg.source_of(f), Source::File("char.yaml".into()), "field {f}");
        }
    }

    /// Property: for every subset of keys the per-character file sets, the result is
    /// exactly "per-character where present, global otherwise" — no key leaks the wrong way.
    #[test]
    fn prop_merge_is_per_key_choice_over_all_subsets() {
        let keys = ["assets_path", "models_path", "asset_server_url", "eq_ui_dir"];
        for mask in 0u8..16 {
            let mut over = String::from("renderer:\n");
            for (i, k) in keys.iter().enumerate() {
                if mask & (1 << i) != 0 {
                    over.push_str(&format!("  {k}: /char/{k}\n"));
                }
            }
            let cfg = AppConfig::from_layers(&layers(&[("g.yaml", GLOBAL), ("c.yaml", &over)]));
            let got = |k: &str| -> String {
                match k {
                    "assets_path" => cfg.assets_path.display().to_string(),
                    "models_path" => cfg.models_path.display().to_string(),
                    "asset_server_url" => cfg.asset_server_url.clone(),
                    _ => cfg.eq_ui_dir.clone().unwrap_or_default(),
                }
            };
            for (i, k) in keys.iter().enumerate() {
                let overridden = mask & (1 << i) != 0;
                let want_val = if overridden {
                    format!("/char/{k}")
                } else if *k == "asset_server_url" {
                    "http://localhost:8088".to_string()
                } else if *k == "eq_ui_dir" {
                    "/global/ui".to_string()
                } else {
                    format!("/global/{}", k.trim_end_matches("_path"))
                };
                assert_eq!(got(k), want_val, "mask {mask:04b} key {k}");
                let want_src = if overridden { "c.yaml" } else { "g.yaml" };
                assert_eq!(cfg.source_of(k), Source::File(want_src.into()), "mask {mask:04b} key {k}");
            }
        }
    }

    #[test]
    fn single_layer_matches_legacy_behavior_and_defaults() {
        // No --config: one layer only, identical to the pre-#597 single-file load.
        let cfg = AppConfig::from_layers(&layers(&[("global.yaml", GLOBAL)]));
        assert_eq!(cfg.asset_server_url, "http://localhost:8088");
        assert_eq!(cfg.assets_path, PathBuf::from("/global/assets"));
        assert!(cfg.warnings.is_empty(), "unexpected warnings: {:?}", cfg.warnings);

        // No layers at all → built-in defaults, marked as such.
        let cfg = AppConfig::from_layers(&[]);
        assert_eq!(cfg.asset_server_url, "http://localhost:8088");
        assert_eq!(cfg.http_port, 8765);
        assert_eq!(cfg.assets_path, PathBuf::from("eq_assets"));
        assert_eq!(cfg.source_of("asset_server_url"), Source::Default);
    }

    #[test]
    fn unknown_renderer_key_warns_instead_of_being_dropped_silently() {
        let cfg = AppConfig::from_layers(&layers(&[
            ("g.yaml", GLOBAL),
            ("c.yaml", "renderer:\n  asset_serve_url: http://typo:1\n  http_port: 9\n"),
        ]));
        let joined = cfg.warnings.join("\n");
        assert!(joined.contains("asset_serve_url"), "no warning naming the typo key: {joined}");
        assert!(joined.contains("c.yaml"), "warning must name the file: {joined}");
        assert!(joined.contains("http_port") && joined.contains("TOP-LEVEL"),
            "renderer.http_port must be called out: {joined}");
        // ...and the misplaced key genuinely did not take effect.
        assert_eq!(cfg.http_port, 8765);
        assert_eq!(cfg.asset_server_url, "http://localhost:8088");
    }

    // ── End-to-end through the real file loader (isolated XDG_CONFIG_HOME) ──────────────
    // These mutate process env, so they share a mutex and never run concurrently.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Run `f` with the given env vars set (`Some`) or removed (`None`), restoring
    /// the previous values afterwards. Shares ENV_LOCK with `with_config_home`.
    fn with_env<R>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> R) -> R {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev: Vec<_> = vars.iter().map(|(k, _)| (*k, std::env::var_os(k))).collect();
        for (k, v) in vars {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        let out = f();
        for (k, v) in prev {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        out
    }

    fn with_config_home<R>(dir: &Path, f: impl FnOnce() -> R) -> R {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", dir);
        let out = f();
        match prev {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        out
    }

    #[test]
    fn load_honors_config_flag_and_names_the_source_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cfgdir = tmp.path().join("eqoxide");
        std::fs::create_dir_all(&cfgdir).unwrap();
        std::fs::write(cfgdir.join("config.yaml"),
            "renderer:\n  assets_path: /global/assets\n  asset_server_url: http://localhost:8088\n").unwrap();
        let per_char = cfgdir.join("config-prod.yaml");
        std::fs::write(&per_char, "renderer:\n  asset_server_url: http://prod-assets:8088\n").unwrap();

        with_config_home(tmp.path(), || {
            // --config prod → per-character URL wins, global assets_path inherited.
            let path = LoginConfig::resolve_path(Some("prod"));
            assert_eq!(path, per_char);
            let cfg = AppConfig::load(Some(path.as_path()));
            assert_eq!(cfg.asset_server_url, "http://prod-assets:8088");
            assert_eq!(cfg.source_of("asset_server_url"),
                Source::File(per_char.display().to_string()));
            assert_eq!(cfg.assets_path, PathBuf::from("/global/assets"));

            // No --config → global only, exactly as before the fix.
            let cfg = AppConfig::load(None);
            assert_eq!(cfg.asset_server_url, "http://localhost:8088");
            assert!(cfg.warnings.is_empty(), "unexpected warnings: {:?}", cfg.warnings);
        });
    }

    /// F2: with no `--config` and the global config living at the documented
    /// `./config.yaml` fallback (no file in the XDG dir), `load()` must be SILENT.
    /// Exercises the fallback branch through the real loader — `from_layers` cannot
    /// reach it, so `single_layer_matches_legacy_behavior_and_defaults` never did.
    ///
    /// Calls the REAL public `load()` (not `load_with_fallback_dir` directly) via
    /// `with_load_fallback_dir_for_test`, so this drives production's actual call
    /// site end-to-end. `std::env::set_current_dir` is process-global and would
    /// race any other cwd-sensitive test later added to this crate; the
    /// thread-local override does not (#604 F1, review of PR #611 — the earlier
    /// version of this test called `load_with_fallback_dir` directly, which left
    /// `load()`'s own hardcoded fallback argument completely uncovered).
    #[test]
    fn load_with_no_config_flag_uses_the_cwd_fallback_silently() {
        let tmp = tempfile::tempdir().unwrap();          // empty XDG_CONFIG_HOME
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join("config.yaml"),
            "renderer:\n  asset_server_url: http://fallback:8088\n").unwrap();

        with_config_home(tmp.path(), || {
            AppConfig::with_load_fallback_dir_for_test(cwd.path(), || {
                let cfg = AppConfig::load(None);

                assert_eq!(cfg.asset_server_url, "http://fallback:8088");
                assert_eq!(cfg.source_of("asset_server_url"),
                    Source::File(cwd.path().join("config.yaml").display().to_string()));
                assert!(cfg.warnings.is_empty(),
                    "the ./config.yaml fallback must not warn (it IS the global config): {:?}",
                    cfg.warnings);
            });
        });
    }

    /// F1: a `http_port` the loader cannot use must not be recorded as a hit. Before
    /// the fix, global 9000 + per-character 70000 produced 8765 — a value in NEITHER
    /// layer — attributed to the per-character file, with no warning: `disclose()`
    /// emitting exactly the kind of confident falsehood this PR exists to remove.
    #[test]
    fn unusable_http_port_does_not_steal_the_source_attribution() {
        let cfg = AppConfig::from_layers(&layers(&[
            ("g.yaml", "http_port: 9000\n"),
            ("c.yaml", "http_port: 70000\n"),
        ]));
        assert_eq!(cfg.http_port, 9000, "the usable layer must stand");
        assert_eq!(cfg.source_of("http_port"), Source::File("g.yaml".into()),
            "the disclosed source must be the file the disclosed VALUE came from");
        let joined = cfg.warnings.join("\n");
        assert!(joined.contains("70000") && joined.contains("c.yaml") && joined.contains("out of range"),
            "out-of-range port must warn by file and value: {joined}");

        // Same rule for a non-integer, and for a lone unusable layer (default stands).
        let cfg = AppConfig::from_layers(&layers(&[("only.yaml", "http_port: \"8765\"\n")]));
        assert_eq!(cfg.http_port, 8765);
        assert_eq!(cfg.source_of("http_port"), Source::Default,
            "a quoted string is not a hit — the default must be disclosed as the default");
        assert!(cfg.warnings.join("\n").contains("not an integer"), "{:?}", cfg.warnings);

        // Every disclosed source names a file that really contains that value.
        for (val, want) in [("9000", true), ("70000", false)] {
            let cfg = AppConfig::from_layers(&layers(&[("x.yaml", &format!("http_port: {val}\n"))]));
            assert_eq!(cfg.source_of("http_port") == Source::File("x.yaml".into()), want);
        }
    }

    /// F3: the pre-#597 docs showed `assets_path:` at the TOP level, so configs
    /// written against them have keys the loader ignores. Ignoring is fine; ignoring
    /// them silently is the bug this PR is about.
    #[test]
    fn top_level_renderer_keys_warn_instead_of_being_dropped_silently() {
        let cfg = AppConfig::from_layers(&layers(&[(
            "old-style.yaml",
            "assets_path: /old/assets\nasset_server_url: http://old:8088\ncharacter_name: X\n",
        )]));
        assert_eq!(cfg.assets_path, PathBuf::from("eq_assets"), "top-level key must NOT take effect");
        assert_eq!(cfg.asset_server_url, "http://localhost:8088");
        let joined = cfg.warnings.join("\n");
        for k in ["assets_path", "asset_server_url"] {
            assert!(joined.contains(&format!("'{k}' must be nested under 'renderer:'")),
                "no warning for top-level {k}: {joined}");
        }
        assert!(joined.contains("old-style.yaml"), "warning must name the file: {joined}");
        // Unrelated top-level keys (login config lives here too) stay quiet.
        assert!(!joined.contains("character_name"), "{joined}");
    }

    /// F4: a renderer key that is PRESENT but unusable (null, a number, a map) fell
    /// back to the previous layer with no warning — a value-shaped silent drop.
    #[test]
    fn wrong_typed_renderer_values_warn_and_leave_the_previous_layer_standing() {
        let cfg = AppConfig::from_layers(&layers(&[
            ("g.yaml", GLOBAL),
            ("c.yaml", "renderer:\n  asset_server_url:\n  assets_path: 42\n"),
        ]));
        assert_eq!(cfg.asset_server_url, "http://localhost:8088");
        assert_eq!(cfg.source_of("asset_server_url"), Source::File("g.yaml".into()));
        assert_eq!(cfg.assets_path, PathBuf::from("/global/assets"));
        let joined = cfg.warnings.join("\n");
        assert!(joined.contains("'renderer.asset_server_url' is present but is null"), "{joined}");
        assert!(joined.contains("'renderer.assets_path' is present but is a number"), "{joined}");

        // An explicitly EMPTY string is a real value: it overrides, and is disclosed.
        let cfg = AppConfig::from_layers(&layers(&[
            ("g.yaml", GLOBAL),
            ("c.yaml", "renderer:\n  asset_server_url: \"\"\n"),
        ]));
        assert_eq!(cfg.asset_server_url, "");
        assert_eq!(cfg.source_of("asset_server_url"), Source::File("c.yaml".into()));
        assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
    }

    /// #604: `renderer:` itself can be the wrong shape — a sequence where a map is
    /// expected. `renderer:` null and `renderer: {}` are legitimate no-ops and must
    /// stay silent; anything else (a list, a scalar) must warn like every other
    /// wrong-typed renderer value, not vanish.
    #[test]
    fn list_shaped_renderer_block_warns_instead_of_being_dropped_silently() {
        let cfg = AppConfig::from_layers(&layers(&[
            ("g.yaml", GLOBAL),
            ("c.yaml", "renderer:\n  - 1\n  - 2\n"),
        ]));
        // The malformed layer contributes nothing; the previous layer stands.
        assert_eq!(cfg.asset_server_url, "http://localhost:8088");
        assert_eq!(cfg.source_of("asset_server_url"), Source::File("g.yaml".into()));
        let joined = cfg.warnings.join("\n");
        assert!(joined.contains("c.yaml") && joined.contains("'renderer:'")
            && joined.contains("a list") && joined.contains("not a map"),
            "no warning for list-shaped renderer: block: {joined}");

        // A scalar under `renderer:` warns the same way.
        let cfg = AppConfig::from_layers(&layers(&[("s.yaml", "renderer: 5\n")]));
        assert!(cfg.warnings.join("\n").contains("is a number"), "{:?}", cfg.warnings);

        // `renderer: null` and `renderer: {}` are legitimate no-ops and stay silent.
        let cfg = AppConfig::from_layers(&layers(&[("n.yaml", "renderer:\n")]));
        assert!(cfg.warnings.is_empty(), "renderer: null must stay silent: {:?}", cfg.warnings);
        let cfg = AppConfig::from_layers(&layers(&[("e.yaml", "renderer: {}\n")]));
        assert!(cfg.warnings.is_empty(), "renderer: {{}} must stay silent: {:?}", cfg.warnings);
    }

    /// F6: pin the disclosure text itself. Without this, swapping two fields inside
    /// `disclose()` — printing one key's value under another key's name — stays green.
    #[test]
    fn disclose_lines_are_pinned_field_by_field() {
        let cfg = AppConfig::from_layers(&layers(&[("g.yaml", GLOBAL), ("c.yaml",
            "renderer:\n  asset_server_url: http://char:1\n")]));
        let lines = with_env(&[("EQ_UI_DIR", None), ("EQ_SPELL_ICONS_DIR", None)],
            || cfg.disclose_lines());
        assert_eq!(lines, vec![
            "config: effective asset_server_url=http://char:1 (from c.yaml)".to_string(),
            "config: effective http_port=8765 (from g.yaml) — base port; actual bound port is logged as API_PORT=".to_string(),
            "config: effective assets_path=/global/assets (from g.yaml)".to_string(),
            "config: effective models_path=/global/models (from g.yaml)".to_string(),
            "config: effective eq_ui_dir=/global/ui (from g.yaml)".to_string(),
        ]);

        // F5: $EQ_UI_DIR beats the config value in `eqoxide-ui`, so the line must say so
        // rather than let the reader believe the config value is what the UI uses.
        let lines = with_env(&[("EQ_UI_DIR", Some("/env/ui")), ("EQ_SPELL_ICONS_DIR", None)],
            || cfg.disclose_lines());
        assert!(lines[4].contains("OVERRIDDEN by $EQ_UI_DIR=/env/ui"), "{}", lines[4]);

        // ...and an unset key must not read as "nothing is in force".
        let bare = AppConfig::from_layers(&[]);
        let lines = with_env(&[("EQ_UI_DIR", None), ("EQ_SPELL_ICONS_DIR", None)],
            || bare.disclose_lines());
        assert!(lines[4].contains("<unset>") && lines[4].contains("default atlas dir"), "{}", lines[4]);
    }
}
