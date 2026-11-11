// 测试夹具：counter-v2-migrate
import { definePlugin, type Json } from "../../../sdk/plugin.ts";

export default definePlugin({
  load(ctx) {
    ctx.provide("counter", {
      add: (args: Json) => {
        const n = (args as { n: number }).n;
        const sum = (ctx.getState<{ sum: number }>()?.sum ?? 0) + n;
        ctx.setState({ sum });
        return sum;
      },
      get: () => ctx.getState<{ sum: number }>()?.sum ?? 0,
      version: () => ctx.manifest.version,
    });
  },
  migrate(state: Json, from: number) {
    if (from !== 1) throw new Error(`不认识的状态版本 ${from}`);
    return { sum: (state as { total: number }).total };
  },
});
