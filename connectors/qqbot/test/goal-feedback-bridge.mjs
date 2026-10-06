// Test-only JSONL peer: deterministic gates and persisted revision observations.
import fs from "node:fs";
import readline from "node:readline";
import { setTimeout as delay } from "node:timers/promises";

const scenario = JSON.parse(fs.readFileSync(process.argv[2], "utf8"));
if (process.env.EVE_OPENAI_API_KEY || process.argv.some(value => value.includes("test-app-secret"))) {
  throw new Error("credentials_leaked_to_bridge");
}
const send = frame => process.stdout.write(JSON.stringify({ version: 1, ...frame }) + "\n");
const record = event => fs.appendFileSync(scenario.events_file, JSON.stringify(event) + "\n");
const pending = new Map();
const commands = [];
const bindings = {};
let stopped = false;
const resolve = value => {
  if (typeof value === "string") {
    return value.replace(/\{\{([a-z_]+)\.(id|revision)\}\}/g, (_, name, field) => {
      if (!bindings[name]) throw new Error("missing_goal_binding");
      return String(bindings[name][field]);
    });
  }
  if (Array.isArray(value)) return value.map(resolve);
  if (value && typeof value === "object") {
    return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, resolve(item)]));
  }
  return value;
};
const documents = () => {
  if (!fs.existsSync(scenario.state_file)) return {};
  const state = JSON.parse(fs.readFileSync(scenario.state_file, "utf8"));
  return Object.fromEntries(Object.entries(state.entries).map(([owner, entries]) =>
    [owner, Object.fromEntries(Object.entries(entries).map(([key, bytes]) =>
      [key, JSON.parse(Buffer.from(bytes).toString("utf8"))]))]));
};
const cognition = () => documents()["eve.cognition"]?.["cognition.v1"]?.state;
const until = async predicate => {
  const deadline = Date.now() + 15000;
  while (!predicate()) {
    if (stopped) return false;
    if (Date.now() >= deadline) throw new Error("scenario_wait_timeout");
    await delay(10);
  }
  return true;
};
const matchesChild = wanted => {
  const state = cognition();
  if (!state) return false;
  const parent = bindings[wanted.parent];
  if (!parent) throw new Error("missing_parent_binding");
  const evidence = new Set(state.events.filter(event => {
    try {
      const data = JSON.parse(event.summary);
      return data.kind === "waiting_input" && data.parent_id === parent.id &&
        data.parent_revision === parent.revision;
    } catch { return false; }
  }).map(event => event.id));
  const childIds = new Set(state.events.filter(event => evidence.has(event.caused_by) &&
    event.kind === "DriveEvaluated").map(event => event.goal_id));
  return Object.values(state.goals).some(goal => childIds.has(goal.id) && goal.status === wanted.status);
};
const execute = async () => {
  send({ type: "ready" });
  for (const raw of scenario.script) {
    if (stopped) return;
    const step = resolve(raw);
    if (step.send) {
      // A repeated platform id may have a different expected finish outcome.
      pending.set(step.send.id, step.send);
      record({ direction: "in", ...step.send });
      send({ type: "message", ...step.send });
    } else if (step.wait_command) {
      const wanted = step.wait_command;
      if (!await until(() => commands.filter(cmd => cmd.id === wanted.id && cmd.type === wanted.type)
        .length >= (wanted.count ?? 1))) return;
    } else if (step.wait_sent) {
      if (!await until(() => documents()["eve.channel.qqbot"]?.["receipts.v1"]?.entries
        .some(entry => entry.message.id === step.wait_sent && entry.state === "Sent"))) return;
    } else if (step.wait_receipt) {
      const wanted = step.wait_receipt;
      if (!await until(() => documents()["eve.channel.qqbot"]?.["receipts.v1"]?.entries
        .some(entry => entry.message.id === wanted.id && entry.state === wanted.state))) return;
    } else if (step.capture_goal) {
      const wanted = step.capture_goal;
      if (!await until(() => {
        const goal = Object.values(cognition()?.goals ?? {}).find(item => item.source.kind === "User" &&
          item.source.reference === wanted.source);
        if (!goal) return false;
        bindings[wanted.name] = { id: goal.id, revision: goal.revision };
        return true;
      })) return;
    } else if (step.wait_child) {
      if (!await until(() => matchesChild(step.wait_child))) return;
    } else if (step.wait_file) {
      if (!await until(() => fs.existsSync(step.wait_file))) return;
    } else if (step.touch) {
      fs.writeFileSync(step.touch, "ready");
    } else {
      throw new Error("unknown_scenario_step");
    }
  }
  fs.writeFileSync(scenario.bindings_file, JSON.stringify(bindings));
  process.stdout.end(() => process.exit(0));
};
const consume = async () => {
  for await (const line of readline.createInterface({ input: process.stdin })) {
    const cmd = JSON.parse(line);
    record({ direction: "out", ...cmd });
    if (cmd.type === "stop") { stopped = true; return; }
    const message = pending.get(cmd.id);
    if (!message) throw new Error("unexpected_message_id");
    if (cmd.type === "reply") {
      if (message.expected_type === "finish") throw new Error("duplicate_was_replied");
      for (const text of message.contains ?? []) {
        if (!cmd.text.includes(text)) throw new Error("missing_reply_fragment:" + text);
      }
      for (const text of message.excludes ?? []) {
        if (cmd.text.includes(text)) throw new Error("unexpected_reply_fragment:" + text);
      }
      send({ type: "delivery", id: cmd.id, ok: message.delivery_ok ?? true, message_id: "out-" + cmd.id });
    } else if (cmd.type !== "finish" || message.expected_type !== "finish") {
      throw new Error("unexpected_command_type");
    }
    commands.push(cmd);
  }
  stopped = true;
};
try {
  await Promise.all([execute(), consume()]);
} catch (error) {
  fs.writeFileSync(scenario.error_file, String(error.message));
  process.exitCode = 1;
  process.stdout.end();
}
