// Silent signaling relay: Cloudflare Worker + one Durable Object per room.
//
// Wire protocol is identical to crates/silent-relay (the Rust relay), and
// crates/silent-relay/tests/relay.rs runs against this Worker when
// SILENT_TEST_RELAY points at `wrangler dev`.
//
// The relay sees room numbers, small peer ids and opaque sealed blobs. It
// stores nothing: room membership lives only on the open WebSockets, and the
// Worker has logging disabled.

import { DurableObject } from "cloudflare:workers";

const MAX_PEERS = 4;
const MAX_RELAY_DATA = 64 * 1024;
const NAMEPLATE_MAX = 9999;
const RATE_BURST = 200;
const RATE_PER_SEC = 50;
// Every join attempt is one online guess at the code. Same budget as
// silent-core's JOIN_BURST / JOIN_REFILL_SECS.
const JOIN_BURST = 20;
const JOIN_REFILL_SECS = 6;
const TURN_TTL_SECONDS = 4 * 60 * 60;
const TURN_CAP_GB_DEFAULT = 900;
const TURN_WINDOW_DAYS = 31;
const USAGE_CACHE_MS = 5 * 60 * 1000;

type ErrorCode =
  | "room_full"
  | "room_taken"
  | "no_room"
  | "bad_request"
  | "too_large"
  | "rate_limited"
  | "too_many_joins"
  | "evicted";

interface IceServer {
  urls: string[];
  username?: string;
  credential?: string;
}

interface IceConfig {
  ice_servers: IceServer[];
}

interface Env {
  ROOMS: DurableObjectNamespace<Room>;
  TURN_KEY_ID?: string;
  TURN_KEY_API_TOKEN?: string;
  CF_ACCOUNT_ID?: string;
  CF_ANALYTICS_TOKEN?: string;
  TURN_CAP_GB?: string;
}

interface JoinBudget {
  tokens: number;
  at: number;
}

// Kept on every live socket so the join budget survives hibernation without
// writing anything to storage.
interface Attachment {
  id: number;
  joins?: JoinBudget;
  // Peers this one asked to remove (see the evict message).
  evicts?: number[];
}

type Peer = { ws: WebSocket } & Attachment;

const STUN: IceServer = { urls: ["stun:stun.cloudflare.com:3478", "stun:stun.l.google.com:19302"] };

function rejectSocket(code: ErrorCode): Response {
  const pair = new WebSocketPair();
  const [client, server] = Object.values(pair);
  server.accept();
  server.send(JSON.stringify({ t: "error", code }));
  server.close(1008, code);
  return new Response(null, { status: 101, webSocket: client });
}

let usageCheck: { at: number; underCap: boolean } | undefined;

// Whether the account's TURN egress over the last TURN_WINDOW_DAYS is under
// TURN_CAP_GB. A rolling window keeps any billing month under the cap,
// whatever day the billing cycle starts. Fails closed: without analytics
// credentials, or when the query fails, no TURN credentials are handed out.
async function turnUnderCap(env: Env): Promise<boolean> {
  if (!env.CF_ACCOUNT_ID || !env.CF_ANALYTICS_TOKEN) return false;
  const now = Date.now();
  if (usageCheck && now - usageCheck.at < USAGE_CACHE_MS) return usageCheck.underCap;

  const day = (ms: number) => new Date(ms).toISOString().slice(0, 10);
  const from = day(now - (TURN_WINDOW_DAYS - 1) * 86_400_000);
  const query = `{ viewer { accounts(filter: { accountTag: ${JSON.stringify(env.CF_ACCOUNT_ID)} }) {
    callsTurnUsageAdaptiveGroups(filter: { date_geq: "${from}", date_leq: "${day(now)}" }, limit: 1) { sum { egressBytes } }
  } } }`;
  const cap = Number(env.TURN_CAP_GB) || TURN_CAP_GB_DEFAULT;
  let underCap = false;
  try {
    const res = await fetch("https://api.cloudflare.com/client/v4/graphql", {
      method: "POST",
      headers: { Authorization: `Bearer ${env.CF_ANALYTICS_TOKEN}`, "Content-Type": "application/json" },
      body: JSON.stringify({ query }),
    });
    if (res.ok) {
      const body = (await res.json()) as {
        errors?: unknown[] | null;
        data?: { viewer?: { accounts?: Array<{ callsTurnUsageAdaptiveGroups?: Array<{ sum?: { egressBytes?: number } }> }> } };
      };
      const groups = body.data?.viewer?.accounts?.[0]?.callsTurnUsageAdaptiveGroups;
      if (!body.errors?.length && Array.isArray(groups)) {
        const bytes = groups.reduce((n, g) => n + (g.sum?.egressBytes ?? 0), 0);
        underCap = bytes < cap * 1e9;
      }
    }
  } catch {
    underCap = false;
  }
  usageCheck = { at: now, underCap };
  return underCap;
}

async function iceConfig(env: Env): Promise<IceConfig> {
  if (!env.TURN_KEY_ID || !env.TURN_KEY_API_TOKEN || !(await turnUnderCap(env))) {
    return { ice_servers: [STUN] };
  }
  try {
    const res = await fetch(
      `https://rtc.live.cloudflare.com/v1/turn/keys/${env.TURN_KEY_ID}/credentials/generate-ice-servers`,
      {
        method: "POST",
        headers: { Authorization: `Bearer ${env.TURN_KEY_API_TOKEN}`, "Content-Type": "application/json" },
        body: JSON.stringify({ ttl: TURN_TTL_SECONDS }),
      },
    );
    if (!res.ok) return { ice_servers: [STUN] };
    const body = (await res.json()) as { iceServers?: unknown };
    const raw = Array.isArray(body.iceServers) ? body.iceServers : [body.iceServers];
    const servers: IceServer[] = [];
    for (const s of raw as Array<{ urls?: string | string[]; username?: string; credential?: string }>) {
      if (!s?.urls) continue;
      servers.push({
        urls: Array.isArray(s.urls) ? s.urls : [s.urls],
        username: s.username,
        credential: s.credential,
      });
    }
    return { ice_servers: servers.length ? servers : [STUN] };
  } catch {
    return { ice_servers: [STUN] };
  }
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname === "/healthz") {
      return new Response("ok");
    }
    const match = url.pathname.match(/^\/v1\/rooms\/([^/]+)$/);
    if (!match) {
      return new Response("not found", { status: 404 });
    }
    if (request.headers.get("Upgrade")?.toLowerCase() !== "websocket") {
      return new Response("expected websocket", { status: 426 });
    }
    const nameplate = /^\d{1,4}$/.test(match[1]) ? Number(match[1]) : NaN;
    const intent = url.searchParams.get("intent");
    if (!(nameplate >= 1 && nameplate <= NAMEPLATE_MAX) || (intent !== "create" && intent !== "join")) {
      return rejectSocket("bad_request");
    }

    const ice = await iceConfig(env);
    const headers = new Headers(request.headers);
    headers.set("x-silent-intent", intent);
    headers.set("x-silent-ice", JSON.stringify(ice));
    const stub = env.ROOMS.get(env.ROOMS.idFromName(String(nameplate)));
    return stub.fetch(new Request(request.url, { headers }));
  },
} satisfies ExportedHandler<Env>;

export class Room extends DurableObject<Env> {
  private nextId = 0;
  private tokens = new Map<number, { tokens: number; at: number }>();

  private peers(): Peer[] {
    const out: Peer[] = [];
    for (const ws of this.ctx.getWebSockets()) {
      const a = ws.deserializeAttachment() as Attachment | null;
      if (a && ws.readyState === WebSocket.OPEN) out.push({ ws, ...a });
    }
    return out;
  }

  private save(peer: Peer): void {
    const { ws, ...attachment } = peer;
    ws.serializeAttachment(attachment satisfies Attachment);
  }

  // Removes `target` once everyone who joined before them (lower ids) asked
  // to. A newcomer can never remove the people who were already there.
  private settleEviction(target: number): void {
    const peers = this.peers();
    const victim = peers.find((p) => p.id === target);
    const earlier = peers.filter((p) => p.id < target);
    if (!victim || earlier.length === 0 || !earlier.every((p) => p.evicts?.includes(target))) return;
    try {
      victim.ws.send(JSON.stringify({ t: "error", code: "evicted" }));
      victim.ws.close(1008, "evicted");
    } catch {
      // already closed
    }
    this.depart(victim.ws);
  }

  // Takes one join attempt from the room's budget, or returns null when it
  // is used up.
  private takeJoin(peers: Array<{ joins?: JoinBudget }>): JoinBudget | null {
    const now = Date.now();
    const known = peers.map((p) => p.joins).filter((j): j is JoinBudget => !!j);
    const last = known.sort((a, b) => b.at - a.at)[0] ?? { tokens: JOIN_BURST, at: now };
    const tokens = Math.min(JOIN_BURST, last.tokens + (now - last.at) / 1000 / JOIN_REFILL_SECS);
    if (tokens < 1) return null;
    return { tokens: tokens - 1, at: now };
  }

  async fetch(request: Request): Promise<Response> {
    const intent = request.headers.get("x-silent-intent");
    const ice = JSON.parse(request.headers.get("x-silent-ice") ?? '{"ice_servers":[]}') as IceConfig;
    const peers = this.peers();

    if (intent === "create" && peers.length > 0) return rejectSocket("room_taken");
    if (intent === "join" && peers.length === 0) return rejectSocket("no_room");
    let joins = peers.find((p) => p.joins)?.joins;
    if (intent === "join") {
      const budget = this.takeJoin(peers);
      if (!budget) return rejectSocket("too_many_joins");
      joins = budget;
      for (const p of peers) this.save({ ...p, joins });
    }
    if (peers.length >= MAX_PEERS) return rejectSocket("room_full");

    // Ids only need to be unique among live peers. After hibernation the
    // in-memory counter is gone, so never go below the highest live id.
    this.nextId = Math.max(this.nextId, ...peers.map((p) => p.id)) + 1;
    const me = this.nextId;

    const pair = new WebSocketPair();
    const [client, server] = Object.values(pair);
    this.ctx.acceptWebSocket(server);
    server.serializeAttachment({ id: me, joins } satisfies Attachment);
    server.send(JSON.stringify({ t: "welcome", you: me, peers: peers.map((p) => p.id), ice }));
    for (const p of peers) p.ws.send(JSON.stringify({ t: "joined", peer: me }));

    return new Response(null, { status: 101, webSocket: client });
  }

  private fail(ws: WebSocket, code: ErrorCode): void {
    try {
      ws.send(JSON.stringify({ t: "error", code }));
      ws.close(1008, code);
    } catch {
      // already closed
    }
    this.depart(ws);
  }

  private allow(id: number): boolean {
    const now = Date.now();
    const bucket = this.tokens.get(id) ?? { tokens: RATE_BURST, at: now };
    bucket.tokens = Math.min(RATE_BURST, bucket.tokens + ((now - bucket.at) / 1000) * RATE_PER_SEC);
    bucket.at = now;
    this.tokens.set(id, bucket);
    if (bucket.tokens < 1) return false;
    bucket.tokens -= 1;
    return true;
  }

  async webSocketMessage(ws: WebSocket, message: string | ArrayBuffer): Promise<void> {
    const me = (ws.deserializeAttachment() as Attachment | null)?.id;
    if (me === undefined) return;
    if (!this.allow(me)) return this.fail(ws, "rate_limited");
    if (typeof message !== "string") return this.fail(ws, "bad_request");

    let msg: { t?: unknown; to?: unknown; data?: unknown; peer?: unknown };
    try {
      msg = JSON.parse(message);
    } catch {
      return this.fail(ws, "bad_request");
    }
    if (typeof msg !== "object" || msg === null) return this.fail(ws, "bad_request");
    if (msg.t === "evict" && Number.isInteger(msg.peer) && msg.peer !== me) {
      const target = msg.peer as number;
      const peers = this.peers();
      const voter = peers.find((p) => p.id === me);
      // Only votes from people who were there first count, and only against
      // someone still here (ids can come back after the room hibernates).
      if (voter && me < target && peers.some((p) => p.id === target) && !voter.evicts?.includes(target)) {
        this.save({ ...voter, evicts: [...(voter.evicts ?? []), target] });
        this.settleEviction(target);
      }
      return;
    }
    if (msg.t === "evict") return this.fail(ws, "bad_request");
    if (msg.t !== "relay" || !Number.isInteger(msg.to) || typeof msg.data !== "string" || msg.to === me) {
      return this.fail(ws, "bad_request");
    }
    if (msg.data.length > MAX_RELAY_DATA) return this.fail(ws, "too_large");

    const target = this.peers().find((p) => p.id === msg.to);
    target?.ws.send(JSON.stringify({ t: "relay", from: me, data: msg.data }));
  }

  async webSocketClose(ws: WebSocket, code: number): Promise<void> {
    try {
      ws.close(code === 1005 ? 1000 : code);
    } catch {
      // already closed
    }
    this.depart(ws);
  }

  async webSocketError(ws: WebSocket): Promise<void> {
    this.depart(ws);
  }

  private depart(ws: WebSocket): void {
    const me = (ws.deserializeAttachment() as Attachment | null)?.id;
    if (me === undefined) return;
    ws.serializeAttachment(null);
    this.tokens.delete(me);
    const targets = new Set<number>();
    for (const p of this.peers()) {
      if (p.id === me) continue;
      p.ws.send(JSON.stringify({ t: "left", peer: me }));
      if (p.evicts?.includes(me)) this.save({ ...p, evicts: p.evicts.filter((id) => id !== me) });
      for (const id of p.evicts ?? []) if (id !== me) targets.add(id);
    }
    // Someone who had not voted may have been all that stood in the way.
    for (const target of targets) this.settleEviction(target);
  }
}
