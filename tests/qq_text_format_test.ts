import assert from "node:assert/strict";
import { mdToPlain, splitForQQ } from "../adapters/qq/text-format.ts";

Deno.test("QQ Markdown fallback preserves readable headings, lists, body and complete link destinations", () => {
  const text =
    "# 标题\n- **重点** 和 _斜体_\n1. [文档](https://example.com/wiki/Foo_(bar)?a=1&b=2)\n> ~~旧说明~~ 还在\n![图](https://example.com/a.png)\n<@user> <https://example.com/page>";
  assert.equal(
    mdToPlain(text),
    "标题\n• 重点 和 斜体\n1. 文档 (https://example.com/wiki/Foo_(bar)?a=1&b=2)\n旧说明 还在\n图 (https://example.com/a.png)\n<@user> https://example.com/page",
  );
});

Deno.test("QQ fallback keeps fenced and inline code literal, escaped punctuation and URL data", () => {
  const source =
    "# literal heading\n  const snake_case = '**not bold**';\n  console.log('[label](url)', `**raw**`);\n";
  const input = "看看 `snake_case **原样**`\n```ts\n" + source +
    "```\n\\*字面星号\\* https://example.com/__file__?x=**value**";
  assert.equal(
    mdToPlain(input),
    "看看 snake_case **原样**\n" + source + "*字面星号* https://example.com/__file__?x=**value**",
  );
  assert.equal(mdToPlain("~~~python\na = 1\n~~~\n完成"), "a = 1\n完成");
  assert.equal(mdToPlain("```\n**未闭合代码也保留**"), "**未闭合代码也保留**");
});

Deno.test("QQ fallback retains malformed markup body and ordinary paths instead of dropping them", () => {
  assert.equal(
    mdToPlain("snake_case a*b*c /tmp/a_b.txt [链接](https://example.com/(broken)"),
    "snake_case a*b*c /tmp/a_b.txt [链接](https://example.com/(broken)",
  );
  assert.equal(mdToPlain("中文**粗体**中文"), "中文粗体中文");
  assert.equal(
    mdToPlain("**[粗体链接](https://example.com/__raw__)**"),
    "粗体链接 (https://example.com/__raw__)",
  );
  assert.equal(
    mdToPlain("\uE000qq-url-0\uE001 [链接](https://example.com/)"),
    "\uE000qq-url-0\uE001 链接 (https://example.com/)",
  );
  assert.equal(mdToPlain("```这一行是行内代码```"), "这一行是行内代码");
  assert.equal(mdToPlain("## 标题 ##\r\n下一行"), "标题\n下一行");
});

Deno.test("QQ splitting prefers semantic boundaries, preserves full input and keeps grapheme clusters", () => {
  const input = "今天先看看群里有什么新消息，等一下再整理便签。然后去检查那几个还没有解决的问题。";
  const parts = splitForQQ(input);
  assert.equal(parts.join(""), input);
  assert(parts.length >= 2 && parts.length <= 4);
  const segmenter = new Intl.Segmenter(undefined, { granularity: "grapheme" });
  assert(parts.every((part) => [...segmenter.segment(part)].length <= 30));
  assert.match(parts[0], /[，。！？]$/);
  const family = "👨‍👩‍👧‍👦";
  const emoji = splitForQQ(family.repeat(35));
  assert.deepEqual(emoji, [family.repeat(30), family.repeat(5)]);
  assert.deepEqual(splitForQQ("一条普通消息"), ["一条普通消息"]);
  assert.deepEqual(splitForQQ(" \n "), []);
});

Deno.test("QQ splitting rejects overflow and code instead of truncation or hidden oversized final messages", () => {
  const input = "字".repeat(121);
  assert.throws(() => splitForQQ(input), /超过 4 条.*未截断/);
  assert.equal(input.length, 121);
  assert.throws(() => splitForQQ("```js\nconst x = 1;\n```"), /代码块/);
  assert.throws(() => splitForQQ("你好", { maxCharacters: 0 }), /参数无效/);
  assert.throws(() => splitForQQ("你好", { maxParts: 33 }), /参数无效/);
});

Deno.test("QQ splitting keeps URLs intact and is deterministic before action admission", () => {
  const input = "给你一个链接：https://example.com/hello 后面还有一些可读的文字和说明。";
  const parts = splitForQQ(input);
  assert.equal(parts.join(""), input);
  assert(parts.some((part) => part.includes("https://example.com/hello")));
  assert.deepEqual(splitForQQ(input), parts);
  assert.throws(() => splitForQQ("https://example.com/" + "a".repeat(40)), /不拆开 URL/);
});

Deno.test("QQ malformed link input is bounded without dropping its literal body or URLs", () => {
  const url = "https://example.com/__raw__?x=**value**";
  for (
    const malformed of [
      "[".repeat(7000) + "正文 " + url,
      "[x](".repeat(1300) + "正文 " + url,
      "[".repeat(200_000) + "正文 " + url,
      "**a ".repeat(50_000) + "正文 " + url,
    ]
  ) {
    const began = performance.now();
    const output = mdToPlain(malformed);
    assert.equal(output, malformed);
    // A generous ceiling detects suffix rescans of the large cases without relying on
    // millisecond benchmark ratios or the exact speed of a particular machine.
    assert(performance.now() - began < 2000, "malformed Markdown must use bounded fallback");
  }
});

Deno.test("QQ long-line fallback and delimiter indexing keep code, URL data and real body intact", () => {
  const code = "# literal [".repeat(12_000) + "\nconst source = '**not markup**';";
  assert.equal(mdToPlain("```js\n" + code + "\n```\n**结尾正文**"), code + "\n结尾正文");
  const unmatched = "保留 " +
    Array.from({ length: 65 }, (_, index) => "`".repeat(index + 1) + "x ").join("");
  assert.equal(mdToPlain(unmatched), unmatched);
  assert.equal(mdToPlain("`原样 **代码**` 和 ``包含 ` 反引号``"), "原样 **代码** 和 包含 ` 反引号");
  const marker = "\uE000";
  assert.equal(
    mdToPlain(marker.repeat(4000) + " https://example.com/__raw__"),
    marker.repeat(4000) + " https://example.com/__raw__",
  );
  assert.equal(
    mdToPlain("https://example.com/" + marker + "__raw__"),
    "https://example.com/" + marker + "__raw__",
  );
  assert.equal(
    mdToPlain("https://example.com/[](https://target.com/)"),
    "https://example.com/https://target.com/",
  );
});
