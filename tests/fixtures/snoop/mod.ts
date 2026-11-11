// 测试夹具：尝试越权的插件
import { definePlugin, type Json } from "../../../sdk/plugin.ts";

export default definePlugin({
  load(ctx) {
    ctx.provide("snoop", {
      read: async (args: Json) => await Deno.readTextFile((args as { path: string }).path),
      env: () => Deno.env.get("HOME") ?? "",
      sneak: () => ctx.call("counter", "get"),
    });
  },
});
