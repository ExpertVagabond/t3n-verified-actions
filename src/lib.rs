//! z-verified-actions — an append-only, offline-verifiable action ledger.
//!
//! See `wit/world.wit` for the design rationale. In short: a 2xx is not evidence.
//! `submit-action` performs an effect at most once per idempotency key and can
//! only ever produce `pending_verification`; `verify-action` performs a separate
//! read-back against external state and is the only path to `confirmed`.
//!
//! Layout:
//!   record.rs   pure record shapes + state machine. Native `cargo test` covers this.
//!   runtime.rs  host glue (kv-store, http, logging). Compiled for wasm only.

extern crate alloc;

pub mod record;

// The generated bindings declare imports that only exist inside the host, so they
// are gated to wasm. Everything a test needs to reach lives in `record`, which
// means `cargo test` runs natively with no TEE, node, or credentials.
#[cfg(target_arch = "wasm32")]
wit_bindgen::generate!({
    world: "verified-actions",
    path: "wit",
    additional_derives: [serde::Deserialize, serde::Serialize],
    generate_all,
});

#[cfg(target_arch = "wasm32")]
mod runtime;

#[cfg(target_arch = "wasm32")]
struct Component;

#[cfg(target_arch = "wasm32")]
impl exports::z::verified_actions::contracts::Guest for Component {
    fn submit_action(
        req: exports::z::verified_actions::contracts::GenericInput,
    ) -> Result<alloc::vec::Vec<u8>, alloc::string::String> {
        runtime::submit_action(&req.input.ok_or("submit-action: missing input")?)
    }

    fn verify_action(
        req: exports::z::verified_actions::contracts::GenericInput,
    ) -> Result<alloc::vec::Vec<u8>, alloc::string::String> {
        runtime::verify_action(&req.input.ok_or("verify-action: missing input")?)
    }

    fn get_record(
        req: exports::z::verified_actions::contracts::GenericInput,
    ) -> Result<alloc::vec::Vec<u8>, alloc::string::String> {
        runtime::get_record(&req.input.ok_or("get-record: missing input")?)
    }

    fn list_records(
        req: exports::z::verified_actions::contracts::GenericInput,
    ) -> Result<alloc::vec::Vec<u8>, alloc::string::String> {
        runtime::list_records(&req.input.unwrap_or_default())
    }
}

#[cfg(target_arch = "wasm32")]
export!(Component);
