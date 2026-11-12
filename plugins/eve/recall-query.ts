import type { Json } from "../../kernel/protocol.ts";

const LIMIT = 16_000;
const TEXT = new Set(["text", "content", "description", "observation", "output"]);
function object(value: Json | undefined): value is Record<string, Json> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

/** 回忆检索只取可读正文；平台句柄、代码与先前召回结果不构成新话题。 */
export function recallQuery(events: readonly { kind: string; data: Json }[]): string {
  let query = "";
  for (const event of events.slice(-4)) {
    if (!object(event.data)) continue;
    let visited = 0;
    const text = (value: string) => {
      if (value.trim()) query = (query + "\n" + value.slice(-LIMIT)).slice(-LIMIT);
    };
    const collect = (value: Json, key = "", depth = 0): void => {
      if (++visited > 512 || depth > 12) return;
      if (typeof value === "string") {
        if (!TEXT.has(key)) return;
        // 工具JSON须先投影正文，不能把序列化的conversationId/code当正文。
        if (key === "output" && /^[\s]*[\[{]/.test(value)) {
          if (value.length > 65_536) return;
          try {
            collect(JSON.parse(value), "", depth + 1);
          } catch { /* 不完整JSON保留原件，不用于回忆。 */ }
        } else text(value);
      } else if (Array.isArray(value)) {
        for (const item of value) {
          collect(item, key, depth + 1);
          if (visited > 512) break;
        }
      } else if (object(value)) {
        for (const field in value) {
          // 这些容器中的文字是参数、来源或原图，不能作为新观察正文。
          if (
            ["args", "toolCalls", "code", "handle", "provenance", "images", "entities", "sources"]
              .includes(field)
          ) continue;
          collect(value[field], field, depth + 1);
          if (visited > 512) break;
        }
      }
    };
    if (event.kind === "model_response") {
      const response = event.data.response;
      if (object(response) && typeof response.content === "string") text(response.content);
    } else if (event.kind === "tool_result") {
      const result = event.data.result;
      if (object(result) && typeof result.output === "string") collect(result.output, "output");
    } else if (event.kind === "notice" && event.data.source !== "memory") {
      collect(event.data);
    }
  }
  return query.trim();
}
