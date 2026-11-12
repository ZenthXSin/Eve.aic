import { type Json, KernelError } from "../../kernel/protocol.ts";
import type { PluginContext } from "../../sdk/plugin.ts";

export const CONDITION_READS: Record<string, readonly string[]> = {
  files: ["read", "list", "search"],
  commands: ["poll"],
  notes: ["get", "list", "search"],
  records: ["page", "text"],
};

/** 无文件、网络、子进程权限。即使条件死循环，宿主也能终止，不影响主意识流。 */
export function checkCondition(ctx: PluginContext, code: string, facts: Json): Promise<boolean> {
  const source = `
    const pending = new Map(); let next = 0;
    const tools = Object.freeze(Object.fromEntries(Object.entries(${
    JSON.stringify(CONDITION_READS)
  }).map(([ns,methods]) => [ns,Object.freeze(Object.fromEntries(methods.map(method => [method,(args={}) => new Promise((resolve,reject) => {const id=++next;pending.set(id,{resolve,reject});postMessage({kind:"read",id,ns,method,args});})])))])));
    onmessage = async ({data}) => {
      if(data.kind==="reply") {const p=pending.get(data.id);if(p){pending.delete(data.id);data.ok?p.resolve(data.value):p.reject(new Error(data.error));}return;}
      try {const run = new Function("facts","tools",'"use strict"; return (async () => {' + data.code + '\\n})();');const value=await run(data.facts,tools);if(typeof value!=="boolean")throw new Error("条件必须 return boolean");postMessage({kind:"done",value});}
      catch(error){postMessage({kind:"error",error:String(error)});}
    };
  `;
  const worker = new Worker(`data:application/javascript,${encodeURIComponent(source)}`, {
    type: "module",
    deno: { permissions: "none" },
  });
  return new Promise((resolve, reject) => {
    let closed = false;
    const finish = (value?: boolean, error?: unknown) => {
      if (closed) return;
      closed = true;
      clearTimeout(timer);
      worker.terminate();
      if (error) reject(error);
      else resolve(value!);
    };
    const timer = setTimeout(
      () => finish(undefined, new KernelError("timeout", "条件检查超过执行期限，提醒保留")),
      2000,
    );
    worker.onmessage = ({ data }) => {
      if (data?.kind === "done") finish(data.value);
      else if (data?.kind === "error") finish(undefined, new KernelError("rejected", data.error));
      else if (data?.kind === "read") {
        const { id, ns, method, args } = data;
        if (!CONDITION_READS[ns]?.includes(method)) {
          finish(undefined, new KernelError("rejected", "条件只能使用只读能力"));
          return;
        }
        void ctx.call(ns, method, args).then(
          (value) => {
            if (!closed) worker.postMessage({ kind: "reply", id, ok: true, value });
          },
          (error) => {
            if (!closed) worker.postMessage({ kind: "reply", id, ok: false, error: String(error) });
          },
        );
      }
    };
    worker.onerror = (event) => {
      event.preventDefault();
      finish(undefined, new KernelError("rejected", event.message));
    };
    worker.postMessage({ kind: "start", code, facts: structuredClone(facts) });
  });
}
