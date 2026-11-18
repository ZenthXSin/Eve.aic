import assert from "node:assert/strict";
import { QqSendActions, type SendStatus } from "../adapters/qq/actions.ts";
import { guildChatSendPlan } from "../adapters/qq/channel-chat.ts";
import { QqClient } from "../adapters/qq/client.ts";
import { type QqInboundEvent, qqMessage } from "../adapters/qq/inbound.ts";
import { QqInboundJournal } from "../adapters/qq/journal.ts";
import { phoneReceipt, QqPhone, QqPhoneError } from "../adapters/qq/phone.ts";
import { QqPhoneReading } from "../adapters/qq/reading.ts";
import { QqTimeline } from "../adapters/qq/timeline.ts";
import {
  createQqPlatformOperator,
  type QqPlatformConfirm,
} from "../adapters/qq/platform-client.ts";
import type { MemorySource } from "../contracts/memory.ts";
import type { MessageScreen } from "../contracts/phone.ts";
import { terminalConfirmation } from "../deployment/qq-platform-operator.ts";
import { KernelError } from "../kernel/protocol.ts";
import { conversationArgs, updateConversation } from "../plugins/eve/social.ts";
import { FactJournal } from "../sdk/fact-journal.ts";
import { historyScreen } from "../sdk/phone-view.ts";
import { MemoryStore } from "../services/runtime/memory.ts";
import { NoteStore } from "../services/runtime/notes.ts";

type Label = string | null;
type Case = readonly [id: string, gold: Label, actual: Label];

function counts(cases: readonly Case[]) {
  let tp = 0, fp = 0, fn = 0, tn = 0;
  for (const [, gold, actual] of cases) {
    if (gold === actual) {
      if (gold === null) tn++;
      else tp++;
    } else {
      if (actual !== null) fp++;
      if (gold !== null) fn++;
    }
  }
  return { tp, fp, fn, tn };
}

function gate(family: string, cases: readonly Case[]) {
  assert.ok(cases.length > 0);
  assert.equal(new Set(cases.map(([id]) => id)).size, cases.length);
  const { tp, fp, fn, tn } = counts(cases);
  const fraction = (numerator: number, denominator: number) =>
    denominator ? `${numerator}/${denominator}` : "N/A";
  console.log(
    `${family}: TP=${tp} FP=${fp} FN=${fn} TN=${tn}; P=${fraction(tp, tp + fp)} R=${
      fraction(tp, tp + fn)
    } exact=${tp + tn}/${cases.length}`,
  );
  assert.deepEqual(
    cases.filter(([, gold, actual]) => gold !== actual),
    [],
    "[case, gold, observed]",
  );
}

Deno.test("accuracy scoring: wrong non-null label is both a false claim and a missed fact", () => {
  assert.deepEqual(
    counts([
      ["right", "r", "r"],
      ["wrong", "r", "s"],
      ["invented", null, "s"],
      ["miss", "r", null],
      ["withheld", null, null],
    ]),
    { tp: 1, fp: 2, fn: 2, tn: 1 },
  );
});

async function local(run: (directory: string) => Promise<void>, ledger = false) {
  const directory = await Deno.makeTempDir({ prefix: "eve-accuracy-" });
  const originalUid = Deno.uid;
  const fixtureUid = ledger &&
    Deno.permissions.querySync({ name: "sys", kind: "uid" }).state !== "granted";
  try {
    if (fixtureUid) {
      const { uid } = await Deno.stat(directory);
      Deno.uid = () => uid;
    }
    await run(directory);
  } finally {
    if (fixtureUid) Deno.uid = originalUid;
    await Deno.remove(directory, { recursive: true });
  }
}

const clock = Date.parse("2026-01-07T08:00:00Z");
const channel = "qq:channel:lab", direct = "qq:guild-dm:lab";

function incoming(type: string, id = "post", fields: Record<string, unknown> = {}): QqInboundEvent {
  return {
    op: 0,
    t: type,
    d: {
      id,
      content: "标注用合成原文",
      author: { id: "actor", user_openid: "lab", member_openid: "member", username: "同名" },
      channel_id: type === "DIRECT_MESSAGE_CREATE" ? "not-dm" : "lab",
      guild_id: type === "DIRECT_MESSAGE_CREATE" ? "lab" : "not-channel",
      src_guild_id: "not-the-target",
      group_openid: "lab",
      ...fields,
    },
  };
}

Deno.test("accuracy R: same display name and opaque IDs never substitute for a typed route", () => {
  function route(raw: QqInboundEvent, version: 1 | 2 = 2): Label {
    const message = qqMessage(raw, version);
    if (!message) return null;
    const { destination } = message;
    if (destination.kind === "user" || destination.kind === "group") {
      return message.message.conversationId;
    }
    const plan = guildChatSendPlan({ destination, text: "仅计算路径" });
    return `${message.message.conversationId} ${plan?.path}`;
  }
  function validRoute(raw: QqInboundEvent) {
    try {
      return route(raw);
    } catch (error) {
      if (error instanceof TypeError) return null;
      throw error;
    }
  }
  gate("R", [
    ["R1 c2c", "qq:user:lab", route(incoming("C2C_MESSAGE_CREATE"))],
    ["R2 group", "qq:group:lab", route(incoming("GROUP_MESSAGE_CREATE"))],
    ["R3 channel", "qq:channel:lab /channels/lab/messages", route(incoming("MESSAGE_CREATE"))],
    ["R4 guild DM", "qq:guild-dm:lab /dms/lab/messages", route(incoming("DIRECT_MESSAGE_CREATE"))],
    ["R5 legacy DM not supported", null, route(incoming("DIRECT_MESSAGE_CREATE"), 1)],
    ["R6 delete not chat", null, route(incoming("DIRECT_MESSAGE_DELETE"))],
    [
      "R7 no fallback to channel or src_guild",
      null,
      validRoute(incoming("DIRECT_MESSAGE_CREATE", "post", { guild_id: null })),
    ],
    [
      "R8 contradictory DM flag",
      null,
      validRoute(incoming("MESSAGE_CREATE", "post", { direct_message: true })),
    ],
  ]);
});

function target(message: MessageScreen) {
  assert.ok(message.reference, "fixture must actually contain a platform reference");
  const handle = message.reference.target;
  return handle ? `${handle.conversationId}#${handle.seq}` : null;
}

Deno.test("accuracy H: reference handles, old/new snapshots and same-name cross-room negatives", () =>
  local(async (directory) => {
    const journal = new QqInboundJournal(`${directory}/incoming`);
    const client = new QqClient({ appId: "synthetic", appSecret: "unused" }, {
      httpsTransport: () => {
        throw new Error("no transport in accuracy gate");
      },
    });
    const actions = new QqSendActions(client, `${directory}/actions`);
    const timeline = new QqTimeline(`${directory}/timeline`),
      reading = new QqPhoneReading(`${directory}/reading`);
    try {
      await journal.initialize();
      await actions.initialize();
      await timeline.initialize();
      await reading.initialize();
      const phone = new QqPhone(
        journal,
        actions,
        timeline,
        { id: "bot", displayName: "样本" },
        reading,
        () => clock,
      );
      const append = (raw: QqInboundEvent) => journal.append(qqMessage(raw)!, raw);
      const quote = (id: string) => ({ message_reference: { message_id: id } });
      await append(incoming("MESSAGE_CREATE", "early-question", quote("late-source")));
      await append(incoming("DIRECT_MESSAGE_CREATE", "late-source"));
      await append(incoming("MESSAGE_CREATE", "source"));
      await append(incoming("MESSAGE_CREATE", "quoted-question", quote("source")));
      for (let seq = 5; seq <= 11; seq++) await append(incoming("MESSAGE_CREATE", `filler-${seq}`));
      const open = await phone.history({ conversationId: channel });
      assert.ok(open.older);
      const older = () => phone.history({ conversationId: channel, cursor: open.older });
      const oldPage = await older();
      const lookup = (conversationId: string, seq: number) =>
        phone.message({ handle: { conversationId, seq } });
      let wrongHandle: Label;
      try {
        wrongHandle = (await lookup(direct, 3)).handle.conversationId;
      } catch (error) {
        if (!(error instanceof QqPhoneError) || error.category !== "not_found") throw error;
        wrongHandle = null;
      }
      await append(incoming("MESSAGE_CREATE", "late-source"));
      const replay = await older(), fresh = await phone.history({ conversationId: channel });
      gate("H", [
        ["H1 cross-room matching ID is not a target", null, target(oldPage.messages[0])],
        ["H2 same-room quote", "qq:channel:lab#3", target(await lookup(channel, 4))],
        ["H3 seq in wrong conversation", null, wrongHandle],
        [
          "H4 first old page",
          "11:1,3",
          `${oldPage.snapshotThroughSeq}:${
            oldPage.messages.map((message) => message.handle.seq).join(",")
          }`,
        ],
        [
          "H5 replayed old page",
          "11:1,3",
          `${replay.snapshotThroughSeq}:${
            replay.messages.map((message) => message.handle.seq).join(",")
          }`,
        ],
        ["H6 evidence visible at old watermark", null, target(replay.messages[0])],
        [
          "H7 new page watermark and tail",
          "12:12",
          `${fresh.snapshotThroughSeq}:${fresh.messages.at(-1)?.handle.seq}`,
        ],
        [
          "H8 same-room source now available",
          "qq:channel:lab#12",
          target(await lookup(channel, 1)),
        ],
      ]);
    } finally {
      await reading.close();
      await timeline.close();
      await actions.close();
    }
  }));

Deno.test("accuracy D: original action receipts, not intent or queue settlement, are evidence of sending", () => {
  const actionId = "fixture-action";
  const success: SendStatus = {
    actionId,
    status: "finished",
    result: { status: "sent", messageId: "platform-id" },
  };
  const failed: SendStatus = {
    actionId,
    status: "finished",
    result: { status: "failed", attempted: true, error: { category: "platform" } },
  };
  const unknown: SendStatus = {
    actionId,
    status: "finished",
    result: { status: "unknown", attempted: true, error: { category: "transport" } },
  };
  const interrupted: SendStatus = { actionId, status: "unknown", reason: "interrupted" };
  const retained: SendStatus = {
    actionId,
    status: "cancelled",
    resolution: {
      schema: 1,
      actionId,
      actor: "eve",
      kind: "unknown_retained",
      fingerprint: "a".repeat(64),
      receiptDigest: "b".repeat(64),
      resolvedAt: new Date(clock).toISOString(),
    },
    originalResult: { status: "unknown", attempted: true, error: { category: "transport" } },
  };
  const receipts = [success, failed, unknown, interrupted, retained].map(phoneReceipt);
  const close = (status: SendStatus): Label => {
    const policy = { pageSize: 8, previewCharacters: 80, attachmentPageSize: 2 };
    const base = {
      conversationId: channel,
      sender: { platformUserId: "fixture", displayName: "同名" },
    };
    const screen = historyScreen(
      [
        { ...base, seq: 1, direction: "incoming", text: "是哪一间？" },
        { ...base, seq: 2, direction: "outgoing", text: "北厅。", delivery: phoneReceipt(status) },
      ],
      channel,
      policy,
      clock,
    );
    const args = conversationArgs({
      expectedSeq: 0,
      conversationId: channel,
      throughSeq: 2,
      mode: "close",
      summary: "已答",
      nextOpening: "",
      pending: [],
    });
    try {
      return updateConversation([], args, screen, clock)[0].mode;
    } catch (error) {
      if (error instanceof KernelError && error.kind === "rejected") return null;
      throw error;
    }
  };
  gate("D", [
    [
      "D1 platform accepted",
      "sent:platform-id",
      `${receipts[0].state}:${receipts[0].platformMessageId}`,
    ],
    ["D2 explicit failure", "failed", receipts[1].state],
    ["D3 transport outcome unknown", "unknown", receipts[2].state],
    ["D4 interrupted, no final receipt", "unknown", receipts[3].state],
    [
      "D5 resolving a queue does not settle delivery",
      "cancelled:unknown_retained",
      `${receipts[4].state}:${receipts[4].resolution?.kind}`,
    ],
    ["D6 no invented platform ID", null, receipts[2].platformMessageId ?? null],
    ["D7 sent reply can close", "close", close(success)],
    ["D8 failed reply cannot close", null, close(failed)],
    ["D9 unknown reply cannot close", null, close(unknown)],
    ["D10 retained uncertainty cannot close", null, close(retained)],
  ]);
});

Deno.test("accuracy S: grounding requires actual matching, non-rolled-back fact records", () =>
  local(async (directory) => {
    const records = new FactJournal(`${directory}/records`),
      memory = new MemoryStore(`${directory}/memory`, records);
    try {
      await records.initialize();
      await memory.initialize();
      const source = await records.append("notice", { text: "合成场馆公告" });
      const withdrawn = await records.append("notice", { text: "已撤回的候选记录" });
      await records.append("upgrade_rollback", { afterSeq: 1, throughSeq: 2 });
      const original = { recordSeq: 1, digest: source.digest, kind: "heard" };
      const remember = async (id: string, sources: unknown): Promise<Label> => {
        const before = memory.journal.events.length;
        try {
          const saved = await memory.remember({
            text: "合成记忆",
            sources: sources as MemorySource[],
          }, id);
          assert.equal(memory.journal.events.length, before + 1);
          return saved.sources.map((item) => item.kind).join("+");
        } catch (error) {
          if (!(error instanceof KernelError) || error.kind !== "rejected") throw error;
          assert.equal(
            memory.journal.events.length,
            before,
            "rejection must not append a formation",
          );
          return null;
        }
      };
      const inputs: [string, Label, unknown][] = [
        ["S1 heard stays attributed", "heard", [original]],
        ["S2 imagined not relabelled as experience", "imagined", [{
          ...original,
          kind: "imagined",
        }]],
        ["S3 no source", null, []],
        ["S4 wrong digest", null, [{ ...original, digest: "0".repeat(64) }]],
        ["S5 future seq with a real digest", null, [{ ...original, recordSeq: 99 }]],
        ["S6 missing record AND missing digest", null, [{ recordSeq: 99, kind: "heard" }]],
        ["S7 quarantined", null, [{
          ...original,
          recordSeq: withdrawn.seq,
          digest: withdrawn.digest,
        }]],
        ["S8 one source cannot vouch for another", null, [original, {
          ...original,
          recordSeq: 99,
        }]],
      ];
      const cases: Case[] = [];
      for (const [id, gold, sources] of inputs) cases.push([id, gold, await remember(id, sources)]);
      gate("S", cases);
    } finally {
      await memory.close();
      await records.close();
    }
  }));

Deno.test("accuracy C: an explicit correction updates only its subject; original revision and source survive", () =>
  local(async (directory) => {
    const notes = new NoteStore(directory);
    try {
      await notes.initialize();
      const source = (description: string) => ({
        kind: "heard",
        at: new Date(clock).toISOString(),
        description,
      });
      const before = await notes.change("create", {
        title: "小林的场次",
        content: "周六北厅",
        sources: [source("qq:channel:lab#3")],
      }, "original");
      const other = await notes.change("create", {
        title: "小林的场次",
        content: "周日西台",
        sources: [source("qq:guild-dm:lab#2")],
      }, "namesake");
      const correction = {
        id: before.id,
        expectedRevision: 1,
        content: "周六东厅",
        sources: [source("qq:channel:lab#12")],
      };
      await notes.change("update", correction, "correction");
      let stale: Label;
      try {
        stale =
          (await notes.change("update", { ...correction, content: "周六北厅" }, "stale")).content;
      } catch (error) {
        if (!(error instanceof KernelError) || error.kind !== "rejected") throw error;
        stale = null;
      }
      const latest = notes.get({ id: before.id }),
        first = notes.get({ id: before.id, revision: 1 });
      gate("C", [
        ["C1 corrected current revision", "2:周六东厅", `${latest.note.revision}:${latest.text}`],
        ["C2 original stays original", "1:周六北厅", `${first.note.revision}:${first.text}`],
        ["C3 namesake untouched", "周日西台", notes.get({ id: other.id }).text],
        ["C4 stale overwrite", null, stale],
        ["C5 new provenance", "qq:channel:lab#12", latest.note.sources[0].description],
        ["C6 original provenance", "qq:channel:lab#3", first.note.sources[0].description],
      ]);
    } finally {
      await notes.close();
    }
  }));

Deno.test("accuracy T: strict per-plan confirmation; only a local, simulated GET can be observed", () =>
  local(async (directory) => {
    const calls: string[] = [];
    const invoke = createQqPlatformOperator({
      directory,
      access: "admin",
      request: (method, path) => {
        calls.push(`${method} ${path}`);
        assert.equal(method, "GET");
        return Promise.resolve({ httpStatus: 200, data: { items: [] } });
      },
    });
    const raw = { operation: "listMediaImages", params: {} };
    let challenge = "";
    const preview = await invoke(raw, (plan) => {
      challenge = `${plan.operation} ${plan.digest.slice(0, 12)}`;
      return false;
    });
    assert.deepEqual(preview, { status: "failed", reason: "confirmation_required" });
    assert.equal(calls.length, 0);
    const terminal = (answer: string | null, interactive = true) =>
      terminalConfirmation("c".repeat(64), () => interactive, () => answer);
    async function observed(
      confirm?: QqPlatformConfirm,
      input: unknown = raw,
      denied = "confirmation_required",
    ): Promise<Label> {
      const before = calls.length, result = await invoke(input, confirm);
      if (result.status === "ok") {
        assert.equal(calls.length, before + 1);
        return calls[before];
      }
      assert.equal(calls.length, before, "no request on denial");
      if (result.status !== "failed" || result.reason !== denied) {
        throw new Error(`unexpected tool result: ${JSON.stringify(result)}`);
      }
      return null;
    }
    gate("T", [
      ["T1 missing callback", null, await observed()],
      [
        "T2 JSON cannot authorize",
        null,
        await observed(undefined, { ...raw, confirmed: true }, "invalid_params"),
      ],
      [
        "T3 truthy string is not boolean true",
        null,
        await observed((() => "true") as unknown as QqPlatformConfirm),
      ],
      ["T4 no terminal", null, await observed(terminal(challenge, false))],
      ["T5 near-miss challenge", null, await observed(terminal(`${challenge} `))],
      ["T6 cancelled", null, await observed(terminal(null))],
      [
        "T7 another plan's response",
        null,
        await observed(terminal(challenge), { ...raw, params: { query: { cursor: "different" } } }),
      ],
      [
        "T8 exact interactive challenge",
        "GET /v2/media-images",
        await observed(terminal(challenge)),
      ],
    ]);
  }, true));
