import { dirname } from "node:path/posix";
import {
  StudioSettingsError,
  type StudioSnapshot,
  validateStudioUpdate,
} from "../services/admin/studio-settings.ts";
import {
  child,
  type Documents,
  studioCatalog,
  type StudioState,
} from "./studio-settings-catalog.ts";
import {
  budgetSnapshotVersion,
  readStudioDocument,
  replaceStudioDocument,
  withStudioLock,
} from "./studio-settings-files.ts";

export type StudioSettingsOptions = {
  configPath: string;
  controller: {
    readonly state: StudioState;
    instance(id: string): { id: string; configFile: string; source: { commit: string } };
  };
  runtime: {
    status(): Promise<unknown>;
    settings(patch: { dailyTokenBudget: number }): Promise<unknown>;
  };
  changePause(paused: boolean): Promise<void>;
  serialControl<T>(run: () => Promise<T>): Promise<T>;
};

const fixed = ["runtime", "qq", "guardian", "admin"] as const;
const smallLimit = 1024 * 1024;
const stale = () => new StudioSettingsError("conflict");
const unavailable = () => new StudioSettingsError("unavailable");
const unconfirmed = () => new StudioSettingsError("unconfirmed");
function fence(state: StudioState) {
  return [state.current, state.initial, state.paused, !!state.upgrade, !!state.pendingGrant].join(
    "|",
  );
}

async function revisionOf(value: unknown) {
  const hash = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(JSON.stringify(value)),
  );
  return Array.from(new Uint8Array(hash), (byte) => byte.toString(16).padStart(2, "0")).join("");
}

export function createStudioSettings(options: StudioSettingsOptions) {
  const directory = dirname(options.configPath);
  async function inspect() {
    const supervisor = await readStudioDocument(options.configPath, 16 * smallLimit);
    const config = child(supervisor?.data, "supervisor");
    if (!supervisor || !config) throw unavailable();
    const docs: Documents = { supervisor };
    const files = await Promise.all(
      fixed.map((scope) => readStudioDocument(`${directory}/${scope}.json`, smallLimit)),
    );
    fixed.forEach((scope, index) => {
      docs[scope] = files[index];
    });
    const state = options.controller.state, era = fence(state);
    const assertEra = () => {
      if (fence(options.controller.state) !== era) throw stale();
    };
    const target = state.current ?? state.initial ?? config.initialInstance;
    if (target !== undefined && target !== null) {
      if (typeof target !== "string" || !/^[A-Za-z0-9_-]{1,128}$/.test(target)) throw unavailable();
      const spec = options.controller.instance(target);
      const base = config.instancePrivateDirectory ?? `${directory}/instances`;
      if (
        typeof base !== "string" || spec.id !== target ||
        spec.configFile !== `${base}/${target}/config.json` ||
        !/^[a-f0-9]{40}$/.test(spec.source.commit)
      ) throw unavailable();
      docs.instance = await readStudioDocument(spec.configFile, smallLimit);
      if (docs.instance) {
        const instance = child(docs.instance.data, "instance"),
          eve = child(docs.instance.data, "eve");
        if (
          instance?.id !== target || instance?.sourceCommit !== spec.source.commit ||
          eve?.sourceCommit !== spec.source.commit
        ) throw unavailable();
      }
    }
    const settingsFile = child(docs.runtime?.data, "runtimeService")?.settingsFile;
    const budgetVersion = await budgetSnapshotVersion(settingsFile);
    const status = await options.runtime.status().catch(() => undefined);
    const entries = await studioCatalog(docs, state, status);
    if (budgetVersion !== await budgetSnapshotVersion(settingsFile)) throw stale();
    if (!budgetVersion) {
      for (const key of ["budget.dailyTokenBudget", "runtime.paused"]) {
        const { item } = entries.get(key)!;
        item.editable = false;
        item.reason = "无法核对运行设置快照版本，请检查执行服务";
      }
    }
    const items = Array.from(entries.values(), ({ item }) => item);
    const selection = child(child(status, "model"), "current")?.revision;
    const revision = await revisionOf([
      "studio-v1",
      era,
      items,
      budgetVersion,
      Number.isSafeInteger(selection) ? selection : null,
      ...(["supervisor", ...fixed, "instance"] as const).map((scope) =>
        docs[scope]?.version ?? null
      ),
    ]);
    assertEra();
    const snapshot: StudioSnapshot = { revision, items };
    return { docs, entries, state, assertEra, snapshot };
  }

  return {
    async read(): Promise<StudioSnapshot> {
      try {
        return await withStudioLock(directory, false, async () => (await inspect()).snapshot);
      } catch (error) {
        throw error instanceof StudioSettingsError ? error : unavailable();
      }
    },
    async update(body: unknown): Promise<StudioSnapshot> {
      const { revision, changes } = validateStudioUpdate(body);
      let writing = false;
      try {
        return await options.serialControl(() =>
          withStudioLock(directory, true, async () => {
            const { docs, entries, snapshot, state, assertEra } = await inspect();
            if (revision !== snapshot.revision) throw stale();
            const batch = Object.entries(changes).map(([key, value]) => {
              const binding = entries.get(key);
              if (!binding) throw new StudioSettingsError("invalid");
              if (!binding.item.editable) throw new StudioSettingsError("restricted");
              if (!binding.accepts?.(value)) throw new StudioSettingsError("invalid");
              return { binding, value };
            });
            const scope = batch[0].binding.item.scope;
            if (
              batch.some(({ binding }) => binding.item.scope !== scope)
            ) throw new StudioSettingsError("scope");
            if (
              batch.every(({ binding, value }) => binding.item.value === value) &&
              !(scope === "pause" && state.paused !== changes["runtime.paused"])
            ) return snapshot;
            assertEra();
            writing = true;
            if (scope === "budget") {
              await options.runtime.settings({
                dailyTokenBudget: changes["budget.dailyTokenBudget"] as number,
              });
            } else if (scope === "pause") {
              await options.changePause(changes["runtime.paused"] as boolean);
            } else {
              for (const { binding, value } of batch) binding.assign!(value);
              await replaceStudioDocument(docs[scope]!, assertEra);
            }
            try {
              const after = await inspect();
              if (
                batch.some(({ binding, value }) =>
                  after.entries.get(binding.item.key)?.item.value !== value
                ) ||
                (scope === "pause" && after.state.paused !== changes["runtime.paused"])
              ) throw unconfirmed();
              return after.snapshot;
            } catch {
              throw unconfirmed();
            }
          })
        );
      } catch (error) {
        if (error instanceof StudioSettingsError) throw error;
        throw writing ? unconfirmed() : unavailable();
      }
    },
  };
}
