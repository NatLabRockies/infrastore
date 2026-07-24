use std::path::Path;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    pub server: ServerSection,
    pub data: DataSection,
    #[serde(default)]
    pub authentication: AuthSection,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerSection {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DataSection {
    /// Paths to NetCDF files served read-only by this server. v0 supports a
    /// single file (the first entry); multi-file is reserved for a follow-up.
    pub files: Vec<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AuthSection {
    /// "none" | "api_key". `oauth` is reserved for a later milestone.
    #[serde(default = "default_auth_method")]
    pub method: String,

    /// API keys accepted when `method = "api_key"`. Checked against the
    /// request's `x-api-key` header without early-exit across keys; see
    /// `auth::any_match` for the exact timing guarantee.
    #[serde(default)]
    pub keys: Vec<String>,
}

/// Must agree with `default_auth_method`: `#[serde(default)]` on
/// `ServerConfig::authentication` builds the section from `Default`, so an
/// omitted `[authentication]` table has to land on a *valid* method.
impl Default for AuthSection {
    fn default() -> Self {
        Self {
            method: default_auth_method(),
            keys: Vec::new(),
        }
    }
}

fn default_auth_method() -> String {
    "none".into()
}

impl AuthSection {
    /// Returns Err(...) on a config-time problem (e.g. method requires keys
    /// but none provided). Called by the server on startup so misconfiguration
    /// fails loudly rather than at the first request.
    pub fn validate(&self) -> Result<(), String> {
        match self.method.as_str() {
            "none" => Ok(()),
            "api_key" => {
                if self.keys.is_empty() {
                    Err(
                        "authentication.method = \"api_key\" requires at least one entry in keys"
                            .into(),
                    )
                } else {
                    Ok(())
                }
            }
            other => Err(format!("unsupported authentication.method: {other}")),
        }
    }
}

impl ServerConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let s = std::fs::read_to_string(path).map_err(ConfigError::Io)?;
        toml::from_str(&s).map_err(ConfigError::Parse)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse error: {0}")]
    Parse(#[from] toml::de::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"
[server]
host = "127.0.0.1"
port = 50051

[data]
files = ["store.nc"]
"#;

    #[test]
    fn omitted_authentication_section_defaults_to_none() {
        let cfg: ServerConfig = toml::from_str(BASE).unwrap();
        assert_eq!(cfg.authentication.method, "none");
        cfg.authentication.validate().unwrap();
    }

    #[test]
    fn authentication_section_without_method_defaults_to_none() {
        let cfg: ServerConfig = toml::from_str(&format!("{BASE}\n[authentication]\n")).unwrap();
        assert_eq!(cfg.authentication.method, "none");
        cfg.authentication.validate().unwrap();
    }

    #[test]
    fn api_key_without_keys_is_rejected() {
        let cfg: ServerConfig =
            toml::from_str(&format!("{BASE}\n[authentication]\nmethod = \"api_key\"\n")).unwrap();
        assert!(cfg.authentication.validate().is_err());
    }
}
