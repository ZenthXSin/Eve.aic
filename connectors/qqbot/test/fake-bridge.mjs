// Test-only process: no SDK/network; exercise the production Rust JSONL boundary.
import fs from "node:fs";
import readline from "node:readline";
import { setTimeout as delay } from "node:timers/promises";
const scenario = JSON.parse(fs.readFileSync(process.argv[2], "utf8"));
if (process.env.EVE_OPENAI_API_KEY || process.argv.some(x => x.includes("test-app-secret"))) {
  throw new Error("model_credentials_leaked");
}
const send = frame => process.stdout.write(JSON.stringify({ version: 1, ...frame }) + "\n");
// 宿主以原子替换写入状态；Windows 上读取恰逢替换时会暂时拒绝打开，视为尚未写入，由等待循环稍后再读。
const readState = path => {
  try {
    return JSON.parse(fs.readFileSync(path, "utf8"));
  } catch (error) {
    if (["ENOENT", "EPERM", "EBUSY", "EACCES"].includes(error?.code) || error instanceof SyntaxError) return null;
    throw error;
  }
};
if (scenario.pid_file) fs.writeFileSync(scenario.pid_file, String(process.pid));
const record = event => {
  if (scenario.events_file) fs.appendFileSync(scenario.events_file, JSON.stringify(event) + "\n");
};
const checkReply = (cmd, message) => {
  if (message.expected_type === "finish") throw new Error("retired_result_was_replied");
  if (message.expected_contains !== undefined) {
    const parts = Array.isArray(message.expected_contains) ? message.expected_contains : [message.expected_contains];
    if (parts.length === 0 || !parts.every(part => typeof part === "string" && cmd.text.includes(part))) {
      throw new Error("missing_reply_fragment");
    }
  } else if (cmd.text !== message.expected) throw new Error("wrong_reply_text");
};
const deliver = id => send({ type: "delivery", id, ok: !scenario.send_fail, message_id: "out-" + id });
const deliverSegment = (id, index, ok = true) => send({ type: "delivery", id, index, ok, message_id: `out-${id}-${index}` });
const checkSegment = (cmd, message) => {
  if (message.expected_type === "finish") throw new Error("retired_result_was_replied");
  const expected = message.expected_segments;
  if (!Array.isArray(expected)) throw new Error("unexpected_segment");
  if (cmd.count !== expected.length || cmd.text !== expected[cmd.index]) throw new Error("wrong_segment_text");
};

// Script steps synchronize against real HTTP request arrival or Rust commands,
// so control commands are injected while a particular generation is in flight.
if (scenario.script) {
  const pending = new Map();
  const commands = [];
  let stopped = false;
  // 主动私聊的平台结果：默认成功；脚本可改为拒绝（带错误码）或暂不回执。
  let pushMode = { ok: true };
  const until = async predicate => {
    const deadline = Date.now() + (scenario.wait_timeout_ms ?? 15000);
    while (!predicate()) {
      if (stopped) return false;
      if (Date.now() >= deadline) throw new Error("scenario_wait_timeout");
      await delay(10);
    }
    return true;
  };
  const execute = async () => {
    send({ type: "ready" });
    for (const step of scenario.script) {
      if (stopped) return;
      if (step.send) {
        for (const message of Array.isArray(step.send) ? step.send : [step.send]) {
          if (!pending.has(message.id)) pending.set(message.id, message);
          record({ direction: "in", ...message });
          send({ type: "message", ...message });
        }
      } else if (step.wait_file) {
        if (!await until(() => fs.existsSync(step.wait_file))) return;
      } else if (step.wait_command) {
        const expected = step.wait_command;
        if (!await until(() => commands.filter(cmd => cmd.type === expected.type && cmd.id === expected.id).length >= (expected.count ?? 1))) return;
      } else if (step.touch) {
        fs.writeFileSync(step.touch, "ready");
      } else if (step.push_mode) {
        pushMode = step.push_mode;
      } else if (step.delivery) {
        if (!commands.some(cmd => cmd.type === "reply" && cmd.id === step.delivery)) throw new Error("delivery_before_reply");
        deliver(step.delivery);
      } else if (step.deliver_segment) {
        const { id, index, ok } = step.deliver_segment;
        if (!commands.some(cmd => cmd.type === "segment" && cmd.id === id && cmd.index === index)) throw new Error("delivery_before_segment");
        deliverSegment(id, index, ok ?? true);
      } else if (step.wait_turn) {
        if (!await until(() => {
          const document = readState(step.wait_turn.path);
          if (!document) return false;
          const sessions = JSON.parse(Buffer.from(document.entries["eve.session"]["sessions.v1"]).toString());
          return Object.values(sessions.sessions).some(session => session.turns.some(turn =>
            turn.input === step.wait_turn.input && turn.status.state === step.wait_turn.state));
        })) return;
      } else if (step.wait_cognition) {
        if (!await until(() => {
          const wanted = step.wait_cognition;
          const document = readState(wanted.path);
          if (!document) return false;
          const bytes = document.entries["eve.cognition"]?.["cognition.v1"];
          if (!bytes) return false;
          const cognition = JSON.parse(Buffer.from(bytes).toString());
          const goals = Object.values(cognition.state.goals);
          const parent = goals.find(goal => goal.source.kind === "User" && goal.description === wanted.parent_description);
          return parent && goals.some(goal => goal.source.reference === parent.id && goal.verification === "reflection:v1" && goal.status === wanted.child_state);
        })) return;
      } else if (step.wait_part) {
        if (!await until(() => {
          const wanted = step.wait_part;
          const document = readState(wanted.path);
          if (!document) return false;
          const bytes = document.entries["eve.channel.qqbot"]?.["receipts.v1"];
          if (!bytes) return false;
          const ledger = JSON.parse(Buffer.from(bytes).toString());
          return ledger.entries.some(entry => entry.message.id === wanted.id &&
            entry.segments?.parts?.[wanted.index]?.state === wanted.state);
        })) return;
      } else if (step.wait_receipt) {
        if (!await until(() => {
          const wanted = step.wait_receipt;
          const document = readState(wanted.path);
          if (!document) return false;
          const bytes = document.entries["eve.channel.qqbot"]?.["receipts.v1"];
          if (!bytes) return false;
          const ledger = JSON.parse(Buffer.from(bytes).toString());
          return ledger.entries.some(entry => entry.message.id === wanted.id && entry.state === wanted.state);
        })) return;
      } else {
        throw new Error("unknown_scenario_step");
      }
    }
    process.stdout.end(() => process.exit(0));
  };
  const consume = async () => {
    const lines = readline.createInterface({ input: process.stdin });
    for await (const line of lines) {
      const cmd = JSON.parse(line);
      record({ direction: "out", ...cmd, ...(cmd.type === "segment" ? { at: Date.now() } : {}) });
      if (cmd.type === "stop") { stopped = true; return; }
      if (cmd.type === "push") {
        if (!/^[A-Za-z0-9_-]{1,128}$/.test(cmd.target_id ?? "") || typeof cmd.text !== "string") throw new Error("invalid_push");
        commands.push(cmd);
        if (pushMode.hold) continue;
        const { ok, ...diagnostic } = pushMode;
        send(ok ? { type: "delivery", id: cmd.id, push: true, ok: true, message_id: "push-" + cmd.id }
                : { type: "delivery", id: cmd.id, push: true, ok: false, ...diagnostic });
        continue;
      }
      const message = pending.get(cmd.id);
      if (!message) throw new Error("wrong_reply_id");
      if (cmd.type === "reply") {
        if (message.expected_segments) throw new Error("expected_segments_got_reply");
        checkReply(cmd, message);
        if (!message.hold_delivery) deliver(cmd.id);
      } else if (cmd.type === "segment") {
        checkSegment(cmd, message);
        if (!(message.hold_segments ?? []).includes(cmd.index)) {
          deliverSegment(cmd.id, cmd.index, message.fail_segment !== cmd.index);
        }
      } else if (cmd.type !== "finish") {
        throw new Error("unexpected_command");
      } else if (message.expected_type === "reply" && !message.allow_finish) {
        throw new Error("expected_reply_was_finished");
      }
      commands.push(cmd);
    }
    stopped = true;
  };
  try {
    await Promise.all([execute(), consume()]);
  } catch (error) {
    if (scenario.error_file) fs.writeFileSync(scenario.error_file, error.message);
    process.exitCode = 1;
    process.stdout.end();
  }
} else {
let index = 0;
const emitNext = () => {
  if (index < scenario.messages.length) send({ type: "message", ...scenario.messages[index] });
  else process.stdout.end(() => process.exit(0));
};
send({ type: "ready" }); emitNext();
const lines = readline.createInterface({ input: process.stdin });
for await (const line of lines) {
  const cmd = JSON.parse(line);
  record({ direction: "out", ...cmd });
  if (cmd.type === "stop") break;
  if (cmd.id !== scenario.messages[index]?.id) throw new Error("wrong_reply_id");
  if (cmd.type === "reply") {
    checkReply(cmd, scenario.messages[index]);
    send({ type: "delivery", id: cmd.id, ok: !scenario.send_fail, message_id: "out-1" });
  } else if (cmd.type !== "finish") throw new Error("unexpected_command");
  index++; emitNext();
}
}
