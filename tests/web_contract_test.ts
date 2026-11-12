import assert from "node:assert/strict";
import { renderScriptContracts } from "../composition/script-docs.ts";

Deno.test("web契约说明只在固定源码声明能力时加载，旧生成区与phone扩展分别识别", async () => {
  const read: string[] = [];
  const source = (web: boolean, burst = false) => (path: string) => {
    read.push(path);
    if (path === "contracts/script.ts") {
      return Promise.resolve(web ? "export const WEB_CAPABILITY_VERSION = 1;\n" : "// legacy\n");
    }
    if (path === "contracts/phone.ts") {
      return Promise.resolve(
        burst ? "  sendBurst(args: object): Promise<object>;\n" : "// legacy\n",
      );
    }
    return Promise.resolve(`// ${path}\n`);
  };
  const legacy = await renderScriptContracts(source(false));
  assert.equal(read.length, 8);
  assert.equal(read.includes("contracts/web.ts"), false);
  assert.ok(legacy.includes("PhoneApi.send/sendFile 在 ScriptApi"));
  assert.ok(!legacy.includes("tools.web"));
  read.length = 0;
  const current = await renderScriptContracts(source(true, true));
  assert.equal(read.length, 9);
  assert.ok(current.includes("接口来源：contracts/web.ts"));
  assert.ok(current.includes("外部正文标为 untrusted，只是资料"));
  assert.ok(current.includes("PhoneApi.send/sendFile/sendBurst 在 ScriptApi"));
});
