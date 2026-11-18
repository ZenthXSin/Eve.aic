import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { QqSendActions } from "../adapters/qq/actions.ts";
import { QqClient } from "../adapters/qq/client.ts";
import { QqGroupNames } from "../adapters/qq/contacts.ts";
import { QqPhoneDevice } from "../adapters/qq/device.ts";
import { QqPhoneDownloads } from "../adapters/qq/downloads.ts";
import { type QqDispatch, qqMessage } from "../adapters/qq/inbound.ts";
import { type InboundRecord, QqInboundJournal } from "../adapters/qq/journal.ts";
import { QqPhone } from "../adapters/qq/phone.ts";
import { QqPhoneReading } from "../adapters/qq/reading.ts";
import { QqTimeline } from "../adapters/qq/timeline.ts";
import type { HistoryCursor } from "../contracts/phone.ts";
import { canonical } from "../kernel/calls.ts";
import type { Json } from "../kernel/protocol.ts";

const USAGE = `从各待测树根目录运行；使用同一份脚本、Deno、/tmp 文件系统和 profile：
  DENO_NO_UPDATE_CHECK=1 /usr/local/lib/eve-rebuild/deno-2.9.6 run --no-config --no-lock --no-remote --no-npm --no-prompt --deny-net --allow-read=/tmp --allow-write=/tmp deployment/bench-qq-read.ts [--smoke] [--expect=SHA256] [--require-fast-path]
A 记录 correctnessSha256；B 加 --require-fast-path --expect=<A 的值>。--smoke 仅作小样本自检。
全部入站均为合成事实，出站为空。种子在私有临时目录批量写普通 JSON，不逐条 fsync；真实 journal 恢复并校验。
之后少量真实 append；只在读出的副本上固定追加记录的 receivedAt，以消除 append 取墙钟的差异。
旧版走全量回调，新版与 service.ts 一样传 journal；sourceReads 统计真实日志调用。冷启动、预热不计时。
每次追加后的首次读分别由 notifications、history 承担。p50/p95 为逐调用最近秩（ms），totalReadMs 含读触发的索引写入。
measuredLoopWallMs 还包括计时循环内的入站追加和断言。workloadSha256 校验事实及请求轨迹，correctnessSha256 同时校验完整 JSON 返回。`;
const VERSION = "qq-read-v1";
const ROOMS = 24, PAGE = 8, WARM_PAIRS = 2;
const EPOCH = Date.UTC(2026, 0, 1), NOW = EPOCH + 86_400_000;
const SELF = { id: "bench-bot", displayName: "合成机器人" };
const normal = { steadyRows: 15_000, steadyPairs: 24, appendPairs: 20 };
const smoke = { steadyRows: 480, steadyPairs: 2, appendPairs: 2 };
type Phase = "warmup" | "steady" | "append" | "check";

function options(args: string[]) {
  const expect = args.filter((arg) => arg.startsWith("--expect="));
  if (
    args.some((arg) =>
      arg !== "--smoke" && arg !== "--require-fast-path" && !/^--expect=[0-9a-f]{64}$/.test(arg)
    ) || expect.length > 1 || new Set(args).size !== args.length
  ) throw new Error(USAGE);
  return {
    profile: args.includes("--smoke") ? smoke : normal,
    requireFastPath: args.includes("--require-fast-path"),
    expected: expect[0]?.slice("--expect=".length),
  };
}

function digest(value: Json) {
  return createHash("sha256").update(canonical(value)).digest("hex");
}

function synthetic(seq: number): InboundRecord {
  const room = (seq - 1) % ROOMS, user = room % 2 === 0;
  const peer = `bench-${room}`, sender = user ? peer : `member-${seq % 7}`;
  const at = new Date(EPOCH + seq * 1000).toISOString(), mention = !user && seq % 11 === 0;
  const raw: QqDispatch = {
    op: 0,
    s: seq,
    t: user ? "C2C_MESSAGE_CREATE" : "GROUP_MESSAGE_CREATE",
    d: {
      id: `msg-${seq}`,
      timestamp: at,
      content: `${mention ? `<@${SELF.id}> ` : ""}合成消息 ${seq} 🌱` +
        (seq % 17 === 0 ? "；这是合成长文本".repeat(32) : ""),
      author: { [user ? "user_openid" : "member_openid"]: sender, username: `昵称-${sender}` },
      ...(user ? {} : { group_openid: peer, group_name: `群-${room}` }),
      ...(mention ? { mentions: [{ member_openid: SELF.id, scope: "single", is_you: true }] } : {}),
      message_scene: {
        ext: [
          `msg_idx=idx-${seq}`,
          ...(seq > ROOMS && seq % 7 === 0 ? [`ref_msg_idx=idx-${seq - ROOMS}`] : []),
        ],
      },
      ...(seq % 47 === 0
        ? { attachments: [{ content_type: "image/png", filename: `图-${seq}.png`, size: 512 }] }
        : {}),
    },
  };
  const message = qqMessage(raw)!;
  return {
    schema: 1,
    projectionVersion: 2,
    seq,
    identity: digest(message.deduplication),
    receivedAt: at,
    raw,
    message,
  };
}

const ms = (value: number) => Number(value.toFixed(6));
function summarize(samples: number[]) {
  const ordered = samples.toSorted((left, right) => left - right);
  const percentile = (fraction: number) => ms(ordered[Math.ceil(ordered.length * fraction) - 1]);
  return {
    calls: ordered.length,
    p50Ms: percentile(0.50),
    p95Ms: percentile(0.95),
    totalMs: ms(samples.reduce((total, sample) => total + sample, 0)),
  };
}

type JournalReads = {
  records(afterSeq?: number): InboundRecord[];
  record?(seq: number): InboundRecord | undefined;
};
const JournalBase: new (directory: string) => QqInboundJournal & JournalReads = QqInboundJournal;
const journalInput = "record" in QqInboundJournal.prototype && "sync" in QqTimeline.prototype;
const counts = () => ({ fullReads: 0, suffixReads: 0, rowsFromLists: 0, pointReads: 0 });

class ReplayJournal extends JournalBase {
  reads = counts();

  constructor(directory: string, readonly initialRows: number) {
    super(directory);
  }

  override records(afterSeq = 0) {
    const rows = super.records(afterSeq);
    if (afterSeq === 0) this.reads.fullReads++;
    else this.reads.suffixReads++;
    this.reads.rowsFromLists += rows.length;
    for (let offset = Math.max(0, this.initialRows - afterSeq); offset < rows.length; offset++) {
      rows[offset].receivedAt = rows[offset].message.message.sentAt!;
    }
    return rows;
  }

  override record(seq: number) {
    this.reads.pointReads++;
    const row = super.record!(seq);
    if (row && seq > this.initialRows) row.receivedAt = row.message.message.sentAt!;
    return row;
  }
}

async function run(
  directory: string,
  { profile, expected, requireFastPath }: ReturnType<typeof options>,
) {
  if (requireFastPath && !journalInput) throw new Error("本树没有生产 journal 增量读接口");
  const { steadyRows, steadyPairs, appendPairs } = profile;
  const initialRows = steadyRows - 2 * WARM_PAIRS;
  const journal = new ReplayJournal(`${directory}/inbound`, initialRows);
  await Deno.mkdir(journal.directory, { mode: 0o700 });
  const additions: InboundRecord[] = [];
  const fixtureHash = createHash("sha256"), byRoom = new Map<string, number[]>();
  for (let seq = 1; seq <= steadyRows + 2 * appendPairs; seq++) {
    const record = synthetic(seq);
    fixtureHash.update(canonical(record as unknown as Json)).update("\n");
    const id = record.message.message.conversationId;
    if (!byRoom.has(id)) byRoom.set(id, []);
    byRoom.get(id)!.push(record.seq);
    if (record.seq <= initialRows) {
      const name = `${String(record.seq).padStart(16, "0")}-${record.identity}.json`;
      Deno.writeTextFileSync(`${journal.directory}/${name}`, JSON.stringify(record), {
        createNew: true,
        mode: 0o600,
      });
    } else additions.push(record);
  }
  assert.equal(byRoom.size, ROOMS);
  const rooms = [...byRoom.keys()];
  const fixtureSha256 = fixtureHash.digest("hex");
  await journal.initialize();
  let visible = initialRows;
  async function publish() {
    const fixture = additions[visible - initialRows];
    const { fresh, record } = await journal.append(fixture.message, fixture.raw);
    assert.ok(fresh);
    assert.deepEqual({ ...record, receivedAt: fixture.receivedAt }, fixture);
    visible++;
  }
  const offline = () => {
    throw new Error("读取基准不应调用平台");
  };
  const client = new QqClient({ appId: "bench", appSecret: "synthetic" }, {
    httpsTransport: offline,
  });
  const actions = new QqSendActions(client, `${directory}/outbox`);
  const timeline = new QqTimeline(`${directory}/timeline`);
  const reading = new QqPhoneReading(`${directory}/reading`);
  const device = new QqPhoneDevice(`${directory}/device`);
  const downloads = new QqPhoneDownloads(`${directory}/downloads`);
  const source = journalInput ? journal : () => journal.records();
  const stores = [actions, timeline, reading, device, downloads];
  await using cleanup = new AsyncDisposableStack();
  for (const store of stores) {
    await store.initialize();
    cleanup.defer(() => store.close());
  }
  const phone = new QqPhone(
    source as ConstructorParameters<typeof QqPhone>[0],
    actions,
    timeline,
    SELF,
    reading,
    () => NOW,
    device,
    downloads,
    true,
    new QqGroupNames(offline),
  );
  const samples: Record<string, number[]> = {};
  const requests: Json[] = [], answers: string[] = [];
  async function call<T>(phase: Phase, name: string, args: object, read: () => Promise<T>) {
    requests.push([phase, name, visible, JSON.parse(JSON.stringify(args))]);
    const started = performance.now();
    const result = await read();
    const elapsed = performance.now() - started;
    if (phase === "steady" || phase === "append") {
      (samples[`${phase}.${name}`] ??= []).push(elapsed);
    }
    answers.push(JSON.stringify(result));
    return result;
  }

  let cursor = initialRows - ROOMS;
  async function notifications(phase: Phase, name: string) {
    const args = { afterSeq: cursor };
    const result = await call(
      phase,
      `notifications.${name}`,
      args,
      () => phone.notifications(args),
    );
    const notices: number[] = [];
    for (let seq = cursor + 1; seq <= visible; seq++) {
      if (seq % 2 === 1 || seq % 11 === 0) notices.push(seq);
    }
    const page = notices.slice(0, PAGE);
    assert.deepEqual(result.items.map((item) => item.handle.seq), page);
    assert.equal(result.hasMore, notices.length > PAGE);
    assert.equal(result.throughSeq, notices.length > PAGE ? page.at(-1) : visible);
    cursor = result.throughSeq;
  }

  async function history(phase: Phase, conversationId: string, older?: HistoryCursor) {
    const args = { conversationId, ...(older ? { cursor: older } : {}) };
    const result = await call(
      phase,
      `history.${older ? "older" : "latest"}`,
      args,
      () => phone.history(args),
    );
    const snapshot = older?.snapshotThroughSeq ?? visible;
    const eligible = byRoom.get(conversationId)!.filter((seq) =>
      seq <= snapshot && (!older || seq < older.beforeSeq)
    );
    const page = eligible.slice(-PAGE);
    assert.deepEqual(
      result.messages.map((item) => item.handle),
      page.map((seq) => ({ conversationId, seq })),
    );
    assert.equal(result.snapshotThroughSeq, snapshot);
    assert.equal(result.readThroughSeq, page.at(-1));
    assert.deepEqual(
      result.older,
      eligible.length > PAGE ? { snapshotThroughSeq: snapshot, beforeSeq: page[0] } : undefined,
    );
    return result.older;
  }

  const roomAt = (seq: number) => rooms[(seq - 1) % ROOMS];
  async function steadyPair(phase: Phase, index: number) {
    assert.equal(cursor, visible);
    const room = roomAt(1 + index % ROOMS);
    await notifications(phase, "emptyTail");
    const older = await history(phase, room);
    assert.ok(older);
    await notifications(phase, "emptyTail");
    await history(phase, room, older);
    return older;
  }
  async function appendPair(phase: Phase) {
    await publish();
    await notifications(phase, "delta");
    await publish();
    await history(phase, roomAt(visible));
  }

  while (cursor < visible) await notifications("warmup", "catchup");
  for (let index = 0; index < WARM_PAIRS; index++) {
    await steadyPair("warmup", index);
    await appendPair("warmup");
    await notifications("warmup", "catchup");
  }
  assert.equal(visible, steadyRows);
  const warmupCalls = requests.length;
  journal.reads = counts();
  let pinned: HistoryCursor | undefined;
  const start = performance.now();
  for (let index = 0; index < steadyPairs; index++) {
    const older = await steadyPair("steady", index);
    pinned ??= older;
  }
  const steady = journal.reads;
  journal.reads = counts();
  for (let index = 0; index < appendPairs; index++) await appendPair("append");
  const measuredLoopWallMs = ms(performance.now() - start);
  const sourceReads = { steady, append: journal.reads };
  journal.reads = counts();
  assert.ok(pinned);
  await history("check", roomAt(1), pinned);

  const workloadSha256 = digest([VERSION, NOW, fixtureSha256, requests]);
  const correctnessSha256 = digest([workloadSha256, answers.map((answer) => JSON.parse(answer))]);
  if (expected) assert.equal(correctnessSha256, expected, "A/B 输入、调用轨迹或返回内容不一致");
  if (requireFastPath) {
    assert.ok(
      steady.suffixReads > 0 && steady.pointReads > 0 && steady.fullReads === 0 &&
        steady.rowsFromLists === 0,
      "稳态未命中生产 journal 增量读路径，不报告 B 的性能",
    );
    assert.equal(sourceReads.append.rowsFromLists, 2 * appendPairs, "追加时没有仅从日志取新增事实");
  }
  return {
    benchmark: VERSION,
    runtime: Deno.version.deno,
    profile: profile === smoke ? "smoke" : "normal",
    rooms: ROOMS,
    initialRows,
    ...profile,
    appendedRows: visible - steadyRows,
    warmupCalls,
    fixtureSha256,
    workloadSha256,
    correctnessSha256,
    sourceMode: journalInput ? "journal" : "callback",
    sourceReads,
    metrics: Object.fromEntries(
      Object.entries(samples).map(([name, times]) => [name, summarize(times)]),
    ),
    totalReadMs: ms(Object.values(samples).flat().reduce((total, sample) => total + sample, 0)),
    measuredLoopWallMs,
  };
}

if (import.meta.main) {
  if (Deno.args.length === 1 && Deno.args[0] === "--help") {
    console.log(USAGE);
  } else {
    const settings = options(Deno.args);
    const directory = await Deno.makeTempDir({ dir: "/tmp", prefix: "eve-bench-qq-read-" });
    try {
      console.log(JSON.stringify(await run(directory, settings), null, 2));
    } finally {
      await Deno.remove(directory, { recursive: true });
    }
  }
}
