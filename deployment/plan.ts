import type { SourceArtifact } from "../supervisor/source.ts";
import { verifySource } from "../supervisor/source.ts";
import { writeEvePrompt } from "../composition/eve.ts";

export interface DeploymentOptions {
  privateDirectory: string;
  runtimeDirectory: string;
  /** Existing adapter data, owner binding and original messages are retained in place. */
  qqDirectory: string;
  /** Must be traversable by the command UID, outside /root and private runtime state. */
  workspaceDirectory: string;
  deno: string;
  source: SourceArtifact;
  commandIdentity: { uid: number; gid: number };
  instanceIdentities: { uid: number; gid: number }[];
  instanceId?: string;
  repositoryDirectory?: string;
  artifactDirectory?: string;
  timeZone?: string;
  dailyTokenBudget?: number;
}
export interface DeploymentPlan {
  directories: {
    path: string;
    owner: "root" | "command" | "source" | "instance" | "instance-private" | "traverse" | "public";
    identity?: { uid: number; gid: number };
  }[];
  files: { path: string; content: string; mode: number; identity?: { uid: number; gid: number } }[];
  copies: { source: string; path: string; identity: { uid: number; gid: number } }[];
  prompt: { path: string; identity: { uid: number; gid: number } };
  units: { name: string; content: string }[];
  commandIdentity: { uid: number; gid: number };
  summary: { instanceId: string; sourceCommit: string; workspace: string; services: string[] };
}
function absolute(path: string) {
  if (!/^\/[A-Za-z0-9_./-]+$/.test(path) || path.split("/").includes("..") || path === "/") {
    throw new Error("部署使用无转义的独立绝对路径");
  }
  return path.replace(/\/$/, "");
}
function key() {
  return [...crypto.getRandomValues(new Uint8Array(32))].map((n) => n.toString(16).padStart(2, "0"))
    .join("");
}
function unit(
  name: string,
  entry: string,
  config: string,
  options: DeploymentOptions,
  after: string[],
  args: string[] = [],
) {
  const directory = options.source.directory;
  return {
    name: `eve-rebuild-${name}.service`,
    content: `[Unit]\nDescription=Eve ${name}\nAfter=network-online.target ${
      after.join(" ")
    }\nWants=network-online.target\n\n[Service]\nType=simple\nWorkingDirectory=${directory}\nExecStart=${options.deno} run --allow-read --allow-write --allow-run --allow-env --allow-net --unstable-worker-options ${directory}/${entry} ${config}${
      args.length ? " " + args.join(" ") : ""
    }\nRestart=on-failure\nRestartSec=3\nTimeoutStopSec=180\nKillMode=control-group\nUMask=0077\nNoNewPrivileges=yes\nProtectSystem=strict\nReadWritePaths=${options.privateDirectory} ${options.runtimeDirectory} ${options.qqDirectory} ${options.workspaceDirectory} ${
      name === "supervisor"
        ? (options.artifactDirectory ??
          options.source.directory.slice(0, options.source.directory.lastIndexOf("/")))
        : ""
    }\nPrivateTmp=yes\nEnvironment=DENO_DIR=${options.runtimeDirectory}/cache/${name}\n\n[Install]\nWantedBy=multi-user.target\n`,
  };
}
/** This returns a private in-memory plan. Never log `files`: they contain generated keys and owner IDs. */
export function buildDeploymentPlan(
  existing: Record<string, unknown>,
  raw: DeploymentOptions,
): DeploymentPlan {
  const options = { ...raw };
  for (
    const name of [
      "privateDirectory",
      "runtimeDirectory",
      "qqDirectory",
      "workspaceDirectory",
      "deno",
    ] as const
  ) options[name] = absolute(options[name]);
  absolute(options.source.directory);
  if (
    [
      options.source.directory,
      options.deno,
      options.workspaceDirectory,
      options.runtimeDirectory,
      options.privateDirectory,
    ].some((path) => path === "/root" || path.startsWith("/root/"))
  ) throw new Error("命令用户所需源码、Deno和工作空间必须放在可遍历的root私有目录之外");
  if (!/^[a-f0-9]{40}$/.test(options.source.commit)) throw new Error("部署必须固定完整源码提交");
  if (
    [
      options.privateDirectory,
      options.runtimeDirectory,
      options.qqDirectory,
      options.source.directory,
    ].some((path) =>
      options.workspaceDirectory === path || options.workspaceDirectory.startsWith(`${path}/`) ||
      path.startsWith(`${options.workspaceDirectory}/`)
    )
  ) throw new Error("命令工作空间必须与私有目录、事实原件及固定源码分离");
  const { uid, gid } = options.commandIdentity;
  if (![uid, gid].every((n) => Number.isSafeInteger(n) && n > 0)) {
    throw new Error("命令执行必须使用独立非 root UID/GID");
  }
  if (
    !Array.isArray(options.instanceIdentities) || options.instanceIdentities.length !== 2 ||
    options.instanceIdentities.some((id) =>
      ![id.uid, id.gid].every((n) => Number.isSafeInteger(n) && n > 0)
    ) || new Set([uid, ...options.instanceIdentities.map((id) => id.uid)]).size !== 3 ||
    new Set([gid, ...options.instanceIdentities.map((id) => id.gid)]).size !== 3
  ) throw new Error("两个实例必须使用彼此及命令用户不同的非root身份");
  const instanceIdentity = options.instanceIdentities[0];
  const model = existing.model as Record<string, unknown>,
    qq = existing.qq as Record<string, unknown>,
    phone = existing.phone as Record<string, unknown>;
  if (
    !model || model.protocol !== "chat" || typeof model.apiKeyFile !== "string" || !qq ||
    (qq.replyScope !== undefined && qq.replyScope !== "private" && qq.replyScope !== "all") ||
    qq.sandbox !== false || !phone ||
    typeof phone.ownerPlatformUserId !== "string" || !phone.ownerPlatformUserId
  ) throw new Error("沿用配置缺少已绑定主人或有效QQ/模型配置");
  for (const path of [model.apiKeyFile, qq.appIdFile, qq.appSecretFile]) absolute(path as string);
  if (
    ![model.timeoutMs, model.maxOutputTokens].every((n) => Number.isSafeInteger(n) && Number(n) > 0)
  ) throw new Error("沿用模型配置缺少有效超时/输出上限");
  const repository = absolute(
    options.repositoryDirectory ?? `${options.workspaceDirectory}/source`,
  );
  if (!repository.startsWith(`${options.workspaceDirectory}/`)) {
    throw new Error("Eve可编辑源码仓库必须放在命令工作空间内");
  }
  const artifactDirectory = absolute(
    options.artifactDirectory ??
      options.source.directory.slice(0, options.source.directory.lastIndexOf("/")),
  );
  if (artifactDirectory.startsWith("/root/")) throw new Error("固定源码产物必须可供命令用户读取");
  const id = options.instanceId ?? "eve-initial";
  if (!/^[a-z][a-z0-9_-]{0,63}$/.test(id)) throw new Error("实例编号无效");
  const timeZone = options.timeZone ?? "Asia/Tokyo";
  new Intl.DateTimeFormat("en", { timeZone }).format();
  const dailyTokenBudget = options.dailyTokenBudget ?? 200_000_000;
  if (!Number.isSafeInteger(dailyTokenBudget) || dailyTokenBudget < 0) throw new Error("预算无效");
  const p = options.privateDirectory, r = options.runtimeDirectory;
  const i = `${r}/instances/${id}`, privateInstance = `${p}/instances/${id}`;
  const guardianConnectionFile = `${r}/public/guardian.json`;
  const guardianAdminKeyFile = `${p}/guardian-admin.key`, exitKeyFile = `${p}/guardian-exit.key`;
  const runtimeKeyFile = `${p}/runtime.key`, runtimeAdminKeyFile = `${p}/runtime-admin.key`;
  const files: DeploymentPlan["files"] = [];
  const json = (path: string, content: unknown) =>
    files.push({ path, content: JSON.stringify(content, null, 2) + "\n", mode: 0o600 });
  for (
    const path of [
      guardianAdminKeyFile,
      exitKeyFile,
      runtimeKeyFile,
      runtimeAdminKeyFile,
      `${p}/supervisor.key`,
      `${p}/admin.key`,
      `${p}/admin-control.key`,
      `${privateInstance}/control.key`,
      `${privateInstance}/credential.key`,
    ]
  ) files.push({ path, content: key() + "\n", mode: 0o600 });
  const runtimeKey = files.find((f) => f.path === runtimeKeyFile)!.content;
  files.push({
    path: `${privateInstance}/runtime.key`,
    content: runtimeKey,
    mode: 0o600,
    identity: instanceIdentity,
  });
  json(`${p}/guardian.json`, {
    ownership: {
      directory: `${r}/guardian`,
      connectionFile: guardianConnectionFile,
      adminKeyFile: guardianAdminKeyFile,
      exitKeyFile,
      leaseMs: 30_000,
    },
  });
  json(`${p}/runtime-settings.json`, { dailyTokenBudget, paused: true, timeZone });
  json(`${p}/runtime.json`, {
    model,
    runtimeService: {
      directory: `${r}/backend`,
      connectionFile: `${r}/public/runtime.json`,
      workspace: options.workspaceDirectory,
      allowedRoots: [options.workspaceDirectory],
      settingsFile: `${p}/runtime-settings.json`,
      keyFile: runtimeKeyFile,
      guardianConnectionFile,
      exitKeyFile,
      adminKeyFile: runtimeAdminKeyFile,
      adminConnectionFile: `${r}/backend/admin-connection.json`,
      commandIdentity: { uid, gid },
      commandUseSystemd: true,
    },
  });
  json(`${p}/qq.json`, {
    phoneConnectionFile: `${r}/public/phone.json`,
    qq: {
      ...qq,
      replyScope: qq.replyScope ?? "private",
      captureActivationBoundary: true,
      mediaRoots: [options.workspaceDirectory],
      mediaIdentity: { uid, gid },
      mediaRuntime: {
        denoBinary: options.deno,
        workerPath: `${options.source.directory}/services/runtime/files-worker.ts`,
        cacheDirectory: `${options.workspaceDirectory}/.eve-tool-cache`,
      },
    },
    qqOwnership: { guardianConnectionFile, exitKeyFile },
  });
  json(`${privateInstance}/config.json`, {
    instance: {
      id,
      sourceCommit: options.source.commit,
      directory: i,
      keyFile: `${privateInstance}/control.key`,
      connectionFile: `${i}/connection.json`,
      guardianConnectionFile,
      credentialFile: `${privateInstance}/credential.key`,
      leaseMs: 30_000,
    },
    runtime: {
      connectionFile: `${r}/public/runtime.json`,
      upgradeConnectionFile: `${r}/public/upgrade.json`,
      keyFile: `${privateInstance}/runtime.key`,
      proofFile: `${privateInstance}/proof.json`,
    },
    phone: {
      describeImages: true,
      adapterUrl: phone.adapterUrl,
      ownerPlatformUserId: phone.ownerPlatformUserId,
      connectionFile: `${r}/public/phone.json`,
      adapterKeyFile: `${privateInstance}/phone.key`,
      ownershipProofFile: `${privateInstance}/proof.json`,
    },
    eve: {
      directory: `${i}/consciousness`,
      promptFile: `${privateInstance}/identity.md`,
      sourceCommit: options.source.commit,
      modelTimeoutMs: model.timeoutMs,
      maxOutputTokens: model.maxOutputTokens,
      pollMs: 1000,
      defaultSleepMs: 3_600_000,
      dailyTokenBudget,
      timeZone,
      remoteRecords: true,
    },
    scripts: {
      directory: `${i}/scripts`,
      cacheDirectory: `${i}/cache/scripts`,
      denoBinary: options.deno,
      defaultTimeoutMs: 120_000,
    },
    mindDirectory: `${i}/mind`,
    kernelDataDirectory: `${i}/kernel`,
  });
  json(`${p}/supervisor.json`, {
    supervisor: {
      directory: `${r}/supervisor`,
      connectionFile: `${r}/supervisor/connection.json`,
      keyFile: `${p}/supervisor.key`,
      adminKeyFile: `${p}/admin-control.key`,
      adminConnectionFile: `${r}/supervisor/admin-connection.json`,
      guardianConnectionFile,
      guardianAdminKeyFile,
      deno: options.deno,
      repository,
      artifactDirectory,
      instancePrivateDirectory: `${p}/instances`,
      instanceRuntimeDirectory: `${r}/instances`,
      instanceIdentities: options.instanceIdentities,
      commandIdentity: options.commandIdentity,
      upgradeConnectionFile: `${r}/public/upgrade.json`,
      initialInstance: id,
      tickMs: 2000,
      confirmationMs: 300_000,
    },
    instances: [{
      id,
      identity: instanceIdentity,
      source: options.source,
      configFile: `${privateInstance}/config.json`,
      connectionFile: `${i}/connection.json`,
      keyFile: `${privateInstance}/control.key`,
      credentialFile: `${privateInstance}/credential.key`,
      proofFile: `${privateInstance}/proof.json`,
    }],
    runtimeAdmin: {
      connectionFile: `${r}/backend/admin-connection.json`,
      keyFile: runtimeAdminKeyFile,
    },
  });
  json(`${p}/admin.json`, {
    admin: {
      directory: `${r}/admin`,
      port: 0,
      tokenFile: `${p}/admin.key`,
      backendConnectionFile: `${r}/supervisor/admin-connection.json`,
      backendKeyFile: `${p}/admin-control.key`,
    },
  });
  const names = ["guardian", "runtime", "qq-adapter", "supervisor", "admin"];
  const units = [
    unit("guardian", "guardian/main.ts", `${p}/guardian.json`, options, []),
    unit("runtime", "services/runtime/main.ts", `${p}/runtime.json`, options, [
      "eve-rebuild-guardian.service",
    ]),
    unit("qq-adapter", "adapters/qq/main.ts", `${p}/qq.json`, options, [
      "eve-rebuild-guardian.service",
    ], [options.qqDirectory]),
    unit("supervisor", "supervisor/main.ts", `${p}/supervisor.json`, options, [
      "eve-rebuild-runtime.service",
      "eve-rebuild-qq-adapter.service",
    ]),
    unit("admin", "services/admin/main.ts", `${p}/admin.json`, options, [
      "eve-rebuild-supervisor.service",
    ]),
  ];
  const directories: DeploymentPlan["directories"] = [
    { path: p, owner: "traverse" },
    { path: `${p}/instances`, owner: "traverse" },
    { path: r, owner: "traverse" },
    { path: `${r}/instances`, owner: "traverse" },
    { path: `${r}/public`, owner: "public" },
    ...[
      `${r}/guardian`,
      `${r}/backend`,
      `${r}/supervisor`,
      `${r}/admin`,
      ...names.map((n) => `${r}/cache/${n}`),
    ].map((path) => ({ path, owner: "root" as const })),
    { path: privateInstance, owner: "instance-private", identity: instanceIdentity },
    ...[
      i,
      `${i}/consciousness`,
      `${i}/scripts`,
      `${i}/mind`,
      `${i}/kernel`,
      `${i}/cache`,
      `${i}/cache/scripts`,
    ].map((path) => ({ path, owner: "instance" as const, identity: instanceIdentity })),
    { path: options.workspaceDirectory, owner: "command" },
    { path: artifactDirectory, owner: "source" },
  ];
  for (const file of files) {
    if (file.path.startsWith(`${privateInstance}/`)) {
      file.identity = instanceIdentity;
      file.mode = 0o440;
    }
  }
  const copies = [{
    source: absolute(phone.adapterKeyFile as string),
    path: `${privateInstance}/phone.key`,
    identity: instanceIdentity,
  }];
  return {
    directories,
    files,
    copies,
    prompt: { path: `${privateInstance}/identity.md`, identity: instanceIdentity },
    units,
    commandIdentity: { uid, gid },
    summary: {
      instanceId: id,
      sourceCommit: options.source.commit,
      workspace: options.workspaceDirectory,
      services: units.map((u) => u.name),
    },
  };
}

async function rejectSymlinkAncestors(path: string) {
  const segments = path.split("/").filter(Boolean);
  let at = "";
  for (const segment of segments) {
    at += `/${segment}`;
    try {
      const stat = await Deno.lstat(at);
      if (stat.isSymlink) throw new Error("部署路径不能经过链接");
    } catch (error) {
      if (!(error instanceof Deno.errors.NotFound)) throw error;
    }
  }
}
/** Source consists only of verified Git blobs, never private runtime config. */
async function readableSource(source: SourceArtifact) {
  const directories = new Set([source.directory]);
  for (const file of source.files) {
    const path = `${source.directory}/${file.path}`;
    await Deno.chmod(path, file.executable ? 0o755 : 0o644);
    await Deno.chown(path, 0, 0);
    let directory = path.slice(0, path.lastIndexOf("/"));
    while (directory.startsWith(source.directory)) {
      directories.add(directory);
      directory = directory.slice(0, directory.lastIndexOf("/"));
    }
  }
  for (const directory of directories) {
    await Deno.chmod(directory, 0o755);
    await Deno.chown(directory, 0, 0);
  }
  let ancestor = source.directory.slice(0, source.directory.lastIndexOf("/"));
  while (ancestor) {
    const stat = await Deno.stat(ancestor);
    await Deno.chmod(ancestor, (stat.mode ?? 0o700) | 0o111);
    ancestor = ancestor.slice(0, ancestor.lastIndexOf("/"));
  }
}

/** Stage only. Exclusive writes refuse to rotate live credentials or overwrite existing data. */
export async function stageDeployment(
  plan: DeploymentPlan,
  source: SourceArtifact,
  unitDirectory: string,
) {
  await verifySource(source);
  absolute(unitDirectory);
  const targets = [
    ...plan.files.map((f) => f.path),
    ...plan.copies.map((f) => f.path),
    plan.prompt.path,
    ...plan.units.map((u) => `${unitDirectory}/${u.name}`),
  ];
  for (const path of [...targets, ...plan.directories.map((d) => d.path), source.directory]) {
    await rejectSymlinkAncestors(path);
  }
  for (const path of targets) {
    try {
      await Deno.lstat(path);
      throw new Deno.errors.AlreadyExists("部署目标已存在，拒绝覆盖");
    } catch (error) {
      if (!(error instanceof Deno.errors.NotFound)) throw error;
    }
  }
  await readableSource(source);
  for (const directory of plan.directories) {
    const mode = directory.owner === "traverse"
      ? 0o711
      : ["source", "public"].includes(directory.owner)
      ? 0o755
      : ["command", "instance-private"].includes(directory.owner)
      ? 0o750
      : 0o700;
    await Deno.mkdir(directory.path, { recursive: true, mode });
    const stat = await Deno.lstat(directory.path);
    if (!stat.isDirectory || stat.isSymlink) throw new Error("部署目录不能为链接");
    await Deno.chmod(directory.path, mode);
    const identity = directory.identity ??
      (directory.owner === "command" ? plan.commandIdentity : undefined);
    if (identity) {
      await Deno.chown(
        directory.path,
        directory.owner === "instance-private" ? 0 : identity.uid,
        identity.gid,
      );
    } else await Deno.chown(directory.path, 0, 0);
  }
  await Deno.mkdir(unitDirectory, { recursive: true, mode: 0o700 });
  for (const file of plan.files) {
    await Deno.writeTextFile(file.path, file.content, { createNew: true, mode: file.mode });
    await Deno.chmod(file.path, file.mode);
    if (file.identity) await Deno.chown(file.path, 0, file.identity.gid);
  }
  for (const copy of plan.copies) {
    const stat = await Deno.lstat(copy.source);
    if (!stat.isFile || stat.isSymlink || stat.size > 512) throw new Error("桥接密钥原件无效");
    await Deno.writeTextFile(copy.path, await Deno.readTextFile(copy.source), {
      createNew: true,
      mode: 0o440,
    });
    await Deno.chmod(copy.path, 0o440);
    await Deno.chown(copy.path, 0, copy.identity.gid);
  }
  await writeEvePrompt(plan.prompt.path);
  await Deno.chmod(plan.prompt.path, 0o440);
  await Deno.chown(plan.prompt.path, 0, plan.prompt.identity.gid);
  for (const file of plan.units) {
    await Deno.writeTextFile(`${unitDirectory}/${file.name}`, file.content, {
      createNew: true,
      mode: 0o600,
    });
  }
}
