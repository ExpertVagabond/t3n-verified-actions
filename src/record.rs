//! Ledger record shapes and the state machine over them.
//!
//! Everything in this module is pure: no host calls, no I/O. That keeps the part
//! most likely to contain a correctness bug testable with a plain `cargo test` on
//! the host, without a TEE, a node, or credentials.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Where a record sits in its lifecycle.
///
/// The important property is that `Submitted` -> `Confirmed` is not a legal
/// single step. Confirmation requires an independent read-back, so the only
/// transition into `Confirmed` lives in `settle()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// The effect was attempted and the transport did not error. This says
    /// nothing about whether the effect actually happened.
    PendingVerification,
    /// An independent probe observed the expected external state.
    Confirmed,
    /// An independent probe ran and did NOT observe the expected state. This is
    /// a terminal, actionable state — not a retry signal. Retrying an action
    /// that may have half-succeeded is how double-sends happen.
    Unverified,
    /// The transport itself errored, so the effect probably never left. Distinct
    /// from `Unverified` because this one is safe to resubmit under a new key.
    Failed,
}

impl Status {
    /// Terminal states are never recomputed; `submit` short-circuits on them.
    pub fn is_terminal(self) -> bool {
        matches!(self, Status::Confirmed | Status::Unverified | Status::Failed)
    }
}

/// What the caller asked us to do, and how to prove it happened.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionSpec {
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub body: Option<String>,
    /// The independent read-back. Required — an action you cannot verify is an
    /// action this contract will not perform.
    pub verify: VerifySpec,
}

/// A second, different request that observes the state the effect changed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifySpec {
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub body: Option<String>,
    pub expect: Expectation,
}

/// What the verification probe must observe for the record to be confirmed.
///
/// `body_absent` matters as much as `body_contains`. The motivating case: a mail
/// transport reports success, and the message exists — but it is flagged as an
/// auto-saved draft rather than a sent message. Presence alone would confirm a
/// send that never happened; requiring the draft marker to be absent catches it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Expectation {
    #[serde(default)]
    pub status: Option<u16>,
    #[serde(default)]
    pub body_contains: Vec<String>,
    #[serde(default)]
    pub body_absent: Vec<String>,
}

impl Expectation {
    /// Returns `Ok(())` if the probe response satisfies every clause, otherwise a
    /// human-readable reason naming the first clause that failed.
    pub fn evaluate(&self, code: u16, body: &str) -> Result<(), String> {
        if let Some(want) = self.status {
            if code != want {
                return Err(alloc::format!("expected HTTP {want}, observed {code}"));
            }
        }
        for needle in &self.body_contains {
            if !body.contains(needle.as_str()) {
                return Err(alloc::format!("expected response to contain {needle:?}"));
            }
        }
        for needle in &self.body_absent {
            if body.contains(needle.as_str()) {
                return Err(alloc::format!("expected response NOT to contain {needle:?}"));
            }
        }
        Ok(())
    }
}

/// One outbound call and what came back. Bodies are stored as a digest, never
/// verbatim: the ledger is an audit trail, not a copy of your payloads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attempt {
    pub at: u64,
    pub code: u16,
    /// SHA-256 of the response body, hex-encoded. Lets an auditor prove two
    /// records saw the same response without the ledger holding the content.
    pub body_sha256: String,
    pub body_len: usize,
    #[serde(default)]
    pub error: Option<String>,
}

/// One entry in the append-only ledger.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub idempotency_key: String,
    pub status: Status,
    pub action: ActionSpec,
    #[serde(default)]
    pub effect: Option<Attempt>,
    #[serde(default)]
    pub verification: Option<Attempt>,
    /// Why the record is in its current state, in words an operator can act on.
    #[serde(default)]
    pub reason: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    pub seq_no: u64,
    pub contract_id: u32,
    /// How many times verification has run. Verification is idempotent, so this
    /// is diagnostic rather than a limit.
    #[serde(default)]
    pub verify_attempts: u32,
}

impl Record {
    /// A freshly submitted record. Deliberately not constructible as `Confirmed`.
    pub fn submitted(
        idempotency_key: String,
        action: ActionSpec,
        effect: Attempt,
        now: u64,
        seq_no: u64,
        contract_id: u32,
    ) -> Self {
        let failed = effect.error.is_some();
        Record {
            idempotency_key,
            status: if failed { Status::Failed } else { Status::PendingVerification },
            reason: if failed {
                effect.error.clone()
            } else {
                Some("transport accepted the request; not yet independently verified".to_string())
            },
            action,
            effect: Some(effect),
            verification: None,
            created_at: now,
            updated_at: now,
            seq_no,
            contract_id,
            verify_attempts: 0,
        }
    }

    /// Apply a verification probe outcome. The ONLY path to `Confirmed`.
    pub fn settle(&mut self, probe: Attempt, verdict: Result<(), String>, now: u64) {
        self.verify_attempts += 1;
        self.updated_at = now;
        match verdict {
            Ok(()) => {
                self.status = Status::Confirmed;
                self.reason = Some("independently verified against external state".to_string());
            }
            Err(why) => {
                self.status = Status::Unverified;
                self.reason = Some(why);
            }
        }
        self.verification = Some(probe);
    }

    /// Canonical bytes for the Merkle claims digest.
    ///
    /// Only the fields an auditor needs to trust are included, in a fixed order,
    /// so the digest is stable across serde version changes and field additions.
    pub fn claims_digest(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(b"t3n-verified-actions/v1\n");
        h.update(self.idempotency_key.as_bytes());
        h.update(b"\n");
        h.update(alloc::format!("{:?}", self.status).as_bytes());
        h.update(b"\n");
        h.update(self.action.method.as_bytes());
        h.update(b" ");
        h.update(self.action.url.as_bytes());
        h.update(b"\n");
        for a in [self.effect.as_ref(), self.verification.as_ref()].into_iter().flatten() {
            h.update(alloc::format!("{}:{}:{}", a.at, a.code, a.body_sha256).as_bytes());
            h.update(b"\n");
        }
        h.update(alloc::format!("{}:{}:{}", self.created_at, self.updated_at, self.seq_no).as_bytes());
        h.finalize().into()
    }
}

/// Hex SHA-256 of arbitrary bytes. Used for response-body fingerprints.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex::encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn spec() -> ActionSpec {
        ActionSpec {
            method: "POST".into(),
            url: "https://example.test/send".into(),
            headers: vec![],
            body: None,
            verify: VerifySpec {
                method: "GET".into(),
                url: "https://example.test/sent".into(),
                headers: vec![],
                body: None,
                expect: Expectation {
                    status: Some(200),
                    body_contains: vec!["message-id-abc".into()],
                    body_absent: vec!["auto-saved-draft".into()],
                },
            },
        }
    }

    fn attempt(code: u16, body: &str) -> Attempt {
        Attempt {
            at: 100,
            code,
            body_sha256: sha256_hex(body.as_bytes()),
            body_len: body.len(),
            error: None,
        }
    }

    #[test]
    fn a_successful_transport_is_never_confirmed() {
        let r = Record::submitted("k1".into(), spec(), attempt(200, "{\"ok\":true}"), 1, 1, 7);
        assert_eq!(r.status, Status::PendingVerification);
        assert!(!r.status.is_terminal());
    }

    #[test]
    fn transport_error_is_failed_not_unverified() {
        let mut a = attempt(0, "");
        a.error = Some("connection refused".into());
        let r = Record::submitted("k2".into(), spec(), a, 1, 1, 7);
        assert_eq!(r.status, Status::Failed);
    }

    #[test]
    fn independent_probe_confirms() {
        let mut r = Record::submitted("k3".into(), spec(), attempt(200, "ok"), 1, 1, 7);
        let body = "{\"id\":\"message-id-abc\"}";
        let verdict = r.action.verify.expect.evaluate(200, body);
        r.settle(attempt(200, body), verdict, 2);
        assert_eq!(r.status, Status::Confirmed);
    }

    /// The case that motivated the whole contract: the artefact exists, but it is
    /// a draft, not a sent message. Presence alone would have confirmed a lie.
    #[test]
    fn presence_of_a_draft_marker_blocks_confirmation() {
        let mut r = Record::submitted("k4".into(), spec(), attempt(200, "ok"), 1, 1, 7);
        let body = "{\"id\":\"message-id-abc\",\"flags\":[\"auto-saved-draft\"]}";
        let verdict = r.action.verify.expect.evaluate(200, body);
        r.settle(attempt(200, body), verdict, 2);
        assert_eq!(r.status, Status::Unverified);
        assert!(r.reason.unwrap().contains("auto-saved-draft"));
    }

    #[test]
    fn missing_evidence_is_unverified() {
        let mut r = Record::submitted("k5".into(), spec(), attempt(200, "ok"), 1, 1, 7);
        let body = "{\"messages\":[]}";
        let verdict = r.action.verify.expect.evaluate(200, body);
        r.settle(attempt(200, body), verdict, 2);
        assert_eq!(r.status, Status::Unverified);
    }

    #[test]
    fn digest_changes_when_status_changes() {
        let mut r = Record::submitted("k6".into(), spec(), attempt(200, "ok"), 1, 1, 7);
        let before = r.claims_digest();
        r.settle(attempt(200, "message-id-abc"), Ok(()), 2);
        assert_ne!(before, r.claims_digest());
    }

    #[test]
    fn digest_is_stable_for_an_unchanged_record() {
        let r = Record::submitted("k7".into(), spec(), attempt(200, "ok"), 1, 1, 7);
        assert_eq!(r.claims_digest(), r.claims_digest());
    }
}
