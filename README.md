# z-verified-actions

**An enterprise agent that will not tell you an action succeeded until it has
checked.**

A TEE contract for [Terminal 3](https://terminal3.io) that performs side effects
(webhooks, payments, notifications, provisioning calls) and records each one in an
append-only, offline-verifiable ledger. Its entire premise is one rule:

> **A 2xx response is not evidence that an action happened.**

Transports lie. A mail API returns 200 and saves a draft. A webhook returns 202 and
drops the job. A retry double-charges because the first attempt was recorded as
failed when it had actually succeeded. An agent that reports success from a status
code will eventually report something that did not happen — and in an enterprise,
that is the failure that costs money and trust.

So this contract makes it structurally impossible:

| | |
|---|---|
| `submit-action` | Performs the effect **at most once** per idempotency key. Can only ever produce `pending_verification`. There is no input that makes it return `confirmed`. |
| `verify-action` | Issues a **different** request that reads back the state the effect was supposed to change. This is the only transition into `confirmed`. |

Every write anchors a SHA-256 of the record in the transaction's Merkle leaf via
`kv-store.set-claims-digest`, so an auditor can verify a receipt offline without
trusting the node that served it.

---

## Why this shape

The obvious enterprise agent on a TEE is a credential vault — seal a secret, gate
who may use it. That answers *"who is allowed to act."*

It does not answer *"did the act happen."* Those are different questions, and the
second one is where agent deployments actually break: an agent reports a
disbursement it never made, a retry sends the same invoice twice, an audit asks for
proof six months later and all anyone has is a log line that says `200 OK`.

The design here is lifted from an action ledger the author has run in production for
his own side effects, where the operating rule is *"a zero exit code is not
evidence."* The contribution is putting that discipline inside an enclave and
anchoring it in a Merkle leaf, so the audit trail is not merely honest — it is
checkable by someone who does not trust the operator.

## States

```
                          transport errored
     submit-action ─────────────────────────► failed        (safe to resubmit)
          │
          │ transport accepted
          ▼
   pending_verification
          │
          │ verify-action  (an INDEPENDENT read-back)
          ├─────────── expectation met ─────► confirmed
          └─────────── expectation missed ──► unverified    (do NOT auto-retry)
```

`unverified` is deliberately terminal. An action that may have half-succeeded is
the single most dangerous thing to retry automatically, so the contract escalates
to a human instead of guessing.

## The expectation model

A verification probe declares what must be true, and what must **not** be:

```json
"expect": {
  "status": 200,
  "body_contains": ["INV-1042", "\"state\":\"delivered\""],
  "body_absent":   ["\"state\":\"queued\"", "\"state\":\"failed\""]
}
```

`body_absent` is not decoration. The case that motivated it: a mail transport
reported success and the message did exist — flagged as an auto-saved draft rather
than a sent message. Presence alone would have confirmed a send that never
happened. That exact scenario is covered by a unit test
(`presence_of_a_draft_marker_blocks_confirmation`).

## Proven end-to-end on testnet

Registered as `contract_id 890` on T3N testnet and run against a deliberately
unreliable endpoint (`demo-endpoint/`) whose `POST /notify` **always** returns 200
while `GET /deliveries` reports what actually happened.

```
$ npm run t3n -- submit example-action-lying.json
  status=pending_verification  effect_code=200  verify_attempts=0
  reason: transport accepted the request; not yet independently verified

$ npm run t3n -- verify demo-lying-001
  status=unverified  effect_code=200  verify_attempts=1
  reason: expected response to contain "\"state\": \"delivered\""

$ npm run t3n -- submit example-action-lying.json     # same key, again
  status=unverified  effect_code=200  verify_attempts=1
  reason: expected response to contain "\"state\": \"delivered\""
```

Three things happened there. The transport returned **200** and the contract still
refused to call it a success. The independent read-back caught that the notification
was never delivered, and said exactly which clause failed. And replaying the same
idempotency key returned the stored record without making a second outbound call —
`verify_attempts` is still 1.

The honest path (`example-action.json`) reaches `confirmed` with
`reason: independently verified against external state`.

## Layout

```
src/record.rs    pure record shapes + state machine   <- `cargo test` covers this natively
src/runtime.rs   host glue (kv-store, http, logging)  <- wasm only
src/lib.rs       bindings + Guest impl
wit/world.wit    the world; the imports ARE the capability set
ops/             one CLI for the whole lifecycle
```

The state machine is deliberately isolated from the host so the part most likely to
contain a correctness bug is testable with `cargo test` — no TEE, no node, no
credentials, no network.

## Quick start

```bash
# 1. Build the contract
rustup target add wasm32-wasip2
cargo test                                     # 7 tests, no credentials needed
cargo build --target wasm32-wasip2 --release

# 2. Configure
cp ops/.env.example ops/.env                   # add your T3N_API_KEY
cd ops && npm install

# 3. Deploy and operate
npm run t3n -- doctor
npm run t3n -- deploy --allow-host api.example.com
npm run t3n -- submit example-action.json
npm run t3n -- verify demo-honest-001
npm run t3n -- list
```

> **Pin `@terminal3/t3n-sdk@5.2.0`.** Versions >= 5.3.0 cannot reach testnet at all —
> they reject the trust manifest before authentication is attempted. Root cause,
> bisect and reproduction: [`t3n-sdk-manifest-bug`](../t3n-sdk-manifest-bug).
> `npm run t3n -- doctor` fails loudly if the pin has drifted.

## Operating it

See [RUNBOOK.md](RUNBOOK.md) for day-to-day operations and failure modes, and
[HANDOVER.md](HANDOVER.md) for what a new owner needs on day one.

## Licence

MIT.
