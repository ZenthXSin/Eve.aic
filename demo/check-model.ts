// 显式运行的两次真实模型探针；不连接 QQ，不启动主循环，不更改私有配置。
import type { ModelRequest } from "../contracts/model.ts";
import { ChatModelClient } from "../plugins/model-chat/client.ts";
import { readPrivateModelConfig } from "../plugins/model-chat/mod.ts";

if (Deno.args.length !== 1) throw new Error("需要私有配置文件路径");
const config = await readPrivateModelConfig(Deno.args[0]);
const apiKey = (await Deno.readTextFile(config.apiKeyFile)).trim();
const client = new ChatModelClient({ ...config, apiKey });
const request: ModelRequest = {
  messages: [
    {
      role: "system",
      content:
        "这是模型传输与工具回传测试，不是真实用户对话。请使用 echo 工具原样返回用户的测试词；拿到工具结果后直接转述。",
    },
    { role: "user", content: "测试词：海风" },
  ],
  tools: [{
    name: "echo",
    description: "原样返回 text 的测试工具",
    parameters: {
      type: "object",
      properties: { text: { type: "string" } },
      required: ["text"],
      additionalProperties: false,
    },
  }],
};
const started = Date.now();
try {
  const first = await client.complete(request);
  const call = first.toolCalls[0];
  if (first.toolCalls.length !== 1 || call.name !== "echo") {
    console.log(
      JSON.stringify({
        model: config.model,
        stage: "first_response",
        passed: false,
        reason: "未选择一次 echo",
        usage: first.usage,
      }),
    );
    Deno.exitCode = 1;
  } else {
    const text = (call.args as { text?: unknown }).text;
    if (typeof text !== "string") throw new Error("模型返回的 echo 参数未通过工具契约");
    const second = await client.complete({
      ...request,
      messages: [
        ...request.messages,
        { role: "assistant", content: first.content, toolCalls: first.toolCalls },
        { role: "tool", toolCallId: call.id, content: text },
      ],
    });
    const passed = second.toolCalls.length === 0 && second.content.includes(text);
    console.log(JSON.stringify({
      model: config.model,
      reasoningEffort: config.reasoningEffort,
      passed,
      requests: 2,
      elapsedMs: Date.now() - started,
      firstUsage: first.usage,
      secondUsage: second.usage,
      testToolOutput: text,
      finalOutput: second.content,
      qqConnected: false,
      styleEvaluated: false,
    }));
    if (!passed) Deno.exitCode = 1;
  }
} finally {
  client.close();
}
