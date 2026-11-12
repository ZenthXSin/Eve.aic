import type { OwnershipProof, OwnershipSnapshot } from "../contracts/ownership.ts";
import { OwnershipRpcClient } from "../guardian/rpc.ts";
import { fingerprint } from "../kernel/calls.ts";
import { type Json, KernelError } from "../kernel/protocol.ts";
import { writeDurableJson } from "../sdk/durable-json.ts";
import { FactJournal } from "../sdk/fact-journal.ts";
import type { InstanceSpec, Supervisor } from "./controller.ts";
import { pinSource } from "./source.ts";
import { readTrustedInstanceConfig } from "./driver.ts";

export interface UpgradeReceipt {
  actionId: string;
  commit: string;
  state: "preparing" | "submitted" | "failed";
  instanceId?: string;
  error?: string;
  outcome?: string;
  upgradeId?: string;
}
export interface UpgradeOptions {
  directory: string;
  repository: string;
  artifactDirectory: string;
  instancePrivateDirectory: string;
  instanceRuntimeDirectory: string;
  guardianConnectionFile: string;
  deno: string;
  commandIdentity: { uid: number; gid: number };
  instanceIdentities: { uid: number; gid: number }[];
}

/** 只有当前在线实例可申请源码升级，没有守护替换/管理凭据能力。 */
export class UpgradeRequests {
  #journal: FactJournal;
  #receipts = new Map<string, UpgradeReceipt>();
  #tail: Promise<unknown> = Promise.resolve();
  #work?: Promise<void>;
  #closed = false;
  constructor(readonly options: UpgradeOptions, readonly supervisor: Supervisor) {
    this.#journal = new FactJournal(`${options.directory}/requests`);
  }
  async initialize() {
    await this.#journal.initialize();
    for (const event of this.#journal.events) {
      const item = event.data as unknown as UpgradeReceipt;
      if (
        event.kind !== "receipt" || typeof item?.actionId !== "string" ||
        !/^[a-f0-9]{40}$/.test(item.commit) ||
        !["preparing", "submitted", "failed"].includes(item.state)
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
              await this.supervisor.upgrade(item.instanceId!, item.upgradeId);
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
    const ownership = data.ownership as unknown as { proof: OwnershipProof; actionId: string };
    const proof = ownership?.proof;
    if (
      !proof || authorization !== `Bearer ${proof.credential}` ||
      typeof ownership.actionId !== "string" || !/^[a-zA-Z0-9_.:-]{1,256}$/.test(ownership.actionId)
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
    if (data.method === "status") {
      if (typeof args?.actionId !== "string") {
        throw new KernelError("rejected", "状态查询需要原申请编号");
      }
      const item = this.#receipts.get(args.actionId);
      if (!item) throw new KernelError("not_found", "升级申请不存在");
      const last = this.supervisor.state.lastOutcome;
      return {
        ...item,
        ...(last && last.upgradeId === item.upgradeId ? { outcome: last.outcome } : {}),
      };
    }
    if (
      data.method !== "request" || typeof args?.commit !== "string" ||
      !/^[a-f0-9]{40}$/.test(args.commit)
    ) throw new KernelError("rejected", "升级只接受固定完整Git提交");
    const operation = this.#tail.then(async () => {
      const old = this.#receipts.get(ownership.actionId);
      if (old) {
        if (old.commit !== args.commit) throw new KernelError("rejected", "同一申请不能换提交");
        return old;
      }
      if (this.#work || this.supervisor.state.upgrade || this.supervisor.state.paused) {
        throw new KernelError("unavailable", "已有升级或暂停尚未完成");
      }
      const instanceId = `candidate_${
        (await fingerprint([ownership.actionId, args.commit])).slice(0, 32)
      }`;
      const receipt: UpgradeReceipt = {
        actionId: ownership.actionId,
        commit: String(args.commit),
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
      await this.#save(receipt);
      await this.supervisor.stageUpgrade(async (active) => {
        const occupied = new Set(
          [
            this.supervisor.state.current,
            this.supervisor.state.upgrade?.old,
            this.supervisor.state.upgrade?.candidate,
          ].filter(Boolean).map((id) => this.supervisor.instance(id!).identity?.uid),
        );
        const identity = this.options.instanceIdentities.find((i) => !occupied.has(i.uid));
        if (!identity) throw new KernelError("unavailable", "没有与并存实例隔离的运行身份");
        await this.supervisor.options.driver.prepareIdentity?.(identity);
        const p = `${this.options.instancePrivateDirectory}/${id}`,
          r = `${this.options.instanceRuntimeDirectory}/${id}`;
        await Deno.mkdir(p, { mode: 0o750 });
        await Deno.chmod(p, 0o750);
        await Deno.chown(p, 0, identity.gid);
        await Deno.mkdir(r, { mode: 0o700 });
        await Deno.chown(r, identity.uid, identity.gid);
        const base = await readTrustedInstanceConfig(active);
        const instance: InstanceSpec = {
          id,
          identity,
          source: artifact,
          configFile: `${p}/config.json`,
          connectionFile: `${r}/connection.json`,
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
        await copy(base.eve.promptFile, `${p}/prompt.md`);
        await Deno.mkdir(`${r}/mind`, { mode: 0o700 });
        // 控制状态在旧kernel真正停止后由 driver.handover 原子冻结，构建期间不复制活动队列。
        await Deno.chown(`${r}/mind`, identity.uid, identity.gid);
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
          directory: `${r}/consciousness`,
          sourceCommit: artifact.commit,
          promptFile: `${p}/prompt.md`,
        };
        base.mindDirectory = `${r}/mind`;
        base.kernelDataDirectory = `${r}/kernel`;
        base.scripts = {
          ...base.scripts,
          directory: `${r}/scripts`,
          cacheDirectory: `${r}/cache/scripts`,
        };
        await writeDurableJson(instance.configFile, base);
        for (const path of [instance.configFile, instance.keyFile, instance.credentialFile]) {
          await Deno.chmod(path, 0o440);
          await Deno.chown(path, 0, identity.gid);
        }
        return instance;
      }, receipt.upgradeId);
      receipt.state = "submitted";
      await this.#save(receipt);
    } catch (error) {
      receipt.state = "failed";
      receipt.error = error instanceof KernelError ? error.kind : "validation_or_staging_failed";
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
