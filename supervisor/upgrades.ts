import type { UpgradeFailure } from "../contracts/upgrades.ts";
import type { OwnershipProof, OwnershipSnapshot } from "../contracts/ownership.ts";
import { OwnershipRpcClient } from "../guardian/rpc.ts";
import { fingerprint } from "../kernel/calls.ts";
import { type Json, KernelError } from "../kernel/protocol.ts";
import { writeDurableJson } from "../sdk/durable-json.ts";
import { FactJournal } from "../sdk/fact-journal.ts";
import type { InstanceSpec, Supervisor } from "./controller.ts";
import { pinSource } from "./source.ts";
import { readTrustedInstanceConfig } from "./driver.ts";
import { refreshUpgradePrompt } from "./prompt.ts";
import { LOG_UNITS, type LogSource, readLogFile, readServiceJournal } from "./logs.ts";

export interface UpgradeReceipt {
  actionId: string;
  requestedBy?: "instance" | "administrator";
  commit: string;
  state: "preparing" | "submitted" | "failed";
  instanceId?: string;
  error?: string | { kind: string; message: string };
  outcome?: string;
  upgradeId?: string;
  phase?: string;
  operation?: string;
  reason?: string;
  failure?: UpgradeFailure;
}
export interface UpgradeOptions {
  directory: string;
  mode?: "replace" | "observed";
  repository: string;
  artifactDirectory: string;
  instancePrivateDirectory: string;
  instanceRuntimeDirectory: string;
  guardianConnectionFile: string;
  deno: string;
  commandIdentity: { uid: number; gid: number };
  instanceIdentities: { uid: number; gid: number }[];
}

/** 实例入口校验当前在线代际；独立控制入口另验管理密钥，复用同一升级与确认流程。 */
export class UpgradeRequests {
  #journal: FactJournal;
  #receipts = new Map<string, UpgradeReceipt>();
  #tail: Promise<unknown> = Promise.resolve();
  #work?: Promise<void>;
  #closed = false;
  constructor(
    readonly options: UpgradeOptions,
    readonly supervisor: Supervisor,
    private readonly administratorKey?: string,
  ) {
    this.#journal = new FactJournal(`${options.directory}/requests`);
  }
  async initialize() {
    await this.#journal.initialize();
    for (const event of this.#journal.events) {
      const item = event.data as unknown as UpgradeReceipt;
      if (
        event.kind !== "receipt" || typeof item?.actionId !== "string" ||
        !/^[a-f0-9]{40}$/.test(item.commit) ||
        !["preparing", "submitted", "failed"].includes(item.state) ||
        (item.requestedBy !== undefined &&
          !["instance", "administrator"].includes(item.requestedBy))
      ) throw new KernelError("corrupt", "升级申请原件损坏");
      this.#receipts.set(item.actionId, item);
    }
    for (const item of this.#receipts.values()) {
      if (item.state === "preparing" || (item.state === "submitted" && !item.upgradeId)) {
        // 已进入控制器的申请继续采用原事实；纯构建中断不猜通过、不重跑命令。
        const known = item.instanceId &&
          this.supervisor.options.instances.some((i) => i.id === item.instanceId);
        item.state = known ? "submitted" : "failed";
        if (!known) item.error = "validation_interrupted";
        else {
          const upgrade = this.supervisor.state.upgrade;
          item.upgradeId ??= `upgrade_${
            (await fingerprint([item.actionId, item.commit])).slice(0, 48)
          }`;
          if (upgrade && upgrade.candidate === item.instanceId) item.upgradeId = upgrade.id;
          else if (
            this.supervisor.state.lastOutcome?.upgradeId !== item.upgradeId &&
            this.supervisor.state.current !== item.instanceId
          ) {
            if (this.supervisor.state.paused || upgrade) {
              item.state = "failed";
              item.error = "submission_interrupted";
            } else {
              // registry先落盘而upgrade意图尚未落盘时，用预存稳定编号补齐。
              if (this.options.mode === "observed") {
                await this.supervisor.upgrade(item.instanceId!, item.upgradeId);
              } else {await this.supervisor.stageReplacement(() =>
                  Promise.resolve(this.supervisor.instance(item.instanceId!)), item.upgradeId);}
            }
          }
        }
        await this.#save(item);
      }
    }
  }
  async #save(item: UpgradeReceipt) {
    await this.#journal.append("receipt", item as unknown as Json);
    this.#receipts.set(item.actionId, structuredClone(item));
  }
  async invoke(data: Record<string, Json>, authorization: string | null): Promise<Json> {
    if (this.#closed) throw new KernelError("unavailable", "升级申请器已关闭");
    if (Object.keys(data).some((key) => !["method", "args", "ownership"].includes(key))) {
      throw new KernelError("rejected", "实例升级信封不接受管理标记");
    }
    const ownership = data.ownership as unknown as { proof: OwnershipProof; actionId: string };
    const proof = ownership?.proof;
    if (
      !proof || authorization !== `Bearer ${proof.credential}` ||
      typeof ownership.actionId !== "string" ||
      !/^[a-zA-Z0-9_.:-]{1,256}$/.test(ownership.actionId) ||
      ownership.actionId.startsWith("administrator_")
    ) throw new KernelError("rejected", "升级需要当前实例证明与稳定申请编号");
    const connection = JSON.parse(await Deno.readTextFile(this.options.guardianConnectionFile));
    const rpc = new OwnershipRpcClient(connection.url, proof.credential);
    let current: OwnershipSnapshot;
    try {
      current = await rpc.call("actorStatus", proof);
    } finally {
      rpc.close();
    }
    if (
      current.mode !== "active" || current.owner?.instanceId !== proof.instanceId ||
      current.epoch !== proof.epoch || current.owner.expiresAt <= Date.now()
    ) throw new KernelError("unavailable", "旧版或过期实例不能申请升级");
    const args = data.args as Record<string, Json>;
    if (data.method === "logs") return await this.#logs(args);
    if (data.method === "status") {
      if (
        !args || Object.keys(args).some((key) => key !== "actionId") ||
        (args.actionId !== undefined && (typeof args.actionId !== "string" || !args.actionId))
      ) throw new KernelError("rejected", "升级状态参数无效");
      const item = args.actionId === undefined
        ? [...this.#receipts.values()].at(-1)
        : this.#receipts.get(String(args.actionId));
      if (!item) throw new KernelError("not_found", "升级申请不存在");
      const state = this.supervisor.state, last = state.lastOutcome;
      const upgrade = state.upgrade?.id === item.upgradeId ? state.upgrade : undefined;
      return {
        ...item,
        requestedBy: item.requestedBy ?? "instance",
        ...(typeof item.error === "string"
          ? { error: { kind: item.error, message: "升级准备未完成，请按原申请核对" } }
          : {}),
        ...(upgrade ? { phase: upgrade.phase, operation: upgrade.operation ?? upgrade.phase } : {}),
        ...(last && last.upgradeId === item.upgradeId
          ? {
            outcome: last.outcome,
            reason: last.reason,
            ...(last.failure ? { failure: last.failure } : {}),
          }
          : {}),
      };
    }
    if (
      data.method !== "request" || typeof args?.commit !== "string" ||
      !/^[a-f0-9]{40}$/.test(args.commit) || Object.keys(args).some((key) => key !== "commit")
    ) throw new KernelError("rejected", "升级只接受固定完整Git提交");
    return await this.#submit(args.commit, ownership.actionId, "instance");
  }
  async #logs(args: Record<string, Json>): Promise<Json> {
    if (
      !args ||
      Object.keys(args).some((key) => !["source", "offset", "cursor", "limit"].includes(key))
    ) {
      throw new KernelError("rejected", "日志参数无效");
    }
    const sources: LogSource[] = LOG_UNITS.map((name) => ({
      id: `journal:${name}`,
      kind: "journal",
    }));
    const paths = new Map<string, string>();
    for (const item of this.#receipts.values()) {
      if (!item.instanceId || !/^[a-zA-Z0-9_-]{1,128}$/.test(item.instanceId)) continue;
      paths.set(
        `source:${item.instanceId}`,
        `${this.options.artifactDirectory}/${item.instanceId}/source-command.log`,
      );
      paths.set(
        `failure:${item.instanceId}`,
        `${this.options.directory}/diagnostics/${item.instanceId}.log`,
      );
      for (const task of ["check", "test"]) {
        paths.set(
          `validation:${item.instanceId}:${task}`,
          `${this.options.artifactDirectory}/${item.instanceId}/validation-${task}.log`,
        );
      }
    }
    for (const instance of this.supervisor.options.instances) {
      paths.set(
        `instance:${instance.id}`,
        `${this.options.directory}/processes/${instance.id}.log`,
      );
    }
    for (const [id, path] of paths) {
      try {
        const stat = await Deno.lstat(path);
        if (stat.isFile && !stat.isSymlink) sources.push({ id, kind: "file", bytes: stat.size });
      } catch (error) {
        if (!(error instanceof Deno.errors.NotFound)) throw error;
      }
    }
    if (args.source === undefined) {
      if (Object.keys(args).length) {
        throw new KernelError("rejected", "先列出日志源，再指定source分页");
      }
      return { sources: sources as unknown as Json };
    }
    if (typeof args.source !== "string" || !sources.some((s) => s.id === args.source)) {
      throw new KernelError("not_found", "日志源不存在，请先查询logs({})");
    }
    if (args.source.startsWith("journal:")) {
      if (args.offset !== undefined) throw new KernelError("rejected", "系统日志使用cursor");
      return await readServiceJournal(
        args.source.slice(8),
        args.cursor as string | undefined,
        args.limit as number | undefined,
      );
    }
    if (args.cursor !== undefined) throw new KernelError("rejected", "文件日志使用offset");
    return await readLogFile(
      paths.get(args.source)!,
      args.offset as number | undefined,
      args.limit as number | undefined,
    );
  }
  /** 只供独立监督控制入口调用；没有实例证明豁免标记或脚本契约。 */
  async requestAdministrator(args: Json, authorization: string | null): Promise<Json> {
    if (!this.administratorKey || authorization !== `Bearer ${this.administratorKey}`) {
      throw new KernelError("rejected", "管理员升级需要独立监督控制凭据");
    }
    if (
      !args || typeof args !== "object" || Array.isArray(args) ||
      Object.keys(args).some((key) => !["commit", "requestId"].includes(key)) ||
      typeof args.commit !== "string" || !/^[a-f0-9]{40}$/.test(args.commit) ||
      typeof args.requestId !== "string" || !/^[a-zA-Z0-9_.:-]{1,64}$/.test(args.requestId)
    ) throw new KernelError("rejected", "管理员升级需要完整Git提交和1至64字符稳定申请编号");
    return await this.#submit(args.commit, `administrator_${args.requestId}`, "administrator");
  }
  async #submit(
    commit: string,
    actionId: string,
    requestedBy: "instance" | "administrator",
  ): Promise<Json> {
    if (this.#closed) throw new KernelError("unavailable", "升级申请器已关闭");
    const operation = this.#tail.then(async () => {
      const old = this.#receipts.get(actionId);
      if (old) {
        if ((old.requestedBy ?? "instance") !== requestedBy) {
          throw new KernelError("rejected", "同一申请不能更换发起来源");
        }
        if (old.commit !== commit) throw new KernelError("rejected", "同一申请不能换提交");
        return old;
      }
      if (this.#work || this.supervisor.state.upgrade || this.supervisor.state.paused) {
        throw new KernelError("unavailable", "已有升级或暂停尚未完成");
      }
      const instanceId = `candidate_${(await fingerprint([actionId, commit])).slice(0, 32)}`;
      const receipt: UpgradeReceipt = {
        actionId,
        requestedBy,
        commit,
        state: "preparing",
        instanceId,
      };
      await this.#save(receipt);
      this.#work = this.#prepare(receipt).finally(() => {
        this.#work = undefined;
      });
      return receipt;
    });
    this.#tail = operation.catch(() => {});
    return await operation as unknown as Json;
  }
  async #prepare(receipt: UpgradeReceipt) {
    try {
      const id = receipt.instanceId!;
      receipt.phase = "validating";
      receipt.operation = "pin_source";
      await this.#save(receipt);
      const artifact = await pinSource({
        repository: this.options.repository,
        commit: receipt.commit,
        directory: `${this.options.artifactDirectory}/${id}`,
        deno: this.options.deno,
        validationIdentity: this.options.commandIdentity,
      });
      receipt.upgradeId = `upgrade_${
        (await fingerprint([receipt.actionId, receipt.commit])).slice(0, 48)
      }`;
      receipt.phase = "staging";
      receipt.operation = "stage_upgrade";
      await this.#save(receipt);
      const replace = this.options.mode !== "observed";
      const stage = replace
        ? this.supervisor.stageReplacement.bind(this.supervisor)
        : this.supervisor.stageUpgrade.bind(this.supervisor);
      await stage(async (active) => {
        const occupied = new Set(
          [
            this.supervisor.state.current,
            this.supervisor.state.upgrade?.old,
            this.supervisor.state.upgrade?.candidate,
          ].filter(Boolean).map((id) => this.supervisor.instance(id!).identity?.uid),
        );
        const identity = replace
          ? active.identity
          : this.options.instanceIdentities.find((i) => !occupied.has(i.uid));
        if (!identity) throw new KernelError("unavailable", "没有与并存实例隔离的运行身份");
        if (!replace) await this.supervisor.options.driver.prepareIdentity?.(identity);
        const base = await readTrustedInstanceConfig(active);
        const p = `${this.options.instancePrivateDirectory}/${id}`,
          r = replace ? base.instance.directory : `${this.options.instanceRuntimeDirectory}/${id}`;
        await Deno.mkdir(p, { mode: 0o750 });
        await Deno.chmod(p, 0o750);
        await Deno.chown(p, 0, identity.gid);
        if (!replace) await Deno.mkdir(r, { mode: 0o700 });
        await Deno.chown(r, identity.uid, identity.gid);
        const instance: InstanceSpec = {
          id,
          identity,
          source: artifact,
          configFile: `${p}/config.json`,
          connectionFile: replace ? `${r}/connection-${id}.json` : `${r}/connection.json`,
          keyFile: `${p}/control.key`,
          credentialFile: `${p}/credential.key`,
          proofFile: `${p}/proof.json`,
        };
        const key = () =>
          crypto.randomUUID().replaceAll("-", "") + crypto.randomUUID().replaceAll("-", "");
        for (const path of [instance.keyFile, instance.credentialFile]) {
          await Deno.writeTextFile(path, key(), { mode: 0o600 });
        }
        const copy = async (from: string, to: string) => {
          await Deno.copyFile(from, to);
          const privateFile = to.startsWith(`${p}/`);
          await Deno.chmod(to, privateFile ? 0o440 : 0o600);
          await Deno.chown(to, privateFile ? 0 : identity.uid, identity.gid);
        };
        await copy(base.runtime.keyFile, `${p}/runtime.key`);
        await copy(base.phone.adapterKeyFile, `${p}/phone.key`);
        const prompt = await refreshUpgradePrompt(
          await Deno.readTextFile(base.eve.promptFile),
          active.source,
          artifact,
        );
        await Deno.writeTextFile(`${p}/prompt.md`, prompt, { mode: 0o440 });
        await Deno.chmod(`${p}/prompt.md`, 0o440);
        await Deno.chown(`${p}/prompt.md`, 0, identity.gid);
        if (!replace) await Deno.mkdir(`${r}/mind`, { mode: 0o700 });
        // 控制状态在旧kernel真正停止后由 driver.handover 原子冻结，构建期间不复制活动队列。
        if (!replace) await Deno.chown(`${r}/mind`, identity.uid, identity.gid);
        base.instance = {
          ...base.instance,
          id,
          sourceCommit: artifact.commit,
          directory: r,
          keyFile: instance.keyFile,
          connectionFile: instance.connectionFile,
          credentialFile: instance.credentialFile,
        };
        base.runtime = {
          ...base.runtime,
          keyFile: `${p}/runtime.key`,
          proofFile: instance.proofFile,
        };
        base.phone = {
          ...base.phone,
          adapterKeyFile: `${p}/phone.key`,
          ownershipProofFile: instance.proofFile,
        };
        base.eve = {
          ...base.eve,
          directory: replace ? base.eve.directory : `${r}/consciousness`,
          sourceCommit: artifact.commit,
          promptFile: `${p}/prompt.md`,
        };
        if (!replace) base.mindDirectory = `${r}/mind`;
        if (!replace) base.kernelDataDirectory = `${r}/kernel`;
        base.scripts = {
          ...base.scripts,
          directory: replace ? base.scripts.directory : `${r}/scripts`,
          cacheDirectory: replace ? base.scripts.cacheDirectory : `${r}/cache/scripts`,
        };
        await writeDurableJson(instance.configFile, base);
        for (const path of [instance.configFile, instance.keyFile, instance.credentialFile]) {
          await Deno.chmod(path, 0o440);
          await Deno.chown(path, 0, identity.gid);
        }
        return instance;
      }, receipt.upgradeId);
      receipt.state = "submitted";
      delete receipt.phase;
      delete receipt.operation;
      await this.#save(receipt);
    } catch (error) {
      await Deno.mkdir(`${this.options.directory}/diagnostics`, { recursive: true, mode: 0o700 });
      await Deno.writeTextFile(
        `${this.options.directory}/diagnostics/${receipt.instanceId}.log`,
        JSON.stringify({
          at: new Date().toISOString(),
          phase: receipt.phase,
          operation: receipt.operation,
        }) + "\n" +
          (error instanceof Error ? error.stack ?? error.message : String(error)) + "\n",
        { mode: 0o600 },
      );
      receipt.failure = {
        phase: receipt.phase ?? "preparing",
        operation: receipt.operation ?? "prepare_upgrade",
        kind: error instanceof KernelError ? error.kind : "remote",
        message: error instanceof KernelError
          ? error.message.slice(0, 1024)
          : "源码验证或候选准备失败，详情保留于宿主诊断",
      };
      receipt.state = "failed";
      receipt.phase = "failed";
      receipt.error = { kind: receipt.failure.kind, message: receipt.failure.message };
      await this.#save(receipt);
    }
  }
  async close() {
    this.#closed = true;
    await this.#tail;
    await this.#work;
    await this.#journal.close();
  }
}
