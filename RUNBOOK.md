# Runbook

Day-to-day operation of `z-verified-actions`. Written for whoever is on call, not
for whoever wrote it.

## Deployed state (testnet, 2026-09-06)

| | |
|---|---|
| tenant DID | `did:t3n:99466752729636dd5f033007a5517924fd792902` |
| script | `z:99466752729636dd5f033007a5517924fd792902:verified-actions` |
| contract_id | **896** |
| contract version | 0.1.6 |
| ledger map | `z:<tid>:actions` (private; reader+writer = contract 896) |
| granted host | `t3n-verified-actions-demo.purplesquirrelnetworks.workers.dev` |
| SDK | **pinned 5.2.0** (confirmed by Terminal 3 DevRel: "yes please use sdkv5.2 for this challenge") |

`contract_id` is recorded here deliberately. Re-registering the tail allocates a
**new** id, and there is no API to read a tail's current id (`contracts.listDetailed()`
returns name, version and status but *not* the id). `deploy` now re-points the map ACL
automatically on every re-registration, so this is handled — but keep the record
current anyway, because it is the only place the id exists.

## First thing to run

```bash
cd ops && ./t3n doctor
```

**Always invoke `./t3n` directly, never `npm run t3n -- ...`.** See the argv note in
failure modes below.

It checks, in order: the SDK pin, that the key is present, that the authenticated
DID matches `T3N_DID`, that the session is an admitted tenant, and whether the
contract is registered. Every failure names the command that fixes it.

## Normal operations

```bash
./t3n submit example-action.json     # perform an effect once
./t3n verify <idempotency_key>       # independently confirm it
./t3n get <idempotency_key>          # read one record
./t3n list --limit 50 --start <key>  # page the ledger
```

`list` prints the parameters it actually sent (`requested: {...}`) and what came back
(`returned : N records via <source>`). If `requested` does not match what you typed,
the flags did not reach the contract — do not trust the listing.

Everything is idempotent. Re-running `submit` with a key that already exists
returns the stored record and makes **no** outbound call. Re-running `verify` on a
confirmed record returns it unchanged.

## Reading a record

The only field that matters operationally is `status`.

| status | meaning | what to do |
|---|---|---|
| `pending_verification` | The transport accepted it. Nothing has been proven. | Run `verify`. |
| `confirmed` | An independent probe observed the expected state. | Nothing. This is the only status you may report as success. |
| `unverified` | A probe ran and the expectation was **not** met. | **Escalate to a human. Do not auto-retry.** |
| `failed` | The transport itself errored, so the effect probably never left. | Safe to resubmit under a **new** idempotency key. |

`reason` always names the specific clause that failed, e.g.
`expected response to contain "\"state\": \"delivered\""`.

### Why `unverified` is not a retry signal

An `unverified` record means the effect may have half-succeeded: the request left,
something happened, and the world does not look the way it should. Retrying that
automatically is how one invoice becomes two payments. The contract deliberately
gives you no automatic path out of `unverified` — a human decides.

## Failure modes seen in practice

**`Trust manifest ... is malformed`**
The SDK pin has drifted above 5.2.0. `doctor` catches this first and prints the fix.
Root cause and reproduction: [`t3n-sdk-manifest-bug`](https://github.com/ExpertVagabond/t3n-sdk-manifest-bug).

**`AccessDenied` reading the ledger**
The map ACL points at a stale `contract_id`. You re-registered without re-running
map setup. Fix: note the new id from `deploy`, and re-create the ACL against it.

**`host/http.egress_denied`**
The target host is not in the caller's grant. Outbound hosts come from the *caller's*
authorization grant, not from the contract, so adding a new destination means
re-running `deploy --allow-host <new-host>`, not rebuilding the WASM.

**`InsufficientCreditError`**
Credits are per-DID and an agent DID starts at zero — it does not inherit the
tenant's balance. Request more from Terminal 3 DevRel with your DID.

**`list` quietly ignores `--limit` / `--start`**
Flags were reaching `process.argv` but were gone by the time `main()` read them: the
SDK initialises a WASI shim at import time. `./t3n` snapshots the arguments into
`T3N_ARGV` in the shell before any JavaScript runs, which is immune to this. If you
invoke `src/cli.ts` some other way, set `T3N_ARGV` yourself or expect defaults.

**Stale behaviour after editing `cli.ts`**
tsx caches compiled output and will happily run an old build. `./t3n` sets
`TSX_DISABLE_CACHE=1`.

**`list` returns fewer records than the ledger holds**
The host applies `limit` to raw rows *including* the internal `__index` key, which is
then filtered out. Ask for a slightly higher limit than the record count you want.

**A 1.6 MB wall of minified JavaScript**
An SDK call rejected and the whole obfuscated bundle is in the stack. The CLI
already filters this; if you call the SDK directly, wrap it and print `e.message`.

## Re-deploying a new contract version

```bash
cargo test                                              # never skip; it is 0.1s
cargo build --target wasm32-wasip2 --release
cd ops && ./t3n deploy --version 0.1.7 --allow-host <host>
```

Then **write the new `contract_id` into this file**. The version must be strictly
higher than the last one or registration is rejected. `deploy` re-points the map ACL
at the new id automatically and prints `map ACL updated -> contract <id>`; if that
line is missing, the contract has no ledger access.

## Verifying a receipt offline

Each write anchors `Record::claims_digest()` in the transaction's Merkle leaf. To
check a record you were handed without trusting whoever handed it to you, recompute
the digest from the record's own fields and compare it against the leaf. The digest
covers a fixed field list in a fixed order (see `src/record.rs`), so it is stable
across serde changes and new optional fields.

## A host quirk worth knowing

`kv-store.scan` returns correct **keys** but not always correct **values**. A value
above the storage inlining threshold comes back as an unresolved envelope,
`T3VR{"value_cid":[...],"size_bytes":1011,...}`, instead of the stored bytes;
`get` on the same key dereferences correctly. Small values come back intact, which is
what makes it easy to miss. `list-records` therefore uses scan for keys and `get` for
values, and reports `source` so you can see which path ran. Range and limit semantics
on `scan` are correct.

## What this contract deliberately does not do

- It does not retry. Retrying an unverified side effect is the failure it exists to prevent.
- It does not store request or response bodies, only their SHA-256 and length. The
  ledger is an audit trail, not a copy of your payloads.
- It does not accept an action without a verification spec. An action you cannot
  check is an action it will not perform.
