// Test-only process: no SDK/network; exercise the production Rust JSONL boundary.
import fs from "node:fs";
import readline from "node:readline";
const scenario = JSON.parse(fs.readFileSync(process.argv[2], "utf8"));
if (process.env.EVE_OPENAI_API_KEY || process.argv.some(x => x.includes("test-app-secret"))) {
  throw new Error("model_credentials_leaked");
}
const send = frame => process.stdout.write(JSON.stringify({ version: 1, ...frame }) + "\n");
let index = 0;
const emitNext = () => {
  if (index < scenario.messages.length) send({ type: "message", ...scenario.messages[index] });
  else process.stdout.end(() => process.exit(0));
};
send({ type: "ready" }); emitNext();
const lines = readline.createInterface({ input: process.stdin });
for await (const line of lines) {
  const cmd = JSON.parse(line);
  if (cmd.type === "stop") break;
  if (cmd.id !== scenario.messages[index]?.id) throw new Error("wrong_reply_id");
  if (cmd.type === "reply") {
    if (cmd.text !== scenario.messages[index].expected) throw new Error("wrong_reply_text");
    send({ type: "delivery", id: cmd.id, ok: !scenario.send_fail, message_id: "out-1" });
  } else if (cmd.type !== "finish") throw new Error("unexpected_command");
  index++; emitNext();
}
