// Test fixture: production bridge normalization/dispatch, local SDK substitute.
import fs from "node:fs";
import { createBridge, consume } from "../bridge-core.mjs";

const scenario = JSON.parse(fs.readFileSync(process.argv[2], "utf8"));
if (process.env.EVE_OPENAI_API_KEY) throw new Error("model_credentials_leaked");
const handlers = new Map();
const touch = name => fs.writeFileSync(scenario[name], "observed");
const bot = {
  appId: "1904159860",
  on: (event, handler) => handlers.set(event, handler),
  stop: () => touch("stopped"),
  async sendText() {
    touch("sending");
    if (scenario.hold_send) await new Promise(() => {});
    return { id: "synthetic-outbound-id" };
  },
};
const bridge = createBridge(bot, frame => process.stdout.write(JSON.stringify(frame) + "\n"));
const command = bridge.command;
bridge.command = frame => {
  if (frame.type === "finish") touch("finished");
  return command(frame);
};
handlers.get("ready")();
for (const frame of scenario.raw_frames ?? []) process.stdout.write(JSON.stringify(frame) + "\n");
for (const content of scenario.contents ?? ["桥接验收消息"]) {
  handlers.get("message")({}, {
    kind: "c2c", messageId: "transport-message", senderId: "transport-user",
    content, replyTarget: { scope: "c2c", targetId: "transport-user", msgId: "transport-message" },
  });
}
await consume(process.stdin, bridge);
