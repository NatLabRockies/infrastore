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

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct AuthSection {
    /// "none" | "api_key". `oauth` is reserved for a later milestone.
    #[serde(default = "default_auth_method")]
    pub method: String,

    /// API keys accepted when `method = "api_key"`. Compared in constant time
    /// against the request's `x-api-key` header.
    #[serde(default)]
    pub keys: Vec<String>,
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
