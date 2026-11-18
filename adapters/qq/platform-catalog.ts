import type { Json } from "../../kernel/protocol.ts";

export type PlatformAccess = "read" | "write" | "admin";
export type PlatformMethod = "GET" | "POST" | "PUT" | "PATCH" | "DELETE";
export type PlatformSurface =
  | "v2"
  | "v2-restricted"
  | "guild"
  | "guild-restricted"
  | "sdk"
  | "gateway";

export interface PlatformOperation {
  readonly name: string;
  readonly method: PlatformMethod;
  readonly path: string;
  readonly access: PlatformAccess;
  readonly surface: PlatformSurface;
  readonly retryable: boolean;
  readonly pathParams: readonly string[];
  readonly queryParams: readonly string[];
  readonly bodyParams: readonly string[];
  readonly requiredQuery: readonly string[];
  readonly requiredBody: readonly string[];
}

type Data = Record<string, Json>;
type Rule = (value: unknown, field: string) => Json;
type Schema = ((value: unknown, field: string) => Data) & {
  keys: readonly string[];
  required: readonly string[];
};

function invalid(field: string): never {
  throw new TypeError(`Invalid QQ platform ${field}`);
}

function fields(
  value: unknown,
  allowed: readonly string[],
  label: string,
): Record<string, unknown> {
  if (
    value === null || typeof value !== "object" || Array.isArray(value) ||
    ![null, Object.prototype].includes(Object.getPrototypeOf(value))
  ) invalid(label);
  const result: Record<string, unknown> = Object.create(null);
  for (const key of Reflect.ownKeys(value)) {
    if (typeof key !== "string" || !allowed.includes(key)) invalid(label);
    const descriptor = Object.getOwnPropertyDescriptor(value, key)!;
    if (!descriptor.enumerable || !("value" in descriptor)) invalid(label);
    result[key] = descriptor.value;
  }
  return result;
}

function schema(rules: Record<string, Rule>, valid: (data: Data) => boolean = () => true): Schema {
  const entries = Object.entries(rules);
  const keys = Object.freeze(entries.map(([name]) => name.replace(/!$/, "")));
  const required = Object.freeze(
    entries.filter(([name]) => name.endsWith("!")).map(([name]) => name.slice(0, -1)),
  );
  function parse(value: unknown, label: string): Data {
    const input = fields(value, keys, label);
    const data: Data = {};
    for (let index = 0; index < keys.length; index++) {
      const key = keys[index];
      if (Object.hasOwn(input, key)) data[key] = entries[index][1](input[key], key);
    }
    if (!required.every((key) => Object.hasOwn(data, key)) || !valid(data)) invalid(label);
    return data;
  }
  return Object.assign(parse, { keys, required });
}

function text(max = 4096, min = 1): Rule {
  return (value, key) =>
    typeof value === "string" && value.length >= min && value.length <= max &&
      !/[\p{Cc}\p{Cs}]/u.test(value.replace(/[\r\n\t]/g, ""))
      ? value
      : invalid(key);
}

function matches(pattern: RegExp, max: number): Rule {
  return (value, key) =>
    typeof value === "string" && value.length <= max && pattern.test(value) ? value : invalid(key);
}

function choice(...choices: (number | string | boolean)[]): Rule {
  return (value, key) => choices.find((option) => option === value) ?? invalid(key);
}

function integer(min: number, max: number, decimalString = false): Rule {
  return (value, key) => {
    const number = decimalString && typeof value === "string" && /^(0|[1-9]\d{0,15})$/.test(value)
      ? Number(value)
      : value;
    if (
      typeof number !== "number" || !Number.isSafeInteger(number) || number < min || number > max
    ) invalid(key);
    return value as string | number;
  };
}

function array(rule: Rule, max: number, min = 1): Rule {
  return (value, key) => {
    if (
      !Array.isArray(value) || Object.getPrototypeOf(value) !== Array.prototype ||
      value.length < min ||
      value.length > max || Reflect.ownKeys(value).length !== value.length + 1
    ) invalid(key);
    return Array.from({ length: value.length }, (_, index) => {
      const item = Object.getOwnPropertyDescriptor(value, index);
      return item?.enumerable && "value" in item ? rule(item.value, key) : invalid(key);
    });
  };
}

const boolean = choice(false, true);
const id = matches(/^(?=.*[A-Za-z0-9])[A-Za-z0-9_~.+=-]+$/, 256);
const opaque = matches(/^[^\s\p{C}]+$/u, 4096);
const cursor = matches(/^[^\p{C}]*$/u, 512);
const uint32 = integer(0, 0xffff_ffff);
const digits = matches(/^(?:0|[1-9]\d{0,18})$/, 19);
const uint64Query: Rule = (value, key) => {
  if (typeof value !== "string") return integer(0, Number.MAX_SAFE_INTEGER)(value, key);
  return /^(0|[1-9]\d{0,19})$/.test(value) && BigInt(value) <= 0xffff_ffff_ffff_ffffn
    ? value
    : invalid(key);
};
const md5 = matches(/^[\da-f]{32}$/i, 32);
const sha1 = matches(/^[\da-f]{40}$/i, 40);
const timestamp: Rule = (value, key) =>
  typeof value === "string" && value.length <= 35 &&
    /^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d{1,9})?(?:Z|[+-]\d\d:\d\d)$/.test(value) &&
    Number.isFinite(Date.parse(value))
    ? value
    : invalid(key);
const filename = matches(/^(?!\.)(?!.*[. ]$)[\p{L}\p{N}_ .()-]+$/u, 255);

function present(data: Data, ...names: string[]) {
  return names.filter((name) => Object.hasOwn(data, name)).length;
}

function publicLink(value: unknown, key: string): string {
  if (
    typeof value !== "string" || value.length > 1024 ||
    !/^https:\/\/([a-z\d-]+\.)+[a-z]{2,63}(?:\/[A-Za-z0-9._~/-]*)?$/.test(value) ||
    /\/(?:\.|\.\.)(?:\/|$)|\/\//.test(value.slice(8))
  ) invalid(key);
  const host = new URL(value).hostname;
  if (
    /(?:^|\.)(?:localhost|local|lan|internal|intranet|corp|home|test|invalid|onion|arpa)$/.test(
      host,
    )
  ) invalid(key);
  return value;
}

function assetUrl(value: unknown, key: string): string {
  const url = publicLink(value, key);
  const host = new URL(url).hostname;
  if (
    !/^(?:[a-z\d-]+\.)*(?:qpic\.cn|qlogo\.cn|gtimg\.cn)$/.test(host) &&
    !/^[a-z\d-]+-\d+\.cos\.[a-z\d-]+\.myqcloud\.com$/.test(host)
  ) invalid(key);
  return url;
}

function markdownText(value: unknown, key: string): string {
  const content = text(16384)(value, key) as string;
  const rest = content.replace(
    /(!?)\[[\p{L}\p{N} #_-]{0,100}\]\(([^\s()]+)\)/gu,
    (_, image: string, url: string) => {
      if (!image) publicLink(url, key);
      else if (!/^qqbot-media:\/\/image\/v1\/[\w-]{1,512}$/.test(url)) assetUrl(url, key);
      return "";
    },
  );
  if (/[\[\]<>\\]|&(?:#|[a-z]+;)/i.test(rest)) invalid(key);
  return content;
}

const markdown = schema({ "content!": markdownText, force_verify_image_resource: boolean });
const ref = schema({ "message_id!": id });
const keyboard = schema({ "id!": id });
const card = schema({
  "type!": choice("tuwen"),
  "content!": schema({
    "title!": text(50),
    "description!": text(200),
    "url!": publicLink,
    "pic_url!": assetUrl,
  }),
});
const inputNotify = schema({ "input_type!": choice(1), "input_second!": integer(1, 60) });

function replyMode(data: Data) {
  return present(data, "msg_id", "event_id") <= 1 &&
    !(data.is_wakeup === true && present(data, "msg_id", "event_id"));
}

function message(c2c: boolean): Schema {
  const rules: Record<string, Rule> = {
    msg_type: c2c ? choice(0, 2, 6, 7, 8) : choice(0, 2, 7, 8),
    content: text(16384),
    markdown,
    keyboard,
    media: schema({ "file_info!": opaque }),
    card,
    message_reference: ref,
    msg_id: id,
    event_id: id,
    msg_seq: uint32,
  };
  if (c2c) Object.assign(rules, { is_wakeup: boolean, input_notify: inputNotify });
  const payloads = {
    0: "content",
    2: "markdown",
    6: "input_notify",
    7: "media",
    8: "card",
  } as const;
  return schema(rules, (data) => {
    const kind = (data.msg_type ?? 0) as keyof typeof payloads;
    const allowedCount = kind === 7 && data.content ? 2 : 1;
    return replyMode(data) && Object.hasOwn(data, payloads[kind]) &&
      present(data, ...Object.values(payloads)) === allowedCount &&
      (!data.keyboard || kind === 2) && (!data.message_reference || kind === 0);
  });
}

const chunkBytes = integer(1, 200 * 1024 * 1024, true);
const fileType = choice(1, 2, 3, 4);
const upload = schema({
  file_type: fileType,
  url: assetUrl,
  srv_send_msg: boolean,
  upload_id: id,
  file_name: filename,
  file_data: matches(/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/, 16384),
}, (data) =>
  present(data, "url", "file_data", "upload_id") === 1 &&
  (data.upload_id ? !data.srv_send_msg : !!data.file_type &&
    (!Object.hasOwn(data, "file_data") || !!data.file_data && !!data.file_name)));
const prepare = schema({
  "file_type!": fileType,
  "file_size!": chunkBytes,
  "file_name!": filename,
  "md5!": md5,
  "sha1!": sha1,
  "md5_10m!": md5,
});
const part = schema({
  "upload_id!": id,
  "part_index!": uint32,
  "block_size!": chunkBytes,
  "md5!": md5,
});
const material = schema({
  "file_name!": filename,
  "file_size!": integer(1, 10 * 1024 * 1024),
  "content_type!": choice("image/png", "image/jpeg", "image/gif", "image/webp"),
  "md5!": md5,
  "sha1!": sha1,
}, (data) => {
  const ext = String(data.file_name).split(".").pop()?.toLowerCase();
  return String(data.file_name).includes(".") &&
    data.content_type === `image/${ext === "jpg" ? "jpeg" : ext}`;
});

const stream = schema({
  input_mode: choice("append", "replace"),
  "input_state!": choice(1, 10),
  "index!": uint32,
  "content_type!": choice("text", "markdown"),
  "content_raw!": text(16384, 0),
  event_id: id,
  msg_id: id,
  stream_msg_id: id,
  msg_seq: uint32,
  is_wakeup: boolean,
}, (data) =>
  replyMode(data) && (data.index === 0 ? !data.stream_msg_id : !!data.stream_msg_id) &&
  (data.content_raw !== "" || data.input_state === 10) &&
  (data.content_type !== "markdown" || !/[\[\]<>\\&]/.test(String(data.content_raw))));

const scope = choice("c2c", "group", "channel", "dm");
const addDel = choice("add", "del");
const targetIds = array(id, 20);
const targets = { user_openids: targetIds, group_openids: targetIds };
function displayLabel(units: number): Rule {
  return (value, key) => {
    const label = text(units)(value, key) as string;
    return label.replace(/[^\x20-\x7e]/gu, "..").length <= units ? label : invalid(key);
  };
}
const panel = schema({
  "items!": array(
    schema({
      "name!": displayLabel(14),
      desc: displayLabel(30),
      "type!": choice("command", "link"),
      only_admin: boolean,
      link: publicLink,
    }, (item) => (item.type === "link") === !!item.link),
    20,
    0,
  ),
  remark: text(255, 0),
  version: uint32,
});
const createPanel = schema({
  "scope!": scope,
  target_type: choice("all", "specific"),
  ...targets,
  "panel!": panel,
}, (data) => {
  const count = present(data, "user_openids", "group_openids");
  return data.target_type === "specific"
    ? count === 1 &&
      ((data.scope === "c2c" && !!data.user_openids) ||
        (data.scope === "group" && !!data.group_openids))
    : count === 0;
});
function menuChoice(item: Data) {
  const key = item.type === "menu" ? "sub_menu_items" : String(item.type);
  return present(item, "send_message", "link", "switch", "sub_menu_items") === 1 &&
    Object.hasOwn(item, key);
}
const menuAction = { send_message: text(1024), link: publicLink };
const subMenu = schema({
  "name!": displayLabel(14),
  "type!": choice("send_message", "link"),
  ...menuAction,
}, menuChoice);
const menuItem = schema({
  "name!": displayLabel(10),
  "type!": choice("switch", "send_message", "link", "menu"),
  ...menuAction,
  switch: schema({ "switch_id!": id, default: boolean }),
  sub_menu_items: array(subMenu, 5),
}, menuChoice);

const page = (max: number) => schema({ cursor, limit: integer(1, max, true) });
const enable = choice("on", "off");
const groups = {
  group_openids: array(id, 100),
  group_ids: array(integer(1, Number.MAX_SAFE_INTEGER), 100),
};
const oneGroupList = (data: Data) => present(data, "group_openids", "group_ids") === 1;
const strategyFields = { is_enable: enable, expire_at: timestamp, remark: text(255, 0) };
const muteState = schema({
  "op!": choice("add", "update", "del"),
  "member_openid!": id,
  mute_expire_at: (value, key) => value === "" ? value : timestamp(value, key),
}, (data) => data.op === "del" ? !data.mute_expire_at : !!data.mute_expire_at);

const embed = schema({
  title: text(256),
  prompt: text(256),
  thumbnail: schema({ "url!": assetUrl }),
  fields: array(schema({ "name!": text(1024) }), 20),
}, (data) => Object.keys(data).length > 0);
const guildMessage = schema({
  content: text(16384),
  embed,
  image: assetUrl,
  msg_id: id,
  event_id: id,
  markdown: schema({ "content!": markdownText }),
  message_reference: schema({ "message_id!": id, ignore_get_message_error: boolean }),
}, (data) =>
  replyMode(data) && present(data, "content", "embed", "image", "markdown") >= 1 &&
  (!data.markdown || present(data, "content", "image", "embed") === 0));
const hideTip = schema({ hidetip: choice(true, false, "true", "false") });
const scheduleFields = {
  description: text(4096, 0),
  jump_channel_id: id,
  remind_type: choice("0", "1", "2", "3", "4", "5"),
};
const scheduleOrder = (data: Data) =>
  Object.keys(data).length > 0 && (!data.start_timestamp || !data.end_timestamp ||
    BigInt(String(data.start_timestamp)) <= BigInt(String(data.end_timestamp)));
const permissions = schema(
  { add: digits, remove: digits },
  (data) =>
    present(data, "add", "remove") > 0 &&
    [data.add, data.remove].every((mask) => (BigInt(String(mask ?? 0)) & ~5n) === 0n),
);
const notEmpty = (data: Data) => Object.keys(data).length > 0;
const roleFields = schema({ name: text(256), color: uint32, hoist: choice(0, 1) }, notEmpty);
const channelFields = {
  parent_id: id,
  private_type: choice(0, 1, 2),
  speak_permission: choice(0, 1, 2),
};
const muteWindow = { mute_end_timestamp: digits, mute_seconds: digits };
const oneWindow = (data: Data) => present(data, "mute_end_timestamp", "mute_seconds") === 1;

interface Options {
  query?: Schema;
  body?: Schema;
  optionalBody?: true;
  valid?: (path: Record<string, unknown>, body: Data) => boolean;
}
type Row = readonly [
  name: string,
  method: PlatformMethod,
  path: string,
  access: PlatformAccess,
  options?: Options,
];
const empty = schema({});
const definitions = new Map<string, { entry: PlatformOperation; options: Options }>();
function add(surface: PlatformSurface, rows: readonly Row[]) {
  for (const [name, method, path, access, options = {}] of rows) {
    if (definitions.has(name)) throw new Error("duplicate QQ platform operation");
    const query = options.query ?? empty;
    const body = options.body ?? empty;
    definitions.set(name, {
      options,
      entry: Object.freeze({
        name,
        method,
        path,
        access,
        surface,
        retryable: method === "GET",
        pathParams: Object.freeze([...path.matchAll(/\{(\w+)\}/g)].map((match) => match[1])),
        queryParams: query.keys,
        requiredQuery: query.required,
        bodyParams: body.keys,
        requiredBody: body.required,
      }),
    });
  }
}

const c2c = "/v2/users/{userOpenid}";
const group = "/v2/groups/{groupOpenid}";
const strategy = "/v2/groups/join_approval_strategy";
const panelPath = "/v2/panels/{panelId}";
const guild = "/guilds/{guildId}";
const channel = "/channels/{channelId}";
const dm = "/dms/{dmGuildId}";
const channelMessage = `${channel}/messages/{messageId}`;
const member = `${guild}/members/{userId}`;
const role = `${guild}/roles/{roleId}`;
const memberPermissions = `${channel}/members/{userId}/permissions`;
const rolePermissions = `${channel}/roles/{roleId}/permissions`;
const reaction = `${channelMessage}/reactions/{emojiType}/{emojiId}`;
const schedule = `${channel}/schedules/{scheduleId}`;
const thread = `${channel}/threads/{threadId}`;

add("v2", [
  ["getMe", "GET", "/users/@me", "read"],
  ["sendC2CMessage", "POST", `${c2c}/messages`, "write", { body: message(true) }],
  ["sendGroupMessage", "POST", `${group}/messages`, "write", { body: message(false) }],
  ["sendC2CStreamMessage", "POST", `${c2c}/stream_messages`, "write", { body: stream }],
  ["recallC2CMessage", "DELETE", `${c2c}/messages/{messageId}`, "write"],
  ["recallGroupMessage", "DELETE", `${group}/messages/{messageId}`, "admin"],
  ["uploadC2CFile", "POST", `${c2c}/files`, "write", { body: upload }],
  ["uploadGroupFile", "POST", `${group}/files`, "write", { body: upload }],
  ["prepareC2CUpload", "POST", `${c2c}/upload_prepare`, "write", { body: prepare }],
  ["prepareGroupUpload", "POST", `${group}/upload_prepare`, "write", { body: prepare }],
  ["finishC2CUploadPart", "POST", `${c2c}/upload_part_finish`, "write", { body: part }],
  ["finishGroupUploadPart", "POST", `${group}/upload_part_finish`, "write", { body: part }],
  ["listMediaImages", "GET", "/v2/media-images", "admin", { query: schema({ cursor }) }],
  ["prepareMediaImage", "POST", "/v2/media-images/prepare", "write", { body: material }],
  ["completeMediaImage", "POST", "/v2/media-images/complete", "write", {
    body: schema({ "upload_id!": id, "md5!": md5 }),
  }],
  ["deleteMediaImage", "DELETE", "/v2/media-images/{imageId}/delete", "admin"],
  ["acknowledgeInteraction", "PUT", "/interactions/{interactionId}", "write", {
    body: schema({ "code!": integer(0, 5) }),
  }],
  ["generateShareLink", "POST", "/v2/generate_url_link", "write", {
    body: schema({ callback_data: text(32) }),
    optionalBody: true,
  }],
  ["getMenu", "GET", "/v2/menu", "read"],
  ["updateMenu", "PUT", "/v2/menu", "admin", {
    body: schema({ "menu!": schema({ "items!": array(menuItem, 10, 0) }) }),
  }],
  ["listPanels", "GET", "/v2/panels", "read", {
    query: schema({ "scope!": scope, cursor, limit: integer(1, 50, true) }),
  }],
  ["getPanel", "GET", panelPath, "admin"],
  ["createPanel", "POST", "/v2/panels", "admin", { body: createPanel }],
  ["updatePanel", "PUT", panelPath, "admin", { body: schema({ "panel!": panel }) }],
  ["deletePanel", "DELETE", panelPath, "admin"],
  ["updatePanelTarget", "PUT", `${panelPath}/target`, "admin", {
    body: schema(
      { "op!": addDel, ...targets },
      (data) => present(data, "user_openids", "group_openids") === 1,
    ),
  }],
]);

add("v2-restricted", [
  ["getUserInfo", "GET", `${c2c}/info`, "admin"],
  ["getGroupInfo", "GET", `${group}/info`, "read"],
  ["getGroupBotState", "GET", `${group}/bot_state`, "read"],
  ["listGroupMembers", "GET", `${group}/members`, "admin", { query: schema({ cursor }) }],
  ["getGroupMember", "GET", `${group}/members/{memberOpenid}`, "admin"],
  ["removeGroupMembers", "POST", `${group}/batch_remove_members`, "admin", {
    body: schema({ "member_openids!": targetIds }),
  }],
  ["listGroupJoinRequests", "GET", `${group}/join_request_list`, "admin", { query: page(50) }],
  ["approveGroupJoinRequest", "POST", `${group}/approval_join_request/{memberOpenid}`, "admin", {
    body: schema(
      {
        "op!": choice("approve", "decline"),
        join_request_id: id,
        reject_reason: text(256),
        add_to_member_blacklist: boolean,
      },
      (data) => data.op === "decline" || !present(data, "reject_reason", "add_to_member_blacklist"),
    ),
  }],
  ["listGroupBlacklist", "GET", `${group}/member_blacklist`, "admin", { query: page(100) }],
  ["updateGroupBlacklist", "POST", `${group}/member_blacklist`, "admin", {
    body: schema({ "op!": addDel, "member_openids!": targetIds }),
  }],
  ["getGroupMute", "GET", `${group}/restrict_chat_setting`, "admin"],
  ["setGroupMute", "POST", `${group}/restrict_chat_setting`, "admin", {
    body: schema({ "members!": array(muteState, 20) }),
  }],
  ["listJoinStrategies", "GET", strategy, "admin", { query: page(50) }],
  ["createJoinStrategy", "POST", strategy, "admin", {
    body: schema({ ...groups, ...strategyFields }, oneGroupList),
  }],
  ["updateJoinStrategy", "PATCH", `${strategy}/{strategyId}`, "admin", {
    body: schema({
      ...strategyFields,
      group_action: schema({ "op!": addDel, ...groups }, oneGroupList),
    }, notEmpty),
  }],
  ["deleteJoinStrategy", "DELETE", `${strategy}/{strategyId}`, "admin"],
  ["executeJoinStrategy", "POST", `${strategy}/{strategyId}/execute`, "admin"],
  ["updateJoinStrategyWhitelist", "POST", `${strategy}/{strategyId}/whitelist_users`, "admin", {
    body: schema({
      "op!": addDel,
      "whitelist_users!": array(matches(/^[1-9]\d{0,19}$/, 20), 256),
    }),
  }],
]);

add("gateway", [["getGateway", "GET", "/gateway", "read"], [
  "getGatewayBot",
  "GET",
  "/gateway/bot",
  "read",
]]);

add("guild", [
  ["getGuilds", "GET", "/users/@me/guilds", "read", {
    query: schema(
      { before: id, after: id, limit: integer(1, 100, true) },
      (data) => present(data, "before", "after") <= 1,
    ),
  }],
  ["getGuild", "GET", guild, "read"],
  ["getChannels", "GET", `${guild}/channels`, "read"],
  ["getChannel", "GET", channel, "read"],
  ["getChannelOnlineNums", "GET", `${channel}/online_nums`, "read"],
  ["sendChannelMessage", "POST", `${channel}/messages`, "write", { body: guildMessage }],
  ["getMessageSetting", "GET", `${guild}/message/setting`, "read"],
  ["createDM", "POST", "/users/@me/dms", "write", {
    body: schema({ "recipient_id!": id, "source_guild_id!": id }),
  }],
  ["sendDM", "POST", `${dm}/messages`, "write", { body: guildMessage }],
  ["listReactionUsers", "GET", reaction, "admin", {
    query: schema(
      { cookie: cursor, limit: integer(1, 50, true) },
      (data) => !data.cookie || !data.limit,
    ),
  }],
  ["addReaction", "PUT", reaction, "write"],
  ["removeOwnReaction", "DELETE", reaction, "write"],
  ["getChannelPins", "GET", `${channel}/pins`, "read"],
  ["addChannelPin", "PUT", `${channel}/pins/{messageId}`, "admin"],
  ["removeChannelPin", "DELETE", `${channel}/pins/{messageId}`, "admin"],
  ["clearChannelPins", "DELETE", `${channel}/pins/all`, "admin"],
  ["createGuildAnnounce", "POST", `${guild}/announces`, "admin", {
    body: schema(
      {
        message_id: id,
        channel_id: id,
        announces_type: choice(0, 1),
        recommend_channels: array(schema({ "channel_id!": id, introduce: text(256) }), 3),
      },
      (data) =>
        data.message_id
          ? !!data.channel_id && !data.recommend_channels && !data.announces_type
          : !!data.recommend_channels && !data.channel_id && Object.hasOwn(data, "announces_type"),
    ),
  }],
  ["removeGuildAnnounce", "DELETE", `${guild}/announces/{messageId}`, "admin"],
  ["clearGuildAnnounce", "DELETE", `${guild}/announces/all`, "admin"],
  ["listSchedules", "GET", `${channel}/schedules`, "read", {
    query: schema({ since: uint64Query }),
  }],
  ["getSchedule", "GET", schedule, "read"],
  ["getGuildRoles", "GET", `${guild}/roles`, "read"],
  ["getGuildMember", "GET", member, "admin"],
  ["getGuildApiPermissions", "GET", `${guild}/api_permission`, "admin"],
  ["requestGuildApiPermission", "POST", `${guild}/api_permission/demand`, "admin", {
    body: schema({
      "channel_id!": id,
      "desc!": text(256),
      "api_identify!": schema({
        "method!": choice("GET", "PUT", "POST", "PATCH", "DELETE"),
        "path!": matches(/^\/(?:[a-z_]+|\{[a-z_]+\})(?:\/(?:[a-z_]+|\{[a-z_]+\}))*$/, 256),
      }),
    }),
  }],
]);

const memberRole: Options = {
  body: schema({ channel: schema({ "id!": id }) }),
  optionalBody: true,
  valid: (path, body) => (path.roleId === "5") === !!body.channel,
};
add("guild-restricted", [
  ["recallChannelMessage", "DELETE", channelMessage, "admin", { query: hideTip }],
  ["recallDM", "DELETE", `${dm}/messages/{messageId}`, "write", { query: hideTip }],
  ["createGuildChannel", "POST", `${guild}/channels`, "admin", {
    body: schema({
      ...channelFields,
      "name!": text(256),
      "type!": choice(0, 2, 4, 10005, 10006, 10007),
      "position!": uint32,
      sub_type: integer(0, 3),
      private_user_ids: targetIds,
      application_id: id,
    }, (data) =>
      (data.type !== 4 || Number(data.position) >= 2) &&
      (data.type === 10006) === !!data.application_id &&
      (!data.private_user_ids || data.private_type === 2)),
  }],
  ["updateChannel", "PATCH", channel, "admin", {
    body: schema({ name: text(256), position: uint32, ...channelFields }, notEmpty),
  }],
  ["deleteChannel", "DELETE", channel, "admin"],
  ["getGuildMembers", "GET", `${guild}/members`, "admin", {
    query: schema({ after: id, limit: integer(1, 400, true) }),
  }],
  ["getGuildRoleMembers", "GET", `${role}/members`, "admin", {
    query: schema({ start_index: id, limit: integer(1, 400, true) }),
  }],
  ["removeGuildMember", "DELETE", member, "admin", {
    body: schema({ add_blacklist: boolean, delete_history_msg_days: choice(-1, 0, 3, 7, 15, 30) }),
    optionalBody: true,
  }],
  ["createGuildRole", "POST", `${guild}/roles`, "admin", { body: roleFields }],
  ["updateGuildRole", "PATCH", role, "admin", { body: roleFields }],
  ["deleteGuildRole", "DELETE", role, "admin"],
  ["addGuildMemberRole", "PUT", `${member}/roles/{roleId}`, "admin", memberRole],
  ["removeGuildMemberRole", "DELETE", `${member}/roles/{roleId}`, "admin", memberRole],
  ["getChannelMemberPermissions", "GET", memberPermissions, "admin"],
  ["getChannelRolePermissions", "GET", rolePermissions, "admin"],
  ["setChannelMemberPermissions", "PUT", memberPermissions, "admin", { body: permissions }],
  ["setChannelRolePermissions", "PUT", rolePermissions, "admin", { body: permissions }],
  ["muteGuild", "PATCH", `${guild}/mute`, "admin", { body: schema(muteWindow, oneWindow) }],
  ["muteGuildMembers", "PATCH", `${guild}/mute`, "admin", {
    body: schema({ ...muteWindow, "user_ids!": array(id, 256) }, oneWindow),
  }],
  ["muteGuildMember", "PATCH", `${member}/mute`, "admin", { body: schema(muteWindow, oneWindow) }],
  ["createSchedule", "POST", `${channel}/schedules`, "admin", {
    body: schema({
      "schedule!": schema({
        ...scheduleFields,
        "name!": text(256),
        "start_timestamp!": digits,
        "end_timestamp!": digits,
      }, scheduleOrder),
    }),
  }],
  ["updateSchedule", "PATCH", schedule, "admin", {
    body: schema({
      "schedule!": schema({
        name: text(256),
        start_timestamp: digits,
        end_timestamp: digits,
        ...scheduleFields,
      }, scheduleOrder),
    }),
  }],
  ["deleteSchedule", "DELETE", schedule, "admin"],
  ["listThreads", "GET", `${channel}/threads`, "read"],
  ["getThread", "GET", thread, "read"],
  ["createThread", "PUT", `${channel}/threads`, "write", {
    body: schema({ "title!": text(256), "content!": text(16384), "format!": choice(1) }),
  }],
  ["deleteThread", "DELETE", thread, "admin"],
  ["controlChannelAudio", "POST", `${channel}/audio`, "write", {
    body: schema(
      { "status!": integer(0, 3), audio_url: assetUrl, text: text(256) },
      (data) => data.status === 0 ? !!data.audio_url : !present(data, "audio_url", "text"),
    ),
  }],
  ["joinChannelMic", "PUT", `${channel}/mic`, "write"],
  ["leaveChannelMic", "DELETE", `${channel}/mic`, "write"],
]);

add("sdk", [
  ["getChannelMessage", "GET", channelMessage, "admin"],
  ["listChannelMessages", "GET", `${channel}/messages`, "admin", {
    query: schema(
      { before: id, after: id, around: id, limit: integer(1, 20, true) },
      (data) => present(data, "before", "after", "around") <= 1,
    ),
  }],
  ["editChannelMessage", "PATCH", channelMessage, "admin", {
    body: schema({ "content!": text(16384) }),
  }],
  ["listVoiceChannelMembers", "GET", `${channel}/voice/members`, "admin"],
  ["sendChannelSettingGuide", "POST", `${channel}/settingguide`, "admin", {
    body: schema({ content: matches(/^(?:<@\d{1,20}>){1,20}$/, 460) }),
    optionalBody: true,
  }],
  ["sendDMSettingGuide", "POST", `${dm}/settingguide`, "admin", {
    body: schema({ "setting_guide!": schema({ "guild_id!": id }) }),
  }],
]);

export const platformCatalog: readonly PlatformOperation[] = Object.freeze(
  [...definitions.values()].map(({ entry }) => entry)
    .sort((left, right) => left.name < right.name ? -1 : left.name > right.name ? 1 : 0),
);

export function lookup(operation: string): PlatformOperation {
  return definitions.get(operation)?.entry ?? invalid("operation");
}

export function listPlatformOperations(offset = 0, limit = 25) {
  if (
    !Number.isSafeInteger(offset) || offset < 0 || offset > platformCatalog.length ||
    !Number.isSafeInteger(limit) || limit < 1 || limit > 100
  ) invalid("pagination");
  const items = platformCatalog.slice(offset, offset + limit);
  const end = offset + items.length;
  return {
    items,
    nextOffset: end < platformCatalog.length ? end : null,
    total: platformCatalog.length,
  };
}

function segment(key: string, path: Record<string, unknown>): string {
  const value = path[key];
  if (typeof value !== "string" || value.length > 256) invalid(key);
  if (key === "emojiType") choice("1", "2")(value, key);
  else if (key === "emojiId") {
    if (!/^\d{1,10}$/.test(value) && !(path.emojiType === "2" && /^\p{RGI_Emoji}$/v.test(value))) {
      invalid(key);
    }
  } else {
    id(value, key);
    if (
      (key === "messageId" && value.toLowerCase() === "all") ||
      (key === "groupOpenid" && value.toLowerCase() === "join_approval_strategy")
    ) invalid(key);
  }
  return encodeURIComponent(value);
}

export function build(operation: string, params: unknown = {}): { path: string; body?: Data } {
  const { entry, options } = definitions.get(operation) ?? invalid("operation");
  const parts = fields(params, ["path", "query", "body"], "params");
  const path = fields(Object.hasOwn(parts, "path") ? parts.path : {}, entry.pathParams, "path");
  const route = entry.path.replace(/\{(\w+)\}/g, (_, key: string) => segment(key, path));
  const query = (options.query ?? empty)(Object.hasOwn(parts, "query") ? parts.query : {}, "query");
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(query)) search.append(key, String(value));
  let body: Data | undefined;
  if (Object.hasOwn(parts, "body")) {
    if (!options.body) invalid("body");
    body = options.body(parts.body, "body");
    if (new TextEncoder().encode(JSON.stringify(body)).length > 65_536) invalid("body");
  } else if (options.body && !options.optionalBody) invalid("body");
  if (options.valid && !options.valid(path, body ?? {})) invalid("body");
  return {
    path: search.size ? `${route}?${search}` : route,
    ...(body === undefined ? {} : { body }),
  };
}

export function resolvePlatformOperation(operation: string, params: unknown = {}) {
  return { ...lookup(operation), ...build(operation, params) };
}
