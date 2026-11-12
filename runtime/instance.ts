/** 一个固定源码版本的宿主。这里只持有实例凭据；守护管理能力始终在监督器中。 */
import { Kernel } from "../kernel/kernel.ts";
import { fingerprint } from "../kernel/calls.ts";
import { type Json, KernelError, toWireError } from "../kernel/protocol.ts";
import { composeRuntimePlugin } from "../composition/runtime.ts";
import { composePhonePlugin } from "../composition/phone.ts";
import { composeScriptPlugin } from "../composition/scripts.ts";
import { composeEvePlugin, composeMindPlugin } from "../composition/eve.ts";
import { RuntimeClient } from "../services/runtime/client.ts";
import { OwnershipRpcClient } from "../guardian/rpc.ts";
import { render } from "../plugins/eve/view.ts";
import type { FactEvent } from "../sdk/fact-journal.ts";
import type {
  Message,
  ModelRequest,
  ModelResponse,
  ToolCall,
  ToolSpec,
} from "../contracts/model.ts";
import type { OwnershipOwner, OwnershipProof } from "../contracts/ownership.ts";
import { writeDurableJson } from "../sdk/durable-json.ts";

const observerTools: ToolSpec[] = [
  {
    name: "confirm_upgrade",
    description: "明确接受或拒绝新版的实际运行，由你判断；未确认会按期限回切。",
    parameters: {
      type: "object",
      properties: { accept: { type: "boolean" }, reason: { type: "string" } },
      required: ["accept", "reason"],
      additionalProperties: false,
    },
  },
  {
    name: "save_observation_note",
    description: "把你想保留的观察写成便签，不写进在线意识流。",
    parameters: {
      type: "object",
      properties: { title: { type: "string" }, content: { type: "string" } },
      required: ["title", "content"],
      additionalProperties: false,
    },
  },
  {
    name: "trial_candidate",
    description: "可选择与候选试聊，真实调用模型但只写测试覆盖层，不操作真实工具或意识流。",
    parameters: {
      type: "object",
      properties: { text: { type: "string" } },
      required: ["text"],
      additionalProperties: false,
    },
  },
];
const MAX_REQUEST_BYTES = 16_777_216;

function object(value: unknown): value is Record<string, Json> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}
function response(value: unknown): ModelResponse {
  if (
    !object(value) || typeof value.content !== "string" || !Array.isArray(value.toolCalls) ||
    value.toolCalls.some((call) =>
      !object(call) || typeof call.id !== "string" ||
      typeof call.name !== "string" || !Object.hasOwn(call, "args")
    )
  ) {
    throw new KernelError("rejected", "观察回复无效");
  }
  return value as unknown as ModelResponse;
}
async function body(request: Request): Promise<Record<string, Json>> {
  if (!request.body) throw new KernelError("rejected", "实例控制请求缺少正文");
  const reader = request.body.getReader(), chunks: Uint8Array[] = [];
  let size = 0;
  try {
    while (true) {
      const part = await reader.read();
      if (part.done) break;
      size += part.value.length;
      if (size > MAX_REQUEST_BYTES) {
        await reader.cancel();
        throw new KernelError("rejected", "实例控制请求过大");
      }
      chunks.push(part.value);
    }
  } finally {
    reader.releaseLock();
  }
  const bytes = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.length;
  }
  let data: unknown;
  try {
    data = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes));
  } catch {
    throw new KernelError("rejected", "实例控制请求不是有效 JSON");
  }
  if (
    !object(data) || typeof data.method !== "string" ||
    (data.args !== undefined && !object(data.args))
  ) {
    throw new KernelError("rejected", "实例控制请求格式无效");
  }
  return data;
}

export async function startInstance(configPath: string) {
  const config = JSON.parse(await Deno.readTextFile(configPath)), c = config.instance;
  if (
    !c || !/^[a-zA-Z0-9_-]{1,128}$/.test(c.id) ||
    !/^[a-f0-9]{40}$/.test(c.sourceCommit) || !Number.isSafeInteger(c.leaseMs) ||
    c.leaseMs < 300 || c.leaseMs > 3_600_000 ||
    [
      c.directory,
      c.keyFile,
      c.connectionFile,
      c.guardianConnectionFile,
      c.credentialFile,
      config.kernelDataDirectory,
      config.mindDirectory,
    ].some((p) => typeof p !== "string" || !p.startsWith("/")) ||
    config.eve?.sourceCommit !== c.sourceCommit || config.eve?.remoteRecords !== true ||
    config.phone?.ownershipProofFile !== config.runtime?.proofFile
  ) {
    throw new KernelError("rejected", "实例控制配置不完整，须使用共享原件与同一代际凭据");
  }
  await Deno.mkdir(c.directory, { recursive: true, mode: 0o700 });
  const key = (await Deno.readTextFile(c.keyFile)).trim();
  if (!/^[a-zA-Z0-9_-]{32,256}$/.test(key)) throw new KernelError("rejected", "实例控制凭据无效");
  const client = new RuntimeClient(config.runtime);
  let kernel: Kernel | undefined, heartbeat: ReturnType<typeof setTimeout> | undefined;
  let state = "standby", lastError: string | null = null, generation = 0, closed = false;
  let tail: Promise<unknown> = Promise.resolve();
  const serialize = <T>(work: () => Promise<T>): Promise<T> => {
    const next = tail.then(work);
    tail = next.catch(() => {});
    return next;
  };
  const deactivate = async () => {
    const previous = kernel;
    kernel = undefined;
    state = "stopping";
    try {
      // The owning lease must survive the final model response and journal writes.
      // Only the guardian may revoke it; the supervisor grants the next epoch after stop.
      if (previous) {
        const errors = await previous.stop();
        if (errors.length) throw new KernelError("storage", "实例停止有未完成保存，原件保留");
      }
      state = "standby";
    } catch (error) {
      state = "failed";
      lastError = "storage";
      throw error;
    } finally {
      generation++;
      clearTimeout(heartbeat);
      heartbeat = undefined;
    }
  };
  const renew = async () => {
    const connection = JSON.parse(await Deno.readTextFile(c.guardianConnectionFile));
    const proof: OwnershipProof = JSON.parse(await Deno.readTextFile(config.runtime.proofFile));
    const credential = (await Deno.readTextFile(c.credentialFile)).trim();
    if (proof.instanceId !== c.id || proof.credential !== credential) {
      throw new KernelError("rejected", "实例凭据与所有权证明不一致");
    }
    const rpc = new OwnershipRpcClient(
      connection.url,
      credential,
      Math.max(100, Math.floor(c.leaseMs / 4)),
    );
    try {
      const owner = await rpc.call<OwnershipOwner>("renew", proof);
      if (
        owner.instanceId !== c.id || owner.epoch !== proof.epoch ||
        !Number.isFinite(owner.expiresAt) || owner.expiresAt <= Date.now()
      ) {
        throw new KernelError("rejected", "守护续租回执不匹配");
      }
    } finally {
      rpc.close();
    }
  };
  const scheduleRenew = (current: number) => {
    heartbeat = setTimeout(() => {
      if (generation !== current) return;
      void renew().then(() => {
        if (generation === current) scheduleRenew(current);
      }, (error) => {
        void serialize(async () => {
          if (generation !== current) return;
          lastError = error instanceof KernelError ? error.kind : "unknown";
          try {
            await deactivate();
          } finally {
            state = "lease_lost";
          }
        }).catch(() => {});
      });
    }, Math.floor(c.leaseMs / 3));
  };
  const snapshot = async () => {
    const events: FactEvent[] = [];
    let afterSeq = 0, previous = "0".repeat(64);
    while (true) {
      const page = await client.call("timeline", "page", { afterSeq }) as unknown as {
        items: FactEvent[];
        throughSeq: number;
        hasMore: boolean;
      };
      if (!Array.isArray(page.items)) throw new KernelError("corrupt", "观察原件分页无效");
      for (const event of page.items) {
        const { digest, ...record } = event;
        if (
          event.seq !== afterSeq + 1 || event.previous !== previous ||
          digest !== await fingerprint(record as unknown as Json)
        ) {
          throw new KernelError("corrupt", "观察原件来源链不连续");
        }
        events.push(event);
        afterSeq = event.seq;
        previous = digest;
      }
      if (page.throughSeq !== afterSeq || (page.hasMore && !page.items.length)) {
        throw new KernelError("corrupt", "观察原件游标没有推进");
      }
      if (!page.hasMore) break;
    }
    return events;
  };
  let observerMessages: Message[] = [], observationStep = 0;
  let observerRequest: ModelRequest | undefined, observerArgs: string | undefined;
  let pendingCalls: ToolCall[] = [], lastResponse: string | undefined;
  let trialMessages: Message[] | undefined, trialPending = false;
  type StepReceipt = { hash: string; value: Json };
  let stepReceipts: Record<string, StepReceipt> = {};
  const sessionFile = `${c.directory}/observer-session.json`;
  try {
    const saved = JSON.parse(await Deno.readTextFile(sessionFile));
    if (
      saved.schema !== 1 || !Array.isArray(saved.observerMessages) ||
      !Number.isSafeInteger(saved.observationStep) || !Array.isArray(saved.pendingCalls) ||
      !object(saved.stepReceipts)
    ) throw new Error("invalid session");
    observerMessages = saved.observerMessages;
    observationStep = saved.observationStep;
    observerRequest = saved.observerRequest ?? undefined;
    observerArgs = saved.observerArgs ?? undefined;
    pendingCalls = saved.pendingCalls;
    lastResponse = saved.lastResponse ?? undefined;
    trialMessages = saved.trialMessages ?? undefined;
    trialPending = saved.trialPending === true;
    stepReceipts = saved.stepReceipts;
  } catch (error) {
    if (!(error instanceof Deno.errors.NotFound)) {
      client.close();
      throw new KernelError("corrupt", "观察覆盖层损坏，原件保留");
    }
  }
  const saveSession = () =>
    writeDurableJson(sessionFile, {
      schema: 1,
      observerMessages,
      observationStep,
      observerRequest: observerRequest ?? null,
      observerArgs: observerArgs ?? null,
      pendingCalls,
      lastResponse: lastResponse ?? null,
      trialMessages: trialMessages ?? null,
      trialPending,
      stepReceipts,
    });
  const control = async (method: string, args: Record<string, Json> = {}): Promise<Json> => {
    if (closed) throw new KernelError("unavailable", "实例正在停止");
    if (method === "status") {
      let eve: Json = null;
      if (kernel) {
        try {
          eve = await kernel.call("eve", "status");
        } catch (error) {
          lastError = error instanceof KernelError ? error.kind : "unknown";
        }
      }
      return {
        state,
        sourceCommit: c.sourceCommit,
        instanceId: c.id,
        error: lastError,
        eve,
        plugins: kernel?.status() as unknown as Json ?? [],
      };
    }
    if (method === "deactivate") {
      await deactivate();
      return null;
    }
    if (method === "activate") {
      if (state === "active") return null;
      if (kernel || state === "failed") throw new KernelError("unavailable", "实例尚未排空");
      state = "starting";
      const current = ++generation;
      let next: Kernel | undefined;
      try {
        await renew();
        scheduleRenew(current);
        const generated = `${c.directory}/plugins`;
        await composeRuntimePlugin(`${generated}/runtime`, configPath);
        await composePhonePlugin(`${generated}/phone`, configPath);
        await composeScriptPlugin(`${generated}/scripts`, configPath);
        await composeMindPlugin(`${generated}/mind`, config.mindDirectory);
        await composeEvePlugin(`${generated}/eve`, configPath);
        next = new Kernel({
          dataDir: config.kernelDataDirectory,
          callTimeoutMs: config.eve.modelTimeoutMs + 30000,
        });
        for (const name of ["runtime", "phone", "scripts", "mind", "eve"]) {
          await next.register(`${generated}/${name}`);
        }
        await next.start();
        // A lease may have failed while workers were starting. Recheck before the loop starts.
        await renew();
        kernel = next;
        await next.call("eve", "start");
        state = "active";
        lastError = null;
        observerMessages = [];
        observerRequest = undefined;
        pendingCalls = [];
      } catch (error) {
        generation++;
        clearTimeout(heartbeat);
        heartbeat = undefined;
        const errors = await next?.stop();
        kernel = undefined;
        state = errors?.length ? "failed" : "standby";
        lastError = error instanceof KernelError ? error.kind : "unknown";
        throw error;
      }
      return null;
    }
    if (method === "prepareObserver") {
      if (kernel || state !== "standby") {
        throw new KernelError("rejected", "观察模式只在在线意识流停止后启用");
      }
      await renew();
      const current = ++generation;
      scheduleRenew(current);
      try {
        const events = await snapshot();
        const messages = render(events, await Deno.readTextFile(config.eve.promptFile));
        messages.push({
          role: "user",
          content:
            "你是升级前的 Eve。在线意识流已暂停，你现在只观察新版。观察与试聊不写进在线经历，不能操作手机或命令。你可以选择试聊、保留观察便签，并在确认新版正常后调用 confirm_upgrade 明确决定。没有默认的接受；拒绝或到期未决定将回切。",
        });
        await writeDurableJson(`${c.directory}/observer-snapshot.json`, events);
        observerMessages = messages;
        observationStep = 0;
        observerRequest = undefined;
        observerArgs = undefined;
        pendingCalls = [];
        lastResponse = undefined;
        return { throughSeq: events.at(-1)?.seq ?? 0 };
      } finally {
        // Observation itself is offline. Renewal only protects reading the old snapshot.
        generation++;
        clearTimeout(heartbeat);
        heartbeat = undefined;
      }
    }
    if (method === "observerRequest") {
      if (kernel || !observerMessages.length) {
        throw new KernelError("rejected", "没有离线观察上下文");
      }
      if (pendingCalls.length) throw new KernelError("rejected", "观察工具回执尚未齐全");
      if (observerRequest) {
        if (observerArgs !== JSON.stringify(args)) {
          throw new KernelError("rejected", "上一次观察请求尚未返回");
        }
        return observerRequest as unknown as Json;
      }
      observerMessages.push({
        role: "user",
        content: JSON.stringify({ source: "upgrade-observation", data: args }),
      });
      observerRequest = structuredClone({ messages: observerMessages, tools: observerTools });
      observerArgs = JSON.stringify(args);
      lastResponse = undefined;
      return observerRequest as unknown as Json;
    }
    if (method === "observerResponse") {
      const received = response(args.response), encoded = JSON.stringify(received);
      if (lastResponse === encoded) return pendingCalls as unknown as Json;
      if (!observerRequest) throw new KernelError("rejected", "没有等待中的观察请求");
      if (
        received.toolCalls.some((call) => !observerTools.some((tool) => tool.name === call.name))
      ) {
        throw new KernelError("rejected", "观察者请求了未授权工具");
      }
      const calls = received.toolCalls.map((call, index) => ({
        ...call,
        id: `observation_${observationStep + 1}_${index}`,
      }));
      const messages: Message[] = [...observerMessages, {
        role: "assistant",
        content: received.content,
        ...(calls.length ? { toolCalls: calls } : {}),
      }];
      await writeDurableJson(`${c.directory}/observer-overlay.json`, messages);
      observationStep++;
      observerMessages = messages;
      pendingCalls = calls;
      observerRequest = undefined;
      observerArgs = undefined;
      lastResponse = encoded;
      return calls as unknown as Json;
    }
    if (method === "observerToolResults") {
      const results = args.items;
      if (
        !Array.isArray(results) || !pendingCalls.length || results.length !== pendingCalls.length ||
        new Set(results.map((item) => object(item) ? item.callId : null)).size !==
          pendingCalls.length ||
        results.some((item) =>
          !object(item) || !Object.hasOwn(item, "value") ||
          !pendingCalls.some((call) => call.id === item.callId)
        )
      ) {
        throw new KernelError("rejected", "观察回执必须与待决调用一一对应");
      }
      const messages: Message[] = [
        ...observerMessages,
        ...results.map((item) => ({
          role: "tool" as const,
          toolCallId: (item as Record<string, Json>).callId as string,
          content: JSON.stringify((item as Record<string, Json>).value),
        })),
      ];
      await writeDurableJson(`${c.directory}/observer-overlay.json`, messages);
      observerMessages = messages;
      pendingCalls = [];
      lastResponse = undefined;
      return null;
    }
    if (method === "trialRequest") {
      if (typeof args.text !== "string" || !args.text.trim()) {
        throw new KernelError("rejected", "试聊内容为空");
      }
      if (trialPending) throw new KernelError("rejected", "上一次试聊尚未返回");
      const events = await snapshot();
      trialMessages = render(events, await Deno.readTextFile(config.eve.promptFile));
      trialMessages.push({
        role: "user",
        content: `这是独立升级试聊，不属于在线经历，不能执行工具或修改真实状态。\n${args.text}`,
      });
      trialPending = true;
      return { messages: trialMessages, tools: [] } as unknown as Json;
    }
    if (method === "trialResponse") {
      if (!trialPending || !trialMessages) throw new KernelError("rejected", "没有等待中的试聊");
      const received = response(args.response);
      if (received.toolCalls.length) throw new KernelError("rejected", "独立试聊不允许执行工具");
      const messages: Message[] = [...trialMessages, {
        role: "assistant",
        content: received.content,
      }];
      await writeDurableJson(`${c.directory}/trial-overlay.json`, messages);
      trialMessages = messages;
      trialPending = false;
      return null;
    }
    throw new KernelError("rejected", "未知实例控制动作");
  };
  let overlayFailed = false;
  const persistedControl = async (
    method: string,
    args: Record<string, Json> = {},
  ): Promise<Json> => {
    const overlay = [
      "prepareObserver",
      "observerRequest",
      "observerResponse",
      "observerToolResults",
      "trialRequest",
      "trialResponse",
    ].includes(method);
    if (overlay && overlayFailed) {
      throw new KernelError("storage", "观察覆盖层保存失败，须核对原件后重启");
    }
    const { stepId, ...parameters } = args;
    let receiptKey: string | undefined, hash: string | undefined;
    if (stepId !== undefined) {
      if (!overlay || typeof stepId !== "string" || !/^[a-zA-Z0-9_.:-]{1,256}$/.test(stepId)) {
        throw new KernelError("rejected", "观察步骤编号无效");
      }
      receiptKey = `${method}:${stepId}`;
      hash = await fingerprint(parameters);
      const original = stepReceipts[receiptKey];
      if (original) {
        if (original.hash !== hash) throw new KernelError("rejected", "同一观察步骤参数发生变化");
        return original.value;
      }
    }
    const value = await control(method, parameters);
    if (receiptKey) stepReceipts[receiptKey] = { hash: hash!, value: structuredClone(value) };
    if (overlay || method === "activate") {
      try {
        await saveSession();
      } catch {
        overlayFailed = true;
        lastError = "storage";
        throw new KernelError("storage", "观察覆盖层保存失败，原件保留");
      }
    }
    return value;
  };
  const hostLock = await Deno.open(`${c.directory}/host.lock`, {
    create: true,
    read: true,
    write: true,
    mode: 0o600,
  });
  if (!(await hostLock.tryLock(true))) {
    hostLock.close();
    client.close();
    throw new KernelError("unavailable", "同一实例目录已有宿主");
  }
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (request) => {
    if (
      request.method !== "POST" || new URL(request.url).pathname !== "/instance" ||
      request.headers.has("origin") || request.headers.get("authorization") !== `Bearer ${key}`
    ) {
      return new Response(null, { status: 403 });
    }
    try {
      const data = await body(request);
      return Response.json({
        ok: true,
        value: await serialize(() =>
          persistedControl(data.method as string, data.args as Record<string, Json> | undefined)
        ),
      });
    } catch (error) {
      return Response.json({ ok: false, error: toWireError(error) });
    }
  });
  const url = `http://127.0.0.1:${server.addr.port}/instance`;
  try {
    await writeDurableJson(c.connectionFile, { url });
  } catch (error) {
    closed = true;
    client.close();
    await server.shutdown();
    hostLock.close();
    throw error;
  }
  let closing: Promise<void> | undefined;
  return {
    url,
    close(): Promise<void> {
      if (!closing) {
        closing = (async () => {
          closed = true;
          try {
            await serialize(deactivate);
          } finally {
            client.close();
            try {
              await server.shutdown();
            } finally {
              hostLock.close();
            }
          }
        })();
      }
      return closing;
    },
  };
}
export async function runInstance(configPath: string) {
  const instance = await startInstance(configPath);
  let stop!: () => void;
  const done = new Promise<void>((resolve) => {
    stop = resolve;
  });
  Deno.addSignalListener("SIGTERM", stop);
  Deno.addSignalListener("SIGINT", stop);
  try {
    await done;
  } finally {
    Deno.removeSignalListener("SIGTERM", stop);
    Deno.removeSignalListener("SIGINT", stop);
    await instance.close();
  }
}
if (import.meta.main) {
  if (Deno.args.length !== 1) throw new Error("参数：独立实例私有配置路径");
  await runInstance(Deno.args[0]);
}
