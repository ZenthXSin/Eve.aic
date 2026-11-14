import { fingerprint } from "../../kernel/calls.ts";
import { KernelError } from "../../kernel/protocol.ts";

/** Stable module URLs let Deno validate its own check cache for repeated, self-contained scripts. */
export class ScriptCheckSources {
  readonly directory: string;
  #entries = new Map<string, { path: string; ready: Promise<void>; users: number }>();
  #created = false;
  #corrupt = false;
  constructor(cacheDirectory: string) {
    // Per-runtime ownership prevents one process pruning another process's active source.
    this.directory = `${cacheDirectory}/script-check-${crypto.randomUUID()}`;
  }
  async acquire(original: string, code: string, body: string) {
    // Relative/dynamic imports and reference directives keep their original checking base.
    if (/\b(?:import|export)\b|<reference/i.test(body)) {
      return { path: original, release: async () => {} };
    }
    const key = await fingerprint(["script-check-source-v1", code]);
    let entry = this.#entries.get(key);
    if (!entry) {
      const path = `${this.directory}/${key}.ts`;
      const ready = (async () => {
        await Deno.mkdir(this.directory, { recursive: true, mode: 0o700 });
        this.#created = true;
        await Deno.writeTextFile(path, code, { createNew: true, mode: 0o600 });
      })();
      entry = { path, ready, users: 0 };
      this.#entries.set(key, entry);
    }
    entry.users++;
    const release = () => {
      entry.users--;
      if (this.#corrupt) return;
      // Keep at most 64 inactive source files, with full originals retained in the job ledger.
      for (const [id, item] of this.#entries) {
        if (this.#entries.size <= 64) break;
        if (item.users) continue;
        Deno.removeSync(item.path);
        this.#entries.delete(id);
      }
    };
    try {
      await entry.ready;
      await this.verify(entry.path, code);
    } catch (error) {
      this.#corrupt = true;
      await release();
      throw error;
    }
    return { path: entry.path, release };
  }
  async verify(path: string, code: string) {
    const stat = await Deno.lstat(path);
    if (!stat.isFile || stat.isSymlink || await Deno.readTextFile(path) !== code) {
      this.#corrupt = true;
      throw new KernelError("corrupt", "脚本检查缓存源码不符，未执行，原件保留");
    }
  }
  async close() {
    if (this.#created && !this.#corrupt) {
      await Deno.remove(this.directory, { recursive: true });
      this.#created = false;
    }
  }
}
