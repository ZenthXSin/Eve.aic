import type { NoteRevision, NoteSource } from "../../contracts/notes.ts";
import { fingerprint } from "../../kernel/calls.ts";
import { type Json, KernelError } from "../../kernel/protocol.ts";
import { FactJournal } from "../../sdk/fact-journal.ts";
import { regexMatches } from "./regex.ts";
import { RollbackQuarantine } from "./quarantine.ts";

export function textPage(text: string, start = 0, limit = 4096) {
  if (
    !Number.isSafeInteger(start) || start < 0 || !Number.isSafeInteger(limit) || limit < 1 ||
    limit > 16384
  ) {
    throw new KernelError("rejected", "文本页大小或位置无效");
  }
  const chars = Array.from(text), shown = chars.slice(start, start + limit);
  return {
    text: shown.join(""),
    characters: chars.length,
    ...(start + shown.length < chars.length ? { nextChar: start + shown.length } : {}),
  };
}
function sourcesValid(sources: NoteSource[]) {
  return Array.isArray(sources) && sources.length > 0 && sources.length <= 32 &&
    sources.every((s) =>
      s && ["experienced", "heard", "imagined"].includes(s.kind) &&
      Number.isFinite(Date.parse(s.at)) &&
      typeof s.description === "string" && s.description.length > 0 && s.description.length <= 4096
    );
}
function valid(note: NoteRevision) {
  return note && /^[a-f0-9]{64}$/.test(note.id) && Number.isSafeInteger(note.revision) &&
    note.revision > 0 &&
    (note.basedOnRevision === undefined ||
      Number.isSafeInteger(note.basedOnRevision) && note.basedOnRevision >= 1 &&
        note.basedOnRevision < note.revision) &&
    typeof note.title === "string" && note.title.length > 0 && note.title.length <= 512 &&
    typeof note.group === "string" && note.group.length <= 512 &&
    typeof note.summary === "string" && note.summary.length <= 4096 &&
    typeof note.content === "string" &&
    new TextEncoder().encode(note.content).length <= 2_097_152 &&
    sourcesValid(note.sources);
}
/** 便签是按需读取的工具；版本原件只追加，永不自动挂载到提示词。 */
export class NoteStore {
  readonly journal: FactJournal;
  #tail: Promise<unknown> = Promise.resolve();
  readonly quarantine: RollbackQuarantine;
  constructor(
    directory: string,
    readonly embed?: (text: string, purpose?: "query" | "passage") => Promise<number[]>,
    readonly records?: FactJournal,
  ) {
    this.journal = new FactJournal(directory);
    this.quarantine = new RollbackQuarantine(records);
  }
  async initialize() {
    await this.journal.initialize();
    const latest = new Map<string, NoteRevision & { requestId: string }>();
    try {
      for (const event of this.journal.events) {
        const note = event.data as unknown as NoteRevision & { requestId: string };
        if (
          event.kind !== "note" || !valid(note) ||
          !this.quarantine.validateContext(note.contextRecordSeq) || note.recordSeq !== event.seq ||
          note.revision !== (latest.get(note.id)?.revision ?? 0) + 1 ||
          typeof note.requestId !== "string"
        ) {
          throw new KernelError("corrupt", "便签版本原件异常，保留现场");
        }
        latest.set(note.id, note);
      }
    } catch (error) {
      await this.journal.close();
      throw error;
    }
  }
  #revisions() {
    return this.journal.events.map((e) =>
      e.data as unknown as NoteRevision & { requestId: string }
    );
  }
  receipt(requestId: string): NoteRevision | undefined {
    const saved = this.#revisions().find((n) => n.requestId === requestId);
    if (!saved) return;
    const { requestId: _requestId, ...note } = saved;
    return this.#annotate(note);
  }
  #annotate<T extends NoteRevision>(note: T): T {
    return structuredClone({ ...note, ...this.quarantine.annotation(note.contextRecordSeq) });
  }
  #accepted(note: NoteRevision) {
    return !this.quarantine.excluded(note.contextRecordSeq).length;
  }
  async change(method: "create" | "update", args: Record<string, Json>, requestId: string) {
    const captured = structuredClone(args);
    const work = this.#tail.then(async () => {
      const oldReceipt = this.receipt(requestId);
      if (oldReceipt) return oldReceipt;
      const id = method === "create" ? await fingerprint(["note-v1", requestId]) : captured.id;
      const revisions = this.#revisions().filter((n) => n.id === id);
      const latest = revisions.at(-1);
      const previous = revisions.filter((n) => this.#accepted(n)).at(-1);
      if (method === "update" && (!previous || previous.revision !== captured.expectedRevision)) {
        throw new KernelError("rejected", "便签版本已经变化或不存在，请重新读取");
      }
      const note = {
        id,
        revision: (latest?.revision ?? 0) + 1,
        recordSeq: this.journal.events.length + 1,
        at: new Date().toISOString(),
        title: captured.title ?? previous?.title,
        group: captured.group ?? previous?.group ?? "",
        summary: captured.summary ?? previous?.summary ?? "",
        content: captured.content ?? previous?.content,
        sources: captured.sources ?? previous?.sources,
        ...(previous ? { basedOnRevision: previous.revision } : {}),
        ...(this.records ? { contextRecordSeq: this.quarantine.capture() } : {}),
      } as NoteRevision;
      if (!valid(note)) throw new KernelError("rejected", "便签需要标题、内容和有效来源");
      await this.journal.append("note", { ...note, requestId } as unknown as Json);
      return this.#annotate(note);
    });
    this.#tail = work.catch(() => {});
    return await work;
  }
  get(
    args: {
      id: string;
      revision?: number;
      startChar?: number;
      limit?: number;
      includeUnconfirmed?: boolean;
    },
  ) {
    const found = this.#revisions().filter((n) =>
      n.id === args.id &&
      (args.revision === undefined
        ? args.includeUnconfirmed || this.#accepted(n)
        : args.revision === n.revision)
    ).at(-1);
    if (!found) throw new KernelError("not_found", "没有该便签版本");
    const { content, requestId: _requestId, ...note } = this.#annotate(found);
    return { note, ...textPage(content, args.startChar, args.limit) };
  }
  list(
    args: {
      group?: string;
      cursor?: { snapshotThroughSeq: number; afterSeq: number };
      includeUnconfirmed?: boolean;
    } = {},
  ) {
    const through = args.cursor?.snapshotThroughSeq ?? this.journal.events.length;
    const after = args.cursor?.afterSeq ?? 0;
    if (
      !Number.isSafeInteger(through) || through < 0 || through > this.journal.events.length ||
      !Number.isSafeInteger(after) || after < 0 || after > through
    ) {
      throw new KernelError("rejected", "便签目录游标无效");
    }
    const latest = new Map<string, NoteRevision & { requestId: string }>();
    for (const note of this.#revisions()) {
      if (note.recordSeq <= through && (args.includeUnconfirmed || this.#accepted(note))) {
        latest.set(note.id, note);
      }
    }
    const all = [...latest.values()].filter((n) =>
      n.recordSeq > after && (args.group === undefined || n.group === args.group)
    ).sort((a, b) => a.recordSeq - b.recordSeq);
    const page = all.slice(0, 8);
    return {
      items: page.map((note) => {
        const { content: _content, requestId: _requestId, sources: _sources, ...metadata } = this
          .#annotate(note);
        return metadata;
      }),
      snapshotThroughSeq: through,
      ...(all.length > page.length
        ? { older: { snapshotThroughSeq: through, afterSeq: page.at(-1)!.recordSeq } }
        : {}),
    };
  }
  versions(args: { id: string; afterRevision?: number }) {
    const after = args.afterRevision ?? 0;
    if (!Number.isSafeInteger(after) || after < 0) {
      throw new KernelError("rejected", "便签版本游标无效");
    }
    const all = this.#revisions().filter((n) => n.id === args.id && n.revision > after);
    return {
      items: all.slice(0, 8).map((revision) => {
        const { content: _content, requestId: _requestId, ...note } = this.#annotate(revision);
        return note;
      }),
      hasMore: all.length > 8,
    };
  }
  async search(
    args: {
      query: string;
      mode?: string;
      group?: string;
      afterSeq?: number;
      includeUnconfirmed?: boolean;
    },
  ) {
    if (
      typeof args.query !== "string" || !args.query || args.query.length > 512 ||
      !["literal", "regex", "semantic"].includes(args.mode ?? "literal")
    ) throw new KernelError("rejected", "搜索参数无效");
    const after = args.afterSeq ?? 0;
    if (!Number.isSafeInteger(after) || after < 0) {
      throw new KernelError("rejected", "搜索游标无效");
    }
    const latest = new Map<string, NoteRevision>();
    for (const note of this.#revisions()) {
      if (args.includeUnconfirmed || this.#accepted(note)) latest.set(note.id, note);
    }
    const candidates = [...latest.values()].filter((n) =>
      n.recordSeq > after && (args.group === undefined || n.group === args.group)
    ).sort((a, b) => a.recordSeq - b.recordSeq);
    const generation = this.quarantine.generation();
    const assertCurrent = () => {
      if (!args.includeUnconfirmed && generation !== this.quarantine.generation()) {
        throw new KernelError("rejected", "搜索期间发生换版回滚，请重新读取已接受的便签版本");
      }
    };
    if (args.mode === "semantic") {
      if (!this.embed) {
        throw new KernelError("unavailable", "语义检索模型接入为 TODO，请先使用 literal 或 regex");
      }
      const query = await this.embed(args.query, "query");
      const scored = [];
      for (const note of candidates) {
        const v = await this.embed(`${note.title}\n${note.summary}\n${note.content}`);
        const score = query.reduce((sum, x, i) => sum + x * v[i], 0);
        scored.push({
          id: note.id,
          revision: note.revision,
          title: note.title,
          excerpt: Array.from(note.content).slice(0, 240).join(""),
          score,
          recordSeq: note.recordSeq,
          ...this.quarantine.annotation(note.contextRecordSeq),
        });
      }
      scored.sort((a, b) => b.score - a.score);
      assertCurrent();
      return {
        items: scored.slice(0, 8),
        throughSeq: this.journal.events.length,
        hasMore: scored.length > 8,
      };
    }
    const searchable = (n: NoteRevision) => `${n.title}\n${n.summary}\n${n.content}`;
    const matches = args.mode === "regex"
      ? await regexMatches(args.query, candidates, searchable)
      : candidates.filter((n) => searchable(n).includes(args.query));
    const page = matches.slice(0, 8);
    assertCurrent();
    return {
      items: page.map((n) => ({
        id: n.id,
        revision: n.revision,
        title: n.title,
        excerpt: Array.from(n.content).slice(0, 240).join(""),
        ...this.quarantine.annotation(n.contextRecordSeq),
      })),
      throughSeq: matches.length > 8 ? page.at(-1)!.recordSeq : this.journal.events.length,
      hasMore: matches.length > 8,
    };
  }
  async close() {
    await this.#tail;
    await this.journal.close();
  }
}
