// 测试夹具：依赖 counter 的消费者
import { definePlugin } from "../../../sdk/plugin.ts";

export default definePlugin({
  load(ctx) {
    ctx.provide("consumer", {
      bump: () => ctx.call("counter", "add", { n: 1 }),
    });
  },
});
