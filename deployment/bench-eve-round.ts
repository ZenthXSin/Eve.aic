import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { QqSendActions } from "../adapters/qq/actions.ts";
import { QqClient } from "../adapters/qq/client.ts";
import { QqPhoneDevice } from "../adapters/qq/device.ts";
import { type QqDispatch, qqMessage } from "../adapters/qq/inbound.ts";
import { type InboundRecord, QqInboundJournal } from "../adapters/qq/journal.ts";
import { QqPhone } from "../adapters/qq/phone.ts";
import { QqPhoneReading } from "../adapters/qq/reading.ts";
import { QqTimeline } from "../adapters/qq/timeline.ts";
import type { ModelRequest, ModelResponse } from "../contracts/model.ts";
import { canonical } from "../kernel/calls.ts";
import type { Json } from "../kernel/protocol.ts";
import { EveRuntime } from "../plugins/eve/runtime.ts";
import { createMindControls } from "../plugins/mind-controls/mod.ts";
import { SCRIPT_TOOL } from "../plugins/script-runtime/mod.ts";
import { ScriptRuntime } from "../plugins/script-runtime/runtime.ts";
import type { FactEvent } from "../sdk/fact-journal.ts";
import type { CallOptions, Handler, PluginContext } from "../sdk/plugin.ts";
import { RuntimeEngine } from "../services/runtime/engine.ts";

const USAGE = `Offline Eve round benchmark. From each source root, with the identical script:
  DENO_NO_UPDATE_CHECK=1 DENO_DIR=$cache /usr/local/lib/eve-rebuild/deno-2.9.6 run --cached-only --no-remote --frozen --no-prompt --deny-net --deny-run --allow-read="$PWD,$scratch" --allow-write="$scratch" deployment/bench-eve-round.ts --scratch="$scratch" [--smoke] [--expect=SHA256]
Use a private existing scratch directory on the same local filesystem and a private Deno cache
with the locked dependency already cached. The script deletes only its own new subdirectory.
Copy it into git archive 2d681834 and a fixed working-tree snapshot. Run sequential fresh
processes (A B B A), same binary and options; pass A's equivalenceSha256 to subsequent runs.
One empty-mirror start, one persisted-mirror reopen, two warmup turns, then paired empty/delta
turns. No OS cache flush. Synthetic wall clock for data, real performance.now() for all timing.
One real pulse to Eve's model.complete and through RuntimeEngine admission to mock model entry.
Post-entry receipt commits and the following native-tool-only pulse are separate measurements.
Nearest-rank percentiles use individual measured rounds. prepReads counts QQ source reads and
synchronous mirror body reads before Eve calls the model, not OS cache misses. Runtime phase
timers can overlap. Compare exact JSON bytes at model entry and per-caller/method arguments and
results in call order. Only timeline timing fields and their derived hash-chain/action IDs are
excluded from the latter; the real engine and mirror still verify the original commits.
No HTTP, ownership fence, provider, transport, subprocess, deliberate delay or semantic server.`;

const EPOCH = Date.UTC(2026, 0, 4, 12), SOURCE = "b".repeat(40), BUDGET = 200_000_000;
const PROMPT = "Offline synthetic Eve fixture. Read the notebook; use only the supplied tools.";
const NORMAL = { turns: 128, qqRows: 8192, pairs: 20, paragraphs: 128 };
const SMOKE = { turns: 12, qqRows: 64, pairs: 2, paragraphs: 4 };
type Profile = typeof NORMAL;
type Dict = Record<string, Json>;
const asJson = (value: unknown) => value as Json;
const sha = (value: string | Uint8Array) => createHash("sha256").update(value).digest("hex");
const digest = (value: unknown) => sha(canonical(asJson(value)));
const ms = (value: number) => Number(value.toFixed(4));
const mono = () => performance.now();
function summary(samples: number[]) {
  assert.ok(samples.length);
  const sorted = samples.toSorted((left, right) => left - right);
  const rank = (percent: number) => ms(sorted[Math.ceil(sorted.length * percent) - 1]);
  return { n: sorted.length, p50Ms: rank(.50), p95Ms: rank(.95) };
}

let wall = EPOCH;
function useFixtureClock() {
  const original = globalThis.Date;
  globalThis.Date = new Proxy(original, {
    get: (target, key, receiver) => key === "now" ? () => wall : Reflect.get(target, key, receiver),
    construct: (target, args, ctor) => Reflect.construct(target, args.length ? args : [wall], ctor),
    apply: () => new original(wall).toString(),
  });
  return () => {
    globalThis.Date = original;
  };
}

function incoming(seq: number, at: string) {
  const room = (seq - 1) % 16;
  const raw: QqDispatch = {
    op: 0,
    s: seq,
    t: "C2C_MESSAGE_CREATE",
    d: {
      id: `bench-message-${seq}`,
      timestamp: at,
      content: `botany garden / 合成观察 ${seq}`,
      author: { user_openid: `bench-peer-${room}`, username: `Peer ${room}` },
    },
  };
  const message = qqMessage(raw)!;
  const record: InboundRecord = {
    schema: 1,
    projectionVersion: 2,
    seq,
    receivedAt: at,
    identity: digest(message.deduplication),
    message,
    raw,
  };
  return record;
}

async function seed(root: string, profile: Profile) {
  const hash = createHash("sha256"), kinds: Record<string, number> = {};
  function file(path: string, text: string) {
    hash.update(path).update("\0").update(text).update("\0");
    Deno.writeTextFileSync(`${root}/${path}`, text, { createNew: true, mode: 0o600 });
  }
  function chain(ledger: string) {
    const path = `engine/${ledger}/events`;
    Deno.mkdirSync(`${root}/${path}`, { recursive: true });
    let previous = "0".repeat(64), seq = 0;
    return (kind: string, data: Json) => {
      seq++;
      const body = {
        schema: 1,
        seq,
        at: new Date(EPOCH - 86_400_000 + seq * 1000).toISOString(),
        kind,
        data,
        previous,
      };
      previous = digest(body);
      file(
        `${path}/${String(seq).padStart(16, "0")}.json`,
        JSON.stringify({ ...body, digest: previous }) + "\n",
      );
      kinds[`${ledger}.${kind}`] = (kinds[`${ledger}.${kind}`] ?? 0) + 1;
      return { seq, digest: previous };
    };
  }
  file("prompt", PROMPT);
  await Deno.mkdir(`${root}/mind`);
  file(
    "mind/index.json",
    JSON.stringify({
      schema: 1,
      events: [],
      alarms: [],
      facts: null,
      unreadReminder: { nextCheck: EPOCH },
    }) + "\n",
  );
  file(
    "settings.json",
    JSON.stringify({ dailyTokenBudget: BUDGET, paused: false, timeZone: "UTC" }) + "\n",
  );
  const fact = chain("consciousness"),
    first = fact("activation", { sourceCommit: SOURCE, reason: "fixture" });
  let through = 0, compressedThrough = 0;
  for (let turn = 1; turn <= profile.turns; turn++) {
    const seq = Math.floor(turn * profile.qqRows / profile.turns);
    fact("notice", {
      source: "phone",
      throughSeq: seq,
      items: [{
        text: `botany: historical note ${turn}`,
        handle: { conversationId: `qq:user:bench-peer-${(seq - 1) % 16}`, seq },
      }],
    });
    const requestId = `seed_${turn}`,
      call = { id: `seed-call-${turn}`, name: "execute_script", args: { code: `return ${turn}` } };
    fact("model_start", { requestId, day: "2026-01-03", sourceCommit: SOURCE, reservation: 8192 });
    const response = fact("model_response", {
      requestId,
      sourceCommit: SOURCE,
      response: {
        content: `Garden ${turn}: ` + "合成样本记录，观察花蕾和土壤。".repeat(profile.paragraphs),
        toolCalls: [call],
      },
    });
    const callId = `eve_${response.seq}_0`;
    fact("tool_start", {
      responseSeq: response.seq,
      index: 0,
      callId,
      call: { ...call, id: callId },
    });
    through = fact("tool_result", {
      callId,
      result: {
        status: "completed",
        output: `botany sample ${turn}: ` + "notebook data. ".repeat(80),
      },
    }).seq;
    if (turn === profile.turns / 2) {
      compressedThrough = through;
      fact("compression", {
        throughSeq: through,
        source: "eve",
        summary: "First half of the synthetic garden notebook.",
      });
    }
  }
  const noteId = digest(["note-v1", "seed-note"]);
  chain("notes")("note", {
    at: new Date(EPOCH - 86_400_000 + 1000).toISOString(),
    id: noteId,
    requestId: "seed-note",
    recordSeq: 1,
    revision: 1,
    contextRecordSeq: 1,
    alias: "想弄明白的事",
    title: "notebook",
    group: "",
    summary: "",
    content: "botany: check the garden",
    sources: [{
      kind: "imagined",
      at: new Date(EPOCH - 86_400_000).toISOString(),
      description: "synthetic",
    }],
  });
  chain("memory")("formation", {
    id: digest(["memory-v1", "seed-memory"]),
    requestId: "seed-memory",
    text: "botany garden notebook",
    entities: ["botany", "garden"],
    contextRecordSeq: 1,
    sources: [{ kind: "imagined", recordSeq: first.seq, digest: first.digest }],
  });
  await Deno.mkdir(`${root}/inbound`);
  for (let seq = 1; seq <= profile.qqRows; seq++) {
    const row = incoming(seq, new Date(EPOCH - 86_400_000 + seq * 1000).toISOString());
    file(
      `inbound/${String(seq).padStart(16, "0")}-${row.identity}.json`,
      JSON.stringify(row) + "\n",
    );
  }
  return { sha256: hash.digest("hex"), consciousnessFacts: through, compressedThrough, kinds };
}

const journalInput = "record" in QqInboundJournal.prototype && "sync" in QqTimeline.prototype;
type ReadSource = {
  records(afterSeq?: number): InboundRecord[];
  record?(seq: number): InboundRecord | undefined;
};
const JournalBase: new (dir: string) => QqInboundJournal & ReadSource = QqInboundJournal;
const readCounts = () => ({
  qqFull: 0,
  qqSuffix: 0,
  qqRows: 0,
  qqPoint: 0,
  mirrorCompressed: 0,
  mirrorRetained: 0,
  mirrorNew: 0,
});
let reads = readCounts();
class CountedJournal extends JournalBase {
  override records(afterSeq = 0) {
    const rows = super.records(afterSeq);
    if (afterSeq === 0) reads.qqFull++;
    else reads.qqSuffix++;
    reads.qqRows += rows.length;
    return rows;
  }
  override record(seq: number) {
    reads.qqPoint++;
    return super.record!(seq);
  }
}
function countMirrorReads(root: string, fixture: Awaited<ReturnType<typeof seed>>) {
  const prefix = `${root}/eve/remote-mirror/events/`, original = Deno.readTextFileSync;
  Deno.readTextFileSync = (path) => {
    if (typeof path === "string" && path.startsWith(prefix)) {
      const seq = Number(path.slice(prefix.length, -5));
      if (seq <= fixture.compressedThrough) reads.mirrorCompressed++;
      else if (seq <= fixture.consciousnessFacts) reads.mirrorRetained++;
      else reads.mirrorNew++;
    }
    return original(path);
  };
  return () => {
    Deno.readTextFileSync = original;
  };
}

type Entry = {
  key: string;
  args: Json;
  options: CallOptions | null;
  result?: Json;
  error?: string;
};
function semanticData(kind: string, data: Dict) {
  if (kind === "model_start") {
    const { runtimeTiming: _timing, ...rest } = data;
    return rest;
  }
  if (kind === "tool_result") {
    const { executionMs: _elapsed, ...rest } = data;
    return rest;
  }
  return data;
}
function semanticEvent(event: FactEvent) {
  return {
    schema: event.schema,
    seq: event.seq,
    at: event.at,
    kind: event.kind,
    data: semanticData(event.kind, event.data as Dict),
  };
}
class Trace {
  entries: Entry[] = [];
  eveModelAt?: number;
  providerAt?: number;
  prepReads?: ReturnType<typeof readCounts>;
  request?: ModelRequest;
  verify() {
    const endpoints: Record<string, string[]> = {};
    for (const entry of this.entries) {
      assert.equal(entry.error, undefined, entry.key + ": " + entry.error);
      let { args, result } = entry;
      if (entry.key === "eve.timeline.append") {
        const { kind, data, expectedSeq } = args as Dict;
        const event = result as unknown as FactEvent;
        assert.equal(event.seq, Number(expectedSeq) + 1);
        assert.equal(event.previous, (args as Dict).expectedPrevious);
        assert.equal(event.kind, kind);
        args = { kind, expectedSeq, data: semanticData(String(kind), data as Dict) };
        result = semanticEvent(event);
      } else if (entry.key === "eve.timeline.page") {
        const page = result as Dict;
        result = { ...page, items: (page.items as unknown as FactEvent[]).map(semanticEvent) };
      }
      (endpoints[entry.key] ??= []).push(digest([args, entry.options, result]));
    }
    return {
      calls: Object.fromEntries(
        Object.entries(endpoints).map(([key, items]) => [key, items.length]),
      ),
      ioSha256: digest(endpoints),
    };
  }
}

type Stage = "first" | "reopen" | "warmup" | "empty" | "delta";
type Round = Awaited<ReturnType<typeof round>>;
type State = { trace: Trace; ordinal: number; visible: number; readId: number };
const engineCalls = new Set([
  "timeline.page",
  "timeline.append",
  "model.budget",
  "model.complete",
  "commands.completions",
  "agents.notifications",
  "notes.self",
  "memory.recall",
]);
async function openRig(root: string, state: State, cleanup: AsyncDisposableStack) {
  const timings: Record<string, number> = {};
  async function timed(name: string, action: () => Promise<void>) {
    const start = mono();
    await action();
    timings[name] = ms(mono() - start);
  }
  const engine = new RuntimeEngine({
    directory: `${root}/engine`,
    workspace: `${root}/workspace`,
    settingsFile: `${root}/settings.json`,
    maxOutputTokens: 128,
    model: {
      complete(request) {
        const at = mono(), trace = state.trace;
        assert.equal(trace.providerAt, undefined);
        trace.providerAt = at;
        trace.request = request;
        const response: ModelResponse = {
          content: `Synthetic garden turn ${state.ordinal}.`,
          toolCalls: [{
            id: `mock_${state.ordinal}`,
            name: "write_thought",
            args: { thought: `botany note ${state.ordinal}` },
          }],
        };
        return Promise.resolve(response);
      },
      close() {},
    },
  });
  await timed("engineInitMs", () => engine.initialize());
  cleanup.defer(() => engine.close());
  const journal = new CountedJournal(`${root}/inbound`);
  await timed("qqJournalInitMs", () => journal.initialize());
  const client = new QqClient({ appId: "offline", appSecret: "fixture" }, {
    httpsTransport: () => {
      throw new Error("no QQ transport");
    },
  });
  cleanup.defer(() => client.close());
  const actions = new QqSendActions(client, `${root}/outbox`),
    timeline = new QqTimeline(`${root}/timeline`),
    reading = new QqPhoneReading(`${root}/reading`),
    device = new QqPhoneDevice(`${root}/device`);
  await timed("qqStoresInitMs", async () => {
    for (const store of [actions, timeline, reading, device]) {
      await store.initialize();
      cleanup.defer(() => store.close());
    }
  });
  const source = journalInput ? journal : () => journal.records().map((record) => record);
  const phone = new QqPhone(
    source as ConstructorParameters<typeof QqPhone>[0],
    actions,
    timeline,
    { id: "bench", displayName: "bench" },
    reading,
    Date.now,
    device,
  );
  const mind = createMindControls(`${root}/mind`, undefined, Date.now, 30 * 60_000);
  let controls: Record<string, Handler> = {};
  const ctx = (caller: string) =>
    ({
      async call(contract: string, method: string, args: Dict = {}, options?: CallOptions) {
        const key = `${contract}.${method}`, trace = state.trace;
        if (key === "model.complete") {
          trace.eveModelAt = mono();
          trace.prepReads = { ...reads };
        }
        const entry: Entry = { key: `${caller}.${key}`, args, options: options ?? null };
        trace.entries.push(entry);
        try {
          let result: unknown;
          if (key === "phone.notifications") result = await phone.notifications(args);
          else if (key === "scripts.completions") result = scripts.completions(args);
          else if (key === "tools.list") result = [SCRIPT_TOOL];
          else if (contract === "mind" && ["poll", "update"].includes(method)) {result =
              await controls[method](args);} else if (engineCalls.has(key)) {
            const { _actionId, ...payload } = structuredClone(args);
            if (key === "model.complete") payload._purpose = "online";
            result = await engine.dispatch(
              contract,
              method,
              payload,
              typeof _actionId === "string" ? _actionId : `read_fixture_${++state.readId}`,
            );
          } else throw new Error(`unexpected call: ${entry.key}`);
          entry.result = asJson(result);
          return result;
        } catch (error) {
          entry.error = String(error);
          throw error;
        }
      },
      provide(contract: string, handlers: Record<string, Handler>) {
        assert.equal(contract, "mind");
        controls = handlers;
      },
      onDisable(fn: () => void | Promise<void>) {
        cleanup.defer(fn);
      },
    }) as unknown as PluginContext;
  const eveCtx = ctx("eve"), mindCtx = ctx("mind");
  const scripts = new ScriptRuntime(eveCtx, {
    directory: `${root}/scripts`,
    cacheDirectory: `${root}/unused-cache`,
    denoBinary: "/disabled",
    defaultTimeoutMs: 1000,
  });
  await timed("scriptStoreAndMindInitMs", async () => {
    await scripts.store.initialize();
    cleanup.defer(() => scripts.store.close());
    await mind.load!(mindCtx);
    await mind.enable!(mindCtx);
  });
  const eve = new EveRuntime(eveCtx, {
    directory: `${root}/eve`,
    promptFile: `${root}/prompt`,
    sourceCommit: SOURCE,
    modelTimeoutMs: 5000,
    maxOutputTokens: 128,
    pollMs: 60_000,
    defaultSleepMs: 60_000,
    dailyTokenBudget: BUDGET,
    timeZone: "UTC",
    remoteRecords: true,
    agentsEnabled: true,
    restReviewEnabled: true,
  });
  cleanup.defer(() => eve.close());
  await timed("eveInitMs", () => eve.initialize());
  return { eve, journal, timings };
}

async function round(
  rig: Awaited<ReturnType<typeof openRig>>,
  state: State,
  phase: Stage,
  delta: boolean,
) {
  wall = EPOCH + state.ordinal++ * 60_000;
  let publishMs = 0;
  if (delta) {
    const row = incoming(++state.visible, new Date().toISOString());
    const start = mono();
    const receipt = await rig.journal.append(row.message, row.raw);
    publishMs = mono() - start;
    assert.ok(receipt.fresh);
    assert.deepEqual(receipt.record, row);
  }
  const providerTrace = state.trace = new Trace();
  reads = readCounts();
  const start = mono();
  await rig.eve.pulse();
  const pulseEnd = mono(), pulseReads = reads;
  assert.ok(
    providerTrace.request && providerTrace.eveModelAt && providerTrace.providerAt,
    `no model at ${phase}: ${
      JSON.stringify(providerTrace.entries.map(({ key, error }) => ({ key, error })))
    }`,
  );
  const requests = providerTrace.entries.filter((entry) => entry.key === "eve.model.complete");
  assert.equal(requests.length, 1);
  const eveRequest = requests[0].args as Dict, request = providerTrace.request;
  assert.deepEqual(request, { messages: eveRequest.messages, tools: eveRequest.tools });
  const writes = providerTrace.entries.filter((entry) => entry.key === "eve.timeline.append").map((
    entry,
  ) => entry.result as unknown as FactEvent);
  const starts = writes.filter((event) => event.kind === "model_start");
  assert.equal(starts.length, 1);
  assert.equal(writes.at(-1)?.kind, "model_response");
  const modelStart = starts[0].data as {
    estimatedTokens: number;
    runtimeTiming: { preparationMs: number } & Dict;
    requestId: string;
  };
  assert.equal(modelStart.requestId, eveRequest._actionId);
  const mainPage = providerTrace.entries.filter((entry) => entry.key === "eve.phone.notifications");
  assert.equal(mainPage.length, 1);
  const page = mainPage[0].result as {
    items: { handle: { seq: number } }[];
    throughSeq: number;
    hasMore: boolean;
  };
  assert.deepEqual(page.items.map((item) => item.handle.seq), delta ? [state.visible] : []);
  assert.equal(page.throughSeq, state.visible);
  assert.equal(page.hasMore, false);
  const unread = providerTrace.entries.filter((entry) => entry.key === "mind.phone.notifications");
  assert.equal(unread.length, (state.ordinal - 1) % 30 === 0 ? 1 : 0);
  if (unread.length) {
    const result = unread[0].result as typeof page;
    assert.deepEqual(unread[0].args, { afterSeq: 0 });
    assert.deepEqual(result.items.map((item) => item.handle.seq), [1, 2, 3, 4, 5, 6, 7, 8]);
    assert.ok(result.hasMore);
  }
  const checked = providerTrace.verify();
  const toolTrace = state.trace = new Trace();
  reads = readCounts();
  const toolStart = mono();
  await rig.eve.pulse();
  const toolMs = mono() - toolStart;
  assert.deepEqual(toolTrace.entries.map((entry) => [entry.key, (entry.args as Dict).kind]), [[
    "eve.timeline.append",
    "tool_start",
  ], ["eve.timeline.append", "tool_result"]]);
  const tool = toolTrace.verify();
  return {
    ordinal: state.ordinal,
    phase,
    delta,
    visible: state.visible,
    publishMs: ms(publishMs),
    pulseToEveModelMs: ms(providerTrace.eveModelAt - start),
    pulseToMockProviderMs: ms(providerTrace.providerAt - start),
    providerAdmissionMs: ms(providerTrace.providerAt - providerTrace.eveModelAt),
    postProviderMs: ms(pulseEnd - providerTrace.providerAt),
    toolPulseMs: ms(toolMs),
    runtimeTiming: modelStart.runtimeTiming,
    reads: pulseReads,
    prepReads: providerTrace.prepReads,
    toolReads: reads,
    estimatedTokens: modelStart.estimatedTokens,
    messages: request.messages.length,
    requestBytes: new TextEncoder().encode(JSON.stringify(request)).length,
    requestSha256: sha(JSON.stringify(request)),
    appendKinds: writes.map((event) => event.kind),
    ...checked,
    tool,
  };
}

async function benchmark(root: string, profile: Profile) {
  const start = mono(), fixture = await seed(root, profile), fixtureMs = ms(mono() - start);
  const state: State = { trace: new Trace(), ordinal: 0, visible: profile.qqRows, readId: 0 };
  const startup = [], rounds: Round[] = [];
  const restoreReads = countMirrorReads(root, fixture);
  try {
    for (const phase of ["first", "reopen"] as const) {
      await using cleanup = new AsyncDisposableStack();
      const trace = state.trace = new Trace();
      reads = readCounts();
      const initStart = mono(),
        rig = await openRig(root, state, cleanup),
        initMs = mono() - initStart,
        initReads = reads;
      rounds.push(await round(rig, state, phase, false));
      startup.push({ phase, initMs: ms(initMs), ...rig.timings, initReads, ...trace.verify() });
      if (phase === "reopen") {
        for (const delta of [false, true]) rounds.push(await round(rig, state, "warmup", delta));
        for (let pair = 0; pair < profile.pairs; pair++) {
          for (const delta of (pair % 2 ? [true, false] : [false, true])) {
            rounds.push(await round(rig, state, delta ? "delta" : "empty", delta));
          }
        }
      }
    }
  } finally {
    restoreReads();
  }
  const measured = rounds.filter((row) => row.phase === "empty" || row.phase === "delta");
  const metrics = Object.fromEntries(["all", "empty", "delta"].map((phase) => {
    const rows = measured.filter((row) => phase === "all" || row.phase === phase);
    return [phase, {
      pulseToEveModel: summary(rows.map((row) => row.pulseToEveModelMs)),
      pulseToMockProvider: summary(rows.map((row) => row.pulseToMockProviderMs)),
      runtimePreparation: summary(rows.map((row) => row.runtimeTiming.preparationMs)),
      providerAdmission: summary(rows.map((row) => row.providerAdmissionMs)),
    }];
  }));
  const equivalenceSha256 = digest({
    fixture: fixture.sha256,
    profile,
    startup: startup.map((row) => row.ioSha256),
    rounds: rounds.map(
      (row) => [row.phase, row.visible, row.requestSha256, row.calls, row.ioSha256, row.tool],
    ),
  });
  return {
    benchmark: "eve-round-offline-v1",
    deno: Deno.version,
    profile,
    fixture,
    fixtureMs,
    qqSource: journalInput ? "journal" : "callback",
    equivalenceSha256,
    startup,
    metrics,
    rounds,
  };
}

if (import.meta.main) {
  if (Deno.args.includes("--help")) console.log(USAGE);
  else {
    const scratch = Deno.args.find((arg) => arg.startsWith("--scratch="))?.slice(10);
    if (
      !scratch?.startsWith("/") || Deno.args.some((arg) =>
        arg !== "--smoke" && !arg.startsWith("--scratch=") && !/^--expect=[0-9a-f]{64}$/.test(arg)
      ) || new Set(Deno.args.map((arg) =>
          arg.split("=")[0]
        )).size !== Deno.args.length
    ) throw new Error(USAGE);
    for (const name of ["net", "run"] as const) {
      assert.equal(
        (await Deno.permissions.query({ name })).state,
        "denied",
        `pass --deny-${name} explicitly`,
      );
    }
    const root = await Deno.makeTempDir({ dir: scratch, prefix: "eve-round-" });
    const restore = useFixtureClock();
    try {
      const report = await benchmark(
        root,
        Deno.args.includes("--smoke") ? SMOKE : NORMAL,
      );
      console.log(JSON.stringify(report, null, 2));
      const expected = Deno.args.find((arg) => arg.startsWith("--expect="))?.slice(9);
      if (expected) {
        assert.equal(
          report.equivalenceSha256,
          expected,
          "fixture, phase calls, semantic replies or byte-exact model request differ",
        );
      }
    } finally {
      restore();
      await Deno.remove(root, { recursive: true });
    }
  }
}
