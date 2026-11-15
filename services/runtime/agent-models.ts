import type { AgentRole, ReasoningEffort } from "../../contracts/agents.ts";
import { ChatModelClient, type ChatModelOptions } from "../../plugins/model-chat/client.ts";
import type { ModelSnapshot } from "./model-selection.ts";

export type AgentModels = Partial<
  Record<AgentRole, ModelSnapshot & { reasoningEffort: ReasoningEffort }>
>;

/** Private role credentials stay in the host configuration, never in agent messages or receipts. */
export async function readAgentModels(
  value: unknown,
  inherited: ChatModelOptions,
): Promise<AgentModels> {
  const profiles: AgentModels = {};
  if (value === undefined) return profiles;
  try {
    if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error();
    for (const [role, raw] of Object.entries(value)) {
      if (
        !["review", "coding", "investigation"].includes(role) || !raw ||
        typeof raw !== "object" || Array.isArray(raw)
      ) throw new Error();
      const config = raw as Record<string, unknown>;
      if (
        Object.keys(config).some((k) =>
          !["model", "apiKeyFile", "reasoningEffort", "maxOutputTokens"].includes(k)
        ) ||
        typeof config.model !== "string" ||
        !/^[a-zA-Z0-9][a-zA-Z0-9._:/@+-]{0,255}$/.test(config.model) ||
        typeof config.apiKeyFile !== "string" || !config.apiKeyFile.startsWith("/") ||
        config.reasoningEffort !== undefined &&
          !["none", "minimal", "low", "medium", "high", "xhigh"].includes(
            String(config.reasoningEffort),
          )
      ) throw new Error();
      const reasoningEffort = (config.reasoningEffort ?? "high") as ReasoningEffort;
      const maxOutputTokens = config.maxOutputTokens ?? inherited.maxOutputTokens;
      if (!Number.isSafeInteger(maxOutputTokens) || Number(maxOutputTokens) < 1) throw new Error();
      const apiKey = (await Deno.readTextFile(config.apiKeyFile)).trim();
      if (!apiKey) throw new Error();
      profiles[role as AgentRole] = {
        modelName: config.model,
        revision: 0,
        maxOutputTokens: Number(maxOutputTokens),
        reasoningEffort,
        client: new ChatModelClient({
          ...inherited,
          model: config.model,
          apiKey,
          reasoningEffort,
          maxOutputTokens: Number(maxOutputTokens),
          sessionId: `agent_${role}`,
        }),
      };
    }
    return profiles;
  } catch {
    for (const profile of Object.values(profiles)) profile.client.close();
    throw new Error("子代理私有模型配置无效，请检查角色、模型与密钥文件");
  }
}
