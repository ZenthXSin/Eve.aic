import { KernelError } from "../../kernel/protocol.ts";

/** 启动另限五秒；全部候选的正则执行共用总时限，回溯不会阻塞其他工具。 */
export async function regexMatches<T>(
  query: string,
  candidates: T[],
  text: (candidate: T) => string,
  maximum = 9,
  timeoutMs = 1000,
): Promise<T[]> {
  const worker = new Worker(new URL("./regex-worker.ts", import.meta.url), {
    type: "module",
    deno: { permissions: "none" },
  } as unknown as WorkerOptions);
  let timer: ReturnType<typeof setTimeout> | undefined;
  let timedOut!: (error: KernelError) => void;
  const timeout = new Promise<T[]>((_, reject) => timedOut = reject);
  timer = setTimeout(() => timedOut(new KernelError("timeout", "正则搜索 worker 启动超时")), 5000);
  try {
    return await Promise.race([
      new Promise<T[]>((resolve, reject) => {
        const matches: T[] = [];
        let at = 0;
        worker.onerror = (event) => {
          event.preventDefault();
          reject(new KernelError("unavailable", "正则搜索 worker 失败"));
        };
        worker.onmessage = (event) => {
          if (event.data.invalid) {
            reject(new KernelError("rejected", "正则表达式无效"));
            return;
          }
          if (event.data.ready) {
            clearTimeout(timer);
            timer = setTimeout(
              () =>
                timedOut(new KernelError("timeout", "正则搜索超过总时限，请缩小分组或简化表达式")),
              timeoutMs,
            );
          }
          if (event.data.matched) matches.push(candidates[at - 1]);
          if (at === candidates.length || matches.length >= maximum) {
            resolve(matches);
          } else worker.postMessage({ text: text(candidates[at++]) });
        };
        worker.postMessage({ query });
      }),
      timeout,
    ]);
  } finally {
    clearTimeout(timer);
    worker.terminate();
  }
}
