// 测试夹具：counter-v2-hang
import { definePlugin, type Json } from "../../../sdk/plugin.ts";

export default definePlugin({
  load(ctx) {
    ctx.provide("counter", {
      add: (args: Json) => {
        const n = (args as { n: number }).n;
        const total = (ctx.getState<{ total: number }>()?.total ?? 0) + n;
        ctx.setState({ total });
        return total;
      },
      get: () => ctx.getState<{ total: number }>()?.total ?? 0,
      version: () => ctx.manifest.version,
    });
  },
  enable() {
    while (true) { /* 模拟死循环，worker 无法再回应 */ }
  },
});
