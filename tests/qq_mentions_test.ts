import assert from "node:assert/strict";
import { QqSendActions } from "../adapters/qq/actions.ts";
import { QqClient } from "../adapters/qq/client.ts";
import { QqPhoneDevice } from "../adapters/qq/device.ts";
import { type QqDispatch, qqMessage } from "../adapters/qq/inbound.ts";
import { type InboundRecord, QqInboundJournal } from "../adapters/qq/journal.ts";
import { QqModelCommands } from "../adapters/qq/model-command.ts";
import { QqPhone } from "../adapters/qq/phone.ts";
import { QqPhoneReading } from "../adapters/qq/reading.ts";
import { QqTimeline } from "../adapters/qq/timeline.ts";
import { fingerprint } from "../kernel/calls.ts";
import type { Json } from "../kernel/protocol.ts";

function group(seq: number, mentions: unknown = [{ scope: "single", is_you: true }]): QqDispatch {
  return {
    op: 0,
    s: seq,
    t: "GROUP_MESSAGE_CREATE",
    d: {
      id: `mention-${seq}`,
      group_openid: "group-a",
      author: { member_openid: "member-a", username: "群友" },
      content: "<@!123> Eve 看看这条",
      timestamp: "2026-11-12T01:00:00Z",
      mentions,
    },
  };
}
async function fixture() {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-mentions-" });
  const journal = new QqInboundJournal(`${directory}/inbound`);
  await journal.initialize();
  const actions = new QqSendActions(
    new QqClient({ appId: "fixture", appSecret: "fixture" }),
    `${directory}/actions`,
  );
  await actions.initialize();
  const timeline = new QqTimeline(`${directory}/timeline`);
  const reading = new QqPhoneReading(`${directory}/reading`);
  const device = new QqPhoneDevice(`${directory}/device`);
  await timeline.initialize();
  await reading.initialize();
  await device.initialize();
  let project = (record: InboundRecord) => record;
  const phone = new QqPhone(
    () => journal.records().map(project),
    actions,
    timeline,
    { id: "fixture-bot", displayName: "Eve" },
    reading,
    Date.now,
    device,
  );
  return {
    directory,
    journal,
    actions,
    timeline,
    reading,
    device,
    phone,
    project: (next: typeof project) => project = next,
    append: (raw: QqDispatch) => journal.append(qqMessage(raw)!, raw),
    close: async () => {
      await actions.close();
      await timeline.close();
      await reading.close();
      await device.close();
      await Deno.remove(directory, { recursive: true });
    },
  };
}

Deno.test("QQ 群提醒：全量事件只认单独 self 提及，不猜正文、全体、其他人或损坏字段", async () => {
  const f = await fixture();
  try {
    const variants: unknown[] = [
      [{ scope: "single", is_you: true }],
      [{ scope: "all", is_you: true }],
      [{ scope: "single", is_you: false }],
      [{ scope: "single", is_you: "true" }],
      [{ is_you: true }],
      [{ scope: "single", user_id: "fixture-bot" }],
      [null, [], "Eve"],
      { scope: "single", is_you: true },
      null,
      [],
      [{ scope: "all", is_you: true }, { scope: "single", is_you: true }],
    ];
    for (const [index, mentions] of variants.entries()) {
      const result = await f.append(group(index + 1, mentions));
      assert.equal(result.record.message.notify, false);
      await f.phone.conversations();
    }
    assert.deepEqual((await f.phone.notifications()).items.map((m) => m.handle.seq), [1, 11]);
    assert.equal((await f.phone.conversations()).items[0].pendingNotifications, 2);
    const officialAt = group(12, []);
    officialAt.t = "GROUP_AT_MESSAGE_CREATE";
    await f.append(officialAt);
    assert.deepEqual((await f.phone.notifications()).items.map((m) => m.handle.seq), [1, 11, 12]);
  } finally {
    await f.close();
  }
});

Deno.test("QQ 群提醒：历史 v1 原件、identity 与部署通知边界摘要保持，重启只增强手机视图", async () => {
  const f = await fixture();
  try {
    const raw = group(1);
    const oldMessage = qqMessage(raw, 1)!;
    const identity = await fingerprint(oldMessage.deduplication as unknown as Json);
    const original: InboundRecord = {
      schema: 1,
      seq: 1,
      receivedAt: "2026-11-12T01:00:01Z",
      identity,
      message: oldMessage,
      raw,
    };
    const path = `${f.directory}/inbound/0000000000000001-${identity}.json`;
    const bytes = JSON.stringify(original) + "\n";
    await Deno.writeTextFile(path, bytes);
    const restored = new QqInboundJournal(`${f.directory}/inbound`);
    await restored.initialize();
    f.project((record) => record);
    const phone = new QqPhone(() => restored.records(), f.actions, f.timeline, {
      id: "fixture-bot",
      displayName: "Eve",
    }, f.reading);
    const expected = {
      throughSeq: 1,
      count: 1,
      notificationCount: 0,
      digest: await fingerprint([{
        seq: 1,
        key: "incoming:1",
        conversationId: "qq:group:group-a",
        notify: false,
      }]),
    };
    assert.deepEqual(await phone.captureNotificationBoundary(), expected);
    assert.deepEqual((await phone.notifications()).items.map((m) => m.handle.seq), [1]);
    assert.deepEqual(await phone.captureNotificationBoundary(1), expected);
    assert.equal((await restored.append(qqMessage(raw)!, raw)).fresh, false);
    assert.deepEqual(restored.records(), [original]);
    assert.equal(await Deno.readTextFile(path), bytes);
    await f.timeline.close();
    const timeline = new QqTimeline(`${f.directory}/timeline`);
    await timeline.initialize();
    try {
      const restarted = new QqPhone(() => restored.records(), f.actions, timeline, {
        id: "fixture-bot",
        displayName: "Eve",
      }, f.reading);
      assert.deepEqual(await restarted.captureNotificationBoundary(), expected);
      assert.deepEqual((await restarted.notifications()).items.map((m) => m.handle.seq), [1]);
      assert.equal(await Deno.readTextFile(path), bytes);
    } finally {
      await timeline.close();
    }
  } finally {
    await f.close();
  }
});

Deno.test("QQ 群提醒：分页游标不跳过后续提及，历史查询不消除提醒，打开后仅消除已到水位", async () => {
  const f = await fixture();
  try {
    // 在追加之间拍快照，使时间线序号与收到顺序一致。
    for (let seq = 1; seq <= 11; seq++) {
      await f.append(group(seq));
      await f.phone.conversations();
    }
    const first = await f.phone.notifications();
    assert.deepEqual(first.items.map((m) => m.handle.seq), [1, 2, 3, 4, 5, 6, 7, 8]);
    assert.equal(first.hasMore, true);
    assert.equal(first.throughSeq, 8);
    await f.phone.history({ conversationId: "qq:group:group-a" });
    assert.equal((await f.phone.notifications()).items.length, 8);
    const second = await f.phone.notifications({ afterSeq: first.throughSeq });
    assert.deepEqual(second.items.map((m) => m.handle.seq), [9, 10, 11]);
    assert.equal(second.hasMore, false);
    assert.equal(second.throughSeq, 11);
    assert.equal(f.reading.get("qq:group:group-a"), null);
    await f.phone.open({ conversationId: "qq:group:group-a" });
    assert.deepEqual((await f.phone.notifications()).items, []);
    await f.append(group(12));
    assert.deepEqual((await f.phone.notifications()).items.map((m) => m.handle.seq), [12]);
    assert.equal((await f.phone.conversations()).items[0].pendingNotifications, 1);
  } finally {
    await f.close();
  }
});

Deno.test("QQ 群提醒：静音沿用设备设置且不消耗通知水位，解除静音可查看未读提及", async () => {
  const f = await fixture();
  try {
    await f.append(group(1));
    await f.device.update("qq:group:group-a", { muted: true });
    assert.deepEqual((await f.phone.notifications()).items, []);
    assert.equal((await f.phone.conversations()).items[0].pendingNotifications, 0);
    assert.equal(f.reading.get("qq:group:group-a"), null);
    await f.device.update("qq:group:group-a", { muted: false });
    assert.deepEqual((await f.phone.notifications()).items.map((m) => m.handle.seq), [1]);
    assert.equal((await f.phone.conversations()).items[0].pendingNotifications, 1);
  } finally {
    await f.close();
  }
});

Deno.test("QQ 群提醒：/model 接管视图保持抑制通知，原始群提及仍不改写", async () => {
  const f = await fixture();
  const commands = new QqModelCommands(`${f.directory}/commands`, f.actions, {
    ownerPlatformUserId: "owner",
  });
  try {
    await commands.initialize([]);
    const raw = group(1);
    (raw.d as Record<string, unknown>).content = "/model";
    const { record } = await f.append(raw);
    assert.equal(commands.handled(record), true);
    assert.equal((await f.phone.notifications()).items.length, 1);
    f.project((value) => commands.project(value));
    assert.deepEqual((await f.phone.notifications()).items, []);
    assert.equal((await f.phone.conversations()).items[0].pendingNotifications, 0);
    assert.deepEqual(f.journal.records(), [record]);
    assert.equal(record.message.message.text, "/model");
  } finally {
    await commands.close();
    await f.close();
  }
});
