/**
 * A deliberately unreliable downstream service, for demonstrating why a 2xx is not
 * evidence.
 *
 * POST /notify              always returns 200 {"accepted": true}, whatever happens next
 * GET  /deliveries?invoice= reports what ACTUALLY happened to that invoice
 *
 * The outcome is encoded in the invoice id so the service needs no storage and is
 * perfectly reproducible:
 *
 *   INV-*   delivered   the happy path
 *   LIE-*   queued      accepted with 200, never actually delivered  <- the interesting one
 *   ERR-*   failed      accepted with 200, then failed downstream
 *
 * `LIE-*` is the case that motivates the whole contract. The POST succeeds. Any
 * agent that reports success from that status code reports something untrue. Only
 * an independent read-back of /deliveries reveals it never went out.
 */

// CORS is open because the submission page calls these endpoints directly from
// the browser to demonstrate the difference between "the transport said 200" and
// "the effect actually happened". Nothing here is sensitive.
const CORS = {
  "access-control-allow-origin": "*",
  "access-control-allow-methods": "GET,POST,OPTIONS",
  "access-control-allow-headers": "content-type",
};

const json = (obj, status = 200) =>
  new Response(JSON.stringify(obj, null, 2), {
    status,
    headers: { "content-type": "application/json", ...CORS },
  });

function outcomeFor(invoice) {
  if (invoice.startsWith("LIE-")) return "queued";
  if (invoice.startsWith("ERR-")) return "failed";
  return "delivered";
}

export default {
  async fetch(request) {
    const url = new URL(request.url);

    if (request.method === "OPTIONS") return new Response(null, { status: 204, headers: CORS });

    if (url.pathname === "/notify" && request.method === "POST") {
      let invoice = "unknown";
      try {
        invoice = (await request.json()).invoice ?? "unknown";
      } catch {
        // A malformed body still gets a cheerful 200 — that is the point.
      }
      // Always 200. This endpoint never tells you the truth on the write path.
      return json({ accepted: true, invoice });
    }

    if (url.pathname === "/deliveries" && request.method === "GET") {
      const invoice = url.searchParams.get("invoice");
      if (!invoice) return json({ error: "invoice query parameter required" }, 400);
      return json({ invoice, state: outcomeFor(invoice), checked_at: new Date().toISOString() });
    }

    return json(
      {
        service: "t3n-verified-actions demo",
        why: "POST /notify always returns 200. GET /deliveries tells the truth.",
        try: [
          "POST /notify  {\"invoice\":\"INV-1042\"}  then GET /deliveries?invoice=INV-1042  -> delivered",
          "POST /notify  {\"invoice\":\"LIE-1043\"}  then GET /deliveries?invoice=LIE-1043  -> queued",
        ],
      },
      404,
    );
  },
};
