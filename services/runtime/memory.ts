import type { MemorySource, MemoryView } from "../../contracts/memory.ts";
import { fingerprint } from "../../kernel/calls.ts";
import { type Json, KernelError } from "../../kernel/protocol.ts";
import { FactJournal } from "../../sdk/fact-journal.ts";
import { RollbackQuarantine } from "./quarantine.ts";

interface Formation {
  id: string;
  requestId: string;
  text: string;
  sources: MemorySource[];
  entities: string[];
  contextRecordSeq?: number;
}
/** 形成由同一个 Eve 主动完成；浮现、强化与淡忘只运行本机代码。原始依据永不删除。 */
export class MemoryStore {
  readonly journal: FactJournal;
  #tail: Promise<unknown> = Promise.resolve();
  readonly quarantine: RollbackQuarantine;
  constructor(
    directory: string,
    readonly records: FactJournal,
    readonly embed?: (text: string, purpose?: "query" | "passage") => Promise<number[]>,
    readonly now = Date.now,
  ) {
    this.journal = new FactJournal(directory);
    this.quarantine = new RollbackQuarantine(records);
  }
  async initialize() {
    await this.journal.initialize();
    const ids = new Set<string>();
    try {
      for (const event of this.journal.events) {
        if (event.kind === "formation") {
          const m = event.data as unknown as Formation;
          if (
            !m || !/^[a-f0-9]{64}$/.test(m.id) || ids.has(m.id) || typeof m.text !== "string" ||
            !m.text || !Array.isArray(m.entities) || m.entities.some((s) =>
              typeof s !== "string"
            ) || !this.#sources(m.sources) || !this.quarantine.validateContext(m.contextRecordSeq)
          ) throw new KernelError("corrupt", "记忆来源或形成记录异常");
          ids.add(m.id);
        } else if (event.kind === "recalled" || event.kind === "used") {
          const data = event.data as { ids: string[]; contextRecordSeq?: number };
          if (
            !Array.isArray(data.ids) || data.ids.some((id) => !ids.has(id)) ||
            !this.quarantine.validateContext(data.contextRecordSeq)
          ) {
            throw new KernelError("corrupt", "记忆强化引用异常");
          }
        } else throw new KernelError("corrupt", "未知记忆记录");
      }
    } catch (error) {
      await this.journal.close();
      throw error;
    }
  }
  #sources(sources: MemorySource[]): boolean {
    return Array.isArray(sources) && sources.length > 0 && sources.length <= 32 &&
      sources.every((s) =>
        s && Number.isSafeInteger(s.recordSeq) && s.recordSeq > 0 &&
        this.records.events[s.recordSeq - 1]?.digest === s.digest &&
        ["experienced", "heard", "imagined"].includes(s.kind)
      );
  }
  receipt(requestId: string) {
    const event = this.journal.events.find((e) =>
      e.kind === "formation" && (e.data as unknown as Formation).requestId === requestId
    );
    return event ? this.get({ id: (event.data as unknown as Formation).id }) : undefined;
  }
  async remember(
    args: { text: string; sources: MemorySource[]; entities?: string[] },
    requestId: string,
  ) {
    const captured = structuredClone(args);
    const work = this.#tail.then(async () => {
      const existing = this.receipt(requestId);
      if (existing) return existing;
      if (
        typeof captured.text !== "string" || !captured.text || captured.text.length > 16384 ||
        !this.#sources(captured.sources) ||
        (captured.entities &&
          (!Array.isArray(captured.entities) || captured.entities.length > 32 ||
            captured.entities.some((s) => typeof s !== "string" || !s || s.length > 256)))
      ) {
        throw new KernelError("rejected", "记忆需要自己的文字及确实存在的来源记录");
      }
      const id = await fingerprint(["memory-v1", requestId]);
      if (this.quarantine.excluded(...captured.sources.map((source) => source.recordSeq)).length) {
        throw new KernelError("rejected", "未确认候选的来源仅保留原件，不能直接形成已接受的记忆");
      }
      await this.journal.append(
        "formation",
        {
          id,
          requestId,
          text: captured.text,
          sources: captured.sources,
          entities: captured.entities ?? [],
          contextRecordSeq: this.quarantine.capture()!,
        } as unknown as Json,
      );
      return this.get({ id });
    });
    this.#tail = work.catch(() => {});
    return await work;
  }
  #views() {
    const views = new Map<string, MemoryView & { formedSeq: number; touched: number }>();
    for (const event of this.journal.events) {
      if (event.kind === "formation") {
        const m = event.data as unknown as Formation;
        views.set(m.id, {
          id: m.id,
          text: m.text,
          sources: m.sources,
          entities: m.entities,
          formedAt: event.at,
          level: 0,
          strength: 1,
          formedSeq: event.seq,
          touched: Date.parse(event.at),
          ...(m.contextRecordSeq !== undefined ? { contextRecordSeq: m.contextRecordSeq } : {}),
          ...this.quarantine.annotation(
            m.contextRecordSeq,
            ...m.sources.map((source) => source.recordSeq),
          ),
        });
      } else {
        const context = (event.data as { contextRecordSeq?: number }).contextRecordSeq;
        if (this.quarantine.excluded(context).length) continue;
        for (const id of (event.data as { ids: string[] }).ids) {
          const m = views.get(id)!;
          if (m.quarantined) continue;
          m.strength += event.kind === "used" ? 2 : 0.1;
          if (event.kind === "used") m.touched = Date.parse(event.at);
        }
      }
    }
    for (const m of views.values()) {
      const idleDays = Math.max(0, (this.now() - m.touched) / 86400000);
      m.level = Math.min(4, Math.floor(idleDays / (7 * Math.sqrt(m.strength))));
      // 机械分层只使用原文中的片段，不发起新的模型调用补造摘要。
      if (m.level === 1) {
        m.text = Array.from(m.text).slice(0, Math.max(40, Math.ceil(Array.from(m.text).length / 2)))
          .join("");
      }
      if (m.level === 2) m.text = m.text.split(/[。！？.!?\n]/u)[0];
      if (m.level >= 3) m.text = Array.from(m.text).slice(0, 20).join("");
    }
    return [...views.values()];
  }
  get(args: { id: string }): MemoryView {
    const m = this.#views().find((m) => m.id === args.id);
    if (!m) throw new KernelError("not_found", "没有该记忆");
    const { formedSeq: _formedSeq, touched: _touched, ...view } = m;
    return structuredClone(view);
  }
  list(args: { afterSeq?: number } = {}) {
    const after = args.afterSeq ?? 0;
    if (!Number.isSafeInteger(after) || after < 0 || after > this.journal.events.length) {
      throw new KernelError("rejected", "记忆目录游标无效");
    }
    const all = this.#views().filter((m) => m.formedSeq > after), page = all.slice(0, 8);
    return {
      items: page.map((m) => this.get({ id: m.id })),
      throughSeq: all.length > 8 ? page.at(-1)!.formedSeq : this.journal.events.length,
      hasMore: all.length > 8,
    };
  }
  async recall(args: { query: string; limit?: number }) {
    const limit = args.limit ?? 3;
    if (
      typeof args.query !== "string" || !args.query || args.query.length > 32768 ||
      !Number.isSafeInteger(limit) || limit < 1 || limit > 8
    ) throw new KernelError("rejected", "记忆检索参数无效");
    const candidates = this.#views().filter((m) => m.level < 4 && !m.quarantined);
    const retrieval = this.embed ? "semantic" : "lexical";
    if (!candidates.length) return { items: [], retrieval };
    const query = this.embed ? await this.embed(args.query, "query") : undefined, scored = [];
    // 模型接入待办期间只按实际重合的词句召回，明确标记，不把词法结果冒充语义相似度。
    const terms = [
      ...new Set(
        [...new Intl.Segmenter("zh", { granularity: "word" })
          .segment(args.query.toLocaleLowerCase())].filter((part) => part.isWordLike)
          .map((part) => part.segment),
      ),
    ];
    for (const m of candidates) {
      let score: number;
      if (this.embed && query) {
        const v = await this.embed(m.text, "passage");
        score = query.reduce((sum, x, i) => sum + x * v[i], 0);
      } else {
        const text = m.text.toLocaleLowerCase(), input = args.query.toLocaleLowerCase();
        const hits = terms.filter((term) => text.includes(term));
        const entityHit = m.entities.some((e) => input.includes(e.toLocaleLowerCase()));
        score = entityHit ? 1 : terms.length ? hits.length / terms.length : 0;
      }
      if (score >= (this.embed ? 0.65 : 0.25)) scored.push({ ...this.get({ id: m.id }), score });
    }
    scored.sort((a, b) => b.score - a.score);
    const items = scored.filter((m) => !this.get({ id: m.id }).quarantined).slice(0, limit);
    if (items.length) {
      await this.journal.append("recalled", {
        ids: items.map((m) => m.id),
        contextRecordSeq: this.quarantine.capture()!,
      });
    }
    return { items, retrieval };
  }
  async used(args: { ids: string[] }) {
    if (
      !Array.isArray(args.ids) || args.ids.length > 32 ||
      args.ids.some((id) => !this.#views().some((m) => m.id === id && !m.quarantined))
    ) throw new KernelError("rejected", "记忆强化编号无效");
    const ids = [...new Set(args.ids)];
    if (ids.length) {
      await this.journal.append("used", { ids, contextRecordSeq: this.quarantine.capture()! });
    }
    return { reinforced: ids };
  }
  async observe(text: string) {
    const ids = this.#views().filter((m) =>
      m.level < 4 && !m.quarantined &&
      (text.includes(m.id) || m.entities.some((e) => text.includes(e)))
    ).map((m) => m.id);
    if (ids.length) await this.used({ ids: ids.slice(0, 32) });
  }
  async close() {
    await this.#tail;
    await this.journal.close();
  }
}
