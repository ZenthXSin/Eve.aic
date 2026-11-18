import type { MessageScreen } from "../../contracts/phone.ts";
import { qqDestination } from "../../contracts/qq-conversation.ts";
import type { Json } from "../../kernel/protocol.ts";

type Placement = "alone" | "opening" | "ending" | "followUp";
type Form = { text: string; placement: Placement };
type Sample = {
  seq: number;
  speaker: string;
  self: boolean;
  at: number | null;
  forms: Form[];
  feedback?: { text?: string; allow: boolean; targetSeq?: number; quoted?: boolean };
};
type Scope = {
  samples: Map<number, Sample>;
  feedback: Map<string, { seq: number; allow: boolean }>;
};
type Hint = { forms: Form[]; avoid: string[] };

const SAMPLES = 48;
const SCOPES = 32;
const FEEDBACK = 16;
const ATOM = /^[a-zA-Z]{2,9}$|^[\p{Script=Han}]{2,4}$|^[（(]$/u;
const FRAGMENT = /^(?:[a-zA-Z]{2,9}|([\p{Script=Han}])\1{1,2}|[（(])$/u;
const MAX_AGE = 21 * 24 * 60 * 60_000;

function object(value: Json | undefined): value is Record<string, Json> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function key(text: string): string {
  return text === "（" ? "(" : text.toLowerCase();
}

function forms(text: string): Form[] {
  if (!text || text.length > 140 || /[\r\n`]|https?:\/\//i.test(text)) return [];
  if (ATOM.test(text)) return [{ text, placement: "alone" }];
  const found: Form[] = [];
  if (/[\p{Script=Han}]/u.test(text)) {
    const opening = text.match(/^([a-zA-Z]{2,9})(?=\s|[\p{Script=Han}，、。？！])/u)?.[1];
    if (opening) found.push({ text: opening, placement: "opening" });
    const ending = text.match(/(?<![a-zA-Z0-9_])([a-zA-Z]{2,9})$/)?.[1];
    if (ending) found.push({ text: ending, placement: "ending" });
    const marker = text.match(/[（(]$/)?.[0];
    if (marker) found.push({ text: marker, placement: "ending" });
  }
  return found;
}

function feedback(text: string): Sample["feedback"] {
  if (/^(?:以后)?别(?:老|总|再)?这么(?:说|发)[了。！!]*$/u.test(text)) return { allow: false };
  const match = text.match(
    /^(?:以后)?(别(?:老|总|再)?|少|可以(?:再)?)(?:说|用|发|加)\s*(?:[“「『"']([^“”「」『』"']+)[”」』"']|([a-zA-Z]{2,9}|[\p{Script=Han}]{2,4}?|[（(]))[了。！!]*$/u,
  );
  if (!match) return;
  const form = match[2] ?? match[3];
  if (ATOM.test(form)) return { text: form, allow: match[1].startsWith("可以") };
}

function screen(value: Json): MessageScreen | undefined {
  if (!object(value) || !object(value.handle) || !object(value.sender) || !object(value.body)) {
    return;
  }
  const { conversationId, seq } = value.handle;
  if (
    !qqDestination(conversationId) ||
    typeof seq !== "number" || !Number.isSafeInteger(seq) || seq < 1 ||
    typeof value.sender.platformUserId !== "string" || !value.sender.platformUserId ||
    (value.direction !== "incoming" && value.direction !== "outgoing") ||
    typeof value.body.text !== "string" || value.body.truncated !== false
  ) return;
  return value as unknown as MessageScreen;
}

export class ChatStyleMemory {
  #scopes = new Map<string, Scope>();

  #applyFeedback(scope: Scope, sample: Sample, previous: Sample | undefined) {
    if (!sample.feedback) return;
    const target = sample.feedback.quoted
      ? scope.samples.get(sample.feedback.targetSeq ?? 0)
      : previous && previous.seq === sample.seq - 1 && previous.at !== null && sample.at !== null &&
          sample.at >= previous.at && sample.at - previous.at <= 120_000
      ? previous
      : undefined;
    const candidates = sample.feedback.text
      ? [sample.feedback.text]
      : target?.self
      ? target.forms.map((form) => form.text)
      : [];
    for (const candidate of candidates) {
      const id = key(candidate);
      if ((scope.feedback.get(id)?.seq ?? 0) >= sample.seq) continue;
      scope.feedback.set(id, { seq: sample.seq, allow: sample.feedback.allow });
    }
  }

  #hint(scope: Scope, isDirect: boolean, now: number): Hint {
    const picks = new Map<
      string,
      { form: Form; count: number; speakers: Set<string>; seq: number }
    >();
    const ordered = [...scope.samples.values()].sort((first, second) => first.seq - second.seq);
    let previous: Sample | undefined;
    for (const sample of ordered) {
      this.#applyFeedback(scope, sample, previous);
      previous = sample;
    }
    const recentFeedback = [...scope.feedback.entries()].sort(([, first], [, second]) =>
      second.seq - first.seq
    );
    for (const [id] of recentFeedback.slice(FEEDBACK)) scope.feedback.delete(id);
    previous = undefined;
    for (const sample of ordered) {
      const before = previous;
      previous = sample;
      if (sample.self || sample.feedback || sample.at === null || now - sample.at > MAX_AGE) {
        continue;
      }
      for (const observed of sample.forms) {
        if (scope.feedback.get(key(observed.text))?.allow === false) continue;
        const follows = observed.placement === "alone" && before?.speaker === sample.speaker &&
          before.at !== null && sample.at !== null && sample.at >= before.at &&
          sample.at - before.at <= 15_000;
        const form: Form = { ...observed, placement: follows ? "followUp" : observed.placement };
        const id = `${key(form.text)}:${form.placement}`;
        const pick = picks.get(id) ?? { form, count: 0, speakers: new Set<string>(), seq: 0 };
        pick.count++;
        pick.speakers.add(sample.speaker);
        pick.seq = sample.seq;
        picks.set(id, pick);
      }
    }
    const avoid = recentFeedback.slice(0, FEEDBACK).filter(([, entry]) => !entry.allow).map((
      [id],
    ) => id);
    const selected = [...picks.values()].filter(({ form, count, speakers }) =>
      count >= (FRAGMENT.test(form.text) ? 3 : 4) && (isDirect || speakers.size >= 2) &&
      scope.feedback.get(key(form.text))?.allow !== false
    ).sort((first, second) => second.count - first.count || second.seq - first.seq);
    return { forms: selected.slice(0, 4).map(({ form }) => form), avoid };
  }

  project(method: string, value: Json, now = Date.now()): Json {
    const page = method === "notifications"
      ? "items"
      : method === "open" || method === "history"
      ? "messages"
      : undefined;
    if (!page || !object(value) || !Array.isArray(value[page])) return value;
    const items = value[page];
    const latest = new Map<string, { index: number; seq: number }>();
    const touched = new Set<string>();
    for (const [index, item] of items.entries()) {
      const message = screen(item);
      if (!message) continue;
      const { conversationId, seq } = message.handle;
      const self = message.direction === "outgoing";
      const isDirect = /^qq:(?:user|guild-dm):/.test(conversationId);
      const text = message.body.text.trim();
      const scope: Scope = this.#scopes.get(conversationId) ?? {
        samples: new Map(),
        feedback: new Map(),
      };
      touched.add(conversationId);
      this.#scopes.delete(conversationId);
      this.#scopes.set(conversationId, scope);
      while (this.#scopes.size > SCOPES) this.#scopes.delete(this.#scopes.keys().next().value!);
      if (!scope.samples.has(seq)) {
        const at = message.time?.status === "known" ? Date.parse(message.time.at ?? "") : NaN;
        const correction = !self && (isDirect || message.sender.relationship === "owner")
          ? feedback(text)
          : undefined;
        const target = message.reference?.target;
        scope.samples.set(seq, {
          seq,
          speaker: message.sender.platformUserId,
          self,
          at: Number.isFinite(at) ? at : null,
          forms: correction || self && message.delivery?.state !== "sent" ? [] : forms(text),
          ...(correction
            ? {
              feedback: {
                ...correction,
                ...(message.reference
                  ? {
                    quoted: true,
                    targetSeq: target?.conversationId === conversationId &&
                        target.seq < seq
                      ? target.seq
                      : undefined,
                  }
                  : {}),
              },
            }
            : {}),
        });
      }
      if (!self && seq > (latest.get(conversationId)?.seq ?? 0)) {
        latest.set(conversationId, { index, seq });
      }
    }
    let projected: Json[] | undefined;
    for (const conversationId of touched) {
      const scope = this.#scopes.get(conversationId);
      if (!scope) continue;
      const ordered = [...scope.samples.keys()].sort((first, second) => second - first);
      for (const seq of ordered.slice(SAMPLES)) scope.samples.delete(seq);
      const index = latest.get(conversationId)?.index;
      if (index === undefined) continue;
      const hint = this.#hint(scope, /^qq:(?:user|guild-dm):/.test(conversationId), now);
      if (!hint.forms.length && !hint.avoid.length) continue;
      projected ??= [...items];
      projected[index] = { ...items[index] as Record<string, Json>, chatStyle: hint };
    }
    return projected ? { ...value, [page]: projected } : value;
  }
}
