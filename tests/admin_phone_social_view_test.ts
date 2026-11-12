import assert from "node:assert/strict";
import { workspaceScript } from "../services/admin/workspace-view.ts";

class Element {
  children: Element[] = [];
  constructor(readonly ownText = "") {}
  append(...children: Element[]) {
    this.children.push(...children);
  }
  get textContent(): string {
    return this.ownText + this.children.map((child) => child.textContent).join("");
  }
}
function paint(method: string, args: unknown, result: unknown): string {
  const render = new Function(
    "node",
    "pretty",
    "document",
    "el",
    workspaceScript + "\nreturn paintWorkspacePhone;",
  )(
    (_tag: string, _className = "", text = "") => new Element(text),
    JSON.stringify,
    { querySelectorAll: () => [] },
    () => ({ addEventListener() {} }),
  );
  const box = new Element();
  render(box, { method }, args, result);
  return box.textContent;
}
Deno.test("管理面板社交：分条只展示真实子回执，未准入不显示已发送；静默等待可见目的和水位", () => {
  const burst = paint("sendBurst", {
    conversationId: "qq:user:friend",
    messages: ["拟发第一句", "拟发第二句"],
  }, {
    actionId: "parent",
    complete: false,
    items: [
      { index: 1, actionId: "child-one", receipt: { actionId: "child-one", state: "unknown" } },
      {
        index: 2,
        actionId: "child-two",
        receipt: { actionId: "child-two", state: "blocked", blockedBy: "child-one" },
      },
    ],
  });
  assert.match(burst, /拟发第一句.*拟发第二句.*分条发送结果.*分条 1.*结果未知.*分条 2.*等待前序/);
  assert.doesNotMatch(burst, /已发送|状态未提供/);
  const prepared = paint("burstStatus", { actionId: "parent" }, {
    actionId: "parent",
    complete: false,
    items: [{ index: 1, actionId: "not-admitted", receipt: null }],
  });
  assert.match(prepared, /分条 1 · 尚未准入.*尚无真实发送回执/);
  const wait = paint("waitForMessages", { conversationId: "qq:user:friend", purpose: "reply" }, {
    reason: "quiet",
    waitedMs: 10003,
    throughSeq: 91,
    lastIncomingSeq: 87,
  });
  assert.match(
    wait,
    /10000 毫秒静默窗口.*最长 60000 毫秒.*等待目的reply.*等待结束原因quiet.*最新真实水位91.*最近真实入站编号87/,
  );
});
