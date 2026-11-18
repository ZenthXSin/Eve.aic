import assert from "node:assert/strict";
import { QqActionError, QqSendActions } from "../adapters/qq/actions.ts";
import {
  QqClient,
  type QqClientOptions,
  QqRequestError,
  type QqSendText,
} from "../adapters/qq/client.ts";
import { type QqInboundEvent, qqMessage } from "../adapters/qq/inbound.ts";
import { QqInboundJournal } from "../adapters/qq/journal.ts";
import { QqModelCommands } from "../adapters/qq/model-command.ts";
import { QqPhone } from "../adapters/qq/phone.ts";
import { QqPhoneReading } from "../adapters/qq/reading.ts";
import { QqReceiver } from "../adapters/qq/receiver.ts";
import { QqTimeline } from "../adapters/qq/timeline.ts";
import { QqTransportError } from "../adapters/qq/transport.ts";

const sameId = "same-123";
const messageId = "opaque.message-id";
const kinds = ["channel", "guild-dm"] as const;

function event(type: string, fields: Record<string, unknown> = {}): QqInboundEvent {
  return {
    op: 0,
    t: type,
    d: {
      id: messageId,
      channel_id: type === "DIRECT_MESSAGE_CREATE" ? "not-the-dm-address" : sameId,
      guild_id: type === "DIRECT_MESSAGE_CREATE" ? sameId : "not-the-channel-address",
      src_guild_id: "never-use-the-source-guild",
      group_openid: "never-use-the-group",
      author: { id: sameId, username: "同名", user_openid: "not-the-guild-author" },
      content: "<@!bot> 原文",
      timestamp: "2026-11-14T00:00:00Z",
      ...fields,
    },
  };
}

function c2c(text: string) {
  return event("C2C_MESSAGE_CREATE", { author: { user_openid: sameId }, content: text });
}

function mockClient(
  options: Pick<QqClientOptions, "sendScope" | "attachmentReader"> = {},
  respond = (index: number): Response => Response.json({ id: `sent-${index}` }),
) {
  const requests: Request[] = [];
  let auth = 0;
  const client = new QqClient({ appId: "fixture", appSecret: "fixture-secret" }, {
    sendScope: "all",
    ...options,
    tokenBaseUrl: "https://auth.invalid",
    baseUrl: "https://api.invalid",
    httpsTransport: (url, init) => {
      if (url.href === "https://auth.invalid/app/getAppAccessToken") {
        auth++;
        return Promise.resolve(Response.json({ access_token: "fixture-token", expires_in: 7200 }));
      }
      requests.push(new Request(url, init));
      return Promise.resolve(respond(requests.length));
    },
  });
  return { client, requests, auths: () => auth };
}

Deno.test("guild projections use only chat-create envelopes, typed addresses and guild message references", () => {
  const fields = {
    message_reference: { message_id: "quoted-message" },
    message_scene: { ext: ["msg_idx=not-a-guild-index", "ref_msg_idx=wrong"] },
    attachments: [{ content_type: "image/png", filename: "图.png", size: 12, url: "private-url" }],
  };
  for (
    const [type, kind, notify] of [
      ["MESSAGE_CREATE", "channel", false],
      ["AT_MESSAGE_CREATE", "channel", true],
      ["DIRECT_MESSAGE_CREATE", "guild-dm", true],
    ] as const
  ) {
    const raw = event(type, fields);
    assert.equal(qqMessage(raw, 1), undefined);
    const conversationId = `qq:${kind}:${sameId}`;
    assert.deepEqual(qqMessage(raw), {
      destination: { kind, openId: sameId },
      deduplication: { conversationId, messageId },
      message: {
        conversationId,
        platformMessageId: messageId,
        referenceIndex: messageId,
        sender: { platformUserId: sameId, displayName: "同名" },
        direction: "incoming",
        sentAt: "2026-11-14T00:00:00Z",
        text: "<@!bot> 原文",
        attachments: [{ kind: "image", name: "图.png", sizeBytes: 12 }],
        quote: { platformMessageId: "quoted-message" },
      },
      referenceIndex: "quoted-message",
      notify,
    });
  }
  const mediaOnly = qqMessage(event("MESSAGE_CREATE", {
    content: undefined,
    attachments: [{ url: "private-url" }],
  }))!;
  assert.equal(mediaOnly.message.text, "");
  assert.deepEqual(mediaOnly.message.attachments, [{ kind: "other" }]);
  for (
    const type of [
      "MESSAGE_DELETE",
      "PUBLIC_MESSAGE_DELETE",
      "DIRECT_MESSAGE_DELETE",
      "MESSAGE_UPDATE",
      "MESSAGE_AUDIT_PASS",
      "MESSAGE_AUDIT_REJECT",
      "MESSAGE_REACTION_ADD",
      "INTERACTION_CREATE",
      "GUILD_CREATE",
      "CHANNEL_CREATE",
      "FORUM_THREAD_CREATE",
      "UNKNOWN_MESSAGE_CREATE",
      "READY",
    ]
  ) assert.equal(qqMessage(event(type, fields)), undefined, type);
  for (
    const fields of [
      { guild_id: undefined },
      { guild_id: 123 },
      { direct_message: false },
      { author: { user_openid: sameId, member_openid: sameId } },
      { id: "" },
      { content: null },
      { message_reference: { message_id: "" } },
      { attachments: [null] },
    ]
  ) assert.throws(() => qqMessage(event("DIRECT_MESSAGE_CREATE", fields)), TypeError);
  for (
    const id of [undefined, "..", "../users/me", "path%2fescape", "white space", "a".repeat(129)]
  ) {
    assert.throws(() => qqMessage(event("MESSAGE_CREATE", { channel_id: id })), TypeError);
  }
});

Deno.test("guild ingress → journal replay → phone history/notification → quoted sends stay in their own conversations", async () => {
  const directory = await Deno.makeTempDir();
  const fixture = mockClient();
  const actions = new QqSendActions(fixture.client, `${directory}/actions`, "all");
  const timeline = new QqTimeline(`${directory}/timeline`);
  const reading = new QqPhoneReading(`${directory}/reading`);
  const direct = event("DIRECT_MESSAGE_CREATE");
  const receiver = new QqReceiver(fixture.client, directory, {}, {
    webhook: async (_signal, accept) => {
      for (
        const raw of [
          event("GUILD_MEMBER_ADD"),
          event("MESSAGE_CREATE"),
          direct,
          c2c("真实 C2C"),
          event("AT_MESSAGE_CREATE", { id: "next", message_reference: { message_id: messageId } }),
          { ...direct, id: "redelivery", s: 200 },
          event("DIRECT_MESSAGE_DELETE"),
        ]
      ) await accept(raw);
    },
  });
  try {
    await receiver.run(new AbortController().signal);
    const records = receiver.messages();
    assert.deepEqual(records.map((record) => record.message.destination.kind), [
      "channel",
      "guild-dm",
      "user",
      "channel",
    ]);
    assert.equal(new Set(records.map((record) => record.identity)).size, 4);
    const replay = new QqInboundJournal(`${directory}/inbound`);
    await replay.initialize();
    assert.deepEqual(replay.records(), records);
    assert.equal((await replay.append(qqMessage(direct)!, { ...direct, s: 201 })).fresh, false);
    await actions.initialize();
    await timeline.initialize();
    await reading.initialize();
    const phone = new QqPhone(
      replay,
      actions,
      timeline,
      { id: "bot", displayName: "Eve" },
      reading,
    );
    const channel = `qq:channel:${sameId}`;
    const dm = `qq:guild-dm:${sameId}`;
    const notifications = (await phone.notifications()).items.map((item) =>
      item.handle.conversationId
    );
    assert.deepEqual(notifications, [dm, `qq:user:${sameId}`, channel]);
    const conversations = (await phone.conversations()).items;
    assert.deepEqual(conversations.map((item) => item.kind).sort(), [
      "channel",
      "guild-dm",
      "user",
    ]);
    const [first, quoted] = (await phone.history({ conversationId: channel })).messages;
    assert.deepEqual(quoted.reference, { index: messageId, target: first.handle });
    assert.equal((await phone.history({ conversationId: dm })).messages.length, 1);
    for (const [index, conversationId] of [channel, dm].entries()) {
      const args = {
        actionId: `reply-${index}`,
        conversationId,
        text: "引用回复",
        referenceIndex: first.referenceIndex!,
      };
      const receipt = await phone.send(args);
      assert.equal(receipt.state, "sent");
      assert.deepEqual(await phone.send(args), receipt);
      const outgoing = (await phone.history({ conversationId })).messages.at(-1)!;
      assert.equal(outgoing.referenceIndex, `sent-${index + 1}`);
      assert.equal(outgoing.reference?.target?.conversationId, conversationId);
    }
    assert.deepEqual(fixture.requests.map((request) => new URL(request.url).pathname), [
      `/channels/${sameId}/messages`,
      `/dms/${sameId}/messages`,
    ]);
    for (const request of fixture.requests) {
      assert.equal(request.method, "POST");
      assert.equal(request.headers.get("Authorization"), "QQBot fixture-token");
      assert.deepEqual(await request.json(), {
        content: "引用回复",
        message_reference: { message_id: messageId },
      });
    }
  } finally {
    await actions.close();
    await timeline.close();
    await reading.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("identical guild-DM and C2C sender, address and message IDs cannot authorize owner commands", async () => {
  const directory = await Deno.makeTempDir();
  const fixture = mockClient();
  const journal = new QqInboundJournal(`${directory}/journal`);
  const actions = new QqSendActions(fixture.client, `${directory}/actions`, "all");
  let listings = 0;
  const commands = new QqModelCommands(`${directory}/commands`, actions, {
    ownerPlatformUserId: sameId,
    control: {
      status: () => {
        listings++;
        return Promise.resolve({
          current: { model: "private-model", revision: 1 },
          availableModels: ["private-model"],
        });
      },
      select: () => assert.fail("unexpected model change"),
      receipt: () => assert.fail("unexpected reconciliation"),
    },
  });
  try {
    await journal.initialize();
    await actions.initialize();
    await commands.initialize([]);
    for (const raw of [event("DIRECT_MESSAGE_CREATE", { content: "/model" }), c2c("/model")]) {
      commands.offer((await journal.append(qqMessage(raw)!, raw)).record);
    }
    await commands.idle();
    assert.deepEqual(
      journal.records().map((record) => commands.receipt(record.identity)?.outcome),
      ["denied", "listed"],
    );
    assert.equal(listings, 1);
  } finally {
    await commands.close();
    await actions.close();
    await Deno.remove(directory, { recursive: true });
  }
  const denial = fixture.requests.find((request) =>
    new URL(request.url).pathname === `/dms/${sameId}/messages`
  )!;
  assert.doesNotMatch((await denial.json()).content, /private-model/);
});

Deno.test("guild images send a single multipart message, never a separate group upload", async () => {
  const directory = await Deno.makeTempDir();
  const bytes = new Uint8Array([137, 80, 78, 71, 13, 10, 26, 10, 0, 255]);
  const paths: string[] = [];
  const fixture = mockClient({
    attachmentReader: (path) => {
      paths.push(path);
      return Promise.resolve(bytes);
    },
  });
  const actions = new QqSendActions(fixture.client, `${directory}/actions`, "all");
  const timeline = new QqTimeline(`${directory}/timeline`);
  const reading = new QqPhoneReading(`${directory}/reading`);
  const phone = new QqPhone(
    () => [],
    actions,
    timeline,
    { id: "bot", displayName: "Eve" },
    reading,
  );
  try {
    await actions.initialize();
    await timeline.initialize();
    await reading.initialize();
    for (const [index, kind] of kinds.entries()) {
      const text = index ? "图片说明" : "";
      const result = await phone.sendFile({
        actionId: `image-${index}`,
        conversationId: `qq:${kind}:${sameId}`,
        text,
        kind: "image",
        path: "/work/private/pic.png",
      });
      assert.equal(result.state, "sent");
      assert.equal(result.platformMessageId, `sent-${index + 1}`);
      const request = fixture.requests[index];
      assert.equal(
        request.url,
        `https://api.invalid/${index ? "dms" : "channels"}/${sameId}/messages`,
      );
      assert.match(request.headers.get("content-type")!, /^multipart\/form-data; boundary=/);
      const form = await request.formData();
      assert.deepEqual([...form.keys()].sort(), text ? ["content", "file_image"] : ["file_image"]);
      assert.equal(form.get("content"), text || null);
      const file = form.get("file_image");
      assert.ok(file instanceof File);
      assert.equal(file.name, "pic.png");
      assert.deepEqual(new Uint8Array(await file.arrayBuffer()), bytes);
    }
    assert.deepEqual(paths, ["/work/private/pic.png", "/work/private/pic.png"]);
    assert.equal(fixture.requests.length, 2);
  } finally {
    await actions.close();
    await timeline.close();
    await reading.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("a guild image with no reader fails locally; a lost message receipt is not a retryable upload", async () => {
  const unreadable = mockClient();
  const fixture = mockClient(
    { attachmentReader: () => Promise.resolve(new Uint8Array([1])) },
    () => {
      throw new QqTransportError(true);
    },
  );
  const directory = await Deno.makeTempDir();
  const actions = new QqSendActions(fixture.client, directory, "all", { retryDelaysMs: [0] });
  const request: QqSendText = {
    destination: { kind: "guild-dm", openId: sameId },
    text: "",
    attachment: { kind: "image", path: "/private/pic.png" },
  };
  try {
    assert.deepEqual(await unreadable.client.sendText(request), {
      status: "failed",
      attempted: false,
      error: { category: "request", failureStage: "upload", reason: "attachment_read_failed" },
    });
    assert.deepEqual([unreadable.auths(), unreadable.requests.length], [0, 0]);
    await actions.initialize();
    const status = await actions.send("image", request);
    assert.ok(status.status === "finished" && status.result.status === "unknown");
    assert.equal(status.result.error.failureStage, "message");
    assert.deepEqual(await actions.send("image", request), status);
    assert.deepEqual(fixture.requests.map((request) => new URL(request.url).pathname), [
      `/dms/${sameId}/messages`,
    ]);
  } finally {
    unreadable.client.close();
    await actions.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("unsupported or malformed guild operations fail before action admission, readers, authentication or send", async () => {
  const directory = await Deno.makeTempDir();
  let reads = 0;
  const fixture = mockClient({
    attachmentReader: () => {
      reads++;
      return Promise.resolve(new Uint8Array());
    },
  });
  const actions = new QqSendActions(fixture.client, directory, "all");
  try {
    await actions.initialize();
    for (const kind of kinds) {
      const destination = { kind, openId: sameId };
      const image = { kind: "image", path: "/test.png" };
      await assert.rejects(
        fixture.client.sendText({ destination, text: "test" }, 27),
        QqRequestError,
      );
      for (
        const fields of [
          { wakeup: true },
          { format: "markdown" },
          { attachment: { kind: "file", path: "/test.txt" } },
          { reply: { messageId, sequence: 1 } },
          { reply: { eventId: "event" } },
          { attachment: image, referenceIndex: messageId },
          { referenceIndex: " " },
          { attachment: { ...image, path: "relative.png" } },
          { attachment: { ...image, url: "https://other.invalid" } },
          { destination: { kind, openId: "../me" } },
          { destination: { kind: "unknown", openId: sameId } },
          { msg_type: 6 },
          { msg_seq: 27 },
          { input_notify: {} },
          { is_wakeup: true },
          { event_id: "event" },
        ]
      ) {
        const request = { destination, text: "test", ...fields } as QqSendText;
        await assert.rejects(
          fixture.client.sendText(request),
          (error) => error instanceof QqRequestError && error.info.category === "request",
        );
        await assert.rejects(
          actions.send("invalid", request),
          (error) => error instanceof QqActionError && error.category === "request",
        );
        assert.equal(actions.record("invalid"), undefined);
      }
    }
    assert.deepEqual([reads, fixture.auths(), fixture.requests.length], [0, 0, 0]);
  } finally {
    await actions.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("private scope denies both guild destinations; refusal and ambiguous delivery never fall back", async () => {
  const privateOnly = mockClient({ sendScope: "private" });
  const refusal = mockClient({}, () => Response.json({ code: 40034105 }, { status: 403 }));
  const pending = mockClient({}, () => Response.json({ code: 304023 }, { status: 202 }));
  try {
    for (const kind of kinds) {
      const request = { destination: { kind, openId: sameId }, text: "guild only" };
      assert.deepEqual(await privateOnly.client.sendText(request), {
        status: "failed",
        attempted: false,
        error: { category: "scope" },
      });
      assert.equal((await refusal.client.sendText(request)).status, "failed");
      assert.equal((await pending.client.sendText(request)).status, "unknown");
    }
    assert.deepEqual([privateOnly.auths(), privateOnly.requests.length], [0, 0]);
    for (const fixture of [refusal, pending]) {
      assert.deepEqual(fixture.requests.map((request) => new URL(request.url).pathname), [
        `/channels/${sameId}/messages`,
        `/dms/${sameId}/messages`,
      ]);
      for (const request of fixture.requests) {
        assert.deepEqual(await request.json(), { content: "guild only" });
      }
    }
  } finally {
    for (const fixture of [privateOnly, refusal, pending]) fixture.client.close();
  }
});
