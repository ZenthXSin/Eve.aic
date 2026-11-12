import type {
  OwnershipGrant,
  OwnershipOwner,
  OwnershipProof,
  OwnershipSnapshot,
} from "../contracts/ownership.ts";
import type { UpgradeFailure } from "../contracts/upgrades.ts";
import type { ModelRequest, ModelResponse } from "../contracts/model.ts";
import { type Json, KernelError } from "../kernel/protocol.ts";
import { FactJournal } from "../sdk/fact-journal.ts";
import type { SourceArtifact } from "./source.ts";

export interface InstanceSpec {
  id: string;
  identity?: { uid: number; gid: number };
  source: SourceArtifact;
  configFile: string;
  connectionFile: string;
  keyFile: string;
  credentialFile: string;
  proofFile: string;
}
export interface InstanceStatus {
  state: string;
  sourceCommit: string;
  instanceId: string;
  error: string | null;
  eve?: Json;
  plugins?: { name: string; status: string; version: string }[];
}
export interface InstanceDriver {
  /** 只用于监督明确配置的专属UID池；在新凭据落盘前清除旧代所有后代。 */
  prepareIdentity?(identity: { uid: number; gid: number }): Promise<void>;
  /** 必须先验证固定源码；存在且身份一致则附着，失效时再拉起同一产物。 */
  ensure(instance: InstanceSpec): Promise<void>;
  call<T = Json>(instance: InstanceSpec, method: string, args?: Json): Promise<T>;
  proof(instance: InstanceSpec, proof: OwnershipProof): Promise<void>;
  terminate(instance: InstanceSpec): Promise<void>;
  handover?(old: InstanceSpec, candidate: InstanceSpec, actionId: string): Promise<void>;
}
export interface GuardianControl {
  status(): Promise<OwnershipSnapshot>;
  register(
    args: { instanceId: string; sourceCommit: string; credential: string },
  ): Promise<unknown>;
  grant(
    args: OwnershipGrant,
  ): Promise<
    { status: "granted"; owner: OwnershipOwner } | { status: "draining"; activePermits: number } | {
      status: "interrupted";
    }
  >;
  pause(args: { requestId: string; expectedEpoch: number }): Promise<unknown>;
}
export interface BackendControl {
  /** 终止长命令并等真实出口结束；不得删除或猜测 unknown 原回执。 */
  quiesce(): Promise<void>;
  watermark(): Promise<number>;
  rollback(
    args: {
      upgradeId: string;
      sourceCommit: string;
      afterSeq: number;
      throughSeq: number;
      reason: string;
    },
  ): Promise<void>;
  complete(instance: InstanceSpec, request: ModelRequest, actionId: string): Promise<ModelResponse>;
  saveNote(
    instance: InstanceSpec,
    args: { title: string; content: string },
    actionId: string,
  ): Promise<Json>;
}
type Phase = "preparing" | "draining" | "activating" | "observing" | "rollback";
interface Upgrade {
  id: string;
  old: string;
  candidate: string;
  phase: Phase;
  operation?: string;
  failure?: UpgradeFailure;
  deadline: number;
  afterSeq: number;
  candidateGranted?: boolean;
  rollbackMarker?: {
    upgradeId: string;
    sourceCommit: string;
    afterSeq: number;
    throughSeq: number;
    reason: string;
  };
  grant?: OwnershipGrant;
  rollbackGrant?: OwnershipGrant;
  reason?: string;
  step: number;
  observation?: {
    requestArgs?: Json;
    request?: ModelRequest;
    response?: ModelResponse;
    calls?: { id: string; name: string; args: Json }[];
    results?: { callId: string; value: Json }[];
  };
}
export interface SupervisorState {
  schema: 1;
  initial?: string;
  current: string | null;
  standby: string | null;
  paused: boolean;
  pendingGrant?: OwnershipGrant;
  upgrade: Upgrade | null;
  lastOutcome: {
    upgradeId: string;
    outcome: string;
    at: number;
    reason: string;
    failure?: UpgradeFailure;
  } | null;
}

/** 独立机械监督。事实先落盘再改变进程/所有权；中断用同一交接编号继续核对。 */
export class Supervisor {
  #journal: FactJournal;
  #state: SupervisorState = {
    schema: 1,
    current: null,
    standby: null,
    paused: false,
    upgrade: null,
    lastOutcome: null,
  };
  #tail: Promise<unknown> = Promise.resolve();
  #closed = false;
  constructor(
    readonly options: {
      directory: string;
      instances: InstanceSpec[];
      driver: InstanceDriver;
      guardian: GuardianControl;
      backend: BackendControl;
      confirmationMs?: number;
      now?: () => number;
    },
  ) {
    this.#journal = new FactJournal(options.directory);
  }
  get now() {
    return this.options.now ?? Date.now;
  }
  get state(): SupervisorState {
    return structuredClone(this.#state);
  }
  instance(id: string): InstanceSpec {
    const found = this.options.instances.find((item) => item.id === id);
    if (!found) throw new KernelError("not_found", "监督配置中没有该固定实例");
    return found;
  }
  async initialize() {
    if (
      new Set(this.options.instances.map((item) => item.id)).size !==
        this.options.instances.length ||
      !Number.isSafeInteger(this.options.confirmationMs ?? 300_000) ||
      (this.options.confirmationMs ?? 300_000) < 100
    ) {
      throw new KernelError("rejected", "监督实例/确认期限无效");
    }
    await this.#journal.initialize();
    for (const event of this.#journal.events) {
      if (event.kind === "instance") {
        const instance = event.data as unknown as InstanceSpec;
        if (!this.options.instances.some((i) => i.id === instance.id)) {
          this.options.instances.push(instance);
        }
        continue;
      }
      if (event.kind !== "state" || !event.data || typeof event.data !== "object") {
        throw new KernelError("corrupt", "监督日志格式无效，保留原件");
      }
      this.#state = event.data as unknown as SupervisorState;
    }
    const s = this.#state;
    if (
      s.schema !== 1 || typeof s.paused !== "boolean" ||
      (s.current && !this.options.instances.some((i) => i.id === s.current)) ||
      (s.standby && !this.options.instances.some((i) => i.id === s.standby)) ||
      (s.upgrade &&
        (!this.options.instances.some((i) => i.id === s.upgrade!.old) ||
          !this.options.instances.some((i) => i.id === s.upgrade!.candidate) ||
          !["preparing", "draining", "activating", "observing", "rollback"].includes(
            s.upgrade.phase,
          )))
    ) {
      throw new KernelError("corrupt", "监督状态不匹配，保留原件");
    }
  }
  addInstance(instance: InstanceSpec) {
    return this.#serial(() => this.#addInstance(instance));
  }
  async #addInstance(instance: InstanceSpec) {
    const existing = this.options.instances.find((i) => i.id === instance.id);
    if (existing) {
      if (JSON.stringify(existing) !== JSON.stringify(instance)) {
        throw new KernelError("rejected", "实例编号不能更换固定产物");
      }
      return;
    }
    if (!/^[a-zA-Z0-9_-]{1,128}$/.test(instance.id)) {
      throw new KernelError("rejected", "实例编号无效");
    }
    await this.#journal.append("instance", instance as unknown as Json);
    this.options.instances.push(structuredClone(instance));
  }
  /** 源码验证在外部完成，身份清理和新凭据准备与tick/回退互斥。 */
  stageUpgrade(stage: (current: InstanceSpec) => Promise<InstanceSpec>, upgradeId: string) {
    return this.#serial(async () => {
      if (this.#state.paused || this.#state.upgrade || !this.#state.current) {
        throw new KernelError("rejected", "当前不能准备升级身份");
      }
      const candidate = await stage(this.instance(this.#state.current));
      await this.#addInstance(candidate);
      return await this.#upgrade(candidate.id, upgradeId);
    });
  }
  #serial<T>(operation: () => Promise<T>): Promise<T> {
    const work = this.#tail.then(() => {
      if (this.#closed) throw new KernelError("unavailable", "监督已关闭");
      return operation();
    });
    this.#tail = work.catch(() => {});
    return work;
  }
  async #save() {
    await this.#journal.append("state", this.#state as unknown as Json);
  }
  async #register(instance: InstanceSpec) {
    const credential = (await Deno.readTextFile(instance.credentialFile)).trim();
    await this.options.guardian.register({
      instanceId: instance.id,
      sourceCommit: instance.source.commit,
      credential,
    });
    return credential;
  }
  async #status(instance: InstanceSpec, active = false) {
    const status = await this.options.driver.call<InstanceStatus>(instance, "status");
    if (
      status.instanceId !== instance.id || status.sourceCommit !== instance.source.commit ||
      status.error ||
      (active &&
        (status.state !== "active" || (status.eve as Record<string, Json>)?.healthy !== true ||
          (status.eve as Record<string, Json>)?.running !== true ||
          !Array.isArray(status.plugins) || status.plugins.length < 5 ||
          status.plugins.some((p) => p.status !== "running")))
    ) {
      throw new KernelError("unavailable", "实例身份或机械健康不通过");
    }
    return status;
  }
  async #applyGrant(instance: InstanceSpec, grant: OwnershipGrant) {
    const credential = await this.#register(instance);
    const before = await this.options.guardian.status();
    // 守护重启或旧 granted 的租约过期时，用新请求续接，不能复用失效历史证明。
    if (
      before.mode !== "paused" && before.mode !== "draining" &&
      (before.epoch !== grant.expectedEpoch &&
        !(before.mode === "active" && before.owner?.instanceId === instance.id &&
          before.epoch === grant.expectedEpoch + 1 && before.owner.expiresAt > this.now()))
    ) {
      grant.expectedEpoch = before.epoch;
      grant.requestId = `recover_${crypto.randomUUID()}`;
      await this.#save();
    }
    const result = await this.options.guardian.grant(grant);
    if (result.status === "interrupted") throw new KernelError("unavailable", "交接被主人暂停中断");
    if (result.status === "draining") return false;
    // 历史 granted 不能证明仍在线；实际当前守护状态必须再次匹配。
    const current = await this.options.guardian.status();
    if (
      current.mode !== "active" || current.owner?.instanceId !== instance.id ||
      current.epoch !== result.owner.epoch || current.owner.expiresAt <= this.now()
    ) {
      throw new KernelError("unavailable", "交接回执已失效，不能激活历史号码");
    }
    await this.options.driver.proof(instance, {
      instanceId: instance.id,
      epoch: result.owner.epoch,
      credential,
    });
    return true;
  }
  bootstrap(id: string) {
    return this.#serial(async () => {
      if (this.#state.current) return;
      if (this.#state.initial !== id) {
        this.#state.initial = id;
        await this.#save();
      }
      if (this.#state.paused) return;
      const instance = this.instance(id);
      await this.options.driver.ensure(instance);
      await this.#status(instance);
      if (!this.#state.pendingGrant) {
        const status = await this.options.guardian.status();
        if (status.mode === "paused") {
          this.#state.paused = true;
          await this.#save();
          return;
        }
        this.#state.pendingGrant = {
          instanceId: id,
          requestId: `initial_${crypto.randomUUID()}`,
          expectedEpoch: status.epoch,
        };
        await this.#save();
      }
      if (!await this.#applyGrant(instance, this.#state.pendingGrant)) return;
      await this.options.driver.call(instance, "activate");
      await this.#status(instance, true);
      this.#state.current = id;
      delete this.#state.pendingGrant;
      await this.#save();
    });
  }
  upgrade(candidateId: string, upgradeId = `upgrade_${crypto.randomUUID()}`) {
    return this.#serial(() => this.#upgrade(candidateId, upgradeId));
  }
  async #upgrade(candidateId: string, upgradeId: string) {
    if (!/^upgrade_[a-zA-Z0-9_-]{1,128}$/.test(upgradeId)) {
      throw new KernelError("rejected", "升级编号无效");
    }
    if (this.#state.upgrade?.id === upgradeId || this.#state.lastOutcome?.upgradeId === upgradeId) {
      return upgradeId;
    }
    if (
      this.#state.paused || this.#state.upgrade || !this.#state.current ||
      this.#state.current === candidateId
    ) throw new KernelError("rejected", "当前不能申请该升级");
    const candidate = this.instance(candidateId);
    const current = this.instance(this.#state.current);
    if (
      candidate.identity && current.identity &&
      (candidate.identity.uid === current.identity.uid ||
        candidate.identity.gid === current.identity.gid)
    ) {
      throw new KernelError("rejected", "并存两代必须使用互异UID和GID");
    }
    this.#state.upgrade = {
      id: upgradeId,
      old: this.#state.current,
      candidate: candidate.id,
      phase: "preparing",
      deadline: this.now() + (this.options.confirmationMs ?? 300_000),
      afterSeq: 0,
      step: 0,
    };
    await this.#save();
    await this.#tick();
    return upgradeId;
  }
  tick() {
    return this.#serial(() => this.#tick());
  }
  async #tick() {
    if (this.#state.paused) {
      await this.#enforcePause();
      return;
    }
    let u = this.#state.upgrade;
    if (!u) {
      if (!this.#state.current) return;
      const current = this.instance(this.#state.current);
      try {
        await this.#status(current, true);
        const status = await this.options.guardian.status();
        if (status.mode === "paused") {
          this.#state.paused = true;
          await this.#save();
          return;
        }
        if (
          status.mode !== "active" || status.owner?.instanceId !== current.id ||
          status.owner.expiresAt <= this.now()
        ) throw new KernelError("unavailable", "在线租约失效");
      } catch {
        if (!this.#state.standby) {
          const status = await this.options.guardian.status();
          if (status.mode === "paused") {
            this.#state.paused = true;
            await this.#save();
            return;
          }
          try {
            await this.options.driver.call(current, "deactivate");
          } catch {
            await this.options.driver.terminate(current);
          }
          await this.options.backend.quiesce();
          await this.options.driver.ensure(current);
          if (!this.#state.pendingGrant) {
            this.#state.pendingGrant = {
              instanceId: current.id,
              requestId: `restart_${crypto.randomUUID()}`,
              expectedEpoch: status.epoch,
            };
            await this.#save();
          }
          if (!await this.#applyGrant(current, this.#state.pendingGrant)) return;
          await this.options.driver.call(current, "activate");
          await this.#status(current, true);
          delete this.#state.pendingGrant;
          await this.#save();
          return;
        }
        u = this.#state.upgrade = {
          id: `recovery_${crypto.randomUUID()}`,
          old: this.#state.standby,
          candidate: current.id,
          phase: "rollback",
          deadline: this.now(),
          afterSeq: await this.options.backend.watermark(),
          step: 0,
          reason: "active_or_lease_lost",
        };
        await this.#save();
      }
    }
    if (!u) return;
    const old = this.instance(u.old), candidate = this.instance(u.candidate);
    if (u.phase !== "rollback" && this.now() >= u.deadline) {
      u.reason = u.phase === "observing" ? "confirmation_timeout" : "preparation_timeout";
      u.phase = "rollback";
      await this.#save();
    }
    try {
      if (u.phase === "preparing") {
        await this.#run(u, "candidate_start", () => this.options.driver.ensure(candidate));
        await this.#run(u, "candidate_register", () => this.#register(candidate));
        await this.#run(u, "candidate_health", () => this.#status(candidate));
        await this.#run(u, "old_deactivate", () => this.options.driver.call(old, "deactivate"));
        // The old proof is required only while taking its read-only observer snapshot.
        // prepareObserver renews during that read; offline ledger copying may outlast
        // the old lease, so it must follow the last operation requiring that lease.
        await this.#run(
          u,
          "prepare_observer",
          () =>
            this.options.driver.call(old, "prepareObserver", {
              stepId: `${u.id}_prepare`,
              upgradeId: u.id,
            }),
        );
        await this.#run(u, "handover", async () => {
          await this.options.driver.handover?.(old, candidate, `${u.id}_mind`);
        });
        u.afterSeq = await this.#run(u, "watermark", () => this.options.backend.watermark());
        u.phase = "draining";
        await this.#save();
      }
      if (u.phase === "draining") {
        await this.#run(u, "quiesce", () => this.options.backend.quiesce());
        if (!u.grant) {
          const status = await this.options.guardian.status();
          u.grant = {
            instanceId: candidate.id,
            requestId: `${u.id}_forward`,
            expectedEpoch: status.epoch,
          };
          await this.#save();
        }
        if (!await this.#run(u, "candidate_grant", () => this.#applyGrant(candidate, u.grant!))) {
          return;
        }
        u.candidateGranted = true;
        u.phase = "activating";
        await this.#save();
      }
      if (u.phase === "activating") {
        await this.#run(
          u,
          "candidate_activate",
          () => this.options.driver.call(candidate, "activate"),
        );
        await this.#run(u, "candidate_active_health", () => this.#status(candidate, true));
        if (this.now() >= u.deadline) {
          u.phase = "rollback";
          u.reason = "preparation_timeout";
        } else {
          this.#state.current = candidate.id;
          this.#state.standby = old.id;
          // Preparation has its own bounded window. Give the old Eve the full
          // confirmation window only once the candidate is healthy and active.
          u.deadline = this.now() + (this.options.confirmationMs ?? 300_000);
          u.phase = "observing";
        }
        await this.#save();
      }
      if (u.phase === "observing") {
        await this.#run(u, "observer_health", () => this.#status(old));
        await this.#run(u, "candidate_active_health", () => this.#status(candidate, true));
        const status = await this.#run(u, "candidate_lease", () => this.options.guardian.status());
        if (
          status.mode !== "active" || status.owner?.instanceId !== candidate.id ||
          status.owner.expiresAt <= this.now()
        ) throw new KernelError("unavailable", "新版租约失效");
        await this.#observe(u, old, candidate);
      }
    } catch (error) {
      // 未确认期间任一版本崩溃/拒绝/观察出错都不准宣布成功。
      u.failure = {
        phase: u.phase,
        operation: u.operation ?? u.phase,
        kind: error instanceof KernelError ? error.kind : "remote",
        message: error instanceof KernelError
          ? error.message.slice(0, 1024)
          : "升级操作抛出未分类异常",
      };
      u.phase = "rollback";
      u.reason = error instanceof KernelError ? error.kind : "observer_or_candidate_crash";
      await this.#save();
    }
    if (u.phase === "rollback") await this.#rollback(u, old, candidate);
  }
  async #run<T>(u: Upgrade, operation: string, work: () => Promise<T>): Promise<T> {
    u.operation = operation;
    await this.#save();
    return await work();
  }
  async #observe(u: Upgrade, old: InstanceSpec, candidate: InstanceSpec) {
    const id = `${u.id}_observe_${u.step}`;
    u.observation ??= {};
    const o = u.observation;
    if (!o.request) {
      if (!o.requestArgs) {
        o.requestArgs = {
          stepId: id,
          candidate: await this.options.driver.call(candidate, "status"),
          sourceCommit: candidate.source.commit,
          deadline: u.deadline,
          resources: "旧版只观察；本轮及可选试聊使用同一真实日预算",
        };
        await this.#save();
      }
      o.request = await this.#run(
        u,
        "observer_request",
        () =>
          this.options.driver.call<ModelRequest>(old, "observerRequest", {
            ...o.requestArgs as Record<string, Json>,
          }),
      );
      await this.#save();
    }
    if (!o.response) {
      o.response = await this.#run(
        u,
        "observer_model",
        () => this.options.backend.complete(candidate, o.request!, id),
      );
      await this.#save();
    }
    if (!o.calls) {
      o.calls = await this.#run(
        u,
        "observer_response",
        () =>
          this.options.driver.call(old, "observerResponse", {
            stepId: id,
            response: o.response as unknown as Json,
          }),
      );
      await this.#save();
    }
    if (!o.results) {
      o.results = [];
      // 只执行旧源码观察器返回的受限工具；永不提供真实命令/手机权限。
      for (const call of o.calls!) {
        const args = call.args as Record<string, Json>;
        let value: Json;
        if (
          call.name === "confirm_upgrade" && typeof args?.accept === "boolean" &&
          typeof args.reason === "string"
        ) {
          value = { accepted: args.accept, reason: args.reason };
        } else if (
          call.name === "save_observation_note" && typeof args?.title === "string" &&
          typeof args.content === "string"
        ) {
          value = await this.#run(
            u,
            "observer_save_note",
            () =>
              this.options.backend.saveNote(candidate, {
                title: String(args.title),
                content: String(args.content),
              }, `${id}_${call.id}`),
          );
        } else if (call.name === "trial_candidate" && typeof args?.text === "string") {
          const request = await this.#run(
            u,
            "trial_request",
            () =>
              this.options.driver.call<ModelRequest>(candidate, "trialRequest", {
                stepId: `${id}_${call.id}`,
                text: args.text,
              }),
          );
          if (request.tools.length) throw new KernelError("rejected", "试聊不能持有真实工具");
          const response = await this.#run(u, "trial_model", () =>
            this.options.backend.complete(
              candidate,
              request,
              `${id}_${call.id}_trial`,
            ));
          await this.#run(
            u,
            "trial_response",
            () =>
              this.options.driver.call(candidate, "trialResponse", {
                stepId: `${id}_${call.id}`,
                response: response as unknown as Json,
              }),
          );
          value = {
            kind: "isolated_test",
            response: response.content,
            usage: response.usage as unknown as Json ?? null,
          };
        } else value = { error: "observer_tool_rejected" };
        o.results.push({ callId: call.id, value });
      }
      await this.#save();
    }
    if (o.calls!.length) {
      await this.#run(
        u,
        "observer_tool_results",
        () =>
          this.options.driver.call(old, "observerToolResults", {
            stepId: id,
            items: o.results as unknown as Json,
          }),
      );
    }
    const decisions = o.calls!.filter((call) => call.name === "confirm_upgrade");
    if (
      decisions.length > 1 || decisions.some((call) => {
        const args = call.args as Record<string, Json>;
        return typeof args?.accept !== "boolean" || typeof args.reason !== "string" ||
          !args.reason.trim();
      })
    ) throw new KernelError("rejected", "旧版确认必须是单个带明确理由的决定");
    const decision = decisions[0];
    if (decision) {
      const args = decision.args as Record<string, Json>;
      if (args.accept) {
        if (this.now() >= u.deadline) throw new KernelError("timeout", "旧版确认到达时已超时");
        await this.#run(u, "confirmation_health", () => this.#status(candidate, true));
        const current = await this.#run(
          u,
          "confirmation_lease",
          () => this.options.guardian.status(),
        );
        if (
          current.mode !== "active" || current.owner?.instanceId !== candidate.id ||
          current.owner.expiresAt <= this.now()
        ) throw new KernelError("unavailable", "确认时新版已不在线");
        this.#state.lastOutcome = {
          upgradeId: u.id,
          outcome: "confirmed",
          at: this.now(),
          reason: String(args.reason),
        };
        this.#state.upgrade = null;
        await this.#save();
        // 已确认后允许旧进程退出，固定产物继续作为可重拉起回退版保存。
        try {
          await this.options.driver.terminate(old);
        } catch { /* 已撤销旧令牌；保留其进程证据，不能撤销已持久确认。 */ }
      } else {
        u.phase = "rollback";
        u.reason = `old_eve_rejected:${args.reason}`;
        await this.#save();
      }
    } else {
      u.step++;
      delete u.observation;
      await this.#save();
    }
  }
  async #rollback(u: Upgrade, old: InstanceSpec, candidate: InstanceSpec) {
    try {
      await this.options.driver.call(candidate, "deactivate");
    } catch {
      await this.options.driver.terminate(candidate);
    }
    await this.options.backend.quiesce();
    const status = await this.options.guardian.status();
    if (status.mode === "paused") {
      this.#state.paused = true;
      await this.#save();
      return;
    }
    // 先记录未确认范围，再给旧实例读；不能把候选经历静默当作旧版经历。
    if (u.candidateGranted && !u.rollbackMarker) {
      u.rollbackMarker = {
        upgradeId: u.id,
        sourceCommit: candidate.source.commit,
        afterSeq: u.afterSeq,
        throughSeq: await this.options.backend.watermark(),
        reason: u.reason ?? "unconfirmed",
      };
      await this.#save();
    }
    if (u.rollbackMarker) await this.options.backend.rollback(u.rollbackMarker);
    await this.options.driver.ensure(old);
    if (!u.rollbackGrant) {
      u.rollbackGrant = {
        instanceId: old.id,
        requestId: `${u.id}_rollback`,
        expectedEpoch: status.epoch,
      };
      await this.#save();
    }
    if (!await this.#applyGrant(old, u.rollbackGrant)) return;
    await this.options.driver.call(old, "activate");
    await this.#status(old, true);
    this.#state.current = old.id;
    this.#state.standby = null;
    this.#state.lastOutcome = {
      upgradeId: u.id,
      outcome: "rolled_back",
      at: this.now(),
      reason: u.reason ?? "unconfirmed",
      ...(u.failure ? { failure: u.failure } : {}),
    };
    this.#state.upgrade = null;
    await this.#save();
    await this.options.driver.terminate(candidate);
  }
  pause() {
    return this.#serial(async () => {
      this.#state.paused = true;
      await this.#save();
      await this.#enforcePause();
    });
  }
  async #enforcePause() {
    const status = await this.options.guardian.status();
    if (status.mode !== "paused") {
      await this.options.guardian.pause({
        requestId: `pause_${crypto.randomUUID()}`,
        expectedEpoch: status.epoch,
      });
    }
    for (
      const id of new Set(
        [
          this.#state.current,
          this.#state.standby,
          this.#state.upgrade?.old,
          this.#state.upgrade?.candidate,
        ].filter((id): id is string => !!id),
      )
    ) {
      const instance = this.instance(id);
      try {
        await this.options.driver.call(instance, "deactivate");
      } catch {
        await this.options.driver.terminate(instance);
      }
    }
    await this.options.backend.quiesce();
  }
  resume() {
    return this.#serial(async () => {
      if (!this.#state.paused || !(this.#state.current ?? this.#state.initial)) return;
      const u = this.#state.upgrade;
      if (u && u.candidateGranted) {
        if (!u.rollbackMarker) {
          u.rollbackMarker = {
            upgradeId: u.id,
            sourceCommit: this.instance(u.candidate).source.commit,
            afterSeq: u.afterSeq,
            throughSeq: await this.options.backend.watermark(),
            reason: "owner_pause",
          };
          await this.#save();
        }
        await this.options.backend.rollback(u.rollbackMarker);
      }
      const instance = this.instance(
        this.#state.upgrade?.old ?? this.#state.current ?? this.#state.initial!,
      );
      await this.options.driver.ensure(instance);
      await this.options.backend.quiesce();
      const status = await this.options.guardian.status();
      if (!this.#state.pendingGrant) {
        this.#state.pendingGrant = {
          instanceId: instance.id,
          requestId: `resume_${crypto.randomUUID()}`,
          expectedEpoch: status.epoch,
          resumePaused: true,
        };
        await this.#save();
      }
      if (!await this.#applyGrant(instance, this.#state.pendingGrant)) return;
      await this.options.driver.call(instance, "activate");
      await this.#status(instance, true);
      this.#state.current = instance.id;
      this.#state.paused = false;
      delete this.#state.pendingGrant;
      if (this.#state.upgrade) {
        await this.options.driver.terminate(this.instance(this.#state.upgrade.candidate));
        this.#state.lastOutcome = {
          upgradeId: this.#state.upgrade.id,
          outcome: "paused_and_rolled_back",
          at: this.now(),
          reason: "owner_pause",
        };
        this.#state.upgrade = null;
      }
      await this.#save();
    });
  }
  async close() {
    await this.#tail;
    this.#closed = true;
    await this.#journal.close();
  }
}
