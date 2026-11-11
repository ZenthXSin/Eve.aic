// 演示：真实启动内核与插件，跑轮次循环，再做一次成功的换版和一次被回滚的换版。
// 运行：deno task demo

import { Kernel } from "../kernel/kernel.ts";

const ROOT = new URL("..", import.meta.url).pathname;
const dataDir = await Deno.makeTempDir({ prefix: "eve-demo-" });
const kernel = new Kernel({
  dataDir,
  log: (event) => {
    if (event.event === "plugin_log") return;
    console.log(`  [内核] ${event.event} ${event.plugin ?? ""} ${event.detail ?? ""}`.trimEnd());
  },
});

console.log("一、启动（按依赖顺序）");
for (
  const dir of [
    "plugins/turn-loop",
    "plugins/model-mock",
    "plugins/tools-echo",
    "tests/fixtures/counter-v1",
  ]
) {
  await kernel.register(`${ROOT}${dir}`);
}
await kernel.start();

console.log("\n二、轮次循环");
for (const input of ["你好", "回声 海风"]) {
  const result = await kernel.call<{ reply: string; modelRequests: number; toolCalls: number }>(
    "turn",
    "run",
    { input },
  );
  console.log(`  输入：${input}`);
  console.log(
    `  回复：${result.reply}（模型请求 ${result.modelRequests} 次，工具 ${result.toolCalls} 次）`,
  );
}

console.log("\n三、换版成功（counter 1.0.0 -> 1.1.0，状态延续）");
console.log(`  换版前 add 5 = ${await kernel.call("counter", "add", { n: 5 })}`);
const good = await kernel.swap("counter", `${ROOT}tests/fixtures/counter-v2`);
console.log(`  结果：${good.outcome}，信号：${good.signals.join("、")}`);
console.log(
  `  换版后 版本 ${await kernel.call("counter", "version")}，数值 ${await kernel.call(
    "counter",
    "get",
  )}`,
);

console.log("\n四、换版失败并回滚（新版本自检不通过）");
const bad = await kernel.swap("counter", `${ROOT}tests/fixtures/counter-v2-unhealthy`);
console.log(`  结果：${bad.outcome}，信号：${bad.signals.join("、") || "无"}`);
console.log(`  原因：${bad.reason}`);
console.log(
  `  回滚后 版本 ${await kernel.call("counter", "version")}，数值 ${await kernel.call(
    "counter",
    "get",
  )}`,
);

console.log("\n五、停止（逆序）");
const problems = await kernel.stop();
console.log(`  停止时的问题：${problems.length === 0 ? "无" : problems.join("；")}`);
console.log(`\n状态、快照与回执保存在：${dataDir}`);
