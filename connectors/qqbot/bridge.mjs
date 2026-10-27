import { optionsFromEnv, createBridge, consume } from "./bridge-core.mjs";
const emit = frame => process.stdout.write(JSON.stringify(frame) + "\n");
let bridge;
try {
  const options = optionsFromEnv(process.env);
  const { QQBot } = await import("@tencent-connect/qqbot-nodejs");
  const bot = new QQBot(options);
  bridge = createBridge(bot, emit, 128, () => performance.now(), {observe:process.env.QQBOT_GROUP_OBSERVE === "true",mention:process.env.QQBOT_GROUP_MENTION !== "false",prefixOnly:process.env.QQBOT_GROUP_PREFIX_ONLY === "true"});
  process.on("SIGINT", () => bridge.stop());
  process.on("SIGTERM", () => bridge.stop());
  // 监督器向 Eve 的独立 Windows 进程组发送 Ctrl+Break；由 Rust 完成回执收尾后发 stop。
  if (process.platform === "win32") process.on("SIGBREAK", () => {});
  const running = bot.start().catch(() => {
    emit({ type: "fatal", version: 1, code: "sdk_start_failed" });
    process.exitCode = 1;
    bridge.stop();
    process.stdin.destroy();
  });
  await consume(process.stdin, bridge);
  await running;
} catch {
  emit({ type: "fatal", version: 1, code: "bridge_start_failed" });
  process.exitCode = 1;
  bridge?.stop();
}
