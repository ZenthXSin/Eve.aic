import assert from "node:assert/strict";
import { Buffer } from "node:buffer";
import { createPrivateKey, sign } from "node:crypto";
import { request } from "node:http";
import type { QqClient } from "../adapters/qq/client.ts";
import { configuredIngress } from "../adapters/qq/ingress-config.ts";
import { QqInboundJournal } from "../adapters/qq/journal.ts";
import { type QqIngressOptions, QqReceiver } from "../adapters/qq/receiver.ts";

const event = {
  op: 0,
  t: "C2C_MESSAGE_CREATE",
  id: "callback",
  d: {
    id: "test-message",
    author: { user_openid: "test-user" },
    content: "合成输入",
  },
};

Deno.test("QQ ingress keeps the original subscription and validates listener bounds before reading secrets", async () => {
  const base = { appIdFile: "/unused-fixture", appSecretFile: "/unused-fixture" };
  assert.deepEqual(await configuredIngress(base), {});
  assert.deepEqual(await configuredIngress({ ...base, ingress: {} }), { intents: 1 << 25 });
  for (const intents of [0, 1, 0xffff_ffff]) {
    assert.deepEqual(
      await configuredIngress({ ...base, ingress: { mode: "websocket", intents } }),
      { intents },
    );
  }
  for (
    const ingress of [
      null,
      [],
      { mode: "other" },
      { mode: "websocket", port: 3456 },
      ...[null, -1, 1.5, 0x1_0000_0000].map((intents) => ({ intents })),
      ...[{}, { port: 0 }, { port: 65536 }, { port: 1023 }, { port: "3456" }, { port: 3456.5 }]
        .map((fields) => ({ mode: "webhook", ...fields })),
      ...[
        { host: "0.0.0.0" },
        { hostname: "::" },
        { intents: 1 },
        { enabled: true },
        { path: "relative" },
        { path: "/callback?query" },
        { path: "/x/../callback" },
        { path: "//other" },
        { path: "/x\\y" },
        { maxBodyBytes: null },
        { maxBodyBytes: 0 },
        { maxBodyBytes: 8 * 1024 * 1024 + 1 },
        { maxClockSkewSeconds: -1 },
        { maxClockSkewSeconds: 3601 },
        { maxInFlight: 0 },
        { maxInFlight: 33 },
        { bodyTimeoutMs: 0 },
        { bodyTimeoutMs: 30_001 },
        { bodyTimeoutMs: 0.5 },
        ...[true, null, -1, 1.5, "300", Math.floor(Date.now() / 1000) + 600].map((
          unsignedRegistrationUntilUnixSeconds,
        ) => ({ unsignedRegistrationUntilUnixSeconds })),
      ]
        .map((fields) => ({ mode: "webhook", port: 3456, ...fields })),
    ]
  ) {
    await assert.rejects(
      configuredIngress({ ...base, ingress: ingress as never }),
      TypeError,
      JSON.stringify(ingress),
    );
  }
});

async function withLocalIngress(
  test: (
    directory: string,
    webhook: NonNullable<QqIngressOptions["webhook"]>,
    endpoint: URL,
    send: () => Promise<Response>,
  ) => Promise<void>,
  unsignedRegistrationUntilUnixSeconds?: number,
) {
  const directory = await Deno.makeTempDir({ prefix: "qq-ingress-test-" });
  try {
    const appIdFile = `${directory}/app-id`, appSecretFile = `${directory}/secret`;
    await Deno.writeTextFile(appIdFile, "123456\n", { mode: 0o600 });
    await Deno.writeTextFile(appSecretFile, "a\n", { mode: 0o600 });
    const reservation = Deno.listen({ hostname: "127.0.0.1", port: 0 });
    const port = reservation.addr.port;
    const path = "/local/callback";
    let ingress: QqIngressOptions;
    try {
      ingress = await configuredIngress({
        appIdFile,
        appSecretFile,
        ingress: {
          mode: "webhook",
          port,
          path,
          maxInFlight: 1,
          bodyTimeoutMs: 500,
          unsignedRegistrationUntilUnixSeconds,
        },
      });
    } finally {
      reservation.close();
    }
    assert.ok(ingress.webhook);
    const endpoint = new URL(`http://127.0.0.1:${port}${path}`);
    const body = JSON.stringify(event);
    const stamp = String(Math.floor(Date.now() / 1000));
    const key = createPrivateKey({
      format: "der",
      type: "pkcs8",
      key: Buffer.from("302e020100300506032b657004220420" + "61".repeat(32), "hex"),
    });
    const headers = {
      "X-Bot-Appid": "123456",
      "X-Signature-Timestamp": stamp,
      "X-Signature-Ed25519": sign(null, Buffer.from(stamp + body), key).toString("hex"),
    };
    await test(
      directory,
      ingress.webhook,
      endpoint,
      () => fetch(endpoint, { method: "POST", headers, body, signal: AbortSignal.timeout(5_000) }),
    );
  } finally {
    await Deno.remove(directory, { recursive: true });
  }
}

Deno.test("configured loopback registration proves possession without delivering an unsigned event", () =>
  withLocalIngress(async (_directory, webhook, endpoint, send) => {
    const ready = Promise.withResolvers<void>(), stop = new AbortController();
    let accepted = 0;
    const run = webhook(stop.signal, () => {
      accepted++;
      return Promise.resolve();
    }, ready.resolve);
    try {
      await Promise.race([ready.promise, run]);
      const event_ts = String(Math.floor(Date.now() / 1000)), plain_token = "Arq0D5A61EgUu4OxUvOp";
      const post = (body: unknown) =>
        fetch(endpoint, {
          method: "POST",
          headers: { "X-Bot-Appid": "123456" },
          body: JSON.stringify(body),
          signal: AbortSignal.timeout(5_000),
        });
      const response = await post({ op: 13, d: { event_ts, plain_token } });
      assert.equal(response.status, 200);
      const key = createPrivateKey({
        format: "der",
        type: "pkcs8",
        key: Buffer.from("302e020100300506032b657004220420" + "61".repeat(32), "hex"),
      });
      assert.deepEqual(await response.json(), {
        plain_token,
        signature: sign(null, Buffer.from(event_ts + plain_token), key).toString("hex"),
      });
      const unsigned = await post(event);
      assert.equal(unsigned.status, 401);
      assert.equal(await unsigned.text(), "");
      assert.equal(accepted, 0);
      await expectAck(await send(), 0);
      assert.equal(accepted, 1);
    } finally {
      stop.abort();
      await run;
    }
  }, Math.floor(Date.now() / 1000) + 60));

async function expectAck(response: Response, code: 0 | 1) {
  assert.equal(response.status, 200);
  assert.deepEqual(await response.json(), { op: 12, d: code });
}

Deno.test("signed loopback delivery waits for the receiver and retries without duplicating a durable chat event", () =>
  withLocalIngress(async (directory, webhook, endpoint, send) => {
    let offered = 0, observations = 0;
    const entered = Promise.withResolvers<void>(), write = Promise.withResolvers<void>();
    const ready = Promise.withResolvers<void>(), stop = new AbortController();
    const receiver = new QqReceiver({} as QqClient, directory, {
      persisted: () => {
        offered++;
      },
    }, {
      webhook,
      observed: (_event, record) => {
        assert.ok(record);
        if (++observations === 1) return Promise.reject(new Error("fixture persistence failed"));
        entered.resolve();
        return write.promise;
      },
    });
    const run = receiver.run(stop.signal, ({ status }) => {
      if (status === "webhook_listening") ready.resolve();
    });
    try {
      await Promise.race([ready.promise, run]);
      using otherLoopback = Deno.listen({ hostname: "127.0.0.2", port: Number(endpoint.port) });
      assert.equal(otherLoopback.addr.hostname, "127.0.0.2");
      const wrongPath = await fetch(new URL("/qq/webhook", endpoint));
      assert.equal(wrongPath.status, 404);
      await wrongPath.body?.cancel();
      const unsigned = await fetch(endpoint, { method: "POST", body: JSON.stringify(event) });
      assert.equal(unsigned.status, 401);
      await unsigned.body?.cancel();
      assert.equal(offered, 0);

      await expectAck(await send(), 1);
      const waiting = send();
      assert.equal(
        await Promise.race([entered.promise.then(() => "writing"), waiting.then(() => "ACK")]),
        "writing",
      );
      const restored = new QqInboundJournal(`${directory}/inbound`);
      await restored.initialize();
      assert.deepEqual(restored.records().map(({ raw }) => raw), [event]);
      const busy = await send();
      assert.equal(busy.status, 503);
      await busy.body?.cancel();
      write.resolve();
      await expectAck(await waiting, 0);
      await expectAck(await send(), 0);
      assert.equal(offered, 1);
      assert.equal(observations, 3);
      assert.deepEqual(receiver.messages().map(({ raw }) => raw), [event]);
    } finally {
      write.resolve();
      stop.abort();
      await run;
    }
  }));

Deno.test("an actual journal write failure returns the retry ACK and no chat record", () =>
  withLocalIngress(async (directory, webhook, _endpoint, send) => {
    const ready = Promise.withResolvers<void>(), stop = new AbortController();
    let offered = 0;
    const receiver = new QqReceiver({} as QqClient, directory, {
      persisted: () => {
        offered++;
      },
    }, { webhook });
    const run = receiver.run(stop.signal, ({ status }) => {
      if (status === "webhook_listening") ready.resolve();
    });
    try {
      await Promise.race([ready.promise, run]);
      const journal = `${directory}/inbound`;
      await Deno.rename(journal, journal + ".original");
      await Deno.writeTextFile(journal, "not a directory", { createNew: true });
      await expectAck(await send(), 1);
      assert.equal(offered, 0);
      assert.deepEqual(await Array.fromAsync(Deno.readDir(journal + ".original")), []);
    } finally {
      stop.abort();
      await run;
    }
  }));

Deno.test("an incomplete loopback upload receives 408 and does not block the next signed delivery", () =>
  withLocalIngress(async (_directory, webhook, endpoint, send) => {
    const ready = Promise.withResolvers<void>(), stop = new AbortController();
    let accepted = 0;
    const run = webhook(stop.signal, () => {
      accepted++;
      return Promise.resolve();
    }, ready.resolve);
    try {
      await Promise.race([ready.promise, run]);
      const status = await new Promise<number | undefined>((resolve, reject) => {
        const upload = request(endpoint, {
          method: "POST",
          signal: AbortSignal.timeout(5_000),
          headers: {
            "X-Bot-Appid": "123456",
            "X-Signature-Timestamp": String(Math.floor(Date.now() / 1000)),
            "X-Signature-Ed25519": "00".repeat(64),
          },
        }, (response) => {
          upload.end();
          response.resume();
          response.once("end", () => resolve(response.statusCode));
          response.once("error", reject);
        });
        upload.once("error", reject);
        upload.write("{");
      });
      assert.equal(status, 408);
      assert.equal(accepted, 0);
      await expectAck(await send(), 0);
      assert.equal(accepted, 1);
    } finally {
      stop.abort();
      await run;
    }
  }));

Deno.test("loopback shutdown keeps ownership until an accepted write settles", () =>
  withLocalIngress(async (_directory, webhook, _endpoint, send) => {
    const entered = Promise.withResolvers<void>(), write = Promise.withResolvers<void>();
    const ready = Promise.withResolvers<void>(), stop = new AbortController();
    let stopped = false;
    const run = webhook(stop.signal, () => {
      entered.resolve();
      return write.promise;
    }, ready.resolve)
      .then(() => {
        stopped = true;
      });
    try {
      await Promise.race([ready.promise, run]);
      const reply = send().then(async (response) => await response.json(), () => null);
      assert.equal(
        await Promise.race([entered.promise.then(() => "writing"), reply.then(() => "done")]),
        "writing",
      );
      stop.abort();
      const result = await reply;
      assert.equal(stopped, false);
      write.resolve();
      if (result !== null) assert.deepEqual(result, { op: 12, d: 1 });
    } finally {
      write.resolve();
      stop.abort();
      await run;
    }
  }));
