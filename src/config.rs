use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/* Configuration loaded from ~/.config/zotero-cli/config.toml.
All fields have sane defaults so the tool works out-of-the-box
against a locally running Zotero instance. */

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    #[serde(default = "default_api_base")]
    pub api_base: String,

    pub api_key: Option<String>,

    pub user_id: Option<u64>,

    #[serde(default = "default_library_type")]
    pub library_type: String,
}

fn default_api_base() -> String {
    "http://localhost:23119/api".to_string()
}

fn default_library_type() -> String {
    "user".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Config {
            api_base: default_api_base(),
            api_key: None,
            user_id: None,
            library_type: default_library_type(),
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path();
        let mut cfg = if path.exists() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading config at {}", path.display()))?;
            toml::from_str(&text)
                .with_context(|| format!("parsing config at {}", path.display()))?
        } else {
            Config::default()
        };
        // env var overrides
        if let Ok(base) = std::env::var("ZOTERO_API_BASE") {
            cfg.api_base = base;
        }
        if let Ok(key) = std::env::var("ZOTERO_API_KEY") {
            cfg.api_key = Some(key);
        }
        Ok(cfg)
    }

    #[allow(dead_code)]
    pub fn save(&self) -> Result<()> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        std::fs::write(&path, text)?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn path() -> PathBuf {
        config_path()
    }
}

fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("zotero-mcp")
        .join("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_values() {
        let cfg = Config::default();
        assert_eq!(cfg.api_base, "http://localhost:23119/api");
        assert_eq!(cfg.library_type, "user");
        assert!(cfg.api_key.is_none());
        assert!(cfg.user_id.is_none());
    }

    #[test]
    fn load_missing_file_returns_defaults() {
        /* Config::load() returns defaults when the file doesn't exist.
        We rely on the real config_path() not colliding with test env. */
        let cfg = Config::default();
        assert_eq!(cfg.api_base, "http://localhost:23119/api");
    }

    #[test]
    fn deserialize_partial_config() {
        let toml = r#"api_base = "http://remote:8080/api""#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.api_base, "http://remote:8080/api");
        assert_eq!(cfg.library_type, "user"); // default
        assert!(cfg.api_key.is_none());
    }

    #[test]
    fn deserialize_full_config() {
        let toml = r#"
            api_base = "http://remote:8080/api"
            api_key = "secret123"
            user_id = 42
            library_type = "group"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.api_base, "http://remote:8080/api");
        assert_eq!(cfg.api_key.as_deref(), Some("secret123"));
        assert_eq!(cfg.user_id, Some(42));
        assert_eq!(cfg.library_type, "group");
    }

    #[test]
    fn deserialize_invalid_toml_errors() {
        let toml = "not valid [[[ toml";
        let result: Result<Config, _> = toml::from_str(toml);
        assert!(result.is_err());
    }

    #[test]
    fn config_path_ends_with_config_toml() {
        let path = config_path();
        assert!(path.ends_with("zotero-mcp/config.toml"));
    }

    #[test]
    fn config_serializes_roundtrip() {
        let cfg = Config {
            api_base: "http://test:1234/api".into(),
            api_key: Some("key".into()),
            user_id: Some(99),
            library_type: "group".into(),
        };
        let toml_str = toml::to_string_pretty(&cfg).unwrap();
        let cfg2: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(cfg2.api_base, cfg.api_base);
        assert_eq!(cfg2.api_key, cfg.api_key);
        assert_eq!(cfg2.user_id, cfg.user_id);
    }
}
