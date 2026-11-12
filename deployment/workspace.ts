/** Prepare a complete independent single-branch Git history, never shared object links into /root. */
export async function prepareWorkspaceRepository(options: {
  repository: string;
  branch: string;
  commit: string;
  target: string;
  uid: number;
  gid: number;
}) {
  if (
    ![options.repository, options.target].every((p) =>
      /^\/[A-Za-z0-9_./-]+$/.test(p) && !p.split("/").includes("..")
    ) || options.target.startsWith("/root/") || !/^[a-f0-9]{40}$/.test(options.commit) ||
    !/^[a-zA-Z0-9][a-zA-Z0-9/_.-]*$/.test(options.branch) || options.branch.includes("..") ||
    ![options.uid, options.gid].every((n) => Number.isSafeInteger(n) && n > 0)
  ) throw new Error("源码工作仓库需要固定提交、独立路径、有效分支和非root身份");
  async function git(args: string[], cwd?: string) {
    const output = await new Deno.Command("git", {
      args,
      cwd,
      stdout: "piped",
      stderr: "piped",
      env: { GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: "/dev/null", GIT_TERMINAL_PROMPT: "0" },
    }).output();
    if (!output.success) throw new Error("源码工作仓库准备失败；保留现场供核对");
    return new TextDecoder().decode(output.stdout).trim();
  }
  await git([
    "clone",
    "--no-local",
    "--single-branch",
    "--branch",
    options.branch,
    "--",
    options.repository,
    options.target,
  ]);
  if (await git(["rev-parse", "HEAD"], options.target) !== options.commit) {
    throw new Error("工作仓库HEAD与待部署固定源码不一致，保留现场");
  }
  await git(["remote", "remove", "origin"], options.target);
  await git(["config", "user.name", "Eve"], options.target);
  await git(["config", "user.email", "eve@localhost"], options.target);
  async function own(path: string) {
    const stat = await Deno.lstat(path);
    if (stat.isSymlink) throw new Error("新工作仓库包含链接，拒绝跟随");
    if (stat.isDirectory) {
      for await (const entry of Deno.readDir(path)) await own(`${path}/${entry.name}`);
    }
    await Deno.chown(path, options.uid, options.gid);
  }
  await own(options.target);
}
