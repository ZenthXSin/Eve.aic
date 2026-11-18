export type QqChatKind = "user" | "group" | "channel" | "guild-dm";

export function qqDestination(value: unknown): { kind: QqChatKind; openId: string } | undefined {
  if (typeof value !== "string") return;
  const match = /^qq:(user|group|channel|guild-dm):([^\r\n]{1,512})$/u.exec(value);
  if (!match) return;
  if ((match[1] === "channel" || match[1] === "guild-dm") && !/^[\w-]{1,128}$/.test(match[2])) {
    return;
  }
  return { kind: match[1] as QqChatKind, openId: match[2] };
}
