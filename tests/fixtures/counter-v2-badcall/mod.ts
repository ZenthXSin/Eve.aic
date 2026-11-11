// 测试夹具：counter-v2-badcall
import { definePlugin, type Json } from "../../../sdk/plugin.ts";

export default definePlugin({
  load(ctx) {
    ctx.provide("counter", {
      add: (args: Json) => {
        ctx.setState({ total: -999 });
        throw new Error(`新版本的 add 坏了：${JSON.stringify(args)}`);
      },
      get: () => ctx.getState<{ total: number }>()?.total ?? 0,
      version: () => ctx.manifest.version,
    });
  },
});
