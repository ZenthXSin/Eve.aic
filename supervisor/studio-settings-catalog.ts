import {
  type StudioFileScope,
  type StudioItem,
  studioRecord,
  type StudioScope,
  type StudioValue,
} from "../services/admin/studio-settings.ts";
import { MODEL_MAX_DURATION_MS } from "../sdk/model-timeouts.ts";
import { secretFileIsSet, type StudioDocument } from "./studio-settings-files.ts";

type Row = Record<string, unknown>;
export type Documents = Partial<Record<StudioFileScope, StudioDocument>>;
export type StudioState = {
  current: string | null;
  initial?: string;
  paused: boolean;
  upgrade: unknown;
  pendingGrant?: unknown;
};

export function child(parent: unknown, field: string): Row | undefined {
  const value = studioRecord(parent) && Object.hasOwn(parent, field) ? parent[field] : undefined;
  return studioRecord(value) ? value : undefined;
}

type Rule = {
  type: "number" | "enum" | "boolean" | "string";
  accepts: (value: unknown) => value is StudioValue;
  min?: number;
  max?: number;
  options?: string[];
};
const number = (min: number, max: number): Rule => ({
  type: "number",
  min,
  max,
  accepts: (value): value is number =>
    typeof value === "number" && Number.isSafeInteger(value) && value >= min && value <= max,
});
const flag: Rule = {
  type: "boolean",
  accepts: (value): value is boolean => typeof value === "boolean",
};
const choice = (...options: string[]): Rule => ({
  type: "enum",
  options,
  accepts: (value): value is string => typeof value === "string" && options.includes(value),
});
const string = (accepts: (text: string) => boolean): Rule => ({
  type: "string",
  accepts: (value): value is string => typeof value === "string" && accepts(value),
});
const id = string((text) => /^[a-zA-Z0-9_-]{1,128}$/.test(text));
const modelName = string((text) =>
  text.length <= 128 && /^[A-Za-z0-9][\w.+-]*(?:\/[A-Za-z0-9][\w.+-]*)?$/.test(text)
);
const timeZone = string((text) => {
  if (text.length > 80 || !/^[A-Za-z]+(?:\/[A-Za-z][A-Za-z0-9_+-]*){0,2}$/.test(text)) return false;
  try {
    new Intl.DateTimeFormat("en", { timeZone: text });
    return true;
  } catch {
    return false;
  }
});
const effort = choice("none", "minimal", "low", "medium", "high", "xhigh");
const outputTokens = number(1, 131_072);
const modelMs = number(1_000, MODEL_MAX_DURATION_MS);
const startupMs = number(1_000, 600_000);
const bigCount = number(0, Number.MAX_SAFE_INTEGER);
const mib = 1024 * 1024;
const none = "未配置或无法核对这一来源";
const restricted = "仅由部署或专门的授权流程管理";

export type Binding = {
  item: StudioItem;
  accepts?: Rule["accepts"];
  assign?: (value: StudioValue) => void;
};

export async function studioCatalog(docs: Documents, state: StudioState, status: unknown) {
  const entries = new Map<string, Binding>();
  function section(scope: StudioScope, group: string, parent?: Row, guard?: string) {
    function add(
      key: string,
      label: string,
      raw: unknown,
      rule: Rule,
      description: string,
      mode: StudioItem["mode"],
      assign?: Binding["assign"],
    ) {
      const { accepts, ...schema } = rule;
      const value = accepts(raw) ||
          rule.type === "number" && typeof raw === "number" && Number.isSafeInteger(raw)
        ? raw
        : null;
      const reason = mode === "restricted" ? restricted : !parent || value === null ? none : guard;
      entries.set(key, {
        item: {
          key,
          label,
          group,
          scope,
          mode,
          editable: !reason,
          description,
          ...schema,
          value,
          ...(reason ? { reason } : {}),
        },
        accepts,
        assign,
      });
    }
    return {
      show(key: string, label: string, raw: unknown, rule: Rule, description: string) {
        add(key, label, raw, rule, description, "restricted");
      },
      pref(
        key: string,
        label: string,
        rule: Rule,
        fallback: unknown,
        description: string,
        assign?: (value: StudioValue) => void,
      ) {
        const field = key.split(".").at(-1)!;
        const raw = parent && (parent[field] === undefined ? fallback : parent[field]);
        add(
          key,
          label,
          raw,
          rule,
          description,
          scope === "budget" || scope === "pause" ? "hot" : "restart",
          assign ?? ((value) => {
            parent![field] = value;
          }),
        );
      },
      secret(key: string, label: string, isSet: boolean, description: string) {
        entries.set(key, {
          item: {
            key,
            label,
            group,
            scope,
            mode: "restricted",
            editable: false,
            type: "secret",
            isSet,
            description,
            reason: restricted,
          },
        });
      },
    };
  }

  const supervisor = child(docs.supervisor?.data, "supervisor");
  const runtime = child(docs.runtime?.data, "runtimeService");
  const model = child(docs.runtime?.data, "model");
  const qq = child(docs.qq?.data, "qq");
  const admin = child(docs.admin?.data, "admin");
  const ownership = child(docs.guardian?.data, "ownership");
  const instance = docs.instance?.data;
  const identity = child(instance, "instance"), eve = child(instance, "eve");
  const phone = child(instance, "phone");
  const budget = child(status, "budget"), selected = child(child(status, "model"), "current");
  const instanceGuard = !state.paused || state.upgrade || state.pendingGrant
    ? "请先通过监督器暂停，待交接结束后重新读取"
    : undefined;
  const supervisorView = section("supervisor", "服务", supervisor);
  supervisorView.pref(
    "supervisor.tickMs",
    "监督轮询（毫秒）",
    number(50, 60_000),
    1_000,
    "监督器下次启动采用的检查间隔。",
  );
  supervisorView.pref(
    "supervisor.confirmationMs",
    "升级确认窗口（毫秒）",
    number(100, 3_600_000),
    300_000,
    "监督器下次启动采用；不会延长已经开始的观察。",
  );
  supervisorView.show(
    "supervisor.registered",
    "启动配置中的实例数",
    Array.isArray(docs.supervisor?.data.instances)
      ? docs.supervisor.data.instances.length
      : undefined,
    bigCount,
    "不含仅在监督日志中追加的候选。",
  );
  supervisorView.show(
    "supervisor.initialInstance",
    "部署初始实例",
    supervisor?.initialInstance,
    id,
    "不是当前在线实例；不允许通过配置切换所有者。",
  );
  supervisorView.show(
    "supervisor.paused",
    "监督器持久暂停",
    state.paused,
    flag,
    "与共享预算暂停分别核对。",
  );
  const guardianView = section("guardian", "服务", ownership);
  guardianView.show(
    "guardian.leaseMs",
    "所有权租期（毫秒）",
    ownership?.leaseMs,
    bigCount,
    "与实例心跳、排空和代际证明关联，不开放孤立修改。",
  );
  const adminView = section("admin", "服务", admin);
  adminView.show(
    "admin.port",
    "配置的本机管理端口",
    admin && (admin.port ?? 0),
    number(0, 65535),
    "0 为动态端口；不开放监听地址、端口或代理配置。",
  );
  adminView.show(
    "admin.publicOriginConfigured",
    "已指定公开来源",
    admin && !!admin.publicOrigin,
    flag,
    "只展示是否配置，不返回地址。",
  );

  const budgetView = section("budget", "预算", budget);
  budgetView.pref(
    "budget.dailyTokenBudget",
    "每日共享 token 预算",
    bigCount,
    undefined,
    "即时提交到共享预算设置账本；0 表示没有可用额度，不是无限制。",
  );
  budgetView.show(
    "budget.timeZone",
    "预算日界时区",
    budget?.timeZone,
    timeZone,
    "运行设置账本的值。管理 IPC 不支持改日界；不写兼容快照冒充生效。",
  );
  section("pause", "预算", budget).pref(
    "runtime.paused",
    "共享执行暂停",
    flag,
    undefined,
    "使用监督器完整暂停/恢复流程；不会解除被拒模型请求的独立阻塞。",
  );

  const instanceView = section("instance", "实例", eve, instanceGuard);
  instanceView.show(
    "instance.current",
    "监督器当前实例",
    state.current,
    id,
    "尚无当前实例时为空。",
  );
  instanceView.show(
    "instance.target",
    "本页实例配置的目标",
    identity?.id,
    id,
    "仅当前代，首次启动前为监督器指定的初始代；不会改写候选或回退副本。",
  );
  instanceView.show(
    "instance.sourceCommit",
    "固定源码提交",
    identity?.sourceCommit,
    string((text) => /^[a-f0-9]{40}$/.test(text)),
    "只读；换版本仍需经过监督流程。",
  );
  instanceView.show(
    "instance.leaseMs",
    "实例心跳租期（毫秒）",
    identity?.leaseMs,
    bigCount,
    "不绕过守护代际边界。",
  );
  instanceView.pref(
    "eve.pollMs",
    "意识轮询（毫秒）",
    number(100, 60_000),
    undefined,
    "实例下次启动的轮询间隔。",
  );
  instanceView.pref(
    "eve.defaultSleepMs",
    "默认休息（毫秒）",
    number(1_000, 86_400_000),
    undefined,
    "新休息决策使用；不改写已经记录的唤醒时间。",
  );
  instanceView.pref(
    "eve.restReviewEnabled",
    "休息收尾",
    flag,
    eve?.remoteRecords === true,
    "实例重启后采用的休息收尾偏好。",
  );
  instanceView.pref(
    "eve.startupTimeoutMs",
    "意识恢复期限（毫秒）",
    startupMs,
    300_000,
    "意识插件下次启动的恢复期限。",
  );
  instanceView.pref(
    "eve.modelTimeoutMs",
    "实例模型期限（毫秒）",
    modelMs,
    undefined,
    "实例侧等待期限，独立于传输超时。",
  );
  instanceView.pref(
    "eve.maxOutputTokens",
    "实例输出预留（token）",
    outputTokens,
    undefined,
    "实例侧作预算预估，不改变执行服务的模型输出上限。",
  );
  instanceView.show(
    "eve.agentsEnabled",
    "子代理委托工具",
    eve && eve.remoteRecords === true && eve.agentsEnabled !== false,
    flag,
    "影响能力集；不通过偏好开通工具。",
  );
  section("instance", "实例", phone).show(
    "phone.describeImages",
    "手机声明模型依赖",
    phone && (phone.describeImages ?? false),
    flag,
    "控制插件模型依赖声明，不是安全的图片访问权限开关。",
  );
  const scripts = section("instance", "脚本", child(instance, "scripts"), instanceGuard);
  scripts.pref(
    "scripts.defaultTimeoutMs",
    "默认脚本期限（毫秒）",
    number(100, 300_000),
    undefined,
    "仅影响未指定期限的新任务；不改变可执行文件或工具权限。",
  );
  scripts.pref(
    "scripts.startupTimeoutMs",
    "脚本恢复期限（毫秒）",
    startupMs,
    300_000,
    "脚本插件下次启动的恢复期限。",
  );
  const nightly = child(instance, "nightlySelfCheck");
  const schedule = section("instance", "自检", nightly, instanceGuard);
  schedule.show(
    "nightly.configured",
    "已部署每晚自检",
    instance && !!nightly,
    flag,
    "未部署时不能创建附件目录或任务。",
  );
  schedule.pref(
    "nightly.timeZone",
    "自检时区",
    timeZone,
    undefined,
    "已有提醒使用的 IANA 时区；不改变预算日界。",
  );
  schedule.pref("nightly.hour", "自检小时", number(0, 23), undefined, "自检时区的 24 小时制小时。");
  schedule.pref(
    "nightly.minute",
    "自检分钟",
    number(0, 59),
    undefined,
    "只改已有提醒的分钟，不执行清理。",
  );

  const modelView = section("runtime", "模型", model);
  modelView.show(
    "model.configured",
    "部署的初始模型",
    model?.model,
    modelName,
    "不是对选择账本的覆盖。",
  );
  modelView.show(
    "model.current",
    "当前模型",
    selected?.model,
    modelName,
    "只显示运行服务报告的选择；不从初始配置推断，不冒用主人切换通道。",
  );
  modelView.show(
    "model.protocol",
    "模型协议",
    model?.protocol,
    choice("chat", "responses"),
    "协议和端点成套部署。",
  );
  modelView.pref(
    "model.reasoningEffort",
    "主模型推理强度",
    choice("provider-default", ...effort.options!),
    "provider-default",
    "执行服务重启后用于请求；provider-default 不发送强度覆盖。",
    (value) => {
      if (value === "provider-default") delete model!.reasoningEffort;
      else model!.reasoningEffort = value;
    },
  );
  modelView.pref(
    "model.maxOutputTokens",
    "主模型输出上限",
    outputTokens,
    undefined,
    "执行服务重启后采用；与实例预留分列。",
  );
  modelView.pref(
    "model.timeoutMs",
    "默认传输期限（毫秒）",
    modelMs,
    undefined,
    "未覆盖的连接、首 token 和空闲期限继承此值。",
  );
  for (
    const [field, label] of [["connectTimeoutMs", "连接"], ["firstTokenTimeoutMs", "首 token"], [
      "idleTimeoutMs",
      "流空闲",
    ]]
  ) {
    modelView.pref(
      `model.${field}`,
      `${label}期限（毫秒）`,
      modelMs,
      model?.timeoutMs,
      "保存后使用显式期限。",
    );
  }
  modelView.pref(
    "model.maxDurationMs",
    "单次传输总期限（毫秒）",
    modelMs,
    MODEL_MAX_DURATION_MS,
    "不超过执行服务安全总期限。",
  );
  modelView.pref(
    "model.maxResponseBytes",
    "模型响应大小上限（字节）",
    number(1024, 16 * mib),
    2 * mib,
    "限制响应缓冲大小。",
  );
  for (const role of ["review", "coding", "investigation"]) {
    const profile = child(child(docs.runtime?.data, "agentModels"), role);
    const agent = section("runtime", "子代理", profile);
    agent.show(
      `agents.${role}.model`,
      `${role} 专用模型`,
      profile?.model,
      modelName,
      "仅展示已装配的专用角色，不创建角色或更换账户。",
    );
    agent.pref(
      `agents.${role}.reasoningEffort`,
      `${role} 推理强度`,
      effort,
      "high",
      "只调整已有角色。",
    );
    agent.pref(
      `agents.${role}.maxOutputTokens`,
      `${role} 输出上限`,
      outputTokens,
      model?.maxOutputTokens,
      "未显式配置时继承主模型输出上限。",
    );
    agent.secret(
      `agents.${role}.apiKey`,
      `${role} 凭据`,
      await secretFileIsSet(profile?.apiKeyFile),
      "只检查引用文件状态，不读内容。",
    );
  }

  const qqView = section("qq", "QQ", qq);
  qqView.show(
    "qq.replyScope",
    "QQ 发送范围",
    qq && (qq.replyScope ?? "private"),
    choice("private", "all"),
    "由部署决定能否向群聊发送；不是界面可放宽的偏好。",
  );
  qqView.show("qq.sandbox", "QQ 沙箱", qq?.sandbox, flag, "当前生产组合只接受 false。");
  qqView.show(
    "qq.captureActivationBoundary",
    "启动时捕获入站边界",
    qq && (qq.captureActivationBoundary ?? false),
    flag,
    "保护旧消息边界，不重建历史捕获状态。",
  );
  qqView.show(
    "qq.operatorInteractionTargets",
    "记录操作员互动回执目标",
    qq && (qq.operatorInteractionTargets ?? false),
    flag,
    "涉及留存外部操作目标，不通过偏好变更。",
  );
  const ingress = child(qq, "ingress");
  qqView.show(
    "qq.ingress.mode",
    "QQ 入站方式",
    qq && (ingress?.mode ?? "websocket"),
    choice("websocket", "webhook"),
    "监听、路径、订阅权限和验签成套配置。",
  );
  if (ingress?.mode === "webhook") {
    const webhook = section("qq", "QQ", ingress);
    webhook.pref(
      "qq.ingress.maxBodyBytes",
      "回调正文上限（字节）",
      number(1, 8 * mib),
      mib,
      "限制实际读取的字节数。",
    );
    webhook.pref(
      "qq.ingress.bodyTimeoutMs",
      "回调正文期限（毫秒）",
      number(1, 30_000),
      5_000,
      "不改变签名验证。",
    );
    webhook.pref(
      "qq.ingress.maxInFlight",
      "回调并发上限",
      number(1, 32),
      16,
      "不放宽回调来源或重放窗口。",
    );
  }
  const downloads = child(qq, "attachmentDownloads") ??
    (qq && qq.attachmentDownloads === undefined ? {} : undefined);
  const downloadSettings = section("qq", "QQ", downloads);
  downloadSettings.pref(
    "qq.attachmentDownloads.maxBytes",
    "单附件下载上限（字节）",
    number(1, 256 * mib),
    64 * mib,
    "适配器重启后生效；不接收下载 URL、媒体根、保存位置或执行身份。",
    (value) => {
      downloads!.maxBytes = value;
      qq!.attachmentDownloads = downloads;
    },
  );

  const permissions = section("runtime", "权限", runtime);
  permissions.show(
    "permissions.accessMode",
    "执行权限模式",
    runtime && (runtime.accessMode ?? "restricted"),
    choice("unrestricted", "restricted"),
    "整机/受限模式只读，不能经此接口提权。",
  );
  permissions.show(
    "permissions.commandUseSystemd",
    "逐命令进程组监督",
    runtime?.commandUseSystemd,
    flag,
    "不接受命令、身份或工具配置。",
  );
  permissions.show(
    "permissions.allowedRootCount",
    "配置的文件根数",
    Array.isArray(runtime?.allowedRoots) ? runtime.allowedRoots.length : undefined,
    bigCount,
    "仅计数，不回传或修改路径。",
  );
  permissions.show(
    "permissions.commandRoot",
    "命令身份为 root",
    runtime && child(runtime, "commandIdentity")?.uid === 0,
    flag,
    "仅标识现有授权，不返回身份配置。",
  );
  permissions.show(
    "services.semantic",
    "已装配语义嵌入",
    runtime && !!runtime.semantic,
    flag,
    "不是服务存活探针。",
  );
  permissions.show(
    "services.searchIndex",
    "已装配搜索索引",
    runtime && !!runtime.searchIndex,
    flag,
    "不暴露侧车连接。",
  );
  modelView.secret(
    "model.session",
    "模型会话标识",
    typeof model?.sessionId === "string" && !!model.sessionId,
    "仅显示配置状态。",
  );
  qqView.secret(
    "qq.ownerBound",
    "手机主人绑定",
    typeof phone?.ownerPlatformUserId === "string" && !!phone.ownerPlatformUserId,
    "不返回平台身份。",
  );
  for (
    const [view, key, label, ref] of [
      [modelView, "model.apiKey", "模型凭据", model?.apiKeyFile],
      [qqView, "qq.appId", "QQ 应用标识", qq?.appIdFile],
      [qqView, "qq.appSecret", "QQ 应用密钥", qq?.appSecretFile],
      [qqView, "qq.adminKey", "QQ 管理 IPC", child(qq, "adminControl")?.keyFile],
      [qqView, "qq.modelControlKey", "主人模型控制 IPC", child(qq, "modelControl")?.keyFile],
      [adminView, "admin.token", "管理登录凭据", admin?.tokenFile],
      [adminView, "admin.backendKey", "管理后端 IPC", admin?.backendKeyFile],
      [supervisorView, "supervisor.controlKey", "监督控制凭据", supervisor?.keyFile],
      [guardianView, "guardian.adminKey", "守护管理凭据", ownership?.adminKeyFile],
      [guardianView, "guardian.exitKey", "守护出口凭据", ownership?.exitKeyFile],
      [permissions, "runtime.bridgeKey", "执行桥接凭据", runtime?.keyFile],
      [permissions, "runtime.adminKey", "执行管理凭据", runtime?.adminKeyFile],
      [instanceView, "instance.credential", "实例代际凭据", identity?.credentialFile],
      [instanceView, "phone.bridgeKey", "实例手机桥接凭据", phone?.adapterKeyFile],
    ] as const
  ) view.secret(key, label, await secretFileIsSet(ref), "只检查非空普通文件，不读秘密内容。");
  return entries;
}
