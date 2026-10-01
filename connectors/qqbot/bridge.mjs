import { optionsFromEnv, createBridge, consume } from "./bridge-core.mjs";
const emit = frame => process.stdout.write(JSON.stringify(frame) + "\n");
let bridge;
try {
  const options = optionsFromEnv(process.env);
  const { QQBot } = await import("@tencent-connect/qqbot-nodejs");
  const bot = new QQBot(options);
  bridge = createBridge(bot, emit);
  process.on("SIGINT", () => bridge.stop());
  process.on("SIGTERM", () => bridge.stop());
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
