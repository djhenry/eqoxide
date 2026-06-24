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

/// Renderer / HTTP server settings from `config.yaml`.
pub struct AppConfig {
    pub assets_path: PathBuf,
    pub models_path: PathBuf,
    pub http_port: u16,
    pub asset_server_url: String,
}

impl AppConfig {
    pub fn load() -> Self {
        // Prefer ~/.config/eqoxide/config.yaml; fall back to ./config.yaml for back-compat.
        let primary = config_dir().join("config.yaml");
        let cfg_text = std::fs::read_to_string(&primary)
            .or_else(|_| std::fs::read_to_string("config.yaml"))
            .unwrap_or_else(|e| {
                tracing::warn!("renderer: no config.yaml in {} or cwd ({}), using defaults",
                    primary.display(), e);
                String::new()
            });
        Self::from_yaml_str(&cfg_text)
    }

    pub fn from_yaml_str(cfg_text: &str) -> Self {
        let cfg: serde_yaml::Value =
            serde_yaml::from_str(cfg_text).unwrap_or(serde_yaml::Value::Null);
        let r = cfg.get("renderer");

        let assets_path = r
            .and_then(|v| v.get("assets_path"))
            .and_then(|v| v.as_str())
            .map(|p| PathBuf::from(shellexpand::tilde(p).into_owned()))
            .unwrap_or_else(|| PathBuf::from("eq_assets"));

        let models_path = r
            .and_then(|v| v.get("models_path"))
            .and_then(|v| v.as_str())
            .map(|p| PathBuf::from(shellexpand::tilde(p).into_owned()))
            .unwrap_or_else(|| PathBuf::from("eq_renderer/assets/models"));

        let http_port = cfg
            .get("http_port")
            .and_then(|v| v.as_u64())
            .unwrap_or(8765) as u16;

        let asset_server_url = r
            .and_then(|v| v.get("asset_server_url"))
            .and_then(|v| v.as_str())
            .unwrap_or("http://localhost:8088")
            .to_string();

        AppConfig { assets_path, models_path, http_port, asset_server_url }
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
                .unwrap_or(5998) as u16,
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
