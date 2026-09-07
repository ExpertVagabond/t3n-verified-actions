// One CLI for the whole lifecycle. Every command is idempotent and prints what it
// actually did, so re-running after a partial failure is always safe.
//
//   npm run t3n -- doctor                     check credentials and the SDK pin
//   npm run t3n -- deploy --allow-host <h>    register + create map + ACL + grant
//   npm run t3n -- submit <spec.json>         perform an action once
//   npm run t3n -- verify <key>               independently confirm it
//   npm run t3n -- get <key>
//   npm run t3n -- list [--limit N]
import { readFile } from "node:fs/promises";
import { existsSync, readFileSync } from "node:fs";
import { join, dirname } from "node:path";
import {
  CONTRACT_TAIL,
  MAP_TAIL,
  connectTenant,
  currentVersion,
  die,
  scriptName,
} from "./t3n.js";

// Relative to ops/src/, so two levels up to the crate root.
const WASM = "../../target/wasm32-wasip2/release/z_verified_actions.wasm";

// Arguments as the shell saw them, snapshotted by ./t3n before any import ran.
// Falls back to process.argv when the CLI is invoked directly.
const ARGV: string[] = process.env.T3N_ARGV
  ? process.env.T3N_ARGV.split(" ").filter((s) => s.length > 0)
  : process.argv.slice(2);

function arg(flag: string): string | undefined {
  const i = ARGV.indexOf(flag);
  return i === -1 ? undefined : ARGV[i + 1];
}
function args(flag: string): string[] {
  return ARGV.reduce<string[]>(
    (acc, v, i) => (v === flag && ARGV[i + 1] ? [...acc, ARGV[i + 1]] : acc),
    [],
  );
}

/** Report the SDK version actually resolved. The exports map blocks deep imports. */
function sdkVersion(): string {
  let d = process.cwd();
  for (;;) {
    const p = join(d, "node_modules", "@terminal3", "t3n-sdk", "package.json");
    if (existsSync(p)) return JSON.parse(readFileSync(p, "utf8")).version;
    const up = dirname(d);
    if (up === d) return "unknown";
    d = up;
  }
}

async function doctor() {
  const v = sdkVersion();
  const [maj, min] = v.split(".").map(Number);
  const pinned = maj < 5 || (maj === 5 && min <= 2);
  console.log(`sdk version : ${v} ${pinned ? "(ok)" : "(TOO NEW — pin 5.2.0)"}`);
  console.log(`api key     : ${process.env.T3N_API_KEY ? "set" : "MISSING"}`);
  console.log(`expected did: ${process.env.T3N_DID ?? "(not set — skipping match check)"}`);
  if (!pinned) {
    console.error("\n>=5.3.0 cannot reach testnet. Run: npm install @terminal3/t3n-sdk@5.2.0");
    process.exit(1);
  }
  const { tenantDid } = await connectTenant();
  const script = scriptName(tenantDid);
  console.log(`tenant      : ok`);
  console.log(`script name : ${script}`);
  try {
    console.log(`registered  : version ${await currentVersion(script)}`);
  } catch {
    console.log(`registered  : not yet — run 'npm run t3n -- deploy'`);
  }
}

/**
 * Register the contract, create its ledger map, and self-grant egress.
 *
 * Ordering matters and is not obvious: the map ACL needs the contract_id that
 * registration returns, so registration must come first. Re-registering a tail
 * allocates a NEW contract_id and there is no API to read a tail's current id,
 * so the id is printed every time — record it, or the map ACL silently points at
 * a stale contract and reads fail with AccessDenied.
 */
async function deploy() {
  const allowHosts = args("--allow-host");
  if (allowHosts.length === 0) {
    console.error(
      "deploy needs at least one --allow-host. Outbound hosts come from the caller's\n" +
        "grant, not the contract, so a contract with no grant cannot dial anything.\n" +
        "  npm run t3n -- deploy --allow-host api.example.com",
    );
    process.exit(1);
  }

  const { client, tenant, tenantDid } = await connectTenant();
  const version = arg("--version") ?? "0.1.0";

  const wasm = await readFile(new URL(WASM, import.meta.url));
  console.log(`registering ${CONTRACT_TAIL} v${version} (${wasm.length} bytes)`);
  const { contract_id } = await tenant.contracts.register({
    tail: CONTRACT_TAIL,
    version,
    wasm,
  });
  const script = scriptName(tenantDid);
  console.log(`registered  ${script} -> contract_id ${contract_id}   <-- RECORD THIS`);

  // readers must be explicit: the KV governor defaults to deny, and omitting it
  // fails at runtime with AccessDenied rather than here.
  const acl = {
    visibility: "private" as const,
    writers: { only: [contract_id] },
    readers: { only: [contract_id] },
  };
  try {
    await tenant.maps.create({ tail: MAP_TAIL, ...acl });
    console.log(`map         z:<tid>:${MAP_TAIL} created`);
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    // The server has used both "MapAlreadyExists" and "map already exists".
    if (!/map\s*already\s*exists/i.test(msg)) throw e;
    // The map outlives the contract, but its ACL is scoped by contract_id and a
    // re-register just allocated a new one. Without this the contract silently
    // loses access to its own ledger and every call fails with AccessDenied.
    console.log(`map         z:<tid>:${MAP_TAIL} exists; re-pointing ACL at ${contract_id}`);
    try {
      // NOTE: maps.create takes an object containing `tail`, but maps.update,
      // maps.getStatus and maps.entryGet take the tail POSITIONALLY. Passing the
      // object form here fails with a confusing "Tenant name tail must match"
      // regex error, because the object lands in the tail slot.
      await tenant.maps.update(MAP_TAIL, acl);
      console.log(`map ACL     updated -> contract ${contract_id}`);
    } catch (e2) {
      const m2 = e2 instanceof Error ? e2.message : String(e2);
      console.error(
        `\nWARNING: could not update the map ACL (${m2}).\n` +
          `The contract will fail with AccessDenied until the 'actions' map grants\n` +
          `read+write to contract ${contract_id}.`,
      );
    }
  }

  // Self-grant: tenant, agent and user are the same DID here, which is what lets
  // a single claimed credential run the whole flow. Split them for production.
  const scriptVersion = await currentVersion(script);
  const userContracts = await currentVersion("tee:user/contracts");
  await client.execute({
    contract_id: "tee:user/contracts",
    contract_version: userContracts,
    function_name: "agent-auth-update",
    input: {
      agents: [
        {
          agentDid: tenantDid,
          scripts: [
            {
              scriptName: script,
              versionReq: scriptVersion,
              functions: ["submit-action", "verify-action", "get-record", "list-records"],
              allowedHosts: allowHosts,
            },
          ],
        },
      ],
    },
  });
  console.log(`grant       self-grant for ${allowHosts.join(", ")}`);
  console.log(`\ndeploy complete. contract_id=${contract_id} version=${scriptVersion}`);
}

async function call(fn: string, input: unknown) {
  if (process.env.T3N_DEBUG_ARGV) console.error("input:", JSON.stringify(input));
  const { client, tenantDid } = await connectTenant();
  const script = scriptName(tenantDid);
  const ver = await currentVersion(script);
  if (process.env.T3N_DEBUG_ARGV) console.error("resolved contract_version:", ver);
  return client.executeAndDecode({
    contract_id: script,
    contract_version: ver,
    function_name: fn,
    input,
  });
}

function show(record: any) {
  console.log(JSON.stringify(record, null, 2));
  const status = record?.status ?? record?.records?.length;
  if (typeof status === "string") {
    console.log(
      `\nstatus: ${status}` +
        (status === "pending_verification"
          ? "  <-- NOT confirmed. Run: npm run t3n -- verify <key>"
          : ""),
    );
  }
}

async function main() {
  if (process.env.T3N_DEBUG_ARGV) {
    console.error("argv:", JSON.stringify(process.argv.slice(1)));
    console.error("arg(--limit)=", JSON.stringify(arg("--limit")), "arg(--start)=", JSON.stringify(arg("--start")));
  }
  const cmd = ARGV[0];
  switch (cmd) {
    case "doctor":
      return doctor();
    case "deploy":
      return deploy();
    case "submit": {
      const path = ARGV[1];
      if (!path) throw new Error("usage: submit <spec.json>");
      return show(await call("submit-action", JSON.parse(await readFile(path, "utf8"))));
    }
    case "verify":
      return show(await call("verify-action", { idempotency_key: ARGV[1] }));
    case "get":
      return show(await call("get-record", { idempotency_key: ARGV[1] }));
    case "list": {
      const input = {
        limit: Number(arg("--limit") ?? 100),
        ...(arg("--start") !== undefined ? { start: arg("--start") } : {}),
        ...(arg("--end") !== undefined ? { end: arg("--end") } : {}),
      };
      // Printed unconditionally. Flag parsing has been observed to silently fall
      // back to defaults in some shells, and a listing that quietly ignores
      // --limit is exactly the kind of unverified answer this project exists to
      // refuse. If this line does not match what you typed, the flags did not
      // take -- pass the payload directly instead.
      console.error(`requested: ${JSON.stringify(input)}`);
      const res: any = await call("list-records", input);
      console.error(
        `returned : ${res.records?.length ?? 0} records via ${res.source} ` +
          `(scan saw ${res.scanned})`,
      );
      return show(res);
    }
    default:
      console.log(readFileSync(new URL("./cli.ts", import.meta.url), "utf8").split("\n").slice(1, 9).join("\n").replace(/^\/\/ ?/gm, ""));
      process.exit(cmd ? 1 : 0);
  }
}

main().catch((e) => die(process.argv[2] ?? "cli", e));
