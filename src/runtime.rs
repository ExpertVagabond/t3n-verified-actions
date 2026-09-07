//! Host glue: the ledger in `kv-store`, outbound calls through `http`, and the
//! four exported operations. Compiled for wasm only — see `lib.rs`.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

use crate::host::interfaces::{http, kv_store, logging};
use crate::host::tenant::tenant_context;
use crate::record::{sha256_hex, ActionSpec, Attempt, Record, Status};

/// Ledger map tail. The full name is `z:<tid>:actions`, built at runtime from
/// `tenant-context.tenant-did()` — never hardcoded, so the same WASM works for
/// any tenant that registers it.
const MAP_TAIL: &str = "actions";

/// Cap on a single `list-records` scan. The host bills a bookkeeping budget per
/// scan and rejects `limit == 0`, so callers page by passing the next `start`.
const DEFAULT_SCAN_LIMIT: u32 = 100;

/// Secondary index of every idempotency key, newline-separated.
///
/// A belt-and-braces fallback for `list-records`. `scan` is the primary path and
/// its range and limit semantics are correct, but its VALUES are not usable (see
/// `list_records`), so this index guarantees the ledger stays enumerable even if
/// scan regresses further.
///
/// The `__` prefix keeps it out of the user key space; it is filtered from scan
/// results so it can never be mistaken for a record.
const INDEX_KEY: &str = "__index";

/// Ceiling on indexed keys. The index is one value, so it is a write hotspot and
/// grows unbounded without a bound. At this point `list-records` still works via
/// the index for the first N keys and `get-record` keeps working for all of them.
const INDEX_MAX: usize = 5000;

fn map_name() -> String {
    format!("z:{}:{}", hex::encode(tenant_context::tenant_did()), MAP_TAIL)
}

fn now() -> u64 {
    tenant_context::cluster_timestamp_secs()
}

// ---------------------------------------------------------------------------
// Request payloads
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SubmitRequest {
    idempotency_key: String,
    #[serde(flatten)]
    action: ActionSpec,
}

#[derive(Debug, Deserialize)]
struct KeyRequest {
    idempotency_key: String,
}

#[derive(Debug, Default, Deserialize)]
struct ListRequest {
    #[serde(default)]
    start: Option<String>,
    #[serde(default)]
    end: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Debug, Serialize)]
struct ListResponse {
    records: Vec<Record>,
    /// Set when the scan hit `limit`; pass it back as `start` for the next page.
    next_start: Option<String>,
    /// Which read path produced these rows: `"scan"` (the host range scan) or
    /// `"index"` (the fallback). Surfaced so an operator can see at a glance
    /// whether the host-side scan bug is still in play.
    source: &'static str,
    /// Raw row count returned by `kv-store.scan` before filtering. Diagnostic:
    /// distinguishes "scan returned nothing" from "scan returned rows we dropped".
    scanned: usize,
    /// Keys `scan` returned, for diagnosing range behaviour. Bounded.
    scanned_keys: Vec<String>,
    /// Byte length of each value `scan` returned, paired with `scanned_keys`.
    /// Distinguishes "scan omits values" from "values are present but unparseable".
    scanned_value_lens: Vec<usize>,
    /// First bytes of the first non-index value scan returned, lossy-decoded.
    scanned_value_head: Option<String>,
}

// ---------------------------------------------------------------------------
// Ledger access
// ---------------------------------------------------------------------------

/// Read the secondary index. A missing or unreadable index is treated as empty:
/// the index is a convenience for listing, never the source of truth.
fn index_load() -> Vec<String> {
    match kv_store::get(&map_name(), INDEX_KEY.as_bytes()) {
        Ok(Some(bytes)) => String::from_utf8_lossy(&bytes)
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect(),
        _ => Vec::new(),
    }
}

/// Append one key to the index. Best-effort: a failure here must not fail the
/// action itself, because the record is already written and the index is only a
/// listing aid. It is logged so an operator can see the index drifting.
fn index_append(key: &str) {
    let mut keys = index_load();
    if keys.iter().any(|k| k == key) {
        return;
    }
    if keys.len() >= INDEX_MAX {
        let _ = logging::error(&format!(
            "index full at {INDEX_MAX} keys; {key} written but not indexed (get-record still works)"
        ));
        return;
    }
    keys.push(key.to_string());
    let joined = keys.join("\n");
    if let Err(e) = kv_store::put(&map_name(), INDEX_KEY.as_bytes(), joined.as_bytes()) {
        let _ = logging::error(&format!("index append failed for {key}: {e}"));
    }
}

fn load(key: &str) -> Result<Option<Record>, String> {
    let raw = kv_store::get(&map_name(), key.as_bytes()).map_err(|e| format!("kv get: {e}"))?;
    match raw {
        None => Ok(None),
        Some(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| format!("ledger entry {key} is corrupt: {e}")),
    }
}

/// Persist a record and anchor it in the transaction's Merkle leaf.
///
/// `set_claims_digest` is what makes a receipt checkable offline: an auditor who
/// recomputes `Record::claims_digest()` from a returned record can match it
/// against the leaf without trusting the node that served it.
fn store(record: &Record) -> Result<(), String> {
    let bytes = serde_json::to_vec(record).map_err(|e| format!("serialize record: {e}"))?;
    kv_store::put(&map_name(), record.idempotency_key.as_bytes(), &bytes)
        .map_err(|e| format!("kv put: {e}"))?;
    kv_store::set_claims_digest(&record.claims_digest())
        .map_err(|e| format!("set claims digest: {e}"))?;
    Ok(())
}

fn verb(method: &str) -> Result<http::Verb, String> {
    match method.to_ascii_uppercase().as_str() {
        "GET" => Ok(http::Verb::Get),
        "POST" => Ok(http::Verb::Post),
        "PUT" => Ok(http::Verb::Put),
        "PATCH" => Ok(http::Verb::Patch),
        "DELETE" => Ok(http::Verb::Delete),
        other => Err(format!("unsupported method {other:?}")),
    }
}

/// Perform one outbound call and fold both outcomes into an `Attempt`.
///
/// A transport error is recorded rather than propagated: "the call failed" is
/// itself an auditable fact, and losing it would leave a caller unable to tell a
/// failed send from a send that was never attempted.
fn perform(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&String>,
) -> (Attempt, Option<Vec<u8>>) {
    let at = now();
    let v = match verb(method) {
        Ok(v) => v,
        Err(e) => return (failed_attempt(at, e), None),
    };
    let request = http::Request {
        method: v,
        url: url.to_string(),
        headers: Some(headers.to_vec()),
        payload: body.map(|b| b.as_bytes().to_vec()),
    };
    match http::call(&request) {
        Ok(resp) => (
            Attempt {
                at,
                code: resp.code,
                body_sha256: sha256_hex(&resp.payload),
                body_len: resp.payload.len(),
                error: None,
            },
            Some(resp.payload),
        ),
        Err(e) => (failed_attempt(at, format!("transport: {e}")), None),
    }
}

fn failed_attempt(at: u64, error: String) -> Attempt {
    Attempt { at, code: 0, body_sha256: sha256_hex(b""), body_len: 0, error: Some(error) }
}

fn json_out<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    serde_json::to_vec(value).map_err(|e| format!("serialize response: {e}"))
}

// ---------------------------------------------------------------------------
// Exported operations
// ---------------------------------------------------------------------------

/// Perform the effect at most once per idempotency key.
///
/// The existence check happens BEFORE the outbound call, so a retried submit of
/// a key that already ran returns the stored record without touching the world.
/// This function cannot produce `Confirmed` under any input.
pub fn submit_action(input: &[u8]) -> Result<Vec<u8>, String> {
    let req: SubmitRequest =
        serde_json::from_slice(input).map_err(|e| format!("submit-action: bad input: {e}"))?;

    if req.idempotency_key.is_empty() {
        return Err("submit-action: idempotency_key must not be empty".to_string());
    }

    // Replay guard. Returning the prior record is the whole point: an agent that
    // retries a send because it did not like the first answer is how double-sends
    // happen, and the caller cannot opt out of this check.
    if let Some(existing) = load(&req.idempotency_key)? {
        let _ = logging::info(&format!(
            "submit-action: replay of {} in state {:?}; no outbound call made",
            existing.idempotency_key, existing.status
        ));
        return json_out(&existing);
    }

    let (attempt, _body) = perform(
        &req.action.method,
        &req.action.url,
        &req.action.headers,
        req.action.body.as_ref(),
    );

    let record = Record::submitted(
        req.idempotency_key.clone(),
        req.action,
        attempt,
        now(),
        tenant_context::seq_no(),
        tenant_context::contract_id(),
    );

    store(&record)?;
    index_append(&record.idempotency_key);
    let _ = logging::info(&format!(
        "submit-action: {} -> {:?} (not yet verified)",
        record.idempotency_key, record.status
    ));
    json_out(&record)
}

/// Independently re-read external state and settle the record.
///
/// Idempotent: an already-confirmed record is returned untouched, so a scheduler
/// can call this on a whole page of pending records without special-casing.
pub fn verify_action(input: &[u8]) -> Result<Vec<u8>, String> {
    let req: KeyRequest =
        serde_json::from_slice(input).map_err(|e| format!("verify-action: bad input: {e}"))?;

    let mut record = load(&req.idempotency_key)?
        .ok_or_else(|| format!("verify-action: no record for {}", req.idempotency_key))?;

    if record.status == Status::Confirmed {
        return json_out(&record);
    }

    // A transport-level failure means the effect probably never left, so there is
    // nothing external to observe. Probing anyway would waste a call and risk
    // reading someone else's state into our record.
    if record.status == Status::Failed {
        return json_out(&record);
    }

    let v = &record.action.verify;
    let (probe, body) = perform(&v.method, &v.url, &v.headers, v.body.as_ref());

    let verdict = match (&probe.error, &body) {
        (Some(e), _) => Err(format!("verification probe failed: {e}")),
        (None, Some(bytes)) => {
            let text = String::from_utf8_lossy(bytes);
            v.expect.evaluate(probe.code, &text)
        }
        (None, None) => Err("verification probe returned no body".to_string()),
    };

    record.settle(probe, verdict, now());
    store(&record)?;

    let _ = logging::info(&format!(
        "verify-action: {} -> {:?}",
        record.idempotency_key, record.status
    ));
    json_out(&record)
}

pub fn get_record(input: &[u8]) -> Result<Vec<u8>, String> {
    let req: KeyRequest =
        serde_json::from_slice(input).map_err(|e| format!("get-record: bad input: {e}"))?;
    let record = load(&req.idempotency_key)?
        .ok_or_else(|| format!("get-record: no record for {}", req.idempotency_key))?;
    json_out(&record)
}

/// Range-scan the ledger. One-shot: there is no host-side cursor, so a full walk
/// re-calls with `start` set to the returned `next_start`.
pub fn list_records(input: &[u8]) -> Result<Vec<u8>, String> {
    let req: ListRequest = if input.is_empty() {
        ListRequest::default()
    } else {
        serde_json::from_slice(input).map_err(|e| format!("list-records: bad input: {e}"))?
    };

    let start = req.start.unwrap_or_default();
    // 0xFF sorts after any printable key, making the default range "everything
    // from `start` onward" without the caller naming an upper bound.
    let end = req.end.map(|e| e.into_bytes()).unwrap_or_else(|| alloc::vec![0xffu8]);
    let limit = req.limit.filter(|l| *l > 0).unwrap_or(DEFAULT_SCAN_LIMIT);

    // Primary path. Range and limit are honoured correctly by the host; only the
    // returned values are unusable, so we take the keys from here.
    let rows = kv_store::scan(&map_name(), start.as_bytes(), &end, limit)
        .map_err(|e| format!("kv scan: {e}"))?;

    let scanned = rows.len();
    let scanned_keys: Vec<String> = rows
        .iter()
        .take(10)
        .map(|(k, _)| String::from_utf8_lossy(k).to_string())
        .collect();
    let scanned_value_lens: Vec<usize> = rows.iter().take(10).map(|(_, v)| v.len()).collect();
    let scanned_value_head: Option<String> = rows
        .iter()
        .find(|(k, _)| !String::from_utf8_lossy(k).starts_with("__"))
        .map(|(_, v)| String::from_utf8_lossy(&v[..v.len().min(160)]).to_string());

    // scan's KEYS are correct; its VALUES are not.
    //
    // Verified 2026-09-06 on testnet: a value above the storage inlining threshold
    // comes back from `scan` as an unresolved storage envelope --
    // `T3VR{"value_cid":[..32 bytes..],"size_bytes":1011,...}` -- rather than the
    // stored bytes. `get` on the same key dereferences correctly. Small values
    // (the 12-byte index here) come back intact, which is what makes this easy to
    // miss in a smoke test.
    //
    // So: take the keys from scan and read each record through `get`. If the host
    // starts dereferencing scan values, this path keeps working unchanged.
    let mut keys: Vec<String> = rows
        .iter()
        .map(|(k, _)| String::from_utf8_lossy(k).to_string())
        .filter(|k| !k.starts_with("__")) // internal bookkeeping, never a record
        .collect();

    let mut source = "scan+get";
    if keys.is_empty() {
        keys = index_load();
        source = "index";
    }

    // Redundant when rows came from `scan` (the host already applied the bounds),
    // but required for the index fallback, and harmless either way.
    let end_str = String::from_utf8_lossy(&end).to_string();
    keys.retain(|k| k.as_str() >= start.as_str() && k.as_str() < end_str.as_str());
    keys.sort();

    let hit_limit = keys.len() > limit as usize;
    keys.truncate(limit as usize);

    let mut records = Vec::with_capacity(keys.len());
    for k in &keys {
        match load(k) {
            Ok(Some(r)) => records.push(r),
            Ok(None) => {
                let _ = logging::error(&format!("list-records: key {k} has no record"));
            }
            Err(e) => {
                let _ = logging::error(&format!("list-records: {k}: {e}"));
            }
        }
    }

    json_out(&ListResponse {
        records,
        next_start: if hit_limit { keys.last().cloned() } else { None },
        source,
        scanned,
        scanned_keys,
        scanned_value_lens,
        scanned_value_head,
    })
}
