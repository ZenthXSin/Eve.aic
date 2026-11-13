import type { PhoneMessage } from "../../contracts/phone.ts";
import type { InboundRecord } from "./journal.ts";

/** Derived only from platform envelopes and known senders, never from nicknames' claimed roles. */
export function contactIndex(
  records: InboundRecord[],
  index = { names: new Map<string, string>(), conversations: new Map<string, string>() },
) {
  const { names, conversations } = index;
  for (const record of records) {
    const m = record.message.message, raw = record.raw.d as Record<string, unknown>;
    if (m.sender.displayName) {
      names.set(JSON.stringify([m.conversationId, m.sender.platformUserId]), m.sender.displayName);
    }
    const group = raw?.group as Record<string, unknown> | undefined;
    const name = m.conversationId.startsWith("qq:user:")
      ? m.sender.displayName
      : raw?.group_name ?? group?.name;
    if (typeof name === "string" && name && name.length <= 512) {
      conversations.set(m.conversationId, name);
    }
    for (const entry of Array.isArray(raw?.mentions) ? raw.mentions : []) {
      const id = mentionId(entry),
        display = entry?.member?.nick ?? entry?.username ?? entry?.nickname ?? entry?.nick ??
          entry?.name;
      if (id && typeof display === "string" && display && display.length <= 512) {
        names.set(JSON.stringify([m.conversationId, id]), display);
      }
    }
  }
  return { names, conversations };
}
function mentionId(entry: Record<string, unknown> | null | undefined) {
  const value = entry?.user_openid ?? entry?.member_openid ?? entry?.user_id ?? entry?.openid ??
    entry?.id;
  return typeof value === "string" && value.length <= 512 && value ? value : undefined;
}
export function messageMentions(
  record: InboundRecord,
  names: Map<string, string>,
  self: { id: string; displayName: string },
): NonNullable<PhoneMessage["mentions"]> {
  const raw = record.raw.d as { mentions?: Record<string, unknown>[] };
  const entries = new Map<string, { isSelf?: boolean }>();
  for (const entry of Array.isArray(raw?.mentions) ? raw.mentions : []) {
    const id = mentionId(entry);
    if (id) {
      entries.set(id, {
        ...(entry.scope === "single" && entry.is_you === true ? { isSelf: true } : {}),
      });
    }
  }
  for (const match of record.message.message.text.matchAll(/<@!?(\w[\w-]{0,511})>/g)) {
    if (!entries.has(match[1])) entries.set(match[1], {});
  }
  return [...entries].map(([platformUserId, metadata]) => {
    const isSelf = platformUserId === self.id || metadata.isSelf === true;
    const displayName = isSelf
      ? self.displayName
      : names.get(JSON.stringify([record.message.message.conversationId, platformUserId]));
    return {
      platformUserId,
      ...(displayName ? { displayName } : {}),
      resolved: !!displayName,
      ...(isSelf ? { isSelf: true } : {}),
    };
  });
}
