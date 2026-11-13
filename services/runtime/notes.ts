import type { NoteApi, NoteRevision, NoteSource } from "../../contracts/notes.ts";
import { countSelfTokens } from "../../sdk/token-count.ts";
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
function aliasValid(alias: unknown): alias is string {
  return typeof alias === "string" && Array.from(alias).length >= 1 &&
    Array.from(alias).length <= 64 && !/[\s\p{Cc}\p{Cf}\\/]/u.test(alias) &&
    !/^[a-f0-9]{64}$/i.test(alias);
}
function valid(note: NoteRevision) {
  return note && /^[a-f0-9]{64}$/.test(note.id) && Number.isSafeInteger(note.revision) &&
    note.revision > 0 &&
    (note.alias === undefined || aliasValid(note.alias)) &&
    (note.selfTokenCount === undefined ||
      Number.isSafeInteger(note.selfTokenCount.tokens) && note.selfTokenCount.tokens >= 0 &&
        typeof note.selfTokenCount.encoding === "string") &&
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
/** 版本原件只追加；自我便签按显式约定挂载到本轮末尾，其他便签按需读取。 */
export class NoteStore {
  #selfCache?: {
    seq: number;
    generation: number;
    about: NoteRevision | null;
    measured?: { tokens: number; encoding: string };
    ideas: { noteId: string; revision: number; text: string }[];
  };
  readonly journal: FactJournal;
  #tail: Promise<unknown> = Promise.resolve();
  readonly quarantine: RollbackQuarantine;
  constructor(
    directory: string,
    readonly embed?: (text: string, purpose?: "query" | "passage") => Promise<number[]>,
    readonly records?: FactJournal,
    readonly count?: (text: string) => Promise<{ tokens: number; encoding: string }>,
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
  #latest(includeUnconfirmed = false) {
    const latest = new Map<string, NoteRevision>();
    for (const note of this.#revisions()) {
      if (includeUnconfirmed || this.#accepted(note)) latest.set(note.id, note);
    }
    return latest;
  }
  #resolveId(reference: unknown, includeUnconfirmed = false): string {
    if (typeof reference !== "string") {
      throw new KernelError("rejected", "请提供便签永久 ID 或短名");
    }
    if (/^[a-f0-9]{64}$/.test(reference)) return reference;
    if (!aliasValid(reference)) throw new KernelError("rejected", "便签短名格式无效");
    const matches = [...this.#latest(includeUnconfirmed).values()].filter((n) =>
      n.alias === reference
    );
    if (matches.length > 1) {
      throw new KernelError("rejected", "便签短名因回滚产生冲突，请通过永久 ID 改名或移除短名");
    }
    if (!matches.length) throw new KernelError("not_found", "没有该便签短名");
    return matches[0].id;
  }
  async change(method: "create" | "update", args: Record<string, Json>, requestId: string) {
    const captured = structuredClone(args);
    const saved = this.receipt(requestId);
    if (saved) return saved;
    // 在进入写队列前固定目标和版本，不能等前一个写入完成后读最新版本来覆盖它。
    if (method === "update") {
      captured.id = this.#resolveId(captured.id);
      if (captured.expectedRevision === undefined) {
        captured.expectedRevision = this.get({ id: captured.id }).note.revision;
      }
    }
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
      const alias = captured.alias === null ? undefined : captured.alias ?? previous?.alias;
      if (alias !== undefined && !aliasValid(alias)) {
        throw new KernelError(
          "rejected",
          "便签短名需为 1–64 字符，不含空白、控制字符、斜线或完整 ID",
        );
      }
      if (
        alias !== undefined &&
        [...this.#latest().values()].some((n) => n.id !== id && n.alias === alias)
      ) throw new KernelError("rejected", "便签短名已被占用，请选择其他短名");
      const note = {
        id,
        ...(alias === undefined ? {} : { alias }),
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
      if (note.alias === "关于我") {
        const measured = await (this.count ?? countSelfTokens)(note.content);
        if (!Number.isSafeInteger(measured.tokens) || measured.tokens < 0) {
          throw new KernelError("rejected", "关于我的分词结果无效，原文未修改");
        }
        if (measured.tokens > 2000) {
          throw new KernelError(
            "rejected",
            `关于我为 ${measured.tokens} token（${measured.encoding}），上限2000；请自行精简，原文未修改`,
          );
        }
        note.selfTokenCount = measured;
      }
      await this.journal.append("note", { ...note, requestId } as unknown as Json);
      return this.#annotate(note);
    });
    this.#tail = work.catch(() => {});
    return await work;
  }
  async self(args: { idle?: boolean; limit?: number } = {}): ReturnType<NoteApi["self"]> {
    if (args.idle !== undefined && typeof args.idle !== "boolean") {
      throw new KernelError("rejected", "idle必须为布尔值");
    }
    const limit = args.limit ?? 3;
    if (!Number.isSafeInteger(limit) || limit < 1 || limit > 10) {
      throw new KernelError("rejected", "浮现条数须为1至10");
    }
    const generation = this.quarantine.generation();
    if (
      !this.#selfCache || this.#selfCache.seq !== this.journal.events.length ||
      this.#selfCache.generation !== generation
    ) {
      const all = [...this.#latest().values()];
      const abouts = all.filter((n) => n.alias === "关于我"),
        curiosities = all.filter((n) => n.alias === "想弄明白的事");
      if (abouts.length > 1 || curiosities.length > 1) {
        throw new KernelError("rejected", "自我便签短名冲突，请用永久编号核对");
      }
      const about = abouts[0] ?? null;
      const ideas: { noteId: string; revision: number; text: string }[] = [];
      for (const note of [about, curiosities[0]]) {
        if (!note) continue;
        let include = note.alias === "想弄明白的事";
        for (const line of note.content.split(/\r?\n/)) {
          if (/^\s*#{0,6}\s*最近想做的事\s*[:：]?\s*$/.test(line)) {
            include = true;
            continue;
          }
          if (/^\s*#{1,6}\s+/.test(line)) {
            include = note.alias === "想弄明白的事";
            continue;
          }
          if (include && line.trim() && !/^\s*[-*]\s+\[[xX]\]/.test(line)) {
            ideas.push({ noteId: note.id, revision: note.revision, text: line });
          }
        }
      }
      this.#selfCache = { seq: this.journal.events.length, generation, about, ideas };
    }
    const cache = this.#selfCache;
    const { about, ideas } = cache;
    const tokens = about
      ? cache.measured ??=
        about.selfTokenCount && about.selfTokenCount.encoding !== "utf8_byte_upper_bound"
          ? about.selfTokenCount
          : await (this.count ?? countSelfTokens)(about.content)
      : undefined;
    // Legacy oversized notes remain intact and readable; never silently truncate them into a persona.
    if (tokens && tokens.tokens > 2000) {
      throw new KernelError(
        "rejected",
        `已有关于我为 ${tokens.tokens} token（${tokens.encoding}），超过2000，请自行精简；原件保留`,
      );
    }
    const chosen: typeof ideas = [];
    const pool = args.idle
      ? [...new Map(ideas.map((idea) => [idea.text.trim(), idea])).values()]
      : [];
    while (args.idle && pool.length && chosen.length < limit) {
      const index = crypto.getRandomValues(new Uint32Array(1))[0] % pool.length;
      chosen.push(pool[index]);
      pool[index] = pool[pool.length - 1];
      pool.pop();
    }
    return structuredClone({
      aboutMe: about && tokens
        ? { id: about.id, revision: about.revision, content: about.content, ...tokens }
        : null,
      ideas: chosen,
    });
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
    const id = this.#resolveId(args.id, args.includeUnconfirmed);
    const found = this.#revisions().filter((n) =>
      n.id === id &&
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
    const id = this.#resolveId(args.id);
    const after = args.afterRevision ?? 0;
    if (!Number.isSafeInteger(after) || after < 0) {
      throw new KernelError("rejected", "便签版本游标无效");
    }
    const all = this.#revisions().filter((n) => n.id === id && n.revision > after);
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
          ...(note.alias === undefined ? {} : { alias: note.alias }),
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
    const searchable = (n: NoteRevision) =>
      `${n.alias ?? ""}\n${n.title}\n${n.summary}\n${n.content}`;
    const matches = args.mode === "regex"
      ? await regexMatches(args.query, candidates, searchable)
      : candidates.filter((n) => searchable(n).includes(args.query));
    const page = matches.slice(0, 8);
    assertCurrent();
    return {
      items: page.map((n) => ({
        id: n.id,
        ...(n.alias === undefined ? {} : { alias: n.alias }),
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
