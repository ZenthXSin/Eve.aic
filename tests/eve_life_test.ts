import assert from "node:assert/strict";
import type { ConsciousEvent } from "../plugins/eve/journal.ts";
import { momentContext, planMoment } from "../plugins/eve/life.ts";
import { continuityMessage } from "../plugins/eve/continuity.ts";
import { lanternContext, planLanterns } from "../plugins/eve/lanterns.ts";

Deno.test("兴趣是有限期的选择；同一次调用、没变化的心情均不再记账，到期不靠发言续命", () => {
  const now = Date.now();
  const args = {
    feeling: "无聊",
    wish: "玩一会",
    interest: "给灯塔故事续一句",
    minutes: 10,
    expectedSeq: 0,
  };
  const initial = planMoment(args, [], "turn_1", now);
  assert.ok(initial.event);
  const events: ConsciousEvent[] = [{
    schema: 1,
    previous: "0".repeat(64),
    seq: 1,
    at: new Date(now).toISOString(),
    kind: "life_moment",
    data: initial.event,
    digest: "a".repeat(64),
  }];
  assert.equal(momentContext(events, now).revision, 1);
  assert.match(continuityMessage(events, new Map()).content, /给灯塔故事续一句/);
  assert.deepEqual(planMoment(args, events, "turn_1", now), { existing: 1 });
  assert.deepEqual(planMoment({ ...args, expectedSeq: 1 }, events, "turn_2", now), { existing: 1 });
  assert.deepEqual(momentContext(events, now + 600_000), { revision: 1 });
  assert.throws(() => planMoment({ ...args, wish: "独处" }, events, "turn_3", now));
  assert.throws(() => planMoment({ ...args, conversationId: "QQ" }, [], "turn_4", now));
  assert.throws(() => planMoment({ ...args, minutes: 0 }, [], "turn_5", now));
  assert.equal(events.length, 1);
});

Deno.test("独自娱乐：棋盘可解，重复动作幂等，重开可续局；不向群聊发送任何消息", () => {
  const events: ConsciousEvent[] = [], now = new Date().toISOString();
  const move = (action: string, expectedSeq: number, id: string, cell?: number) => {
    const plan = planLanterns(
      { action, expectedSeq, ...(cell ? { cell } : {}) },
      events,
      id,
      () => 16,
    );
    if (plan.event) {
      events.push({
        schema: 1,
        seq: events.length + 1,
        at: now,
        previous: "0".repeat(64),
        digest: "1".repeat(64),
        kind: "leisure_lanterns",
        data: plan.event,
      });
    }
    return lanternContext(events);
  };
  assert.equal(move("start", 0, "a").status, "playing");
  assert.deepEqual(lanternContext(JSON.parse(JSON.stringify(events))), lanternContext(events));
  assert.equal(move("start", 0, "a").revision, 1);
  assert.throws(() => move("press", 0, "b", 5));
  assert.equal(move("press", 1, "c", 5).status, "won");
  assert.equal(move("press", 1, "c", 5).moves, 1);
  assert.equal(move("start", 2, "d").status, "playing");
  assert.equal(move("leave", 3, "e").status, "left");
});
