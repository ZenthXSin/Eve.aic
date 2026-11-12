import { KernelError } from "../../kernel/protocol.ts";
import type { FactJournal } from "../../sdk/fact-journal.ts";

/** 只保留回滚区间索引，原始意识、便签和记忆均不删除。 */
export class RollbackQuarantine {
  #scanned = 0;
  #ranges: { afterSeq: number; throughSeq: number }[] = [];
  constructor(readonly records?: FactJournal) {}
  capture() {
    return this.records?.events.length;
  }
  generation() {
    this.#refresh();
    return this.#ranges.length;
  }
  validateContext(value: unknown) {
    return value === undefined || typeof value === "number" && Number.isSafeInteger(value) &&
        value >= 0 && (!this.records || value <= this.records.events.length);
  }
  #refresh() {
    if (!this.records) return;
    if (this.records.events.length < this.#scanned) {
      throw new KernelError("corrupt", "意识原件水位回退，无法判断未确认版本范围");
    }
    for (let i = this.#scanned; i < this.records.events.length; i++) {
      const event = this.records.events[i];
      if (event.kind === "upgrade_rollback") {
        const range = event.data as { afterSeq: number; throughSeq: number };
        if (
          !range || !Number.isSafeInteger(range.afterSeq) ||
          !Number.isSafeInteger(range.throughSeq) ||
          range.afterSeq < 0 || range.throughSeq < range.afterSeq || range.throughSeq >= event.seq
        ) {
          throw new KernelError("corrupt", "换版回滚事实区间无效，保留原件");
        }
        this.#ranges.push({ afterSeq: range.afterSeq, throughSeq: range.throughSeq });
      }
      this.#scanned = i + 1;
    }
  }
  excluded(...sequences: (number | undefined)[]): number[] {
    this.#refresh();
    return [
      ...new Set(
        sequences.filter((seq): seq is number =>
          seq !== undefined &&
          this.#ranges.some((range) => seq > range.afterSeq && seq <= range.throughSeq)
        ),
      ),
    ];
  }
  annotation(...sequences: (number | undefined)[]) {
    const unconfirmedRecordSeqs = this.excluded(...sequences);
    return unconfirmedRecordSeqs.length
      ? { quarantined: true as const, unconfirmedRecordSeqs }
      : {};
  }
}
