/** 接口说明直接取契约源码，类型检查也导入同一文件，不另写一份签名。 */
export async function renderScriptContracts(): Promise<string> {
  const files = [
    new URL("../contracts/phone.ts", import.meta.url),
    new URL("../contracts/model.ts", import.meta.url),
    new URL("../contracts/script.ts", import.meta.url),
    new URL("../contracts/mind.ts", import.meta.url),
    new URL("../contracts/notes.ts", import.meta.url),
    new URL("../contracts/workspace.ts", import.meta.url),
    new URL("../contracts/memory.ts", import.meta.url),
    new URL("../contracts/upgrades.ts", import.meta.url),
  ];
  const sections: string[] = [];
  for (const file of files) {
    sections.push(
      `接口来源：contracts/${file.pathname.split("/").at(-1)}\n\`\`\`ts\n${await Deno.readTextFile(
        file,
      )}\`\`\``,
    );
  }
  return "唯一原生工具 execute_script 的 code 是异步函数正文。当前参数名 tools，类型为 ScriptApi；可 await 和 return。\n" +
    "脚本默认异步返回持久任务编号，具体动作编号由宿主生成。不要把同一任务重新写成新调用来猜结果；查 scripts.status/receipt 或 phone.action。\n" +
    "mode:sync 等待完成后，原生回执的 result 是脚本 return 值的 JSON 文本页；result.truncated 为 true 时保留原 jobId，按 nextChar 分页读取。tools.scripts.result 的 limit 为 1 至 8192；用 return await tools.scripts.result(...) 将该页交回主意识。\n" +
    "upgrades.request 只提交完整 Git commit，独立监督校验并由旧 Eve 明确确认；回执 actionId 用于 upgrades.status，不以 submitted 冒充升级完成。\n" +
    "以下为当前契约原件；PhoneApi.send/sendFile 在 ScriptApi 中省去 actionId，由宿主提供。ScriptHostApi 供宿主领取任务/通知，不是 tools 内的额外对象。\n\n" +
    sections.join("\n\n");
}
