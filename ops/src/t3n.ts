// Session helpers. Every command goes through here so there is exactly one place
// that knows how to authenticate, and one place to change when the SDK moves.
import {
  T3nClient,
  TenantClient,
  setEnvironment,
  loadWasmComponent,
  eth_get_address,
  metamask_sign,
  createEthAuthInput,
  fetchTrustedManifest,
  getNodeUrl,
  getContractVersion,
} from "@terminal3/t3n-sdk";

export const CONTRACT_TAIL = "verified-actions";
export const MAP_TAIL = "actions";

/** Fail loudly and early rather than three calls later with a confusing error. */
export function requireEnv(name: string): string {
  const v = process.env[name];
  if (!v) throw new Error(`${name} is not set. Copy ops/.env.example to ops/.env and fill it in.`);
  return v;
}

/**
 * Authenticate one credential.
 *
 * The DID is always read back from the session. It is an opaque server-assigned
 * id with no relationship to the key's address, so deriving it locally produces a
 * value that looks plausible and is wrong.
 */
export async function connect(apiKey: string, label: string) {
  setEnvironment("testnet");
  const wasmComponent = await loadWasmComponent();
  const address = eth_get_address(apiKey);

  const client = new T3nClient({
    trustAnchor: await fetchTrustedManifest("testnet"),
    wasmComponent,
    handlers: { EthSign: metamask_sign(address, undefined, apiKey) },
  });

  await client.handshake();
  const did = (await client.authenticate(createEthAuthInput(address))).value;
  console.log(`[${label}] authenticated as ${did}`);
  return { client, did, address };
}

/** Tenant management session — registers contracts and owns maps. */
export async function connectTenant() {
  const { client, did } = await connect(requireEnv("T3N_API_KEY"), "tenant");

  const expected = process.env.T3N_DID;
  if (expected && expected !== did) {
    throw new Error(
      `authenticated DID ${did} does not match T3N_DID ${expected}. ` +
        `The key in your environment belongs to a different account.`,
    );
  }

  const tenant = new TenantClient({ t3n: client, baseUrl: getNodeUrl(), tenantDid: did });
  await tenant.tenant.me(); // throws if the session is not an admitted tenant
  return { client, tenant, tenantDid: did };
}

export function scriptName(tenantDid: string, tail = CONTRACT_TAIL) {
  return `z:${tenantDid.slice("did:t3n:".length)}:${tail}`;
}

export async function currentVersion(script: string) {
  return getContractVersion(getNodeUrl(), script);
}

/**
 * SDK calls reject with the entire obfuscated bundle in the stack (~1.6MB), which
 * buries the message. Print only what is useful and exit non-zero.
 */
export function die(context: string, e: unknown): never {
  const msg = e instanceof Error ? e.message : String(e);
  console.error(`\n${context}: ${msg}`);
  if (msg.includes("Trust manifest") && msg.includes("malformed")) {
    console.error(
      "\nThis is the known SDK regression: >=5.3.0 requires an rtmr1_allowlist field\n" +
        "that the testnet manifest does not publish. Pin 5.2.0:\n" +
        "  npm install @terminal3/t3n-sdk@5.2.0\n" +
        "See https://github.com/ExpertVagabond/t3n-sdk-manifest-bug",
    );
  }
  process.exit(1);
}
