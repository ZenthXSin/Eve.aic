import assert from "node:assert/strict";
import { Buffer } from "node:buffer";
import { createPrivateKey, sign } from "node:crypto";
import {
  createQqWebhookHandler,
  type QqWebhookOptions,
  type QqWebhookReceipt,
} from "../adapters/qq/webhook.ts";

const appId = "11111111";
const stamp = "1725442341";
const clock = Number(stamp) * 1000;
const secret = "naOC0ocQE3shWLAfffVLB1rhYPG7";
const dispatchBody = `{\n  "op": 0,\n  "d": {},\n  "t": "GATEWAY_EVENT_NAME"\n}`;
const dispatchHex =
  "865ad13a61752ca65e26bde6676459cd36cf1be609375b37bd62af366e1dc25a8dc789ba7f14e017ada3d554c671a911bfdf075ba54835b23391d509579ed002";
const challengeBody =
  '{"d":{"plain_token":"Arq0D5A61EgUu4OxUvOp","event_ts":"1725442341"},"op":13}';
const syntheticChallengeEnvelopeHex =
  "b68a482390a9a510db6cdb6fdcb9442ab811cc5df786808a318cbc36bb1b6c677a05531849cfe9524be0f7dc5ed2e2ff8db2d96e37bb1ad58d79db19d6cd2509";
const challengeReplyHex =
  "87befc99c42c651b3aac0278e71ada338433ae26fcb24307bdc5ad38c1adc2d01bcfcadc0842edac85e85205028a1132afe09280305f13aa6909ffc2d652c706";
const key = createPrivateKey({
  format: "jwk",
  key: {
    kty: "OKP",
    crv: "Ed25519",
    d: "bmFPQzBvY1FFM3NoV0xBZmZmVkxCMXJoWVBHN25hT0M",
    x: "18Ni_niu-B_yMoe0k2KLXbAqPE_jCyFeTRlgm112Zzo",
  },
});
const encode = (text: string) => new TextEncoder().encode(text);
const url = "http://localhost/qq/webhook";
const ok = { op: 12, d: 0 };
const retry = { op: 12, d: 1 };

function handler(options: Partial<QqWebhookOptions> = {}) {
  return createQqWebhookHandler({
    appId,
    secret,
    now: () => clock,
    onDispatch: () => Promise.resolve("persisted"),
    ...options,
    config: { enabled: true, ...options.config },
  });
}

function headers(hex = dispatchHex, timestamp = stamp) {
  return new Headers({
    "X-Bot-Appid": appId,
    "X-Signature-Timestamp": timestamp,
    "X-Signature-Ed25519": hex,
  });
}

function post(body: BodyInit | null = dispatchBody, auth = headers(), signal?: AbortSignal) {
  return new Request(url, { method: "POST", body, headers: auth, signal });
}

function signed(body: Uint8Array<ArrayBuffer> | string, timestamp = stamp) {
  const bytes = typeof body === "string" ? encode(body) : body;
  const hex = sign(null, Buffer.concat([encode(timestamp), bytes]), key).toString("hex");
  return post(bytes, headers(hex, timestamp));
}

async function jsonReply(response: Response, expected: unknown) {
  assert.equal(response.status, 200);
  assert.match(response.headers.get("content-type")!, /application\/json/);
  assert.equal(response.headers.get("cache-control"), "no-store");
  assert.deepEqual(await response.json(), expected);
}

async function denied(response: Response, status: number) {
  assert.equal(response.status, status);
  assert.equal(await response.text(), "");
  assert.equal(response.headers.get("cache-control"), "no-store");
}

Deno.test("Tencent bot-docs@2f4c013 sign.md dispatch vector verifies exact bytes without a nonce header", async () => {
  let calls = 0;
  const handle = handler({
    onDispatch(event, delivery) {
      calls++;
      assert.deepEqual(event, { op: 0, t: "GATEWAY_EVENT_NAME", d: {} });
      assert.deepEqual(delivery.rawBody, encode(dispatchBody));
      assert.equal(delivery.timestamp, stamp);
      return Promise.resolve("persisted");
    },
  });
  await jsonReply(await handle(post()), ok);
  await jsonReply(await handle(post(dispatchBody, headers(dispatchHex.toUpperCase()))), ok);
  await jsonReply(await handler({ secret: secret + "naOC-ignored-after-byte-32" })(post()), ok);
  await denied(await handle(post('{ "op": 0,"d": {}, "t": "GATEWAY_EVENT_NAME"}')), 401);
  await denied(await handle(post(JSON.stringify(JSON.parse(dispatchBody)))), 401);
  await denied(await handle(post(dispatchBody + "\n")), 401);
  await denied(
    await handle(post(dispatchBody, headers(dispatchHex, String(Number(stamp) + 1)))),
    401,
  );
  assert.equal(calls, 2);
});

Deno.test("Tencent bot-docs@0aa50a8 event-emit.md op 13 signs event_ts + plain_token, not the header timestamp", async () => {
  let calls = 0;
  const handle = handler({
    secret: "DG5g3B4j9X2KOErG",
    onDispatch() {
      calls++;
      return Promise.resolve("persisted");
    },
  });
  const auth = headers(syntheticChallengeEnvelopeHex, "1725442359");
  const documentedHeaders = new Headers({ "User-Agent": "QQBot-Callback", "X-Bot-Appid": appId });
  await denied(await handle(post(challengeBody, documentedHeaders)), 401);
  await jsonReply(await handle(post(challengeBody, auth)), {
    plain_token: "Arq0D5A61EgUu4OxUvOp",
    signature: challengeReplyHex,
  });
  await denied(await handle(post(challengeBody.replace("Arq0", "other"), auth)), 401);
  auth.delete("x-signature-ed25519");
  auth.set("X-Signature-Nonce", "not-authentication");
  await denied(await handle(post(challengeBody, auth)), 401);
  assert.equal(calls, 0);
});

const registrationHeaders = () =>
  new Headers({ "User-Agent": "QQBot-Callback", "X-Bot-Appid": appId });
const registrationUntil = Number(stamp) + 60;

function registration(options: Partial<QqWebhookOptions> = {}) {
  return handler({
    secret: "DG5g3B4j9X2KOErG",
    onDispatch: () => assert.fail("registration reached dispatch"),
    ...options,
    config: { unsignedRegistrationUntilUnixSeconds: registrationUntil, ...options.config },
  });
}

Deno.test("opt-in unsigned Tencent challenge matches event-emit.md's proof exactly once and never dispatches", async () => {
  let dispatched = 0;
  const handle = registration({
    onDispatch() {
      dispatched++;
      return Promise.resolve("persisted");
    },
  });
  const auth = registrationHeaders();
  const expected = { plain_token: "Arq0D5A61EgUu4OxUvOp", signature: challengeReplyHex };
  const reversed =
    `{ "op": 13, "d": { "event_ts": "${stamp}", "plain_token": "${expected.plain_token}" } }`;
  await jsonReply(await registration()(post(reversed, auth)), expected);
  await jsonReply(await handle(post(challengeBody, auth)), expected);
  await denied(await handle(post(challengeBody, auth)), 401);
  await denied(await handle(post(dispatchBody, headers(challengeReplyHex))), 401);
  assert.equal(dispatched, 0);
});

Deno.test("unsigned registration rejects ambiguous envelopes and tokens outside the narrow proof domain", async () => {
  const auth = registrationHeaders();
  for (
    const body of [
      dispatchBody,
      '{"op":1,"d":42}',
      challengeBody.replace('"op":13', '"op":0,"op":13'),
      challengeBody.replace('"op":13', '"op":13,"t":"READY"'),
      challengeBody.replace('"op":13', '"op":13.0'),
      challengeBody.replace('"event_ts":', '"extra":0,"event_ts":'),
      challengeBody.replace('"plain_token":', '"plain_token":"duplicate","plain_token":'),
      ...["0" + stamp, String(Number(stamp) - 31), String(Number(stamp) + 31)].map((ts) =>
        challengeBody.replace(stamp, ts)
      ),
      ...[
        "",
        "a".repeat(15),
        "a".repeat(65),
        "a".repeat(16) + "_",
        "9".repeat(64),
        "e" + "0".repeat(15),
        "12345678901234E7",
        "nonASCIIé0123456789",
        '{"op":0,"d":{},"t":"EVENT"}',
        "A".repeat(16) + "\n",
        "A".repeat(16) + "\u0000",
      ].map((token) => JSON.stringify({ op: 13, d: { event_ts: stamp, plain_token: token } })),
      challengeBody.replace("Arq0", "\\u0041rq0"),
      new Uint8Array([255]),
    ]
  ) await denied(await registration()(post(body, auth)), 401);
  await denied(await registration()(post(" ".repeat(513), auth)), 413);
  for (const offset of [-30, 30]) {
    await jsonReply(
      await registration({ now: () => clock + 1000 * offset })(post(challengeBody, auth)),
      {
        plain_token: "Arq0D5A61EgUu4OxUvOp",
        signature: challengeReplyHex,
      },
    );
  }
});

Deno.test("registration never downgrades signed headers or trusts a forwarded app or another route", async () => {
  const handle = registration();
  for (
    const extra of [
      { "X-Signature-Ed25519": "" },
      { "X-Signature-Timestamp": stamp },
      { "X-Signature-Nonce": "nonce" },
      { "X-Signature-Ed25519": "00".repeat(64), "X-Signature-Timestamp": stamp },
      { "X-Bot-Appid": "wrong", "X-Forwarded-Appid": appId },
      { "X-Bot-Appid": `${appId}, ${appId}` },
    ]
  ) {
    const auth = registrationHeaders();
    for (const [name, value] of Object.entries(extra)) auth.set(name, value!);
    await denied(await handle(post(challengeBody, auth)), 401);
  }
  await denied(await handle(post(challengeBody, new Headers())), 401);
  for (const [suffix, status] of [["/different", 404], ["?appid=" + appId, 401]] as const) {
    await denied(
      await handle(
        new Request(url + suffix, {
          method: "POST",
          body: challengeBody,
          headers: registrationHeaders(),
        }),
      ),
      status,
    );
  }
  await jsonReply(await handle(post(challengeBody, registrationHeaders())), {
    plain_token: "Arq0D5A61EgUu4OxUvOp",
    signature: challengeReplyHex,
  });
});

Deno.test("registration throttle counts invalid attempts; signed events still require and accept their own signatures", async () => {
  let now = clock, dispatched = 0;
  const handle = registration({
    secret,
    now: () => now,
    onDispatch() {
      dispatched++;
      return Promise.resolve("persisted");
    },
  });
  const auth = registrationHeaders();
  for (let attempt = 0; attempt < 3; attempt++) {
    await denied(await handle(post(dispatchBody, auth)), 401);
    await denied(await handle(post(challengeBody, auth)), 429);
    now += 1000;
  }
  await denied(await handle(post(challengeBody, auth)), 429);
  assert.equal(dispatched, 0);
  await jsonReply(await handle(post()), ok);
  assert.equal(dispatched, 1);
});

Deno.test("a registration cannot finish after expiry, reopen on clock rollback, or renew by restarting with an expired deadline", async () => {
  let now = clock;
  const settings = { unsignedRegistrationUntilUnixSeconds: Number(stamp) + 2 };
  const handle = registration({ config: settings, now: () => now });
  const auth = registrationHeaders();
  let writer!: ReadableStreamDefaultController<Uint8Array>;
  const pending = handle(post(
    new ReadableStream({
      start(controller) {
        writer = controller;
      },
    }),
    auth,
  ));
  now += 1000;
  await denied(await handle(post(challengeBody, auth)), 429);
  now += 1000;
  writer.enqueue(encode(challengeBody));
  writer.close();
  await denied(await pending, 401);
  await denied(
    await registration({ config: settings, now: () => now })(post(challengeBody, auth)),
    401,
  );
  now = clock;
  await denied(await handle(post(challengeBody, auth)), 401);
});

Deno.test("POST and the explicit route are required; bad or duplicate auth headers never dispatch", async () => {
  let calls = 0;
  const handle = handler({
    onDispatch() {
      calls++;
      return Promise.resolve("persisted");
    },
  });
  await denied(
    await createQqWebhookHandler({ appId, secret, onDispatch: () => assert.fail("disabled") })(
      post(),
    ),
    404,
  );
  await denied(await handler({ config: { enabled: false } })(post()), 404);
  await denied(await handle(new Request(url + "/")), 404);
  const wrongMethod = await handle(new Request(url));
  assert.equal(wrongMethod.headers.get("allow"), "POST");
  await denied(wrongMethod, 405);
  for (const name of ["x-bot-appid", "x-signature-timestamp", "x-signature-ed25519"]) {
    const missing = headers();
    missing.delete(name);
    missing.set("X-Signature-Nonce", stamp);
    await denied(await handle(post(dispatchBody, missing)), 401);
    const duplicate = headers();
    duplicate.append(name, duplicate.get(name)!);
    await denied(await handle(post(dispatchBody, duplicate)), 401);
  }
  const wrongApp = headers();
  wrongApp.set("X-Bot-Appid", "other");
  await denied(await handle(post(dispatchBody, wrongApp)), 401);
  for (
    const value of [
      "",
      "0".repeat(126),
      "f".repeat(130),
      "zz".repeat(64),
      "00".repeat(64),
      dispatchHex.slice(0, -2) + "e0",
    ]
  ) {
    await denied(await handle(post(dispatchBody, headers(value))), 401);
  }
  for (const invalid of ["", "0" + stamp, `+${stamp}`, `${stamp}.0`, `${stamp}000`, "1e9", "NaN"]) {
    await denied(await handle(signed(dispatchBody, invalid)), 401);
  }
  await denied(await handler({ secret: secret + "another-key" })(post()), 401);
  assert.equal(calls, 0);
});

Deno.test("clock skew has inclusive Unix-second bounds and is rechecked after reading", async () => {
  let now = clock;
  const handle = handler({ now: () => now });
  for (const diff of [-301, -300, 0, 300, 301]) {
    now = clock + diff * 1000;
    const response = await handle(post());
    if (Math.abs(diff) <= 300) await jsonReply(response, ok);
    else await denied(response, 401);
  }
  now = clock;
  let send!: ReadableStreamDefaultController<Uint8Array>;
  const response = handle(post(
    new ReadableStream({
      start(controller) {
        send = controller;
      },
    }),
  ));
  now = clock + 301_000;
  send.enqueue(encode(dispatchBody));
  send.close();
  await denied(await response, 401);
});

Deno.test("signed heartbeats echo uint32 sequences; malformed signed UTF-8, JSON and envelopes are 400", async () => {
  let calls = 0;
  const handle = handler({
    onDispatch() {
      calls++;
      return Promise.resolve("persisted");
    },
  });
  for (const seq of [0, 42, 0xffff_ffff]) {
    await jsonReply(await handle(signed(JSON.stringify({ op: 1, d: seq }))), { op: 11, d: seq });
  }
  for (
    const data of [null, "", 7, [], {}, { plain_token: "token", event_ts: 42 }, {
      plain_token: "",
      event_ts: stamp,
    }, { plain_token: "token", event_ts: "not-seconds" }]
  ) {
    await denied(await handle(signed(JSON.stringify({ op: 13, d: data }))), 400);
  }
  for (
    const body of [
      "",
      "{",
      "null",
      "[]",
      "false",
      "{}",
      '{"op":12,"d":0}',
      ...[null, -1, 1.5, 0x1_0000_0000, "42"].map((data) => JSON.stringify({ op: 1, d: data })),
      ...[
        { d: {} },
        { t: "EVENT" },
        { t: "  ", d: {} },
        { t: "READY", d: {} },
        { t: "RESUMED", d: {} },
        { t: "EVENT", d: {}, s: -1 },
        { t: "EVENT", d: {}, s: 1.5 },
        { t: "EVENT", d: {}, id: 42 },
      ]
        .map((fields) => JSON.stringify({ op: 0, ...fields })),
      Uint8Array.of(...encode('{"op":0,"t":"EVENT","d":"'), 255, ...encode('"}')),
    ]
  ) await denied(await handle(signed(body)), 400);
  await denied(await handle(post("{")), 401);
  assert.equal(calls, 0);
});

Deno.test("actual streamed bytes and declared length are bounded before verification", async () => {
  const length = encode(dispatchBody).length;
  const handle = handler({ config: { maxBodyBytes: length } });
  const stream = (last: string) =>
    new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(encode(dispatchBody.slice(0, 3)));
        controller.enqueue(encode(dispatchBody.slice(3) + last));
        controller.close();
      },
    });
  await jsonReply(await handle(post(stream(""))), ok);
  await denied(await handle(post(stream(" "))), 413);
  for (
    const [declared, status] of [[String(length + 1), 413], [String(length - 1), 400], [
      "1, 1",
      400,
    ], ["-1", 400]] as const
  ) {
    const auth = headers();
    auth.set("content-length", declared);
    await denied(await handle(post(stream(""), auth)), status);
  }
  await denied(await handle(post(null)), 400);
  await denied(
    await handle(post(
      new ReadableStream({
        start(controller) {
          controller.error(new Error("fixture"));
        },
      }),
    )),
    400,
  );
});

Deno.test("one stalled unverified body times out, cancels the stream and releases capacity", async () => {
  let calls = 0, cancellations = 0;
  const handle = handler({
    config: { maxInFlight: 1, bodyTimeoutMs: 30 },
    onDispatch() {
      calls++;
      return Promise.resolve("persisted");
    },
  });
  const stalled = handle(post(
    new ReadableStream({
      start(controller) {
        controller.enqueue(encode("{"));
      },
      cancel() {
        cancellations++;
      },
    }),
  ));
  await denied(
    await handle(post(
      new ReadableStream({
        cancel() {
          cancellations++;
        },
      }),
    )),
    503,
  );
  await denied(await stalled, 408);
  assert.equal(cancellations, 2);
  assert.equal(calls, 0);
  await jsonReply(await handle(post()), ok);
  assert.equal(calls, 1);
  const stop = new AbortController();
  const aborted = handle(post(
    new ReadableStream({
      cancel() {
        cancellations++;
      },
    }),
    headers(),
    stop.signal,
  ));
  stop.abort();
  await denied(await aborted, 503);
  assert.equal(cancellations, 3);
  await jsonReply(await handle(post()), ok);
});

Deno.test("ACK waits for persistence, failures request retry; the same signed delivery can be retried", async () => {
  const entered = Promise.withResolvers<void>();
  const write = Promise.withResolvers<QqWebhookReceipt>();
  let attempts = 0, answered = false;
  const handle = handler({
    config: { maxInFlight: 1 },
    onDispatch() {
      switch (++attempts) {
        case 1:
          entered.resolve();
          return write.promise;
        case 2:
          return Promise.reject(new Error("private storage failure"));
        case 3:
          return Promise.resolve(undefined as unknown as QqWebhookReceipt);
        default:
          return Promise.resolve("already_persisted");
      }
    },
  });
  const response = handle(post()).then((result) => {
    answered = true;
    return result;
  });
  await entered.promise;
  try {
    await denied(await handle(post()), 503);
    assert.equal(answered, false);
  } finally {
    write.resolve("persisted");
  }
  await jsonReply(await response, ok);
  await jsonReply(await handle(post()), retry);
  await jsonReply(await handle(post()), retry);
  await jsonReply(await handle(post()), ok);
  assert.equal(attempts, 4);
});

Deno.test("shutdown during storage does not claim a successful ACK", async () => {
  const entered = Promise.withResolvers<void>();
  const write = Promise.withResolvers<QqWebhookReceipt>();
  const stop = new AbortController();
  const handle = handler({
    signal: stop.signal,
    onDispatch(_event, delivery) {
      assert.equal(delivery.signal.aborted, false);
      entered.resolve();
      return write.promise;
    },
  });
  const response = handle(post());
  await entered.promise;
  stop.abort();
  write.resolve("persisted");
  await jsonReply(await response, retry);
  await denied(await handle(post()), 503);
});
