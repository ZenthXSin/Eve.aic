// 测试夹具：记录生命周期顺序
import { definePlugin } from "../../../sdk/plugin.ts";

export default definePlugin({
  load(ctx) {
    ctx.log("load");
    ctx.provide("lifecycle", { ping: () => "pong" });
    ctx.onDisable(() => ctx.log("cleanup-1"));
    ctx.onDisable(() => ctx.log("cleanup-2"));
  },
  enable(ctx) {
    ctx.log("enable");
  },
  disable(ctx) {
    ctx.log("disable");
  },
});
