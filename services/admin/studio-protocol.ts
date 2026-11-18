import { type Json, KernelError } from "../../kernel/protocol.ts";

export type StudioRequest =
  | { action: "settings" }
  | { action: "settingsUpdate"; revision: string; changes: Record<string, Json> }
  | { action: "chat"; before?: number }
  | { action: "chatSend"; id: string; text: string };

function record(value: unknown): value is Record<string, Json> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

export function validateStudio(value: unknown): StudioRequest {
  if (!record(value)) throw new KernelError("rejected", "需要工作室请求对象");
  const keys = Object.keys(value);
  if (value.action === "settings" && keys.length === 1) return { action: "settings" };
  if (value.action === "chat" && keys.every((key) => key === "action" || key === "before")) {
    if (value.before === undefined) return { action: "chat" };
    if (Number.isSafeInteger(value.before) && Number(value.before) > 0) {
      return { action: "chat", before: Number(value.before) };
    }
  }
  if (
    value.action === "chatSend" && keys.length === 3 && typeof value.id === "string" &&
    /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(value.id) &&
    typeof value.text === "string" && value.text.trim() && value.text.length <= 4096 &&
    !/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/u.test(value.text)
  ) {
    return { action: "chatSend", id: value.id, text: value.text.trim() };
  }
  if (
    value.action === "settingsUpdate" && keys.length === 3 && typeof value.revision === "string" &&
    /^[0-9a-f]{64}$/.test(value.revision) && record(value.changes)
  ) {
    const fields = Object.entries(value.changes);
    if (
      fields.length > 0 && fields.length <= 128 &&
      fields.every(([key, val]) =>
        /^[a-zA-Z][a-zA-Z0-9_.-]{0,95}$/.test(key) &&
        !["constructor", "prototype", "__proto__"].includes(key) &&
        (typeof val === "boolean" || (typeof val === "number" && Number.isFinite(val)) ||
          (typeof val === "string" && val.length <= 1024))
      )
    ) return { action: "settingsUpdate", revision: value.revision, changes: value.changes };
  }
  throw new KernelError("rejected", "工作室参数无效，请刷新页面并检查输入");
}

export function studioRead(url: URL): StudioRequest | undefined {
  if (url.pathname === "/api/settings" && !url.search) return { action: "settings" };
  if (url.pathname !== "/api/chat" && url.pathname !== "/api/settings") return;
  const query = [...url.searchParams];
  if (url.pathname === "/api/chat") {
    if (query.length === 0) return { action: "chat" };
    if (query.length === 1 && query[0][0] === "before" && /^[1-9]\d{0,15}$/.test(query[0][1])) {
      return validateStudio({ action: "chat", before: Number(query[0][1]) });
    }
  }
  throw new KernelError("rejected", "工作室分页参数无效");
}
