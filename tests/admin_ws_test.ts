import assert from "node:assert/strict";
import { startAdminServer } from "../services/admin/server.ts";
import type { Json } from "../kernel/protocol.ts";
import type { AdminBackend } from "../services/admin/contracts.ts";
const token = "workspace_ws_fixture_password";

class RawWebSocket {
  #buffer = new Uint8Array();
  constructor(readonly conn: Deno.TcpConn) {}
  async #need(size: number) {
    while (this.#buffer.length < size) {
      const bytes = new Uint8Array(65536), read = await this.conn.read(bytes);
      if (read === null) throw new Error("unexpected closed WebSocket");
      const joined = new Uint8Array(this.#buffer.length + read);
      joined.set(this.#buffer);
      joined.set(bytes.subarray(0, read), this.#buffer.length);
      this.#buffer = joined;
    }
  }
  async headers() {
    while (true) {
      const end = new TextDecoder().decode(this.#buffer).indexOf("\r\n\r\n");
      if (end >= 0) {
        const text = new TextDecoder().decode(this.#buffer.subarray(0, end));
        this.#buffer = this.#buffer.slice(end + 4);
        return text;
      }
      await this.#need(this.#buffer.length + 1);
    }
  }
  async frame(): Promise<{ opcode: number; text: string; bytes: Uint8Array }> {
    await this.#need(2);
    const opcode = this.#buffer[0] & 15;
    let length = this.#buffer[1] & 127, offset = 2;
    if (length === 126) {
      await this.#need(4);
      length = new DataView(this.#buffer.buffer).getUint16(2);
      offset = 4;
    }
    if (length === 127) {
      await this.#need(10);
      length = Number(new DataView(this.#buffer.buffer).getBigUint64(2));
      offset = 10;
    }
    assert(length < 2_000_000);
    await this.#need(offset + length);
    const bytes = this.#buffer.slice(offset, offset + length);
    this.#buffer = this.#buffer.slice(offset + length);
    return { opcode, text: new TextDecoder().decode(bytes), bytes };
  }
  async sendText(text: string) {
    const payload = new TextEncoder().encode(text), mask = new Uint8Array([1, 2, 3, 4]);
    assert(payload.length < 126);
    const frame = new Uint8Array(payload.length + 6);
    frame.set([0x81, 0x80 | payload.length]);
    frame.set(mask, 2);
    for (let i = 0; i < payload.length; i++) frame[6 + i] = payload[i] ^ mask[i % 4];
    await this.conn.write(frame);
  }
  async closedFrame() {
    for (let i = 0; i < 20; i++) {
      const frame = await this.frame();
      if (frame.opcode === 8) return frame;
    }
    throw new Error("close frame missing");
  }
  close() {
    try {
      this.conn.close();
    } catch { /* closed */ }
  }
}
async function login(url: string) {
  const response = await fetch(url + "/api/login", {
    method: "POST",
    headers: { "content-type": "application/json", origin: url },
    body: JSON.stringify({ token }),
  });
  const value = await response.json();
  return { cookie: response.headers.get("set-cookie")!.split(";")[0], csrf: value.value.csrf };
}
async function connect(url: string, cookie?: string, origin?: string, query = "") {
  const parsed = new URL(url),
    conn = await Deno.connect({ hostname: "127.0.0.1", port: Number(parsed.port) });
  const raw = new RawWebSocket(conn);
  await conn.write(
    new TextEncoder().encode(
      `GET /api/live/ws${query} HTTP/1.1\r\nHost: ${parsed.host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n${
        cookie ? `Cookie: ${cookie}\r\n` : ""
      }${origin ? `Origin: ${origin}\r\n` : ""}\r\n`,
    ),
  );
  return { raw, headers: await raw.headers() };
}
function backendFixture() {
  let statusReads = 0, controls = 0;
  const backend: AdminBackend = {
    status: () => {
      statusReads++;
      return Promise.resolve({ supervisor: {}, runtime: { available: true, throughSeq: 0 } });
    },
    query: ({ resource }): Promise<Json> =>
      Promise.resolve<Json>(
        resource === "workspace"
          ? { throughSeq: 0, items: [] }
          : { items: [], snapshotThroughSeq: 0 },
      ),
    control: () => {
      controls++;
      throw new Error("stream must not control");
    },
  };
  return { backend, reads: () => statusReads, controls: () => controls };
}
Deno.test("admin WebSocket rejects anonymous, absent/foreign origin and query credentials before reads", async () => {
  const f = backendFixture(), server = await startAdminServer(f.backend, { token });
  const connections: RawWebSocket[] = [];
  try {
    const session = await login(server.url);
    for (
      const [cookie, origin, query, status] of [
        [undefined, server.url, "", 401],
        [session.cookie, undefined, "", 403],
        [session.cookie, "https://evil.invalid", "", 403],
        [session.cookie, server.url, "?token=x", 403],
      ] as const
    ) {
      const result = await connect(server.url, cookie, origin, query);
      connections.push(result.raw);
      assert.match(result.headers, new RegExp(`^HTTP/1.1 ${status}`));
      result.raw.close();
    }
    assert.equal(f.reads(), 0);
    assert.equal(f.controls(), 0);
  } finally {
    for (const c of connections) c.close();
    await server.close();
  }
});
Deno.test("admin WebSocket shares snapshots, closes on logout and session TTL without tool execution", async () => {
  let now = 1000;
  const f = backendFixture(), server = await startAdminServer(f.backend, { token, now: () => now });
  const connections: RawWebSocket[] = [];
  try {
    const session = await login(server.url);
    const a = await connect(server.url, session.cookie, server.url);
    connections.push(a.raw);
    assert.match(a.headers, /^HTTP\/1.1 101/);
    const first = JSON.parse((await a.raw.frame()).text);
    assert.equal(first.type, "snapshot");
    assert.equal(first.value.workspace.version, 1);
    const b = await connect(server.url, session.cookie, server.url);
    connections.push(b.raw);
    const second = JSON.parse((await b.raw.frame()).text);
    assert.deepEqual(second, first);
    assert.equal(f.reads(), 1);
    const logout = await fetch(server.url + "/api/logout", {
      method: "POST",
      headers: { cookie: session.cookie, "x-eve-csrf": session.csrf },
    });
    assert.equal(logout.status, 200);
    await logout.body?.cancel();
    assert.equal((await a.raw.closedFrame()).opcode, 8);
    assert.equal((await b.raw.closedFrame()).opcode, 8);
    a.raw.close();
    b.raw.close();
    const session2 = await login(server.url);
    const c = await connect(server.url, session2.cookie, server.url);
    connections.push(c.raw);
    assert.equal(JSON.parse((await c.raw.frame()).text).type, "snapshot");
    await c.raw.sendText('{"action":"pause"}');
    const readOnly = await c.raw.closedFrame();
    assert.equal(new DataView(readOnly.bytes.buffer).getUint16(0), 1008);
    c.raw.close();
    const d = await connect(server.url, session2.cookie, server.url);
    connections.push(d.raw);
    assert.equal(JSON.parse((await d.raw.frame()).text).type, "snapshot");
    now += 8 * 60 * 60 * 1000;
    const closed = await d.raw.closedFrame();
    assert.equal(closed.opcode, 8);
    assert.equal(new DataView(closed.bytes.buffer).getUint16(0), 4001);
    assert.equal(f.controls(), 0);
  } finally {
    for (const c of connections) c.close();
    await server.close();
  }
});
