// 测试夹具：counter-v2-typeerror
import { definePlugin, type Json } from "../../../sdk/plugin.ts";
const wrong: number = "类型不对";
void wrong;

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
});
