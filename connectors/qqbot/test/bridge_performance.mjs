// Production normalization + JSONL command parser; no QQ network or credentials.
import assert from "node:assert/strict";
import fs from "node:fs";
import { performance } from "node:perf_hooks";
import { Readable } from "node:stream";
import { createBridge, consume } from "../bridge-core.mjs";

const budget = JSON.parse(fs.readFileSync(process.argv[2], "utf8"));
const payload = "x".repeat(budget.payload_bytes);
async function run(count) {
  const handlers = new Map(), latencies = [];
  let accepted = 0, delivered = 0, sent = 0, peakRss = 0;
  const targets = new Map(), starts = new Map();
  const bot = {
    on: (name, handler) => handlers.set(name, handler),
    stop() {},
    async sendText(target, text) {
      assert.deepEqual(target, targets.get(target.msgId));
      assert.equal(text, payload);
      sent++;
      return { id: "out." + sent + "!" };
    },
  };
  const bridge = createBridge(bot, frame => {
    if (frame.type === "message") {
      assert.ok(targets.has(frame.id));
      assert.equal(frame.text, payload);
      accepted++;
    } else if (frame.type === "delivery") {
      assert.equal(frame.ok, true);
      delivered++;
      assert.ok(starts.has(frame.id));
      latencies.push(performance.now() - starts.get(frame.id));
      targets.delete(frame.id); starts.delete(frame.id);
      if (delivered % 128 === 0) peakRss = Math.max(peakRss, process.memoryUsage().rss / 1048576);
    } else throw new Error("unexpected_bridge_frame");
  });
  async function* commands() {
    for (let i = 0; i < count; i++) {
      const scope = i % 2 ? "group" : "c2c", id = "ROBOT1.0_perf." + i + "!";
      const current = {
        kind: scope, messageId: id, senderId: "perf-user", groupOpenid: "perf-group",
        content: payload, rawEventType: scope === "group" ? "GROUP_AT_MESSAGE_CREATE" : "C2C_MESSAGE_CREATE",
        replyTarget: { scope, targetId: scope === "group" ? "perf-group" : "perf-user", msgId: id },
      };
      targets.set(id, current.replyTarget); starts.set(id, performance.now());
      handlers.get("message")({}, current);
      yield Buffer.from(JSON.stringify({ type: "reply", version: 1, id, text: payload }) + "\n");
    }
  }
  const cpu = process.cpuUsage(), begin = performance.now();
  await consume(Readable.from(commands()), bridge);
  const elapsed_ms = performance.now() - begin, used = process.cpuUsage(cpu);
  assert.equal(accepted, count); assert.equal(delivered, count); assert.equal(sent, count);
  return { messages: count, elapsed_ms, messages_per_second: count * 1000 / elapsed_ms,
    cpu_ms: (used.user + used.system) / 1000, rss_mib: Math.max(peakRss, process.memoryUsage().rss / 1048576),
    latency_ms: latencies };
}
await run(budget.node.warmup_messages);
const samples = [];
for (let i = 0; i < budget.node.samples; i++) samples.push(await run(budget.node.messages));
console.log(JSON.stringify({ node_version: process.version, samples }));
