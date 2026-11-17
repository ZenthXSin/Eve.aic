import assert from "node:assert/strict";
import { contextMaintenanceState } from "../plugins/eve/context-maintenance.ts";
import { inspectContext } from "../plugins/eve/context-tools.ts";
import { ConsciousJournal } from "../plugins/eve/journal.ts";

Deno.test("整理提醒：实测输入触线，四轮或15分钟重复；整理后旧实测不再触发", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-context-maintenance-" });
  const journal = new ConsciousJournal(directory);
  const commit = "a".repeat(40);
  const model = async (inputTokens: number) => {
    const requestId = `model_${journal.events.length}`;
    await journal.append("model_start", { requestId, sourceCommit: commit });
    await journal.append("model_response", {
      requestId,
      response: {
        content: "继续",
        toolCalls: [],
        usage: { status: "reported", inputTokens, outputTokens: 1 },
      },
    });
  };
  try {
    await journal.initialize();
    await model(160_000);
    let state = contextMaintenanceState(journal.events, 100_000, commit);
    assert.equal(state.level, "urgent");
    assert.equal(state.due, true);
    const perception = await journal.append("perception", { contextMaintenance: state });
    assert.equal(contextMaintenanceState(journal.events, 100_000, commit).due, false);
    for (let i = 0; i < 3; i++) await model(160_000);
    assert.equal(contextMaintenanceState(journal.events, 100_000, commit).due, false);
    assert.equal(
      contextMaintenanceState(journal.events, 100_000, commit, Date.parse(perception.at) + 900_000)
        .due,
      true,
    );
    await model(160_000);
    assert.equal(contextMaintenanceState(journal.events, 100_000, commit).due, true);
    await journal.append("compression", { throughSeq: 1, summary: "承接" });
    state = contextMaintenanceState(journal.events, 95_000, commit);
    assert.equal(state.providerInput?.currentView, false);
    assert.equal(state.level, "normal");
    assert.equal(state.due, false);
    await model(125_000);
    assert.equal(contextMaintenanceState(journal.events, 100_000, commit).level, "due");
    assert.equal(contextMaintenanceState(journal.events, 100_000, "new-release").level, "normal");
    await model(0);
    assert.equal(contextMaintenanceState(journal.events, 120_000, commit).level, "due");
  } finally {
    await journal.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("整理提醒：120k只准备一次，不每四轮催压缩；150k才要求整理", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-context-watch-" });
  const journal = new ConsciousJournal(directory);
  try {
    await journal.initialize();
    const state = contextMaintenanceState(journal.events, 125_000);
    assert.equal(state.compressionDue, false);
    assert.equal(state.due, true);
    assert.ok(state.reminder?.includes("不因预提醒或睡前就压缩"));
    await journal.append("perception", { contextMaintenance: state });
    for (let i = 0; i < 5; i++) {
      await journal.append("model_response", { response: { content: "接着聊", toolCalls: [] } });
    }
    assert.equal(contextMaintenanceState(journal.events, 140_000).due, false);
    const urgent = contextMaintenanceState(journal.events, 150_000);
    assert.equal(urgent.compressionDue, true);
    assert.equal(urgent.due, true);
  } finally {
    await journal.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("整理建议：完整边界优先满足100k目标，保留最近片段，不切未完成工具批次", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-context-boundaries-" });
  const journal = new ConsciousJournal(directory);
  try {
    await journal.initialize();
    for (let i = 0; i < 12; i++) {
      await journal.append("notice", { source: "phone", items: [{ text: "史".repeat(15_000) }] });
    }
    const response = await journal.append("model_response", {
      response: { content: "", toolCalls: [{ id: "pending", name: "act", args: {} }] },
    });
    const inspected = inspectContext(journal.events, "身份", [], { limit: 1 });
    const recommendation = inspected.recommendation;
    assert.ok(recommendation.candidate);
    assert.equal(recommendation.candidate.throughSeq, 6);
    assert.equal(recommendation.retainedRecentBlocks, 6);
    assert.ok(recommendation.candidate.throughSeq < response.seq);
    assert.equal(inspected.totalCandidates, 12);
    assert.equal(inspected.candidates.length, 1, "建议不依赖第一页是否包含合适边界");
  } finally {
    await journal.close();
    await Deno.remove(directory, { recursive: true });
  }
});
