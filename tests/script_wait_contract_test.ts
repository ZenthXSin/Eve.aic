import assert from "node:assert/strict";
import { renderScriptContracts } from "../composition/script-docs.ts";
import { SCRIPT_AUTO_WAIT_CAPABILITY_VERSION } from "../contracts/script.ts";
import { SCRIPT_TOOL } from "../plugins/script-runtime/mod.ts";

Deno.test("脚本前台最多10秒；提示升级仍能复原旧等待契约", async () => {
  const render = (declaration: string) =>
    renderScriptContracts((path) =>
      Promise.resolve(path === "contracts/script.ts" ? declaration : `// ${path}\n`)
    );
  const legacy = await render("// legacy\n");
  assert.ok(legacy.includes("脚本默认异步返回持久任务编号，具体动作编号由宿主生成。"));
  const old = await render("export const SCRIPT_AUTO_WAIT_CAPABILITY_VERSION = 1;\n");
  assert.ok(old.includes(
    "脚本默认最多等待5秒：完成就直接带回真实结果，未完成则返回原持久任务编号，在后台并行继续。mode:async可立即返回；mode:sync也最多等待5秒，等待结束不会取消或重跑任务。具体动作编号由宿主生成。",
  ));
  assert.equal(old.includes("15秒"), false);
  const current = await render("export const SCRIPT_AUTO_WAIT_CAPABILITY_VERSION = 2;\r\n");
  assert.ok(current.includes("默认最多等待15秒（含类型检查和沙箱准备）"));
  assert.ok(current.includes("mode:sync仍最多等待5秒"));
  assert.ok(current.includes("mode:async可立即返回"));
  // Untrusted expressions cannot opt into a capability or execute during prompt generation.
  assert.ok((await render("export const SCRIPT_AUTO_WAIT_CAPABILITY_VERSION = 1 + 1;\n"))
    .includes("脚本默认异步返回持久任务编号"));
  assert.equal(SCRIPT_AUTO_WAIT_CAPABILITY_VERSION, 3);
  const bounded = await render("export const SCRIPT_AUTO_WAIT_CAPABILITY_VERSION = 3;\r\n");
  assert.ok(bounded.includes("mode:sync 最多等待10秒"));
  assert.ok((await renderScriptContracts()).includes("最多等待10秒"));
  assert.ok(SCRIPT_TOOL.description.includes("最多等待10秒"));
});
