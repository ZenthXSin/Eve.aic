// Test-only QQ facade around the production bridge and JSONL parser.
import assert from "node:assert/strict";
import fs from "node:fs";
import { performance } from "node:perf_hooks";
import { createBridge, consume } from "../bridge-core.mjs";

const scenario = JSON.parse(fs.readFileSync(process.argv[2], "utf8"));
if (process.env.EVE_OPENAI_API_KEY) throw new Error("model_credentials_leaked");
const handlers = new Map(), latencies = [];
let index = 0, start = 0, peakRss = 0;
const bot = {
  on: (name, handler) => handlers.set(name, handler),
  stop() {},
  async sendText(target, text) {
    const message = scenario.messages[index];
    assert.deepEqual(target, { scope: "c2c", targetId: "perf-user", msgId: message.id });
    assert.equal(text, message.expected);
    return { id: "perf-out." + index + "!" };
  },
};
function next() {
  if (index === scenario.messages.length) {
    fs.writeFileSync(scenario.metrics_path, JSON.stringify({ latency_ms: latencies, node_peak_rss_mib: peakRss }));
    process.stdout.end(() => process.exit(0));
    return;
  }
  const message = scenario.messages[index];
  handlers.get("message")({}, {
    kind: "c2c", messageId: message.id, senderId: "perf-user", content: message.text,
    rawEventType: "C2C_MESSAGE_CREATE",
    replyTarget: { scope: "c2c", targetId: "perf-user", msgId: message.id },
  });
}
const bridge = createBridge(bot, frame => {
  if (frame.type === "message") start = performance.now();
  process.stdout.write(JSON.stringify(frame) + "\n");
  if (frame.type === "delivery") {
    assert.equal(frame.ok, true);
    assert.equal(frame.id, scenario.messages[index].id);
    latencies.push(performance.now() - start);
    peakRss = Math.max(peakRss, process.memoryUsage().rss / 1048576);
    index++;
    setImmediate(next);
  } else if (frame.type === "warning" || frame.type === "fatal") throw new Error("unexpected_bridge_frame");
});
handlers.get("ready")();
next();
await consume(process.stdin, bridge);
