import assert from "node:assert/strict";
import { QqSendActions } from "../adapters/qq/actions.ts";
import { QqClient, type QqSendResult } from "../adapters/qq/client.ts";
import { QqPhoneDevice } from "../adapters/qq/device.ts";
import { QqPhoneDownloads } from "../adapters/qq/downloads.ts";
import { type QqDispatch, qqMessage } from "../adapters/qq/inbound.ts";
import { QqInboundJournal } from "../adapters/qq/journal.ts";
import { QqModelCommands } from "../adapters/qq/model-command.ts";
import { QqPhone } from "../adapters/qq/phone.ts";
import { QqPhoneReading } from "../adapters/qq/reading.ts";
import { QqTimeline, QqTimelineError, type TimelineFact } from "../adapters/qq/timeline.ts";
import type { PhoneDownload } from "../contracts/phone.ts";

const self = { id: "bot", displayName: "Eve" };
const room = "qq:user:u", group = "qq:group:g";
const now = () => Date.UTC(2026, 10, 18);

class Journal extends QqInboundJournal {
  tails: [number, number][] = [];
  lookups: number[] = [];
  override records(after = 0) {
    const records = super.records(after);
    this.tails.push([after, records.length]);
    return records;
  }
  override record(seq: number) {
    this.lookups.push(seq);
    return super.record(seq);
  }
}
class Timeline extends QqTimeline {
  batches: [number, string[]][] = [];
  override sync(through: number, facts: readonly TimelineFact[]) {
    this.batches.push([through, facts.map(({ key }) => key)]);
    return super.sync(through, facts);
  }
}
class Downloads extends QqPhoneDownloads {
  probes: string[] = [];
  state: "ready" | "cleared" | "failed" = "ready";
  override hasReceipt() {
    return true;
  }
  override local(id: string): Promise<PhoneDownload | undefined> {
    this.probes.push(id);
    if (this.state === "failed") return Promise.reject(new Error("local IO"));
    return Promise.resolve(
      this.state === "ready"
        ? {
          path: `${this.options.temporaryDirectory}/local`,
          sizeBytes: 1,
          sha256: "0".repeat(64),
          mimeType: "text/plain",
          source: {
            attachmentId: id,
            conversationId: room,
            kind: "phone_attachment",
            sourceSeq: 1,
            senderPlatformUserId: "u",
          },
        }
        : undefined,
    );
  }
}

async function fixture() {
  const root = await Deno.makeTempDir();
  const journal = new Journal(`${root}/inbox`), timeline = new Timeline(`${root}/timeline`);
  const client = new QqClient({ appId: "fixture", appSecret: "fixture" }, {
    httpsTransport: () => {
      throw new Error("unexpected platform call");
    },
  });
  const actions = new QqSendActions(client, `${root}/outbox`);
  const reading = new QqPhoneReading(`${root}/reading`),
    device = new QqPhoneDevice(`${root}/device`);
  const downloads = new Downloads(`${root}/files`, undefined, 30_000, {
    temporaryDirectory: `${root}/tmp`,
  });
  const commands = new QqModelCommands(`${root}/commands`, actions);
  await journal.initialize();
  for (const store of [timeline, actions, reading, device]) await store.initialize();
  const phone = new QqPhone(
    journal,
    actions,
    timeline,
    self,
    reading,
    now,
    device,
    downloads,
    true,
    undefined,
    commands,
  );
  return {
    phone,
    actions,
    client,
    journal,
    timeline,
    commands,
    downloads,
    reading,
    device,
    append(seq: number, text: string, extra = {}, grouped = false) {
      const raw: QqDispatch = {
        op: 0,
        s: seq,
        t: grouped ? "GROUP_MESSAGE_CREATE" : "C2C_MESSAGE_CREATE",
        d: {
          id: `id-${seq}`,
          content: text,
          author: grouped
            ? { member_openid: "a", username: "A" }
            : { user_openid: "u", username: "U" },
          ...(grouped ? { group_openid: "g" } : {}),
          ...extra,
        },
      };
      return journal.append(qqMessage(raw)!, raw);
    },
    async close() {
      try {
        await commands.close();
        client.close();
        await Promise.all([actions.close(), reading.close(), device.close()]);
        await timeline.close();
      } finally {
        await Deno.remove(root, { recursive: true });
      }
    },
  };
}

Deno.test("QQ projection: repeated tail/handle reads and a small append have bounded work", async () => {
  const setup = await fixture();
  try {
    const attachment = {
      attachments: [{
        filename: "a.txt",
        content_type: "text/plain",
        url: "https://private.invalid/token",
      }],
    };
    for (let seq = 1; seq <= 24; seq++) await setup.append(seq, `text-${seq}`, attachment);
    const empty = { afterSeq: 24 }, handle = { conversationId: room, seq: 1 };
    await setup.phone.notifications(empty);
    await Promise.all([setup.phone.notifications(empty), setup.phone.notifications(empty)]);
    assert.deepEqual(setup.journal.tails, [[0, 24], [24, 0], [24, 0]]);
    assert.deepEqual(setup.journal.lookups, []);
    assert.deepEqual(setup.downloads.probes, []);
    const first = await setup.phone.history({ conversationId: room });
    assert.deepEqual(first.messages.map((message) => message.handle.seq), [
      17,
      18,
      19,
      20,
      21,
      22,
      23,
      24,
    ]);
    assert.deepEqual(setup.journal.lookups, first.messages.map((message) => message.handle.seq));
    assert.equal(setup.downloads.probes.length, 8);
    const ready = await setup.phone.message({ handle });
    assert.equal(ready.attachments[0].download?.state, "ready");
    assert.ok(!JSON.stringify(ready).includes("private.invalid"));
    ready.body.text = "caller mutation";
    setup.downloads.state = "cleared";
    const cleared = await setup.phone.message({ handle });
    assert.deepEqual(cleared.attachments[0].download, { state: "cleared" });
    assert.equal(cleared.body.text, "text-1");
    setup.downloads.state = "failed";
    assert.deepEqual((await setup.phone.message({ handle })).attachments[0].download, {
      state: "failed",
    });
    assert.equal(setup.timeline.batches.length, 1);
    await setup.append(25, "text-25");
    const [notice, older] = await Promise.all([
      setup.phone.notifications(empty),
      setup.phone.history({ conversationId: room, cursor: first.older }),
    ]);
    assert.deepEqual(notice.items.map((message) => message.handle.seq), [25]);
    assert.equal(older.snapshotThroughSeq, 24);
    assert.deepEqual(older.messages.map((message) => message.handle.seq), [
      9,
      10,
      11,
      12,
      13,
      14,
      15,
      16,
    ]);
    assert.deepEqual(setup.timeline.batches.filter(([, keys]) => keys.length).slice(1), [[24, [
      "incoming:25",
    ]]]);
    assert.deepEqual(setup.journal.tails.filter(([, length]) => length), [[0, 24], [24, 1]]);
    await setup.phone.settings({
      conversationId: room,
      muted: true,
      alias: "私聊",
      remark: "备注",
      pinned: true,
      draft: "secret draft",
    });
    assert.deepEqual((await setup.phone.notifications(empty)).items, []);
    assert.equal((await setup.phone.search({ query: "备注" })).messages.length, 8);
    const opened = await setup.phone.open({ conversationId: "私聊" });
    assert.equal(opened.earlierMessagesNotShown, 17);
    assert.equal(opened.notificationThroughSeq, 25);
    await setup.phone.settings({ conversationId: room, muted: false, remark: "", alias: null });
    assert.deepEqual((await setup.phone.notifications()).items, []);
    assert.deepEqual((await setup.phone.search({ query: "备注" })).messages, []);
    const list = await setup.phone.conversations();
    assert.equal(list.items[0].settings.hasDraft, true);
    assert.ok(!JSON.stringify(list).includes("secret draft"));
  } finally {
    await setup.close();
  }
});

Deno.test("QQ projection: command takeover and late contact names are not frozen in the index", async () => {
  const setup = await fixture();
  try {
    await setup.append(1, "/model");
    await setup.append(2, "<@later>", { mentions: [{ scope: "single", is_you: true }] }, true);
    assert.equal((await setup.phone.notifications()).items.length, 2);
    assert.equal((await setup.phone.captureNotificationBoundary()).notificationCount, 1);
    await setup.commands.initialize([]);
    assert.deepEqual(
      (await setup.phone.notifications()).items.map((message) => message.handle.seq),
      [2],
    );
    assert.equal((await setup.phone.captureNotificationBoundary()).notificationCount, 0);
    assert.equal((await setup.phone.search({ query: "适配器已接管" })).messages[0].handle.seq, 1);
    assert.equal(
      (await setup.phone.text({
        handle: { conversationId: room, seq: 1 },
        startChar: 0,
        limit: 200,
      })).text.includes("已接管"),
      true,
    );
    const handle = { conversationId: group, seq: 2 };
    assert.equal((await setup.phone.message({ handle })).mentions?.[0].resolved, false);
    await setup.append(3, "here", {
      author: { member_openid: "later", username: "Later" },
      group_name: "新群名",
    }, true);
    const updated = await setup.phone.message({ handle });
    assert.equal(updated.mentions?.[0].displayName, "Later");
    assert.equal(updated.conversation?.platformName, "新群名");
    assert.deepEqual(setup.timeline.batches.map(([through, keys]) => [through, keys.length]), [[
      0,
      2,
    ], [2, 1]]);
  } finally {
    await setup.close();
  }
});

Deno.test("QQ projection: same outbox key changes from running to sent; references can become ambiguous", async () => {
  const setup = await fixture();
  const entered = Promise.withResolvers<void>(), release = Promise.withResolvers<QqSendResult>();
  setup.client.sendText = () => {
    entered.resolve();
    return release.promise;
  };
  try {
    await setup.append(1, "quote", { message_scene: { ext: ["ref_msg_idx=idx"] } });
    await setup.phone.history({ conversationId: room });
    const sending = setup.actions.send("send", {
      destination: { kind: "user", openId: "u" },
      text: "out",
    });
    await entered.promise;
    const outgoing = { conversationId: room, seq: 2 }, incoming = { conversationId: room, seq: 1 };
    assert.equal((await setup.phone.message({ handle: outgoing })).delivery?.state, "running");
    assert.deepEqual((await setup.phone.message({ handle: incoming })).reference, { index: "idx" });
    release.resolve({
      status: "sent",
      messageId: "platform",
      referenceIndex: "idx",
      timestamp: "2026-01-01T00:00:00Z",
    });
    await sending;
    const sent = await setup.phone.message({ handle: outgoing });
    assert.equal(sent.delivery?.state, "sent");
    assert.equal(sent.platformMessageId, "platform");
    assert.equal(sent.referenceIndex, "idx");
    assert.deepEqual((await setup.phone.message({ handle: incoming })).reference?.target, outgoing);
    assert.equal(setup.timeline.batches.length, 2);
    await setup.append(2, "duplicate", { message_scene: { ext: ["msg_idx=idx"] } });
    const ambiguous = await setup.phone.message({ handle: incoming });
    assert.deepEqual(ambiguous.reference, { index: "idx" });
    assert.equal(ambiguous.quote, undefined);
    assert.deepEqual(setup.timeline.batches, [[0, ["incoming:1"]], [1, ["outgoing:send"]], [2, [
      "incoming:2",
    ]]]);
  } finally {
    release.resolve({ status: "sent", messageId: "cleanup" });
    await setup.close();
  }
});

Deno.test("QQ projection: a cached reader still fails closed after a missing source is indexed", async () => {
  const setup = await fixture();
  try {
    await setup.append(1, "real");
    await setup.phone.notifications();
    const original = setup.journal.record(1)!;
    const fact = {
      key: "incoming:1",
      fingerprint: original.identity,
      observedAt: original.receivedAt,
      message: original.message.message,
      notify: true,
    };
    await setup.timeline.snapshot([fact, { ...fact, key: "outgoing:absent" }]);
    await assert.rejects(
      () => setup.phone.notifications({ afterSeq: 1 }),
      (error) => error instanceof QqTimelineError && error.category === "corrupt",
    );
    await assert.rejects(() => setup.phone.history({ conversationId: room }), QqTimelineError);
  } finally {
    await assert.rejects(() => setup.close(), QqTimelineError);
  }
});

Deno.test("QQ projection: legacy callback can change envelope labels and reference fields without appending", async () => {
  const setup = await fixture();
  try {
    await setup.append(1, "target", {}, true);
    await setup.append(2, "quote", {}, true);
    let label = "before";
    const legacy = new QqPhone(
      () =>
        setup.journal.records().map((record) => {
          const message = record.message.message;
          message.conversation = { platformName: label };
          if (record.seq === 1) message.referenceIndex = "custom";
          else message.reference = { index: "custom" };
          return record;
        }),
      setup.actions,
      setup.timeline,
      self,
      setup.reading,
      now,
    );
    assert.equal((await legacy.search({ query: label })).messages.length, 2);
    label = "after";
    assert.deepEqual((await legacy.search({ query: "before" })).messages, []);
    const updated = await legacy.search({ query: label });
    assert.equal(updated.messages.length, 2);
    assert.deepEqual(updated.messages[1].reference?.target, updated.messages[0].handle);
  } finally {
    await setup.close();
  }
});

Deno.test("QQ projection: cancellation changes both the head and its blocked successor without new keys", async () => {
  const setup = await fixture();
  try {
    setup.client.sendText = () =>
      Promise.resolve({ status: "unknown", attempted: true, error: { category: "pending" } });
    const send = (actionId: string) =>
      setup.phone.send({ actionId, conversationId: room, text: actionId });
    assert.equal((await send("head")).state, "unknown");
    assert.equal((await send("next")).state, "blocked");
    const history = () => setup.phone.history({ conversationId: room });
    assert.deepEqual((await history()).messages.map((message) => message.delivery?.state), [
      "unknown",
      "blocked",
    ]);
    const head = (await setup.phone.listSendQueue({ conversationId: room })).items.find((item) =>
      item.actionId === "head"
    )!;
    assert.ok(head.canCancel && head.receiptDigest);
    await setup.phone.cancelSend({
      actionId: "head",
      expectedFingerprint: head.fingerprint,
      expectedReceiptDigest: head.receiptDigest,
    });
    assert.deepEqual((await history()).messages.map((message) => message.delivery?.state), [
      "cancelled",
      "unknown",
    ]);
    assert.equal(setup.timeline.batches.length, 2);
  } finally {
    await setup.close();
  }
});

Deno.test("QQ projection: overlapping reads keep their own prefix; restart agrees on mixed ordering", async () => {
  const setup = await fixture();
  const committed = Promise.withResolvers<void>(), release = Promise.withResolvers<void>();
  const persist = setup.timeline.sync.bind(setup.timeline);
  setup.timeline.sync = async (through, facts) => {
    const keys = await persist(through, facts);
    if (!through) {
      committed.resolve();
      await release.promise;
    }
    return keys;
  };
  try {
    await setup.append(1, "old", { group_name: "old name" }, true);
    const earlierRead = setup.phone.history({ conversationId: group });
    await committed.promise;
    setup.client.sendText = () => Promise.resolve({ status: "sent", messageId: "sent-id" });
    await setup.actions.send("between", {
      destination: { kind: "user", openId: "u" },
      text: "out",
    });
    const later = await setup.append(2, "new", { group_name: "new name" }, true);
    const laterRead = setup.phone.history({ conversationId: group });
    release.resolve();
    const [old, current] = await Promise.all([earlierRead, laterRead]);
    assert.equal(old.snapshotThroughSeq, 1);
    assert.equal(old.messages[0].conversation?.platformName, "old name");
    assert.equal(current.snapshotThroughSeq, 3);
    const startedAt = setup.actions.record("between")!.startedAt;
    const incomingSeq = Date.parse(later.record.receivedAt) <= Date.parse(startedAt) ? 2 : 3;
    assert.deepEqual(current.messages.map((message) => message.handle.seq), [1, incomingSeq]);
    assert.equal(current.messages[0].conversation?.platformName, "new name");
    const outgoing = await setup.phone.history({ conversationId: room });
    assert.equal(outgoing.messages[0].handle.seq, 5 - incomingSeq);
    await setup.timeline.close();
    await assert.rejects(() => setup.phone.history({ conversationId: group }), QqTimelineError);
    const timeline = new Timeline(setup.timeline.directory);
    await timeline.initialize();
    try {
      const recovered = new QqPhone(
        setup.journal,
        setup.actions,
        timeline,
        self,
        setup.reading,
        now,
        setup.device,
      );
      assert.deepEqual(await recovered.history({ conversationId: group }), current);
      assert.deepEqual(await recovered.history({ conversationId: room }), outgoing);
      assert.deepEqual(timeline.batches, [[0, ["incoming:1", "incoming:2", "outgoing:between"]]]);
    } finally {
      await timeline.close();
    }
  } finally {
    release.resolve();
    await setup.close();
  }
});
