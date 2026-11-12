/** Pure presentation helpers. Never read files, submit messages or allocate action IDs. */

/** Convert common Markdown chat markup to readable text without interpreting HTML. */
export function mdToPlain(text: string): string {
  const lines = text.replace(/\r\n?/g, "\n").split("\n");
  let fence: { marker: string; length: number } | undefined;
  return lines.map((line) => {
    const opening = /^ {0,3}(`{3,}|~{3,})(.*)$/.exec(line);
    if (fence) {
      if (
        opening && opening[1][0] === fence.marker && opening[1].length >= fence.length &&
        !opening[2].trim()
      ) {
        fence = undefined;
        return undefined;
      }
      return line; // Code is copied exactly, including markup-looking text and indentation.
    }
    if (opening && !(opening[1][0] === "`" && opening[2].includes("`"))) {
      fence = { marker: opening[1][0], length: opening[1].length };
      return undefined;
    }
    line = line.replace(/^ {0,3}#{1,6}[\t ]+(.+?)(?:[\t ]+#+[\t ]*)?$/, "$1")
      .replace(/^ {0,3}(?:>[\t ]?)+/, "")
      .replace(/^(\s*)[-+*][\t ]+/, "$1• ");
    return inlinePlain(line);
  }).filter((line) => line !== undefined).join("\n");
}

function inlinePlain(line: string): string {
  // Inline code is a literal island: remove only its matching delimiter, not its contents.
  const pieces: string[] = [];
  let start = 0, index = 0;
  while (index < line.length) {
    if (line[index] === "\\" && /[\\`*_[\]{}()#+.!>~-]/.test(line[index + 1] ?? "")) {
      pieces.push(markupPlain(line.slice(start, index)), line[index + 1]);
      index += 2;
      start = index;
      continue;
    }
    if (line[index] !== "`" || (index > 0 && line[index - 1] === "\\")) {
      index++;
      continue;
    }
    let end = index + 1;
    while (line[end] === "`") end++;
    const delimiter = line.slice(index, end), close = line.indexOf(delimiter, end);
    if (close < 0) {
      index = end;
      continue;
    }
    pieces.push(markupPlain(line.slice(start, index)), line.slice(end, close));
    index = close + delimiter.length;
    start = index;
  }
  pieces.push(markupPlain(line.slice(start)));
  return pieces.join("");
}

function markupPlain(text: string): string {
  // Protect URL data while removing emphasis around a whole link. The prefix is
  // deterministic and absent from input, so user text cannot collide with a token.
  let prefix = "\uE000qq-url-";
  while (text.includes(prefix)) prefix += "x";
  const urls: string[] = [];
  const protect = (url: string) => `${prefix}${urls.push(url) - 1}\uE001`;
  // Balanced URL parentheses survive link conversion, e.g. /wiki/Foo_(bar).
  let linked = "", index = 0;
  while (index < text.length) {
    const image = text[index] === "!" && text[index + 1] === "[";
    const begin = index + (image ? 1 : 0);
    if (text[begin] === "[" && (index === 0 || text[index - 1] !== "\\")) {
      const labelEnd = text.indexOf("](", begin + 1);
      if (labelEnd >= 0) {
        let cursor = labelEnd + 2, depth = 1;
        while (cursor < text.length && depth) {
          if (text[cursor] === "\\") {
            cursor += 2;
            continue;
          }
          if (text[cursor] === "(") depth++;
          if (text[cursor] === ")") depth--;
          if (depth) cursor++;
        }
        if (!depth) {
          const label = text.slice(begin + 1, labelEnd);
          const destination = text.slice(labelEnd + 2, cursor).trim()
            .replace(/^<([^>]+)>$/, "$1");
          // Keep destinations and optional titles verbatim: no URL or visible body is discarded.
          linked += label === destination
            ? protect(destination)
            : label
            ? `${label} (${protect(destination)})`
            : protect(destination);
          index = cursor + 1;
          continue;
        }
      }
    }
    linked += text[index++];
  }
  // URLs remain literal islands too; emphasis-like characters can be valid URL data.
  const protectedText = linked.replace(/<(https?:\/\/[^<>\s]+)>/g, "$1")
    .replace(/https?:\/\/[^\s<>]+/g, protect);
  return emphasisPlain(protectedText).replace(
    new RegExp(`${prefix}(\\d+)\uE001`, "g"),
    (_match, index: string) => urls[Number(index)],
  );
}

function emphasisPlain(text: string): string {
  return text.replace(/\*\*(?=\S)(.+?\S|\S)\*\*/g, "$1")
    .replace(/__(?=\S)(.+?\S|\S)__/g, "$1")
    .replace(/~~(?=\S)(.+?\S|\S)~~/g, "$1")
    .replace(/(^|[^\p{L}\p{N}])\*(?=\S)([^*\n]+?\S|\S)\*(?=$|[^\p{L}\p{N}])/gu, "$1$2")
    .replace(/(^|[^\p{L}\p{N}])_(?=\S)([^_\n]+?\S|\S)_(?=$|[^\p{L}\p{N}])/gu, "$1$2")
    .replace(/\\([\\`*_[\]{}()#+.!>~-])/g, "$1");
}

export interface QqSplitOptions {
  maxCharacters?: number;
  maxParts?: number;
}

/**
 * Split ordinary chat text before action admission. No Markdown conversion, truncation,
 * file processing, or sending occurs here. Joining returned parts reproduces the input.
 * Long URLs and fenced code are rejected instead of corrupting them into chat fragments.
 */
export function splitForQQ(text: string, options: QqSplitOptions = {}): string[] {
  const maximum = options.maxCharacters ?? 30, maxParts = options.maxParts ?? 4;
  if (
    !Number.isSafeInteger(maximum) || maximum < 1 || maximum > 10_000 ||
    !Number.isSafeInteger(maxParts) || maxParts < 1 || maxParts > 32
  ) {
    throw new RangeError("QQ 分条参数无效：每条 1–10000 字，1–32 条。");
  }
  if (!text.trim()) return [];
  if (/^ {0,3}(`{3,}|~{3,})/m.test(text)) {
    throw new RangeError("代码块请保留为明确长文或文件，不自动拆成聊天消息。");
  }
  const segments = [...new Intl.Segmenter(undefined, { granularity: "grapheme" }).segment(text)]
    .map((segment) => segment.segment);
  const parts: string[] = [];
  let start = 0;
  while (start < segments.length) {
    if (parts.length === maxParts) {
      throw new RangeError(
        `QQ 普通聊天超过 ${maxParts} 条，请先精简或明确作为长文发送；原文未截断。`,
      );
    }
    let end = Math.min(start + maximum, segments.length);
    if (end < segments.length) {
      const minimum = start + Math.ceil(maximum / 2);
      const choose = (pattern: RegExp) => {
        for (let cursor = end; cursor >= minimum; cursor--) {
          if (pattern.test(segments[cursor - 1])) return cursor;
        }
        return 0;
      };
      end = choose(/[\n。！？!?；;]/u) || choose(/[，,、：:\s]/u) || end;
      // URLs are atomic. A short prefix is preferable to breaking a link in half.
      const prefix = segments.slice(0, start).join("").length;
      const offset = segments.slice(0, end).join("").length;
      for (const match of text.matchAll(/https?:\/\/[^\s<>]+/g)) {
        const urlStart = match.index, urlEnd = urlStart + match[0].length;
        if (urlStart < offset && urlEnd > offset && urlEnd > prefix) {
          if (urlStart <= prefix) {
            throw new RangeError("链接超过单条字数，请保留为明确长文，不拆开 URL。");
          }
          end = start + [...new Intl.Segmenter(undefined, { granularity: "grapheme" })
            .segment(text.slice(prefix, urlStart))].length;
          break;
        }
      }
    }
    parts.push(segments.slice(start, end).join(""));
    start = end;
  }
  return parts;
}
