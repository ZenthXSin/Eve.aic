import { Buffer } from "node:buffer";
import { createHash } from "node:crypto";
import { dirname, resolve } from "node:path";
import type {
  PhonePlatformEvent,
  PhonePlatformEvents,
  PhonePlatformEventsArgs,
} from "../../contracts/phone-platform.ts";
import { KernelError } from "../../kernel/protocol.ts";
import { writeDurableJson } from "../../sdk/durable-json.ts";
import type { QqInboundEvent } from "./inbound.ts";
import type { InboundRecord } from "./journal.ts";
import { readPrivatePlatformFile } from "./platform-tool.ts";

const MAX_EVENTS = 8192;
const MAX_RECORD_BYTES = 1280;
const MAX_INTERACTION_KEYS = 8;
const MAX_PENDING = 256;
const MAX_PAGE = 100;
const skipped = new Set([
  "READY",
  "RESUMED",
  "C2C_MESSAGE_CREATE",
  "GROUP_AT_MESSAGE_CREATE",
  "GROUP_MESSAGE_CREATE",
  "MESSAGE_CREATE",
  "AT_MESSAGE_CREATE",
  "DIRECT_MESSAGE_CREATE",
]);
const knownTypes = new Set(
  `
  FRIEND_ADD FRIEND_DEL C2C_MSG_REJECT C2C_MSG_RECEIVE
  GROUP_ADD_ROBOT GROUP_DEL_ROBOT GROUP_MSG_REJECT GROUP_MSG_RECEIVE
  GUILD_CREATE GUILD_UPDATE GUILD_DELETE GUILD_MEMBER_ADD GUILD_MEMBER_UPDATE GUILD_MEMBER_REMOVE
  CHANNEL_CREATE CHANNEL_UPDATE CHANNEL_DELETE AUDIO_OR_LIVE_CHANNEL_MEMBER_ENTER AUDIO_OR_LIVE_CHANNEL_MEMBER_EXIT
  MESSAGE_DELETE PUBLIC_MESSAGE_DELETE DIRECT_MESSAGE_DELETE MESSAGE_REACTION_ADD MESSAGE_REACTION_REMOVE
  MESSAGE_AUDIT_PASS MESSAGE_AUDIT_REJECT INTERACTION_CREATE
  FORUM_THREAD_CREATE FORUM_THREAD_UPDATE FORUM_THREAD_DELETE FORUM_POST_CREATE FORUM_POST_DELETE
  FORUM_REPLY_CREATE FORUM_REPLY_DELETE FORUM_PUBLISH_AUDIT_RESULT
  OPEN_FORUM_THREAD_CREATE OPEN_FORUM_THREAD_UPDATE OPEN_FORUM_THREAD_DELETE OPEN_FORUM_POST_CREATE
  OPEN_FORUM_POST_DELETE OPEN_FORUM_REPLY_CREATE OPEN_FORUM_REPLY_DELETE
  AUDIO_START AUDIO_FINISH AUDIO_ON_MIC AUDIO_OFF_MIC
`.trim().split(/\s+/),
);

type Row = PhonePlatformEvent & {
  key: string;
  target?: { interactionId: string; aliases: string[] };
};

export interface QqPlatformEventsOptions {
  operatorInteractionTargets?: boolean;
}

export interface QqOperatorInteractionTarget {
  seq: number;
  operation: "acknowledgeInteraction";
  interactionId: string;
}

function corrupt() {
  return new KernelError("corrupt", "QQ 平台事件收件箱异常，原件保留");
}

function storageFailure(cause: unknown) {
  return cause instanceof KernelError
    ? cause
    : new KernelError("storage", "QQ 平台事件尚未确认持久化");
}

function invalidEvent(): never {
  throw new TypeError("无效或过大的 QQ 平台事件");
}

function sha256(text: string): string {
  return createHash("sha256").update(text).digest("hex");
}

function eventKey(type: string, id: unknown, data: unknown): string {
  if (id !== undefined) {
    if (typeof id !== "string" || !id.length || id.length > 512) invalidEvent();
    return sha256(JSON.stringify(["id", type, id]));
  }

  const hash = createHash("sha256").update(JSON.stringify(["payload", type]));
  let bytesLeft = 1_048_576, nodesLeft = 16_384;
  function token(text: string) {
    bytesLeft -= Buffer.byteLength(text);
    if (bytesLeft < 0) invalidEvent();
    hash.update(text);
  }
  function string(text: string) {
    if (text.length > bytesLeft) invalidEvent();
    token(JSON.stringify(text));
  }
  function visit(value: unknown, depth: number) {
    if (--nodesLeft < 0 || depth > 32) invalidEvent();
    if (typeof value === "string") string(value);
    else if (value === null || typeof value === "boolean" || Number.isFinite(value)) {
      token(JSON.stringify(value));
    } else if (Array.isArray(value)) {
      if (value.length > nodesLeft) invalidEvent();
      token("[");
      for (let index = 0; index < value.length; index++) {
        if (index) token(",");
        visit(value[index], depth + 1);
      }
      token("]");
    } else if (
      typeof value === "object" && value !== null &&
      [null, Object.prototype].includes(Object.getPrototypeOf(value))
    ) {
      const keys = Object.keys(value);
      if (keys.length > nodesLeft) invalidEvent();
      token("{");
      for (const [index, key] of keys.sort().entries()) {
        if (index) token(",");
        string(key);
        token(":");
        visit((value as Record<string, unknown>)[key], depth + 1);
      }
      token("}");
    } else invalidEvent();
  }
  visit(data, 0);
  return hash.digest("hex");
}

function validInteractionId(value: unknown): value is string {
  return typeof value === "string" && value.length <= 256 && /[a-z0-9]/i.test(value) &&
    !/[^a-zA-Z0-9_~.+=-]/.test(value);
}

function authoritativeId(data: unknown): string | undefined {
  if (data === null || typeof data !== "object" || Array.isArray(data)) return;
  const id: unknown = Object.getOwnPropertyDescriptor(data, "id")?.value;
  return validInteractionId(id) ? id : undefined;
}

function isDigest(value: unknown): value is string {
  return typeof value === "string" && value.length === 64 && !/[^a-f0-9]/.test(value);
}

function stored({ seq, key, type, receivedAt, target }: Row) {
  const body = {
    schema: target ? 2 : 1,
    seq,
    key,
    type,
    receivedAt,
    ...(target ? { interactionId: target.interactionId, aliases: target.aliases } : {}),
  };
  return { ...body, checksum: sha256(JSON.stringify(body)) };
}

function filename(seq: number) {
  return `${String(seq).padStart(8, "0")}.json`;
}

async function readRow(directory: string, seq: number, owner: number): Promise<Row> {
  try {
    const text = await readPrivatePlatformFile(
      `${directory}/${filename(seq)}`,
      owner,
      MAX_RECORD_BYTES,
    );
    const { schema, seq: storedSeq, key, type, receivedAt, interactionId, aliases } = JSON.parse(
      text,
    );
    if (
      storedSeq !== seq || !isDigest(key) || (type !== "UNKNOWN" && !knownTypes.has(type)) ||
      typeof receivedAt !== "string" || new Date(receivedAt).toISOString() !== receivedAt
    ) throw corrupt();
    const row: Row = { seq, key, type, receivedAt };
    if (schema === 2) {
      if (
        type !== "INTERACTION_CREATE" || !validInteractionId(interactionId) ||
        !Array.isArray(aliases) ||
        aliases.length >= MAX_INTERACTION_KEYS || !aliases.every(isDigest) ||
        new Set([key, ...aliases]).size !== aliases.length + 1
      ) throw corrupt();
      row.target = { interactionId, aliases };
    }
    if (text !== JSON.stringify(stored(row)) + "\n") throw corrupt();
    return row;
  } catch {
    throw corrupt();
  }
}

export async function readQqOperatorInteractionTarget(
  directory: string,
  seq: number,
  owner: number,
): Promise<QqOperatorInteractionTarget | null> {
  if (
    !Number.isSafeInteger(seq) || seq < 1 || seq > MAX_EVENTS || !Number.isSafeInteger(owner) ||
    owner < 0
  ) {
    throw new TypeError("无效的 QQ 平台事件查询");
  }
  const info = await Deno.lstat(directory);
  if (!info.isDirectory || info.uid !== owner || info.mode === null || (info.mode & 0o077) !== 0) {
    throw corrupt();
  }
  const { target } = await readRow(directory, seq, owner);
  return target
    ? { seq, operation: "acknowledgeInteraction", interactionId: target.interactionId }
    : null;
}

async function syncDirectory(path: string) {
  using handle = await Deno.open(path, { read: true });
  await handle.sync();
}

async function directoryChain(path: string, create: boolean): Promise<Deno.FileInfo> {
  const parent = dirname(path);
  const ancestor = parent === path ? undefined : await directoryChain(parent, create);
  let info;
  try {
    info = await Deno.lstat(path);
  } catch (error) {
    if (!create || !(error instanceof Deno.errors.NotFound)) throw error;
    await Deno.mkdir(path, { mode: 0o700 });
    await syncDirectory(parent);
    info = await Deno.lstat(path);
  }
  if (
    !info.isDirectory || info.uid === null || info.mode === null ||
    ((info.mode & 0o022) !== 0 && (info.mode & 0o1000) === 0) ||
    (ancestor && ancestor.uid !== 0 && ancestor.uid !== info.uid)
  ) throw corrupt();
  return info;
}

async function verifyWriterOwner(directory: string, owner: number) {
  if (owner === 0) return;
  if (Deno.permissions.querySync({ name: "sys", kind: "uid" }).state === "granted") {
    if (Deno.uid() !== owner) throw corrupt();
    return;
  }
  const probe = `${directory}/.${crypto.randomUUID()}.tmp`;
  using file = await Deno.open(probe, { createNew: true, write: true, mode: 0o600 });
  try {
    if ((await file.stat()).uid !== owner) throw corrupt();
  } finally {
    await Deno.remove(probe);
  }
}

/** The receiver owns the writer lock. Full inboxes retain every acknowledged row and retry key. */
export class QqPlatformEvents {
  readonly #directory: string;
  readonly #captureTargets: boolean;
  #directoryInfo?: Deno.FileInfo;
  #state: "new" | "starting" | "ready" | "failed" = "new";
  #rows: Row[] = [];
  #byKey = new Map<string, Row>();
  #byInteraction = new Map<string, Row>();
  #metadataOnlyInteractions = 0;
  #tail: Promise<void> = Promise.resolve();
  #pending = 0;

  constructor(
    directory: string,
    { operatorInteractionTargets = false }: QqPlatformEventsOptions = {},
  ) {
    if (!directory) throw new TypeError("缺少 QQ 平台事件目录");
    if (typeof operatorInteractionTargets !== "boolean") {
      throw new TypeError("无效的 QQ 平台交互回执选项");
    }
    this.#directory = resolve(directory);
    this.#captureTargets = operatorInteractionTargets;
  }

  get available(): boolean {
    return this.#state === "ready" && this.#rows.length < MAX_EVENTS;
  }

  async #checkDirectory(create = false) {
    const info = await directoryChain(this.#directory, create);
    if (
      !info.isDirectory || info.uid === null || info.mode === null || (info.mode & 0o077) !== 0 ||
      (this.#directoryInfo &&
        (info.ino !== this.#directoryInfo.ino || info.dev !== this.#directoryInfo.dev ||
          info.uid !== this.#directoryInfo.uid))
    ) throw corrupt();
    if (!this.#directoryInfo) await verifyWriterOwner(this.#directory, info.uid);
    this.#directoryInfo = info;
    return info.uid;
  }

  #index(row: Row) {
    const old = this.#rows[row.seq - 1];
    if (old?.type === "INTERACTION_CREATE" && !old.target) this.#metadataOnlyInteractions--;
    this.#rows[row.seq - 1] = row;
    this.#byKey.set(row.key, row);
    if (row.target) {
      this.#byInteraction.set(row.target.interactionId, row);
      for (const alias of row.target.aliases) this.#byKey.set(alias, row);
    } else if (row.type === "INTERACTION_CREATE") this.#metadataOnlyInteractions++;
  }

  async initialize(): Promise<void> {
    if (this.#state !== "new") {
      throw new KernelError("unavailable", "QQ 平台事件收件箱已启动或失效");
    }
    this.#state = "starting";
    try {
      const owner = await this.#checkDirectory(true);
      let count = 0, lastSeq = 0;
      for await (const entry of Deno.readDir(this.#directory)) {
        if (!entry.isFile || !/^\d{8}\.json$/.test(entry.name) || ++count > MAX_EVENTS) {
          throw corrupt();
        }
        const seq = Number(entry.name.slice(0, 8));
        if (seq < 1 || seq > MAX_EVENTS) throw corrupt();
        lastSeq = Math.max(lastSeq, seq);
      }
      if (lastSeq !== count) throw corrupt();
      for (let seq = 1; seq <= count; seq++) {
        const row = await readRow(this.#directory, seq, owner);
        if (
          this.#byKey.has(row.key) ||
          (row.target && (this.#byInteraction.has(row.target.interactionId) ||
            row.target.aliases.some((alias) => this.#byKey.has(alias))))
        ) throw corrupt();
        this.#index(row);
      }
      await syncDirectory(this.#directory);
      await syncDirectory(dirname(this.#directory));
      this.#state = "ready";
    } catch (error) {
      this.#state = "failed";
      throw storageFailure(error);
    }
  }

  #accepting() {
    if (this.#state !== "ready" || this.#pending >= MAX_PENDING) {
      throw new KernelError("unavailable", "QQ 平台事件收件箱不可用或繁忙");
    }
  }

  #queue<Result>(operation: () => Promise<Result> | Result): Promise<Result> {
    this.#pending++;
    const result = this.#tail.then(() => {
      if (this.#state !== "ready") throw new KernelError("unavailable", "QQ 平台事件收件箱不可用");
      return operation();
    }).finally(() => this.#pending--);
    this.#tail = result.then(() => {}, () => {});
    return result;
  }

  async observe(event: QqInboundEvent, record?: InboundRecord): Promise<void> {
    this.#accepting();
    if (
      !event || event.op !== 0 || typeof event.t !== "string" || event.t.length > 128 ||
      !event.t.trim() || !Object.hasOwn(event, "d")
    ) invalidEvent();
    if (record !== undefined || skipped.has(event.t)) return;
    const type = knownTypes.has(event.t) ? event.t : "UNKNOWN";
    const receivedAt = new Date().toISOString();
    const interactionId = type === "INTERACTION_CREATE" ? authoritativeId(event.d) : undefined;
    const capture = this.#captureTargets && type === "INTERACTION_CREATE";
    if (capture && interactionId === undefined) invalidEvent();
    const explicit = event.id !== undefined;
    const byIdOnly = interactionId !== undefined && !explicit &&
      (capture || this.#byInteraction.has(interactionId));
    const key = byIdOnly
      ? sha256(JSON.stringify(["interaction", interactionId]))
      : eventKey(event.t, event.id, event.d);
    let legacyKey: string | undefined;
    if (
      capture && interactionId !== undefined && this.#metadataOnlyInteractions &&
      !this.#byKey.has(key) && !this.#byInteraction.has(interactionId)
    ) {
      try {
        legacyKey = eventKey(event.t, undefined, event.d);
      } catch (error) {
        if (!(error instanceof TypeError)) throw error;
      }
    }
    return await this.#queue(async () => {
      const primary = this.#byKey.get(key);
      const legacy = legacyKey === undefined ? undefined : this.#byKey.get(legacyKey);
      const target = interactionId === undefined
        ? undefined
        : this.#byInteraction.get(interactionId);
      const previous = primary ?? legacy ?? target;
      if (
        previous && ((legacy && legacy !== previous) || (target && target !== previous) ||
          (previous.target && previous.target.interactionId !== interactionId))
      ) {
        throw new KernelError("rejected", "QQ 平台交互身份冲突，原回执保留");
      }
      if (capture && previous && !previous.target && previous !== legacy) {
        throw new KernelError("rejected", "历史元数据无法证明交互目标，原件保留");
      }
      const bindAlias = previous !== undefined && explicit && !primary &&
        (previous.target !== undefined || (capture && previous === legacy));
      if (bindAlias && (previous.target?.aliases.length ?? 0) >= MAX_INTERACTION_KEYS - 1) {
        throw new KernelError("rejected", "QQ 平台交互重投标识已达上限，原回执保留");
      }
      if (!previous && this.#rows.length === MAX_EVENTS) {
        throw new KernelError("storage", "QQ 平台事件收件箱已满");
      }
      try {
        const owner = await this.#checkDirectory();
        let row: Row;
        if (previous) {
          row = await readRow(this.#directory, previous.seq, owner);
          if (stored(row).checksum !== stored(previous).checksum) throw corrupt();
          if (!bindAlias && (!capture || row.target)) return;
        } else {
          row = { seq: this.#rows.length + 1, key, type, receivedAt };
          try {
            await Deno.lstat(`${this.#directory}/${filename(row.seq)}`);
            throw corrupt();
          } catch (error) {
            if (!(error instanceof Deno.errors.NotFound)) throw error;
          }
        }
        if ((capture || bindAlias) && interactionId !== undefined) {
          row.target = {
            interactionId,
            aliases: [...(row.target?.aliases ?? []), ...(bindAlias ? [key] : [])],
          };
        }
        await writeDurableJson(`${this.#directory}/${filename(row.seq)}`, stored(row));
        this.#index(row);
      } catch (error) {
        this.#state = "failed";
        throw storageFailure(error);
      }
    });
  }

  /** afterSeq is exclusive (default 0); limit is 1..100 (default 16). Resume at throughSeq,
   * the last returned seq or the unchanged cursor on an empty page. Future cursors are rejected. */
  async page(args: PhonePlatformEventsArgs = {}): Promise<PhonePlatformEvents> {
    this.#accepting();
    if (
      !args || typeof args !== "object" || Array.isArray(args) ||
      Object.keys(args).some((key) => key !== "afterSeq" && key !== "limit")
    ) throw new TypeError("无效的 QQ 平台事件分页");
    const { afterSeq = 0, limit = 16 } = args;
    if (
      !Number.isSafeInteger(afterSeq) || afterSeq < 0 || !Number.isSafeInteger(limit) ||
      limit < 1 || limit > MAX_PAGE
    ) throw new TypeError("无效的 QQ 平台事件分页");
    return await this.#queue(() => {
      if (afterSeq > this.#rows.length) throw new TypeError("QQ 平台事件游标超过当前水位");
      const items = this.#rows.slice(afterSeq, afterSeq + limit).map((
        { seq, type, receivedAt },
      ) => ({ seq, type, receivedAt }));
      const throughSeq = afterSeq + items.length;
      return { items, throughSeq, hasMore: throughSeq < this.#rows.length };
    });
  }
}
