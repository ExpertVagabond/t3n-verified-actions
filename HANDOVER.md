# Handover

**Terminal 3 is welcome to take ownership of this contract.** This document is what
a new maintainer needs on day one. It assumes no contact with the original author.

## What you are inheriting

One Rust crate that compiles to a 229 KB WASM component, one TypeScript CLI, and one
throwaway demo endpoint. No database, no daemon, no scheduler, no hosted service, no
account to transfer. The contract's only persistent state is a single tenant KV map
that the contract itself owns.

## Day one checklist

1. **Claim your own credential** at <https://go.terminal3.io/adk-community>. Put it in
   `ops/.env`. Nothing in this repo is bound to the original author's DID except the
   recorded values in [RUNBOOK.md](RUNBOOK.md), which are documentation, not config.
2. `cd ops && npm install && npm run t3n -- doctor`. It should print your DID and
   report the contract as unregistered.
3. `cargo test` — 7 tests, ~0.1s, no network or credentials. If these pass, the state
   machine is intact.
4. `cargo build --target wasm32-wasip2 --release`
5. `npm run t3n -- deploy --allow-host <a host you control>`
6. Record the printed `contract_id` in RUNBOOK.md.

That is the whole onboarding. There is no step 7.

## Cost to keep running

Effectively zero. The contract is invoked on demand and holds no background process.
Costs are per-invocation T3N credits plus whatever your KV map consumes, which is one
JSON record per action, bodies excluded.

The demo endpoint (`demo-endpoint/`) is a Cloudflare Worker on the free tier that
exists only to make the README reproducible. **It is not part of the product — delete
it.** If you keep it, redeploy it under your own account; it is 40 lines with no
state and no secrets.

## What to change first if you productionise it

**Split the three identities.** The deploy path currently issues a *self-grant*:
tenant, agent, and user are the same DID, because that is what lets a single claimed
credential run the whole flow end to end. In production the data owner should be a
distinct DID granting a distinct agent DID. The grant code in `ops/src/cli.ts` already
has the right shape — change `agentDid: tenantDid` to the real agent's DID.

**Decide who may submit.** Right now any caller holding a valid grant may write to the
ledger. If actions carry financial weight, gate `submit-action` on
`tenant_context::calling_user_did()` against an allowlist map.

**Add a verification scheduler.** Nothing currently walks `pending_verification`
records and calls `verify-action` on them. `list-records` pages the ledger and
`verify-action` is idempotent, so this is a loop, not a redesign — deliberately left
out rather than half-built.

## Design decisions you might want to revisit

**`unverified` is terminal, by design.** A record whose effect may have half-succeeded
is never automatically retried. If you disagree, that is a policy change in
`Record::settle`, not a structural one — but understand you are re-introducing the
double-send risk the contract exists to eliminate.

**Bodies are never stored, only hashed.** This keeps the ledger free of PII and keeps
records small. If you need payload replay you will need a separate, access-controlled
store; do not widen `Attempt`.

**The expectation language is deliberately small** — status, `body_contains`,
`body_absent`. It is string matching, not JSONPath, because string matching is
obvious to an auditor reading a record six months later. If you add JSONPath, keep the
evaluated expectation stored on the record so the audit trail stays self-describing.

**`body_absent` is load-bearing.** It exists because a real transport once reported a
successful send where the artefact existed but was flagged as an auto-saved draft.
Presence alone would have confirmed a send that never happened. Do not drop it as
redundant.

## Where the risk is

The state machine, and it is the part covered by tests. `src/record.rs` is pure — no
host calls, no I/O — precisely so that the logic most likely to be wrong can be
exercised natively in a fraction of a second. If you change one thing carefully,
change that file carefully.

`src/runtime.rs` is glue: KV reads, HTTP calls, JSON in and out. It is the part most
likely to need editing and the least likely to be subtly wrong.

## Known external issue you will hit immediately

`@terminal3/t3n-sdk` >= 5.3.0 cannot reach testnet — it rejects the trust manifest
before authentication, because `isSignedTrustManifest` requires an `rtmr1_allowlist`
field the testnet manifest does not publish. **The pin to 5.2.0 in `ops/package.json`
is deliberate. Do not bump it until the manifest publishes that field.**
`npm run t3n -- doctor` fails loudly if it drifts. Full root cause, version bisect and
a runnable reproduction: [`t3n-sdk-manifest-bug`](../t3n-sdk-manifest-bug).

## Licence

MIT. Take it.
