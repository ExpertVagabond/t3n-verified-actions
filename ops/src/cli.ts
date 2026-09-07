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

function arg(flag: string): string | undefined {
  const i = process.argv.indexOf(flag);
  return i === -1 ? undefined : process.argv[i + 1];
}
function args(flag: string): string[] {
  return process.argv.reduce<string[]>(
    (acc, v, i) => (v === flag && process.argv[i + 1] ? [...acc, process.argv[i + 1]] : acc),
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
  try {
    await tenant.maps.create({
      tail: MAP_TAIL,
      visibility: "private",
      writers: { only: [contract_id] },
      readers: { only: [contract_id] },
    });
    console.log(`map         z:<tid>:${MAP_TAIL} created`);
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    if (!msg.includes("MapAlreadyExists")) throw e;
    console.log(`map         z:<tid>:${MAP_TAIL} already exists (ok)`);
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
  const { client, tenantDid } = await connectTenant();
  const script = scriptName(tenantDid);
  return client.executeAndDecode({
    contract_id: script,
    contract_version: await currentVersion(script),
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
  const cmd = process.argv[2];
  switch (cmd) {
    case "doctor":
      return doctor();
    case "deploy":
      return deploy();
    case "submit": {
      const path = process.argv[3];
      if (!path) throw new Error("usage: submit <spec.json>");
      return show(await call("submit-action", JSON.parse(await readFile(path, "utf8"))));
    }
    case "verify":
      return show(await call("verify-action", { idempotency_key: process.argv[3] }));
    case "get":
      return show(await call("get-record", { idempotency_key: process.argv[3] }));
    case "list":
      return show(await call("list-records", { limit: Number(arg("--limit") ?? 100) }));
    default:
      console.log(readFileSync(new URL("./cli.ts", import.meta.url), "utf8").split("\n").slice(1, 9).join("\n").replace(/^\/\/ ?/gm, ""));
      process.exit(cmd ? 1 : 0);
  }
}

main().catch((e) => die(process.argv[2] ?? "cli", e));
