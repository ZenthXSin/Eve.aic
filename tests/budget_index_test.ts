import assert from "node:assert/strict";
import { SharedBudget } from "../services/runtime/budget.ts";
import { writeDurableJson } from "../sdk/durable-json.ts";
Deno.test("shared budget indexes committed facts once, concurrent admission cannot overspend, live settings and restart stay exact", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-budget-index-" });
  const settings = `${directory}/settings.json`,
    config = { dailyTokenBudget: 10000, paused: false, timeZone: "UTC" };
  await writeDurableJson(settings, config);
  let budget = new SharedBudget(`${directory}/facts`, settings);
  await budget.initialize();
  try {
    const admitted = await Promise.allSettled(
      Array.from({ length: 12 }, (_, n) => budget.reserve(`request_${n}`, 1000)),
    );
    assert.equal(admitted.filter((x) => x.status === "fulfilled").length, 10);
    assert.equal((await budget.status()).safeRemaining, 0);
    let reads = 0;
    const events = budget.journal.events;
    Object.defineProperty(budget.journal, "events", {
      value: new Proxy(events, {
        get(target, key, receiver) {
          if (typeof key === "string" && /^\d+$/.test(key)) reads++;
          return Reflect.get(target, key, receiver);
        },
      }),
    });
    for (let n = 0; n < 5; n++) {
      await budget.response(`request_${n}`, {
        content: "",
        toolCalls: [],
        usage: { status: "reported", inputTokens: 100, cachedInputTokens: 80, outputTokens: 10 },
      });
    }
    const expected = await budget.status();
    assert.equal(expected.knownNoncachedTokens, 150);
    assert.equal(expected.unknownReservedTokens, 5000);
    assert.equal(expected.safeRemaining, 4850);
    assert.equal(reads, 0, "status, reservation lookup and settlement never rescan old bodies");
    await budget.updateSettings({ paused: true });
    await assert.rejects(budget.reserve("paused", 1), /暂停/);
    await budget.updateSettings(config);
    await budget.close();
    budget = new SharedBudget(`${directory}/facts`, settings);
    await budget.initialize();
    assert.deepEqual(await budget.status(), expected);
    await budget.response("request_0", {
      content: "duplicate",
      toolCalls: [],
      usage: { status: "reported", inputTokens: 999, cachedInputTokens: 0, outputTokens: 999 },
    });
    assert.deepEqual(await budget.status(), expected);
  } finally {
    await budget.close();
    await Deno.remove(directory, { recursive: true });
  }
});
