import assert from "node:assert/strict";
import { LiveReader } from "../services/admin/live.ts";
import { workspaceScript } from "../services/admin/workspace-view.ts";
import type { AdminBackend } from "../services/admin/contracts.ts";

class Element {
  children: Element[] = [];
  constructor(readonly ownText = "") {}
  append(...children: Element[]) {
    this.children.push(...children);
  }
  get textContent(): string {
    return this.ownText + this.children.map((child) => child.textContent).join("\n");
  }
}
function paint(name: string, op: unknown, args: unknown, result: unknown): string {
  const render = new Function(
    "node",
    "pretty",
    "document",
    "el",
    workspaceScript + `\nreturn ${name};`,
  )(
    (_tag: string, _className = "", text = "") => new Element(text),
    JSON.stringify,
    { querySelectorAll: () => [] },
    () => ({ addEventListener() {} }),
  );
  const box = new Element();
  render(box, op, args, result);
  return box.textContent;
}
Deno.test("面板在预览截断之前解开脚本结果，跨刷新关联调用；QQ正文不被误当JSON", async () => {
  const body = '{"status":"sent"}\n带引号的正文🙂'.repeat(100);
  const value = { handle: { conversationId: "qq:group:test", seq: 6361 }, text: body };
  const wire = {
    status: "completed",
    output: JSON.stringify({
      jobId: "original-job",
      status: "completed",
      actions: 1,
      result: { text: JSON.stringify(value), characters: 3500, truncated: false },
    }),
  };
  let throughSeq = 1;
  const events = [
    {
      seq: 1,
      at: "2026-11-14T14:00:00Z",
      kind: "tool_start",
      data: {
        callId: "read-1",
        call: { name: "read_script", args: { jobId: "original-job" } },
      },
    },
    {
      seq: 2,
      at: "2026-11-14T14:00:01Z",
      kind: "tool_result",
      data: {
        callId: "read-1",
        result: wire,
      },
    },
  ];
  const backend = {
    status: () => Promise.resolve({ runtime: { throughSeq } }),
    query: ({ args }: { args: { afterSeq: number } }) =>
      Promise.resolve({
        items: events.filter((e) => e.seq > args.afterSeq && e.seq <= throughSeq),
      }),
    control: () => {
      throw new Error("只读展示不可控制服务");
    },
  } as unknown as AdminBackend;
  const reader = new LiveReader(backend);
  await reader.read();
  throughSeq = 2;
  const snapshot = await reader.read() as unknown as { events: { toolReceipt?: unknown }[] };
  const receipt = snapshot.events[1].toolReceipt as {
    output: { result: { value: unknown; text?: string } };
  };
  assert.deepEqual(JSON.parse(JSON.stringify(receipt.output.result.value)), value);
  assert.equal(receipt.output.result.text, undefined);
  assert.equal(typeof wire.output, "string");
  const shown = paint(
    "paintWorkspaceScripts",
    { namespace: "native", method: "read_script" },
    {},
    receipt,
  );
  assert.ok(shown.includes(body));
  assert.ok(shown.includes("original-job"));
  assert.doesNotMatch(shown, /\\"handle\\"|\\n/);
});
Deno.test("面板兼容旧脚本完整页；数组、null、false及JSON形状正文保留", () => {
  const value = { items: [false, null, { text: '{"hello":"原文"}' }] };
  const output = {
    jobId: "old",
    status: "completed",
    result: {
      text: JSON.stringify(value),
      characters: 90,
      truncated: false,
    },
  };
  const shown = paint("paintWorkspaceScripts", { namespace: "native", method: "read_script" }, {}, {
    status: "completed",
    output: JSON.stringify(output),
  });
  assert.match(shown, /条目 1[\s\S]*false[\s\S]*条目 2[\s\S]*null/);
  assert.ok(shown.includes('{"hello":"原文"}'));
});
Deno.test("面板保留脚本片段和同一任务游标，不解码尾页；QQ文本显示消息位置", () => {
  const shown = paint("paintWorkspaceScripts", { namespace: "native", method: "read_script" }, {}, {
    output: {
      jobId: "original",
      status: "completed",
      resultStartChar: 4000,
      result: { text: '{"tail":1}', characters: 9000, truncated: true, nextChar: 4010 },
    },
  });
  assert.ok(shown.includes('{"tail":1}'));
  assert.match(shown, /字符偏移 4000.*后续位置 4010/);
  assert.ok(shown.includes('read_script({jobId:"original",startChar:4010,limit:8192})'));
  const phone = paint("paintWorkspacePhone", { method: "text" }, { startChar: 1024 }, {
    handle: { conversationId: "qq:group:test", seq: 6361 },
    text: "原始正文\n第二行",
    characters: 3000,
    truncated: true,
    nextChar: 2048,
  });
  assert.match(phone, /qq:group:test[\s\S]*6361[\s\S]*3000/);
  assert.ok(phone.includes("原始正文\n第二行"));
  assert.match(phone, /后续位置 2048/);
});
