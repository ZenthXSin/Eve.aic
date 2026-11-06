// 使用生产 bridge-core；收件人、入站消息和平台回执均为本地替身。
import fs from "node:fs";
import path from "node:path";
import readline from "node:readline";
import {createBridge} from "../bridge-core.mjs";

const directory = process.argv[2];
const outcome = process.argv[3];
const handlers = new Map();
const record = value => fs.appendFileSync(path.join(directory, "bridge.jsonl"), JSON.stringify(value) + "\n");
const bot = {
  appId: "app",
  on: (name, callback) => handlers.set(name, callback),
  stop() {},
  async sendText(target, text) {
    record({type: "platform", target, text});
    if (outcome === "unknown") throw Object.assign(new Error("fixture_timeout"), {name: "TimeoutError"});
    return {id: "local-notice-receipt"};
  },
};
const bridge = createBridge(bot, frame => {
  record({direction: "channel", ...frame});
  process.stdout.write(JSON.stringify(frame) + "\n");
});
handlers.get("ready")();
handlers.get("message")({}, {
  kind: "c2c", messageId: "failed-input", senderId: "alice", content: "核对错误后通知",
  replyTarget: {scope: "c2c", targetId: "alice", msgId: "failed-input"},
});
const input = readline.createInterface({input: process.stdin});
for await (const line of input) {
  const frame = JSON.parse(line);
  record({direction: "host", ...frame});
  await bridge.command(frame);
  if (frame.type === "stop") break;
}
