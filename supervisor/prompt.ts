import { renderScriptContracts } from "../composition/script-docs.ts";
import { KernelError } from "../kernel/protocol.ts";
import type { SourceArtifact } from "./source.ts";

/** Read candidate files as data only. Never import or run candidate composition code as root. */
async function generatedPrompt(source: SourceArtifact): Promise<string> {
  const read = async (path: string) => {
    const member = source.files.find((file) => file.path === path);
    if (!member) throw new KernelError("corrupt", "固定源码缺少生成提示所需原件");
    const bytes = await Deno.readFile(`${source.directory}/${path}`);
    const digest = [...new Uint8Array(await crypto.subtle.digest("SHA-256", bytes))]
      .map((byte) => byte.toString(16).padStart(2, "0")).join("");
    if (digest !== member.sha256) throw new KernelError("corrupt", "生成提示所需源码摘要变化");
    return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  };
  const qq = await read("prompts/qq-social.md");
  if (!qq.trim()) throw new KernelError("corrupt", "QQ 提示原件为空");
  const hasMechanics = source.files.some((file) => file.path === "prompts/qq-mechanics.md");
  const splitLayout = /^<!-- eve-qq-prompt-layout: social-mechanics-v1 -->\r?$/m.test(qq);
  if (splitLayout && !hasMechanics) {
    throw new KernelError("corrupt", "固定源码缺少 QQ 机制提示原件");
  }
  // Old source artifacts have only social + contracts. Reproduce those exact bytes
  // to identify the existing generated suffix before introducing the new section.
  const mechanics = hasMechanics ? await read("prompts/qq-mechanics.md") : undefined;
  if (mechanics !== undefined && !mechanics.trim()) {
    throw new KernelError("corrupt", "QQ 机制提示原件为空");
  }
  return `${qq}\n\n${
    mechanics === undefined ? "" : `${mechanics}\n\n`
  }${await renderScriptContracts(read)}`;
}

/** Preserve the complete identity/custom prefix; ambiguous legacy prompts require explicit repair. */
export async function refreshUpgradePrompt(
  prompt: string,
  active: SourceArtifact,
  candidate: SourceArtifact,
): Promise<string> {
  const oldGenerated = `\n\n${await generatedPrompt(active)}`;
  const boundary = prompt.indexOf(oldGenerated);
  if (
    boundary < 0 || boundary !== prompt.lastIndexOf(oldGenerated) ||
    boundary + oldGenerated.length !== prompt.length || !prompt.slice(0, boundary).trim()
  ) {
    throw new KernelError("rejected", "旧提示生成区无法唯一确认，保留原件并先修复提示格式");
  }
  let identity = prompt.slice(0, boundary);
  // Only migrate an unmodified, pinned stock identity. Custom identities remain byte-exact.
  const readIdentity = async (source: SourceArtifact) => {
    const member = source.files.find((file) => file.path === "prompts/identity.md");
    if (!member) return undefined;
    const bytes = await Deno.readFile(`${source.directory}/${member.path}`);
    const digest = [...new Uint8Array(await crypto.subtle.digest("SHA-256", bytes))]
      .map((b) => b.toString(16).padStart(2, "0")).join("");
    if (digest !== member.sha256) throw new KernelError("corrupt", "身份原件摘要变化");
    return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  };
  const oldIdentity = await readIdentity(active);
  if (oldIdentity !== undefined && identity === oldIdentity) {
    identity = await readIdentity(candidate) ?? identity;
  }
  return `${identity}\n\n${await generatedPrompt(candidate)}`;
}
