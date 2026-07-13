//! Tonic interceptor for `api_key` authentication.

use std::sync::Arc;

use tonic::Status;
use tonic::service::Interceptor;

const HEADER: &str = "x-api-key";

#[derive(Clone)]
pub struct ApiKeyInterceptor {
    keys: Arc<Vec<String>>,
}

impl ApiKeyInterceptor {
    pub fn new(keys: Vec<String>) -> Self {
        Self {
            keys: Arc::new(keys),
        }
    }
}

impl Interceptor for ApiKeyInterceptor {
    fn call(&mut self, req: tonic::Request<()>) -> Result<tonic::Request<()>, Status> {
        let header = req
            .metadata()
            .get(HEADER)
            .ok_or_else(|| Status::unauthenticated("missing x-api-key header"))?;
        let supplied = header
            .to_str()
            .map_err(|_| Status::unauthenticated("x-api-key is not valid ASCII"))?;
        if !any_match(&self.keys, supplied) {
            return Err(Status::unauthenticated("invalid x-api-key"));
        }
        Ok(req)
    }
}

/// Compares against every configured key regardless of where a match falls, so
/// there is no early-exit timing leak across keys. The comparison is only
/// constant-time among keys of the same length as `supplied`: a length mismatch
/// is rejected before the XOR loop, which leaks the supplied key's length.
fn any_match(keys: &[String], supplied: &str) -> bool {
    let supplied = supplied.as_bytes();
    let mut found = false;
    for k in keys {
        let k = k.as_bytes();
        if k.len() != supplied.len() {
            // Length itself isn't secret; cheap reject.
            continue;
        }
        let mut diff: u8 = 0;
        for (a, b) in k.iter().zip(supplied.iter()) {
            diff |= a ^ b;
        }
        found |= diff == 0;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_valid_key() {
        let keys = vec!["secret-1".to_string(), "secret-2".to_string()];
        assert!(any_match(&keys, "secret-1"));
        assert!(any_match(&keys, "secret-2"));
    }

    #[test]
    fn rejects_unknown_key() {
        let keys = vec!["secret-1".to_string()];
        assert!(!any_match(&keys, "nope"));
        assert!(!any_match(&keys, ""));
        assert!(!any_match(&keys, "secret-12"));
    }
}
