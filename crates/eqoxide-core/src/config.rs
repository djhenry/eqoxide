//! Application and EQ connection configuration, loaded from YAML files.

use std::path::{Path, PathBuf};

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
    /// `config_path` is the file `--config` resolved to (see
    /// [`LoginConfig::resolve_path`]); it is layered **on top of** the global
    /// `config.yaml`. Passing the global path itself (the no-`--config` case)
    /// yields exactly the legacy single-file behavior.
    pub fn load(config_path: &Path) -> Self {
        let mut layers: Vec<(String, String)> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();

        // Layer 1: the global config. Prefer ~/.config/eqoxide/config.yaml; fall
        // back to ./config.yaml for back-compat.
        let primary = config_dir().join("config.yaml");
        let fallback = PathBuf::from("config.yaml");
        let mut global_path: Option<PathBuf> = None;
        if let Ok(t) = std::fs::read_to_string(&primary) {
            global_path = Some(primary.clone());
            layers.push((primary.display().to_string(), t));
        } else if let Ok(t) = std::fs::read_to_string(&fallback) {
            global_path = Some(fallback.clone());
            layers.push((fallback.display().to_string(), t));
        }

        // Layer 2: the per-character file from --config, when it is a different
        // file than the global one (otherwise we'd read the same file twice).
        let is_global = global_path.as_deref().is_some_and(|g| same_file(g, config_path));
        if !is_global {
            match std::fs::read_to_string(config_path) {
                Ok(t) => layers.push((config_path.display().to_string(), t)),
                Err(e) => warnings.push(format!(
                    "config: could not read {} ({e}) — renderer settings come from the global config only",
                    config_path.display()
                )),
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

        // Report renderer keys we do not understand instead of dropping them.
        for (label, cfg) in &parsed {
            let Some(serde_yaml::Value::Mapping(m)) = cfg.get("renderer") else { continue };
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

        // `pick` walks the layers in order and keeps the last hit, so a later
        // (per-character) layer overrides an earlier (global) one key by key.
        let pick = |get: &dyn Fn(&serde_yaml::Value) -> Option<String>| -> Option<(String, Source)> {
            let mut found = None;
            for (label, cfg) in &parsed {
                if let Some(v) = get(cfg) {
                    found = Some((v, Source::File((*label).to_string())));
                }
            }
            found
        };
        let renderer_str = |key: &'static str| {
            move |cfg: &serde_yaml::Value| {
                cfg.get("renderer")
                    .and_then(|v| v.get(key))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            }
        };

        let mut sources: Vec<(&'static str, Source)> = Vec::new();
        let mut src = |field: &'static str, s: Option<Source>| {
            sources.push((field, s.unwrap_or(Source::Default)));
        };

        let assets_path_hit = pick(&renderer_str("assets_path"));
        let assets_path = assets_path_hit
            .as_ref()
            .map(|(p, _)| PathBuf::from(shellexpand::tilde(p).into_owned()))
            .unwrap_or_else(|| PathBuf::from("eq_assets"));
        src("assets_path", assets_path_hit.map(|(_, s)| s));

        let models_path_hit = pick(&renderer_str("models_path"));
        let models_path = models_path_hit
            .as_ref()
            .map(|(p, _)| PathBuf::from(shellexpand::tilde(p).into_owned()))
            .unwrap_or_else(|| PathBuf::from("eqoxide/assets/models"));
        src("models_path", models_path_hit.map(|(_, s)| s));

        // `http_port` is a TOP-LEVEL key (not under `renderer:`), and is only the
        // BASE port: the HTTP server still scans upward for a free port and prints
        // `API_PORT=<bound>`, and `--api-port N` still overrides it exactly.
        let http_port_hit = pick(&|cfg: &serde_yaml::Value| {
            cfg.get("http_port").and_then(|v| v.as_u64()).map(|n| n.to_string())
        });
        let http_port = http_port_hit
            .as_ref()
            .and_then(|(v, _)| v.parse::<u16>().ok())
            .unwrap_or(8765);
        src("http_port", http_port_hit.map(|(_, s)| s));

        let url_hit = pick(&renderer_str("asset_server_url"));
        let asset_server_url = url_hit
            .as_ref()
            .map(|(v, _)| v.clone())
            .unwrap_or_else(|| "http://localhost:8088".to_string());
        src("asset_server_url", url_hit.map(|(_, s)| s));

        let ui_hit = pick(&renderer_str("eq_ui_dir"));
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

    /// Log the effective renderer/HTTP settings and the file each came from, plus
    /// any merge warnings. Called once at startup: a wrong `asset_server_url` must
    /// be readable in the log, never inferred later from a world with no geometry.
    pub fn disclose(&self) {
        for w in &self.warnings {
            tracing::warn!("{w}");
        }
        tracing::info!(
            "config: effective asset_server_url={} (from {})",
            self.asset_server_url,
            self.source_of("asset_server_url")
        );
        tracing::info!(
            "config: effective http_port={} (from {}) — base port; actual bound port is logged as API_PORT=",
            self.http_port,
            self.source_of("http_port")
        );
        tracing::info!(
            "config: effective assets_path={} (from {})",
            self.assets_path.display(),
            self.source_of("assets_path")
        );
        tracing::info!(
            "config: effective models_path={} (from {})",
            self.models_path.display(),
            self.source_of("models_path")
        );
        tracing::info!(
            "config: effective eq_ui_dir={} (from {})",
            self.eq_ui_dir.as_deref().unwrap_or("<unset>"),
            self.source_of("eq_ui_dir")
        );
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
            let cfg = AppConfig::load(&path);
            assert_eq!(cfg.asset_server_url, "http://prod-assets:8088");
            assert_eq!(cfg.source_of("asset_server_url"),
                Source::File(per_char.display().to_string()));
            assert_eq!(cfg.assets_path, PathBuf::from("/global/assets"));

            // No --config → global only, exactly as before the fix.
            let path = LoginConfig::resolve_path(None);
            let cfg = AppConfig::load(&path);
            assert_eq!(cfg.asset_server_url, "http://localhost:8088");
            assert!(cfg.warnings.is_empty(), "unexpected warnings: {:?}", cfg.warnings);
        });
    }
}
