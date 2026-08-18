use std::path::Path;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The server's TOML configuration.
///
/// Every section rejects unknown fields. Serde ignores them by default, which
/// here meant a misspelling resolved to the *permissive* option: `[auth]` in
/// place of `[authentication]`, or `methodd = "api_key"`, parsed cleanly, left
/// `method` on its `"none"` default, passed `validate()`, and served the whole
/// read surface to anyone — with a single `tracing` line as the only clue. Every
/// other mistake in this file already fails loudly (`api_key` with no keys, an
/// unknown method, an empty `files` list), so this closes the one path that
/// failed open. The cost is that a config written for a newer version, carrying
/// a key this binary does not know, is refused rather than partly honoured;
/// for a file that decides whether authentication happens, that is the safer
/// direction to fail in.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub server: ServerSection,
    pub data: DataSection,
    #[serde(default)]
    pub authentication: AuthSection,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSection {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DataSection {
    /// Paths to HDF5 files served read-only by this server. v0 supports a
    /// single file (the first entry); multi-file is reserved for a follow-up.
    pub files: Vec<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
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
files = ["store.h5"]
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
    fn a_misspelled_auth_key_is_a_parse_error_not_a_silent_none() {
        // The failure this guards against is not a wrong value but a wrong
        // *name*: serde ignores unknown fields by default, so a typo left
        // `method` on its "none" default, passed `validate()`, and served the
        // whole read surface unauthenticated. Every one of these means the
        // operator intended authentication and would otherwise not have got it.
        for bad in [
            // The section itself is misspelled, so the real one is absent.
            "[auth]\nmethod = \"api_key\"\nkeys = [\"s3cret\"]",
            // The section is right; the key inside it is not.
            "[authentication]\nmethodd = \"api_key\"\nkeys = [\"s3cret\"]",
            "[authentication]\nmethod = \"api_key\"\nkey = [\"s3cret\"]",
        ] {
            let err = toml::from_str::<ServerConfig>(&format!("{BASE}\n{bad}\n"))
                .expect_err("unknown keys must fail the parse");
            assert!(err.to_string().contains("unknown field"), "{bad}\n-> {err}");
        }

        // Unknown keys in the other sections are refused the same way.
        for bad in [
            "[server]\nhost = \"::1\"\nport = 1\nprot = 2",
            "[data]\nfiles = []\nfile = \"x\"",
        ] {
            assert!(toml::from_str::<ServerConfig>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn api_key_without_keys_is_rejected() {
        let cfg: ServerConfig =
            toml::from_str(&format!("{BASE}\n[authentication]\nmethod = \"api_key\"\n")).unwrap();
        assert!(cfg.authentication.validate().is_err());
    }
}
