// 依赖图：谁提供哪个契约、启动顺序，以及换版前的契约兼容检查。

import { KernelError, type Manifest } from "./protocol.ts";
import { satisfies } from "./semver.ts";

export interface Plan {
  /** 启动顺序；停止按逆序。 */
  order: string[];
  /** 契约名到提供者插件名。 */
  providers: Map<string, string>;
}

/** 检查一组清单能否一起运行，返回启动顺序。有环、缺依赖、版本不满足、重复提供都拒绝。 */
export function plan(manifests: Manifest[]): Plan {
  const byName = new Map<string, Manifest>();
  const providers = new Map<string, string>();
  for (const manifest of manifests) {
    if (byName.has(manifest.name)) {
      throw new KernelError("rejected", `插件名重复：${manifest.name}`);
    }
    byName.set(manifest.name, manifest);
    for (const contract of Object.keys(manifest.provides)) {
      const existing = providers.get(contract);
      if (existing) {
        throw new KernelError(
          "rejected",
          `契约 ${contract} 被 ${existing} 和 ${manifest.name} 同时提供`,
        );
      }
      providers.set(contract, manifest.name);
    }
  }

  const edges = new Map<string, Set<string>>();
  for (const manifest of manifests) {
    const deps = new Set<string>();
    for (const [contract, range] of Object.entries(manifest.requires)) {
      const provider = providers.get(contract);
      if (!provider) {
        throw new KernelError("rejected", `${manifest.name} 依赖的契约 ${contract} 没有提供者`);
      }
      const version = byName.get(provider)!.provides[contract];
      if (!satisfies(version, range)) {
        throw new KernelError(
          "rejected",
          `${manifest.name} 需要 ${contract}@${range}，但 ${provider} 提供的是 ${version}`,
        );
      }
      deps.add(provider);
    }
    for (const [contract, range] of Object.entries(manifest.optional)) {
      const provider = providers.get(contract);
      if (provider && satisfies(byName.get(provider)!.provides[contract], range)) {
        deps.add(provider);
      }
    }
    deps.delete(manifest.name);
    edges.set(manifest.name, deps);
  }

  const order: string[] = [];
  const state = new Map<string, "visiting" | "done">();
  const visit = (name: string, trail: string[]) => {
    const mark = state.get(name);
    if (mark === "done") return;
    if (mark === "visiting") {
      const cycle = [...trail.slice(trail.indexOf(name)), name].join(" -> ");
      throw new KernelError("rejected", `依赖有环：${cycle}`);
    }
    state.set(name, "visiting");
    for (const dep of [...edges.get(name)!].sort()) visit(dep, [...trail, name]);
    state.set(name, "done");
    order.push(name);
  };
  for (const name of [...byName.keys()].sort()) visit(name, []);
  return { order, providers };
}

/** 换版前检查：把 name 的清单换成 next 之后，整组是否仍然成立。返回拒绝原因或 undefined。 */
export function checkReplacement(
  current: Manifest[],
  next: Manifest,
): string | undefined {
  const replaced = current.map((manifest) => manifest.name === next.name ? next : manifest);
  try {
    plan(replaced);
    return undefined;
  } catch (error) {
    return (error as Error).message;
  }
}
