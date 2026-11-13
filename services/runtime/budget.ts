import type { ModelResponse } from "../../contracts/model.ts";
import { type Json, KernelError } from "../../kernel/protocol.ts";
import { FactJournal } from "../../sdk/fact-journal.ts";
import { accountingDay } from "../../plugins/eve/view.ts";
import { writeDurableJson } from "../../sdk/durable-json.ts";

export interface BudgetSettings {
  dailyTokenBudget: number;
  paused: boolean;
  timeZone: string;
}

function validateSettings(value: unknown): asserts value is BudgetSettings {
  const v = value as BudgetSettings | null;
  if (
    !v || typeof v !== "object" || Array.isArray(v) ||
    !Number.isSafeInteger(v.dailyTokenBudget) || v.dailyTokenBudget < 0 ||
    typeof v.paused !== "boolean" || typeof v.timeZone !== "string"
  ) throw new KernelError("corrupt", "运行设置无效，保留原件");
  try {
    new Intl.DateTimeFormat("en", { timeZone: v.timeZone }).format();
  } catch {
    throw new KernelError("corrupt", "预算时区无效");
  }
}

export class SharedBudget {
  readonly journal: FactJournal;
  readonly settingsJournal: FactJournal;
  #tail: Promise<unknown> = Promise.resolve();
  #reservations = new Map<string, { day: string; amount: number }>();
  #usages = new Set<string>();
  #days = new Map<string, { known: number; unknown: number }>();
  #settings?: BudgetSettings;
  #settingsRevision = 0;
  #index(
    kind: string,
    data: { id: string; day?: string; amount?: number; known?: number; unknown?: number },
  ) {
    if (kind === "reserve") {
      this.#reservations.set(data.id, { day: data.day!, amount: data.amount! });
      const total = this.#days.get(data.day!) ?? { known: 0, unknown: 0 };
      total.unknown += data.amount!;
      this.#days.set(data.day!, total);
    } else {
      const reservation = this.#reservations.get(data.id)!;
      const total = this.#days.get(reservation.day)!;
      total.known += data.known!;
      total.unknown += data.unknown! - reservation.amount;
      this.#usages.add(data.id);
    }
  }
  constructor(directory: string, readonly settingsFile: string) {
    this.journal = new FactJournal(directory);
    this.settingsJournal = new FactJournal(`${directory}/settings`);
  }
  async initialize() {
    await this.journal.initialize();
    try {
      const reservations = new Set<string>(), usages = new Set<string>();
      for (const event of this.journal.events) {
        const data = event.data as {
          id?: unknown;
          day?: unknown;
          amount?: unknown;
          known?: unknown;
          unknown?: unknown;
        };
        if (!data || typeof data.id !== "string" || !data.id || data.id.length > 1024) {
          throw new KernelError("corrupt", "预算记录缺少有效请求编号");
        }
        if (event.kind === "reserve") {
          if (
            reservations.has(data.id) || typeof data.day !== "string" ||
            !validDay(data.day) || !tokenAmount(data.amount)
          ) throw new KernelError("corrupt", "预算预留记录无效");
          reservations.add(data.id);
        } else if (event.kind === "usage") {
          if (
            !reservations.has(data.id) || usages.has(data.id) || !tokenAmount(data.known) ||
            !tokenAmount(data.unknown) ||
            !Number.isSafeInteger(Number(data.known) + Number(data.unknown))
          ) throw new KernelError("corrupt", "预算用量缺少唯一有效预留");
          usages.add(data.id);
        } else throw new KernelError("corrupt", "未知预算记录");
        this.#index(
          event.kind,
          data as { id: string; day?: string; amount?: number; known?: number; unknown?: number },
        );
      }
      await this.settingsJournal.initialize();
      for (const event of this.settingsJournal.events) {
        const data = event.data as unknown as { revision: number; settings: BudgetSettings };
        if (event.kind !== "settings" || !data || data.revision !== this.#settingsRevision + 1) {
          throw new KernelError("corrupt", "运行设置修订链无效");
        }
        validateSettings(data.settings);
        this.#settings = structuredClone(data.settings);
        this.#settingsRevision = data.revision;
      }
      if (!this.#settings) {
        const initial = await this.#bootstrapSettings();
        await this.settingsJournal.append("settings", { revision: 1, settings: { ...initial } });
        this.#settings = initial;
        this.#settingsRevision = 1;
      }
      // This file is a compatibility snapshot. Deployments cannot replace committed settings.
      await this.#snapshotSettings();
    } catch (error) {
      await this.settingsJournal.close();
      await this.journal.close();
      throw error;
    }
  }
  async #bootstrapSettings(): Promise<BudgetSettings> {
    const stat = await Deno.lstat(this.settingsFile);
    if (!stat.isFile || stat.isSymlink || stat.size > 65536) {
      throw new KernelError("corrupt", "运行设置原件无效");
    }
    const value = JSON.parse(await Deno.readTextFile(this.settingsFile));
    validateSettings(value);
    return {
      dailyTokenBudget: value.dailyTokenBudget,
      paused: value.paused,
      timeZone: value.timeZone,
    };
  }
  async #snapshotSettings() {
    try {
      const stat = await Deno.lstat(this.settingsFile);
      if (!stat.isFile || stat.isSymlink) {
        throw new KernelError("corrupt", "运行设置快照不是普通文件");
      }
      if (
        stat.size <= 65536 &&
        await Deno.readTextFile(this.settingsFile) === JSON.stringify(this.#settings) + "\n"
      ) return;
    } catch (error) {
      if (!(error instanceof Deno.errors.NotFound)) throw error;
    }
    await writeDurableJson(this.settingsFile, this.#settings);
  }
  settings(): Promise<BudgetSettings> {
    if (!this.#settings || !this.journal.available || !this.settingsJournal.available) {
      return Promise.reject(new KernelError("unavailable", "持久运行设置尚未就绪"));
    }
    return Promise.resolve(structuredClone(this.#settings));
  }
  async updateSettings(patch: Partial<BudgetSettings>) {
    const work = this.#tail.then(async () => {
      if (
        !patch || typeof patch !== "object" || Array.isArray(patch) ||
        Object.keys(patch).some((key) => !["dailyTokenBudget", "paused", "timeZone"].includes(key))
      ) throw new KernelError("rejected", "运行设置字段无效");
      const previous = await this.settings(), next = { ...previous, ...patch };
      validateSettings(next);
      if (JSON.stringify(previous) === JSON.stringify(next)) {
        await this.#snapshotSettings();
        return this.status();
      }
      const revision = this.#settingsRevision + 1;
      await this.settingsJournal.append("settings", { revision, settings: { ...next } });
      this.#settings = next;
      this.#settingsRevision = revision;
      await this.#snapshotSettings();
      return this.status();
    });
    this.#tail = work.catch(() => {});
    return await work;
  }
  async status() {
    const settings = await this.settings(), day = accountingDay(settings.timeZone);
    const { known, unknown } = this.#days.get(day) ?? { known: 0, unknown: 0 };
    if (!Number.isSafeInteger(known + unknown)) {
      throw new KernelError("corrupt", "预算总量超出安全整数，停止新请求");
    }
    return {
      ...settings,
      day,
      knownNoncachedTokens: known,
      unknownReservedTokens: unknown,
      safetyTotal: known + unknown,
      safeRemaining: Math.max(0, settings.dailyTokenBudget - known - unknown),
    };
  }
  async reserve(id: string, amount: number) {
    if (typeof id !== "string" || !id || id.length > 1024 || !tokenAmount(amount)) {
      throw new KernelError("rejected", "预算预留参数无效");
    }
    const work = this.#tail.then(async () => {
      const existing = this.#reservations.get(id);
      if (existing) {
        if (existing.amount !== amount) {
          throw new KernelError("rejected", "同一请求不能更改预算预留");
        }
        return;
      }
      const status = await this.status();
      if (status.paused || amount > status.safeRemaining) {
        throw new KernelError(
          "unavailable",
          status.paused ? "主人已暂停模型请求" : "日预算安全余量不足",
        );
      }
      await this.journal.append("reserve", { id, day: status.day, amount });
      this.#index("reserve", { id, day: status.day, amount });
    });
    this.#tail = work.catch(() => {});
    await work;
  }
  async response(id: string, response: ModelResponse) {
    const work = this.#tail.then(() => this.#response(id, response));
    this.#tail = work.catch(() => {});
    await work;
  }
  async #response(id: string, response: ModelResponse) {
    if (!this.#reservations.has(id)) {
      throw new KernelError("rejected", "模型用量没有对应的预算预留");
    }
    if (this.#usages.has(id)) return;
    const usage = response.usage;
    if (
      usage?.status === "invalid" || usage?.inputTokens === undefined ||
      usage.outputTokens === undefined
    ) return;
    const valid =
      [usage.inputTokens, usage.outputTokens, usage.cachedInputTokens ?? 0].every((n) =>
        Number.isSafeInteger(n) && n >= 0
      ) && (usage.cachedInputTokens ?? 0) <= usage.inputTokens;
    if (!valid || !Number.isSafeInteger(usage.inputTokens + usage.outputTokens)) return;
    const data = {
      id,
      known: usage.outputTokens +
        (usage.cachedInputTokens === undefined ? 0 : usage.inputTokens - usage.cachedInputTokens),
      unknown: usage.cachedInputTokens === undefined ? usage.inputTokens : 0,
    };
    await this.journal.append("usage", data as Json);
    this.#index("usage", data);
  }
  async close() {
    await this.#tail;
    await this.settingsJournal.close();
    await this.journal.close();
  }
}

function tokenAmount(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

function validDay(value: string) {
  if (!/^\d{4}-\d{2}-\d{2}$/.test(value)) return false;
  const date = new Date(`${value}T00:00:00.000Z`);
  return Number.isFinite(date.getTime()) && date.toISOString().slice(0, 10) === value;
}
