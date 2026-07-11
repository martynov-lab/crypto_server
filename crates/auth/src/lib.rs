//! Client authentication (NOT exchange keys — those never exist in Phase 1).
//!
//! Phase 1 is intentionally minimal: either open (local dev) or a shared static
//! token. This is the seam where real per-tenant auth (JWT/session) lands later.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode")]
pub enum AuthPolicy {
    /// Accept any client (local development).
    Open,
    /// Require an exact bearer token match.
    StaticToken { token: String },
}

impl Default for AuthPolicy {
    fn default() -> Self {
        AuthPolicy::Open
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthOutcome {
    Ok,
    Rejected,
}

impl AuthPolicy {
    /// Validate a presented token (may be `None` when the client sends none).
    pub fn check(&self, presented: Option<&str>) -> AuthOutcome {
        match self {
            AuthPolicy::Open => AuthOutcome::Ok,
            AuthPolicy::StaticToken { token } => match presented {
                Some(t) if t == token => AuthOutcome::Ok,
                _ => AuthOutcome::Rejected,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_accepts_all() {
        assert_eq!(AuthPolicy::Open.check(None), AuthOutcome::Ok);
    }

    #[test]
    fn static_token_matches() {
        let p = AuthPolicy::StaticToken { token: "s3cret".into() };
        assert_eq!(p.check(Some("s3cret")), AuthOutcome::Ok);
        assert_eq!(p.check(Some("nope")), AuthOutcome::Rejected);
        assert_eq!(p.check(None), AuthOutcome::Rejected);
    }
}
