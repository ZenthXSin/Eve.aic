import { qqDestination } from "../../contracts/qq-conversation.ts";
import type { QqSendText } from "./client.ts";

type GuildChatKind = "channel" | "guild-dm";

export function isGuildChatKind(kind: unknown): kind is GuildChatKind {
  return kind === "channel" || kind === "guild-dm";
}

export function guildChatDestination(kind: GuildChatKind, id: unknown) {
  if (typeof id !== "string" || !qqDestination(`qq:${kind}:${id}`)) return;
  return { kind, openId: id };
}

export function guildMessageId(value: unknown): value is string {
  return typeof value === "string" && value.length > 0 && value.length <= 512 &&
    !/[\s\p{Cc}]/u.test(value);
}

function onlyKeys(value: unknown, keys: readonly string[]): boolean {
  return value !== null && typeof value === "object" && !Array.isArray(value) &&
    Object.keys(value).every((key) => keys.includes(key));
}

export function guildChatSendPlan(request: QqSendText) {
  const { destination, text, format, attachment, referenceIndex, wakeup } = request;
  const { kind, openId } = destination;
  if (
    !onlyKeys(request, [
      "destination",
      "text",
      "format",
      "attachment",
      "referenceIndex",
      "wakeup",
      "reply",
    ]) ||
    request.reply !== undefined || !isGuildChatKind(kind) || !guildChatDestination(kind, openId) ||
    !onlyKeys(destination, ["kind", "openId"]) ||
    typeof text !== "string" || (!text.trim() && attachment === undefined) ||
    (format !== undefined && format !== "plain") || (wakeup !== undefined && wakeup !== false) ||
    (referenceIndex !== undefined && !guildMessageId(referenceIndex))
  ) return;
  if (
    attachment !== undefined &&
    (!onlyKeys(attachment, ["kind", "path"]) || attachment.kind !== "image" ||
      typeof attachment.path !== "string" || !attachment.path.startsWith("/") ||
      attachment.path.includes("\0") || referenceIndex !== undefined)
  ) return;

  const target = encodeURIComponent(openId);
  const body: Record<string, unknown> = { content: text };
  if (referenceIndex !== undefined) body.message_reference = { message_id: referenceIndex };
  return {
    kind: "guild" as const,
    path: kind === "channel" ? `/channels/${target}/messages` : `/dms/${target}/messages`,
    body,
  };
}

export async function guildImageBody(text: string, bytes: Uint8Array, filename: string) {
  const form = new FormData();
  if (text) form.set("content", text);
  form.set("file_image", new Blob([new Uint8Array(bytes)]), filename);
  const encoded = new Response(form);
  return {
    body: new Uint8Array(await encoded.arrayBuffer()),
    contentType: encoded.headers.get("content-type")!,
  };
}
