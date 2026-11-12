import assert from "node:assert/strict";
import { historyScript } from "../services/admin/history-view.ts";

class Storage {
  values = new Map<string, string>();
  failWrites = false;
  writes = 0;
  get length() {
    return this.values.size;
  }
  key(index: number) {
    return [...this.values.keys()][index] ?? null;
  }
  getItem(key: string) {
    return this.values.get(key) ?? null;
  }
  setItem(key: string, value: string) {
    if (this.failWrites) throw new Error("QuotaExceededError");
    this.writes++;
    this.values.set(key, value);
  }
  removeItem(key: string) {
    this.values.delete(key);
  }
}
function fixture(storage = new Storage(), now = Date.now()) {
  class Clock extends Date {
    static override now() {
      return now;
    }
  }
  const history = new Function(
    "localStorage",
    "Date",
    historyScript + `
    return {init:initBrowserHistory,merge:mergeBrowserHistory,clear:clearBrowserHistory,info:browserHistoryInfo};
  `,
  )(storage, Clock);
  return {
    history,
    storage,
    advance(ms: number) {
      now += ms;
    },
  };
}
const scope = "a".repeat(64);
function snapshot(seq: number, extra: Record<string, unknown> = {}) {
  return {
    throughSeq: seq,
    asOf: "live-time",
    state: { title: "真实状态" },
    events: [{ seq, at: `2026-11-12T00:00:${seq}Z`, kind: "model_response", text: `文字 ${seq}` }],
    workspace: {
      currentApp: { appId: "qq" },
      operations: [{
        id: "op-" + seq,
        namespace: "files",
        method: "read",
        appId: "files",
        at: String(seq),
        result: { text: "片段" },
      }],
      apps: {
        qq: { messages: [{ body: "私信不缓存" }] },
        files: { available: true, items: [] },
        tools: { items: [] },
      },
    },
    ...extra,
  };
}
Deno.test("browser history restores authenticated previews and labels retained entries without replacing current state or QQ", () => {
  const first = fixture();
  assert.equal(first.history.merge(snapshot(1)).browserHistory, undefined);
  first.history.init(scope);
  first.history.merge(snapshot(1));
  const second = fixture(first.storage);
  assert.equal(second.history.init(scope).restored, true);
  const fresh = snapshot(2), merged = second.history.merge(fresh);
  assert.deepEqual(merged.events.map((e: { seq: number }) => e.seq), [1, 2]);
  assert.equal(merged.events[0].retained, true);
  assert.equal(merged.events[1].retained, undefined);
  assert.equal(merged.state, fresh.state);
  assert.equal(merged.workspace.currentApp, fresh.workspace.currentApp);
  assert.equal(merged.workspace.apps.qq, fresh.workspace.apps.qq);
  assert.equal(merged.workspace.apps.tools.items.length, 2);
  assert.equal(merged.workspace.apps.files.items[0].retained, true);
  const saved = [...first.storage.values.values()][0];
  assert.doesNotMatch(saved, /私信不缓存|真实状态|live-time/);
});
Deno.test("browser history isolates sessions and clears local previews at logout", () => {
  const { history, storage } = fixture();
  history.init(scope);
  history.merge(snapshot(1));
  history.init("b".repeat(64));
  assert.equal(storage.length, 0);
  assert.equal(history.info().events, 0);
  history.merge(snapshot(2));
  history.clear();
  assert.equal(storage.length, 0);
  assert.equal(history.info().events, 0);
  assert.equal(history.merge(snapshot(3)).browserHistory, undefined);
  history.init("plain-csrf-not-a-hash");
  assert.equal(history.info().available, false);
});
Deno.test("browser history resets reused journal identities but tolerates mutable projection metadata", () => {
  const { history } = fixture();
  history.init(scope);
  history.merge(snapshot(1));
  history.merge(snapshot(2));
  const amended = snapshot(2);
  Object.assign(amended.events[0], { thoughtWrite: true });
  assert.equal(history.merge(amended).events.length, 2);
  const different = snapshot(2);
  different.events[0].at = "different-log-time";
  const replaced = history.merge(different);
  assert.equal(replaced.events.length, 1);
  assert.equal(replaced.workspace.operations.length, 1);
  assert.equal(history.merge(snapshot(1)).events.length, 1);
});
Deno.test("browser history is bounded by entry count, TTL and storage bytes and excludes binary credentials", () => {
  const { history, storage, advance } = fixture();
  history.init(scope);
  const value = snapshot(700);
  value.events = Array.from(
    { length: 700 },
    (_, i) => ({ seq: i + 1, at: "at-" + i, kind: "model_response", text: "文".repeat(4000) }),
  );
  value.workspace.operations = Array.from(
    { length: 300 },
    (_, i) => ({
      id: "op-" + i,
      namespace: "files",
      method: "read",
      appId: "files",
      at: String(i),
      result: {
        text: "result",
        token: "sensitive",
        imageData: "binary",
        other: "data:image/png;base64,AAAA",
      },
    }),
  );
  const merged = history.merge(value);
  assert.ok(merged.events.length <= 500);
  assert.ok(merged.workspace.operations.length <= 200);
  const serialized = [...storage.values.values()][0];
  assert.ok(serialized.length * 2 <= 2 * 1024 * 1024);
  assert.doesNotMatch(serialized, /sensitive|imageData|data:image/);
  advance(8 * 86400000);
  const expired = fixture(storage, Date.now() + 8 * 86400000);
  assert.equal(expired.history.init(scope).events, 0);
  assert.equal(expired.history.info().operations, 0);
});
Deno.test("browser history survives corrupt cache and denied quota while keeping live data usable", () => {
  const storage = new Storage();
  storage.setItem("eve.preview-history.v1." + scope, "{invalid");
  const { history } = fixture(storage);
  assert.equal(history.init(scope).events, 0);
  storage.failWrites = true;
  const merged = history.merge(snapshot(1));
  assert.equal(merged.events.length, 1);
  assert.equal(merged.browserHistory.available, false);
  history.clear();
  assert.equal(history.info().events, 0);
});

Deno.test("browser history writes changed previews once and skips unchanged live frames including after refresh", () => {
  const { history, storage } = fixture();
  history.init(scope);
  history.merge(snapshot(1));
  assert.equal(storage.writes, 1);
  const savedAt = history.info().savedAt;
  history.merge(snapshot(1, { asOf: "later-live-time", state: { title: "新状态" } }));
  assert.equal(storage.writes, 1);
  assert.equal(history.info().savedAt, savedAt);
  const restored = fixture(storage);
  restored.history.init(scope);
  restored.history.merge(snapshot(1));
  assert.equal(storage.writes, 1);
  const updated = snapshot(1);
  Object.assign(updated.workspace.operations[0], {
    status: "completed",
    args: { path: "/tmp/updated" },
    result: { text: "新结果" },
  });
  restored.history.merge(updated);
  assert.equal(storage.writes, 2);
  assert.match([...storage.values.values()][0], /completed|新结果/);
  restored.history.merge(updated);
  assert.equal(storage.writes, 2);
});
