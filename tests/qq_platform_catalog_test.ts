import assert from "node:assert/strict";
import {
  build,
  listPlatformOperations,
  lookup,
  platformCatalog,
  type PlatformMethod,
  resolvePlatformOperation,
} from "../adapters/qq/platform-catalog.ts";
import { createQqPlatformOperator } from "../adapters/qq/platform-client.ts";

const user = "/v2/users/{userOpenid}";
const group = "/v2/groups/{groupOpenid}";
const strategy = "/v2/groups/join_approval_strategy";
const guild = "/guilds/{guildId}";
const channel = "/channels/{channelId}";
const dm = "/dms/{dmGuildId}";
const member = `${guild}/members/{userId}`;
const role = `${guild}/roles/{roleId}`;
const message = `${channel}/messages/{messageId}`;
const reaction = `${message}/reactions/{emojiType}/{emojiId}`;
const panel = "/v2/panels/{panelId}";
const ids: Record<string, string> = {
  userOpenid: "u-01",
  groupOpenid: "g-02",
  memberOpenid: "m-03",
  guildId: "91",
  channelId: "92",
  userId: "93",
  dmGuildId: "dm-94",
  roleId: "5",
  scheduleId: "s9",
  threadId: "t-4",
  strategyId: "s-7",
  panelId: "p+007=",
  messageId: "ROBOT1.0_m",
  imageId: "i_8",
  interactionId: "raw-id",
  emojiType: "2",
  emojiId: "☕",
};
const md5 = "a".repeat(32);
const sha1 = "b".repeat(40);
const asset = "https://example-123456.cos.ap-guangzhou.myqcloud.com/picture.png";
const prepare = { file_type: 1, file_name: "图.png", file_size: "120", md5, sha1, md5_10m: md5 };
const part = { upload_id: "upload-1", part_index: 0, block_size: "120", md5 };
const panelConfig = { items: [{ name: "运行", type: "command", only_admin: false }] };
const scopeChannel = { channel: { id: "92" } };
const reply = { msg_type: 0, content: "hello", msg_seq: 1, msg_id: "ROBOT1.0_reply" };
const scopeQuery = { scope: "c2c", limit: 10 };

type Fixture = readonly [string, PlatformMethod, string, Record<string, unknown>?];
const fixtures: Fixture[] = [
  ["getMe", "GET", "/users/@me"],
  ["getGateway", "GET", "/gateway"],
  ["getGatewayBot", "GET", "/gateway/bot"],
  ["getUserInfo", "GET", `${user}/info`],
  ["getGroupInfo", "GET", `${group}/info`],
  ["getGroupBotState", "GET", `${group}/bot_state`],
  ["sendC2CMessage", "POST", `${user}/messages`, reply],
  ["sendGroupMessage", "POST", `${group}/messages`, {
    msg_type: 7,
    media: { file_info: "abc=" },
    content: "caption",
  }],
  ["sendC2CStreamMessage", "POST", `${user}/stream_messages`, {
    input_state: 1,
    index: 0,
    content_type: "markdown",
    content_raw: "**hello**",
  }],
  ["recallC2CMessage", "DELETE", `${user}/messages/{messageId}`],
  ["recallGroupMessage", "DELETE", `${group}/messages/{messageId}`],
  ["uploadC2CFile", "POST", `${user}/files`, { file_type: 1, url: asset, srv_send_msg: false }],
  ["uploadGroupFile", "POST", `${group}/files`, { upload_id: "upload-1" }],
  ["prepareC2CUpload", "POST", `${user}/upload_prepare`, prepare],
  ["prepareGroupUpload", "POST", `${group}/upload_prepare`, { ...prepare, file_size: 120 }],
  ["finishC2CUploadPart", "POST", `${user}/upload_part_finish`, part],
  ["finishGroupUploadPart", "POST", `${group}/upload_part_finish`, {
    ...part,
    part_index: 1,
    block_size: 120,
  }],
  ["listMediaImages", "GET", "/v2/media-images"],
  ["prepareMediaImage", "POST", "/v2/media-images/prepare", {
    file_name: "图.jpg",
    file_size: 21,
    content_type: "image/jpeg",
    md5,
    sha1,
  }],
  ["completeMediaImage", "POST", "/v2/media-images/complete", { upload_id: "123", md5 }],
  ["deleteMediaImage", "DELETE", "/v2/media-images/{imageId}/delete"],
  ["acknowledgeInteraction", "PUT", "/interactions/{interactionId}", { code: 0 }],
  ["generateShareLink", "POST", "/v2/generate_url_link", { callback_data: "invite" }],
  ["getMenu", "GET", "/v2/menu"],
  ["updateMenu", "PUT", "/v2/menu", {
    menu: {
      items: [{
        type: "menu",
        name: "Tools",
        sub_menu_items: [{ type: "send_message", name: "Go", send_message: "/go" }],
      }],
    },
  }],
  ["listPanels", "GET", "/v2/panels"],
  ["getPanel", "GET", panel],
  ["createPanel", "POST", "/v2/panels", {
    scope: "group",
    target_type: "specific",
    group_openids: ["g"],
    panel: panelConfig,
  }],
  ["updatePanel", "PUT", panel, { panel: panelConfig }],
  ["deletePanel", "DELETE", panel],
  ["updatePanelTarget", "PUT", `${panel}/target`, { op: "add", user_openids: ["user-2"] }],
  ["listGroupMembers", "GET", `${group}/members`],
  ["getGroupMember", "GET", `${group}/members/{memberOpenid}`],
  ["removeGroupMembers", "POST", `${group}/batch_remove_members`, { member_openids: ["user-1"] }],
  ["listGroupJoinRequests", "GET", `${group}/join_request_list`],
  ["approveGroupJoinRequest", "POST", `${group}/approval_join_request/{memberOpenid}`, {
    op: "decline",
    add_to_member_blacklist: false,
  }],
  ["listGroupBlacklist", "GET", `${group}/member_blacklist`],
  ["updateGroupBlacklist", "POST", `${group}/member_blacklist`, {
    op: "del",
    member_openids: ["user-1"],
  }],
  ["getGroupMute", "GET", `${group}/restrict_chat_setting`],
  ["setGroupMute", "POST", `${group}/restrict_chat_setting`, {
    members: [{ op: "del", member_openid: "user-2", mute_expire_at: "" }],
  }],
  ["listJoinStrategies", "GET", strategy],
  ["createJoinStrategy", "POST", strategy, { group_ids: [123], is_enable: "off" }],
  ["updateJoinStrategy", "PATCH", `${strategy}/{strategyId}`, {
    group_action: { op: "add", group_openids: ["g"] },
  }],
  ["deleteJoinStrategy", "DELETE", `${strategy}/{strategyId}`],
  ["executeJoinStrategy", "POST", `${strategy}/{strategyId}/execute`],
  ["updateJoinStrategyWhitelist", "POST", `${strategy}/{strategyId}/whitelist_users`, {
    op: "add",
    whitelist_users: ["123"],
  }],
  ["getGuilds", "GET", "/users/@me/guilds"],
  ["getGuild", "GET", guild],
  ["getChannels", "GET", `${guild}/channels`],
  ["getChannel", "GET", channel],
  ["getChannelOnlineNums", "GET", `${channel}/online_nums`],
  ["createGuildChannel", "POST", `${guild}/channels`, { name: "talk", type: 0, position: 2 }],
  ["updateChannel", "PATCH", channel, { private_type: 0, name: "renamed" }],
  ["deleteChannel", "DELETE", channel],
  ["sendChannelMessage", "POST", `${channel}/messages`, { content: "hello" }],
  ["recallChannelMessage", "DELETE", message],
  ["getMessageSetting", "GET", `${guild}/message/setting`],
  ["createDM", "POST", "/users/@me/dms", { recipient_id: "user-1", source_guild_id: "91" }],
  ["sendDM", "POST", `${dm}/messages`, { embed: { title: "Info", fields: [{ name: "Field" }] } }],
  ["recallDM", "DELETE", `${dm}/messages/{messageId}`],
  ["listReactionUsers", "GET", reaction],
  ["addReaction", "PUT", reaction],
  ["removeOwnReaction", "DELETE", reaction],
  ["getChannelPins", "GET", `${channel}/pins`],
  ["addChannelPin", "PUT", `${channel}/pins/{messageId}`],
  ["removeChannelPin", "DELETE", `${channel}/pins/{messageId}`],
  ["clearChannelPins", "DELETE", `${channel}/pins/all`],
  ["createGuildAnnounce", "POST", `${guild}/announces`, { message_id: "msg-1", channel_id: "92" }],
  ["removeGuildAnnounce", "DELETE", `${guild}/announces/{messageId}`],
  ["clearGuildAnnounce", "DELETE", `${guild}/announces/all`],
  ["listSchedules", "GET", `${channel}/schedules`],
  ["getSchedule", "GET", `${channel}/schedules/{scheduleId}`],
  ["createSchedule", "POST", `${channel}/schedules`, {
    schedule: { name: "meeting", start_timestamp: "1700000000000", end_timestamp: "1700000100000" },
  }],
  ["updateSchedule", "PATCH", `${channel}/schedules/{scheduleId}`, {
    schedule: { remind_type: "1" },
  }],
  ["deleteSchedule", "DELETE", `${channel}/schedules/{scheduleId}`],
  ["getGuildRoles", "GET", `${guild}/roles`],
  ["getGuildMembers", "GET", `${guild}/members`],
  ["getGuildMember", "GET", member],
  ["getGuildRoleMembers", "GET", `${role}/members`],
  ["removeGuildMember", "DELETE", member, { add_blacklist: true, delete_history_msg_days: 3 }],
  ["createGuildRole", "POST", `${guild}/roles`, { name: "role", hoist: 0 }],
  ["updateGuildRole", "PATCH", role, { color: 4294967295 }],
  ["deleteGuildRole", "DELETE", role],
  ["addGuildMemberRole", "PUT", `${member}/roles/{roleId}`, scopeChannel],
  ["removeGuildMemberRole", "DELETE", `${member}/roles/{roleId}`, scopeChannel],
  ["getChannelMemberPermissions", "GET", `${channel}/members/{userId}/permissions`],
  ["getChannelRolePermissions", "GET", `${channel}/roles/{roleId}/permissions`],
  ["setChannelMemberPermissions", "PUT", `${channel}/members/{userId}/permissions`, { add: "1" }],
  ["setChannelRolePermissions", "PUT", `${channel}/roles/{roleId}/permissions`, { remove: "4" }],
  ["getGuildApiPermissions", "GET", `${guild}/api_permission`],
  ["requestGuildApiPermission", "POST", `${guild}/api_permission/demand`, {
    channel_id: "92",
    api_identify: { method: "GET", path: "/guilds/{guild_id}" },
    desc: "display guild",
  }],
  ["muteGuild", "PATCH", `${guild}/mute`, { mute_seconds: "0" }],
  ["muteGuildMembers", "PATCH", `${guild}/mute`, { mute_seconds: "120", user_ids: ["93"] }],
  ["muteGuildMember", "PATCH", `${member}/mute`, { mute_end_timestamp: "0" }],
  ["listThreads", "GET", `${channel}/threads`],
  ["getThread", "GET", `${channel}/threads/{threadId}`],
  ["createThread", "PUT", `${channel}/threads`, {
    title: "hello",
    content: "plain text",
    format: 1,
  }],
  ["deleteThread", "DELETE", `${channel}/threads/{threadId}`],
  ["controlChannelAudio", "POST", `${channel}/audio`, { status: 1 }],
  ["joinChannelMic", "PUT", `${channel}/mic`],
  ["leaveChannelMic", "DELETE", `${channel}/mic`],
  ["getChannelMessage", "GET", message],
  ["listChannelMessages", "GET", `${channel}/messages`],
  ["editChannelMessage", "PATCH", message, { content: "corrected" }],
  ["listVoiceChannelMembers", "GET", `${channel}/voice/members`],
  ["sendChannelSettingGuide", "POST", `${channel}/settingguide`, { content: "<@123>" }],
  ["sendDMSettingGuide", "POST", `${dm}/settingguide`, { setting_guide: { guild_id: "91" } }],
];

function fixtureParams([name, , template, body]: Fixture) {
  const path = Object.fromEntries(
    Array.from(template.matchAll(/\{(\w+)\}/g), ([, key]) => [key, ids[key]]),
  );
  const query = name === "listPanels" ? scopeQuery : name === "recallDM" ? { hidetip: false } : {};
  return { path, query, ...(body === undefined ? {} : { body }) };
}

function expectedPath(name: string, template: string) {
  return template.replace(/\{(\w+)\}/g, (_, key: string) => encodeURIComponent(ids[key])) +
    (name === "listPanels" ? "?scope=c2c&limit=10" : name === "recallDM" ? "?hidetip=false" : "");
}

Deno.test("real catalog: independent wire fixtures cover every operation and its required fields", () => {
  assert.deepEqual(fixtures.map(([name]) => name).sort(), platformCatalog.map(({ name }) => name));
  for (const fixture of fixtures) {
    const [name, method, template, body] = fixture;
    const params = fixtureParams(fixture);
    const entry = resolvePlatformOperation(name, params);
    assert.equal(entry.method, method, name);
    assert.equal(entry.path, expectedPath(name, template), name);
    assert.deepEqual(entry.body, body, name);
    assert.equal(entry.retryable, method === "GET", name);
    assert.equal(new Set(entry.bodyParams).size, entry.bodyParams.length, name);
    assert.ok(method === "GET" || entry.access !== "read", name);
    for (
      const [part, required] of [["path", entry.pathParams], ["query", entry.requiredQuery], [
        "body",
        entry.requiredBody,
      ]] as const
    ) {
      for (const key of required) {
        const fields: Record<string, unknown> = { ...params[part] };
        delete fields[key];
        assert.throws(
          () => build(name, { ...params, [part]: fields }),
          TypeError,
          `${name} requires ${key}`,
        );
      }
    }
    for (const part of ["path", "query", "body"] as const) {
      assert.throws(
        () => build(name, { ...params, [part]: { ...params[part], authorization: "no" } }),
        TypeError,
        name,
      );
    }
  }
});

Deno.test("real catalog: the operator dispatches each validated request unchanged to an in-memory transport", async () => {
  const directory = await Deno.makeTempDir({ prefix: "qq-catalog-" });
  const savedUid = Deno.uid;
  try {
    if (Deno.permissions.querySync({ name: "sys", kind: "uid" }).state !== "granted") {
      const { uid } = await Deno.stat(directory);
      Deno.uid = () => uid;
    }
    const calls: unknown[][] = [];
    const operator = createQqPlatformOperator({
      access: "admin",
      directory,
      request: (method, path, body) => {
        calls.push([method, path, body]);
        return Promise.resolve({ httpStatus: 200, data: { id: "fixture-id" } });
      },
    });
    for (const fixture of fixtures) {
      const [operation, method, template, body] = fixture;
      const result = await operator({
        operation,
        params: fixtureParams(fixture),
        ...(method === "GET" ? {} : { idempotencyKey: operation }),
      }, () => true);
      assert.equal(result.status, "ok", `${operation}: ${JSON.stringify(result)}`);
      assert.deepEqual(calls.at(-1), [method, expectedPath(operation, template), body]);
    }
    assert.equal(calls.length, fixtures.length);
    const denied = await operator({
      operation: "sendC2CMessage",
      params: { path: { userOpenid: "abc" }, body: { content: 7 } },
      idempotencyKey: "bad-body",
    }, () => true);
    assert.deepEqual(denied, { status: "failed", reason: "invalid_params" });
    assert.equal(calls.length, fixtures.length);
  } finally {
    Deno.uid = savedUid;
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("real catalog: stable paged metadata does not offer retired or unsubstantiated routes", () => {
  const seen: string[] = [];
  let offset: number | null = 0;
  while (offset !== null) {
    const { items, total, nextOffset } = listPlatformOperations(offset, 11);
    assert.equal(total, fixtures.length);
    seen.push(...items.map((entry) => entry.name));
    offset = nextOffset;
  }
  assert.deepEqual(seen, fixtures.map(([name]) => name).sort());
  assert.deepEqual(listPlatformOperations(seen.length).items, []);
  for (const [offset, limit] of [[-1, 1], [0, 0], [0, 101], [1.5, 1], [seen.length + 1, 1]]) {
    assert.throws(() => listPlatformOperations(offset, limit), TypeError);
  }
  for (const entry of platformCatalog) {
    assert.ok(Object.isFrozen(entry));
    for (
      const keys of [
        entry.queryParams,
        entry.requiredQuery,
        entry.pathParams,
        entry.requiredBody,
        entry.bodyParams,
      ]
    ) assert.ok(Object.isFrozen(keys));
  }
  for (
    const name of [
      "createChannelAnnounce",
      "deleteChannelAnnounce",
      "getGroupHistory",
      "sendGroupStreamMessage",
      "createWebhookSession",
      "getAppAccessToken",
      "uploadToPresignedUrl",
      "muteGuildOrMembers",
      "constructor",
      "GET /users/@me",
    ]
  ) assert.throws(() => build(name), TypeError);
  assert.equal(lookup("listGroupMembers").surface, "v2-restricted");
  assert.equal(lookup("recallDM").surface, "guild-restricted");
  assert.equal(lookup("listChannelMessages").surface, "sdk");
});

Deno.test("real catalog: query wire types, ID namespaces, canonical encoding and bounds", () => {
  const path = { guildId: "g" };
  assert.equal(
    build("getGuildMembers", { path, query: { limit: 400 } }).path,
    "/guilds/g/members?limit=400",
  );
  for (const limit of [0, 401, "01", "1.0", true, Infinity, [1], { value: 1 }]) {
    assert.throws(() => build("getGuildMembers", { path, query: { limit } }), TypeError);
  }
  assert.equal(
    build("listSchedules", { path: { channelId: "c" }, query: { since: "18446744073709551615" } })
      .path,
    "/channels/c/schedules?since=18446744073709551615",
  );
  assert.throws(
    () =>
      build("listSchedules", {
        path: { channelId: "c" },
        query: { since: "18446744073709551616" },
      }),
    TypeError,
  );
  assert.equal(
    build("listPanels", { query: { limit: "50", cursor: "a&b+=", scope: "dm" } }).path,
    "/v2/panels?scope=dm&cursor=a%26b%2B%3D&limit=50",
  );
  assert.equal(
    build("listGroupMembers", { path: { groupOpenid: "g" }, query: { cursor: "" } }).path,
    "/v2/groups/g/members?cursor=",
  );
  for (
    const query of [{}, { scope: "all" }, { scope: "group", limit: 51 }, {
      scope: "dm",
      cursor: "a\nb",
    }]
  ) assert.throws(() => build("listPanels", { query }), TypeError);
  const messagePath = { channelId: "c", messageId: "m" };
  for (const hidetip of [true, false, "true", "false"]) {
    assert.equal(
      build("recallChannelMessage", { path: messagePath, query: { hidetip } }).path,
      `/channels/c/messages/m?hidetip=${hidetip}`,
    );
  }
  for (const hidetip of [1, "False", {}]) {
    assert.throws(
      () => build("recallChannelMessage", { path: messagePath, query: { hidetip } }),
      TypeError,
    );
  }
  for (
    const invalidId of [
      ".",
      "..",
      "a/b",
      "%2F",
      "//example.com",
      "a\\b",
      "a?b",
      "a#b",
      "all",
      "ALL",
      "x".repeat(257),
    ]
  ) {
    assert.throws(
      () => build("removeChannelPin", { path: { ...messagePath, messageId: invalidId } }),
      TypeError,
    );
  }
  assert.throws(
    () => build("getGroupInfo", { path: { groupOpenid: "join_approval_strategy" } }),
    TypeError,
  );
  assert.throws(
    () => build("sendDM", { path: { userOpenid: "u" }, body: { content: "test" } }),
    TypeError,
  );
  assert.throws(
    () => build("addReaction", { path: { ...messagePath, emojiType: "1", emojiId: "☕" } }),
    TypeError,
  );
  assert.throws(
    () =>
      build("listReactionUsers", {
        path: { ...messagePath, emojiType: "2", emojiId: "9749" },
        query: { limit: 51 },
      }),
    TypeError,
  );
});

function bodyFor(name: string, body: unknown) {
  const fixture = fixtures.find(([operation]) => operation === name)!;
  return build(name, { ...fixtureParams(fixture), body }).body;
}

Deno.test("real catalog: payload variants, field requirements, unsafe combinations and numeric bounds", () => {
  for (
    const [operation, body] of [
      ["sendC2CMessage", { msg_type: 6, input_notify: { input_type: 1, input_second: 60 } }],
      ["sendGroupMessage", {
        msg_type: 8,
        card: {
          type: "tuwen",
          content: {
            title: "title",
            description: "details",
            url: "https://example.com/page",
            pic_url: asset,
          },
        },
      }],
      ["sendGroupMessage", {
        msg_type: 2,
        markdown: {
          content: "**sample** ![图](qqbot-media://image/v1/test-1)",
          force_verify_image_resource: true,
        },
        keyboard: { id: "3" },
      }],
      ["uploadC2CFile", {
        file_type: 4,
        file_name: "note.txt",
        file_data: "dGVzdA==",
        srv_send_msg: false,
      }],
      ["uploadGroupFile", {
        upload_id: "up",
        file_type: 1,
        file_name: "img.png",
        srv_send_msg: false,
      }],
      ["sendC2CStreamMessage", {
        input_mode: "append",
        index: 2,
        input_state: 10,
        stream_msg_id: "previous",
        content_type: "text",
        content_raw: "",
      }],
      ["createGuildAnnounce", { announces_type: 1, recommend_channels: [{ channel_id: "c" }] }],
    ] as const
  ) assert.deepEqual(bodyFor(operation, body), body);

  for (
    const [operation, body] of [
      ["sendC2CMessage", {}],
      ["sendC2CMessage", { content: "yes", msg_id: "m", event_id: "e" }],
      ["sendC2CMessage", { content: "yes", is_wakeup: true, msg_id: "m" }],
      ["sendC2CMessage", { content: 5 }],
      ["sendC2CMessage", { content: "yes", msg_seq: 1.5 }],
      ["sendC2CMessage", { msg_type: 2, content: "yes", markdown: { content: "yes" } }],
      ["sendC2CMessage", { msg_type: 6, input_notify: { input_type: 1, input_second: 61 } }],
      ["sendGroupMessage", { msg_type: 6, input_notify: { input_type: 1, input_second: 2 } }],
      ["sendGroupMessage", { msg_type: 7, media: { file_info: "abc", token: "not-permitted" } }],
      ["sendC2CStreamMessage", {
        content_type: "text",
        content_raw: "ok",
        index: 1,
        input_state: 1,
      }],
      ["prepareGroupUpload", { ...prepare, md5: "bad" }],
      ["finishC2CUploadPart", { ...part, block_size: 0 }],
      ["prepareMediaImage", {
        file_name: "png.jpg",
        file_size: 3,
        content_type: "image/png",
        md5,
        sha1,
      }],
      ["uploadGroupFile", { file_type: 4, file_data: "a_b=", file_name: "note.txt" }],
      ["uploadGroupFile", { file_type: 1, upload_id: "up", url: asset }],
      ["uploadC2CFile", { file_type: 2, url: asset, srv_send_msg: "false" }],
      ["acknowledgeInteraction", { code: 6 }],
      ["acknowledgeInteraction", { code: "0" }],
      ["generateShareLink", { callback_data: "a".repeat(33) }],
      ["removeGroupMembers", { member_openids: Array(21).fill("member") }],
      ["updateGroupBlacklist", { op: "delete", member_openids: ["m"] }],
      ["setGroupMute", { members: [{ op: "add", member_openid: "m" }] }],
      ["approveGroupJoinRequest", { op: "approve", add_to_member_blacklist: true }],
      ["createJoinStrategy", { group_ids: [4], group_openids: ["g"] }],
      ["createJoinStrategy", { group_ids: [Number.MAX_SAFE_INTEGER + 1] }],
      ["createPanel", {
        scope: "dm",
        target_type: "specific",
        user_openids: ["u"],
        panel: panelConfig,
      }],
      ["updatePanelTarget", { op: "add", user_openids: [], group_openids: ["g"] }],
      ["updateMenu", { menu: { items: Array(11).fill({ name: "a" }) } }],
      ["createGuildChannel", { name: "group", type: 4, position: 1 }],
      ["removeGuildMember", { delete_history_msg_days: 2 }],
      ["removeGuildMemberRole", {}],
      ["setChannelMemberPermissions", { add: "2" }],
      ["createGuildRole", {}],
      ["muteGuildMembers", { mute_seconds: "10" }],
      ["muteGuild", { mute_seconds: "10", user_ids: [] }],
      ["muteGuild", { mute_seconds: 10 }],
      ["updateSchedule", { schedule: { id: "change-target" } }],
      ["createGuildAnnounce", { message_id: "m" }],
      ["controlChannelAudio", { status: 0 }],
      ["sendDMSettingGuide", { content: "wrong legacy shape" }],
    ] as const
  ) assert.throws(() => bodyFor(operation, body), TypeError, operation);
  for (
    const operation of [
      "sendC2CMessage",
      "updateMenu",
      "setGroupMute",
      "createJoinStrategy",
      "updateChannel",
      "muteGuildMembers",
    ]
  ) {
    const fixture = fixtures.find(([name]) => name === operation)!;
    assert.throws(
      () => build(operation, { path: fixtureParams(fixture).path }),
      TypeError,
      operation,
    );
  }
  assert.deepEqual(build("generateShareLink"), { path: "/v2/generate_url_link" });
});

Deno.test("real catalog: no local files, private/signed URLs, escape hatches or implicit object conversions", () => {
  for (
    const url of [
      "/etc/passwd",
      "C:\\private\\a",
      "file:///root/image.jpg",
      "data:image/png,secret",
      "http://gchat.qpic.cn/a",
      "https://localhost/x",
      "https://169.254.169.254/a",
      "https://[::1]/a",
      "https://0x7f000001/a",
      "https://cdn.home.arpa/a",
      "https://gchat.qpic.cn@outside.example/a",
      "https://name:secret@gchat.qpic.cn/a",
      "https://gchat.qpic.cn.evil.com/a",
      "https://gchat.qpic.cn:8443/a",
      "https://gchat.qpic.cn/a?token=secret",
      "https://gchat.qpic.cn/a#secret",
      "https://gchat.qpic.cn/a/../etc/passwd",
      "https://gchat.qpic.cn/%2e%2e/a",
      "https://untrusted.example/a",
    ]
  ) {
    assert.throws(() => bodyFor("uploadGroupFile", { file_type: 1, url }), TypeError);
    assert.throws(() => bodyFor("sendChannelMessage", { image: url }), TypeError);
    assert.throws(
      () => bodyFor("sendC2CMessage", { msg_type: 2, markdown: { content: `![alt](${url})` } }),
      TypeError,
    );
    assert.throws(() => bodyFor("controlChannelAudio", { status: 0, audio_url: url }), TypeError);
  }
  for (
    const file_name of [
      "/root/a.jpg",
      "..",
      "../a.jpg",
      "C:\\user\\a.jpg",
      "file:///a.jpg",
      "name\n.jpg",
    ]
  ) assert.throws(() => bodyFor("prepareC2CUpload", { ...prepare, file_name }), TypeError);
  for (
    const content of [
      "<img src='http://localhost/'>",
      "![x][hidden]\n[hidden]: file:///secret",
      "![x](&#x66;ile:///secret)",
    ]
  ) {
    assert.throws(
      () => bodyFor("sendGroupMessage", { msg_type: 2, markdown: { content } }),
      TypeError,
    );
  }
  let getterCalls = 0;
  const getter = {
    get content() {
      getterCalls++;
      return "not JSON";
    },
  };
  const sparse = new Array(1);
  for (
    const object of [
      getter,
      [],
      new Date(),
      { [Symbol()]: 2 },
      JSON.parse('{"__proto__":{"content":"no"}}'),
      Object.defineProperty({}, "content", { value: "hidden" }),
    ]
  ) assert.throws(() => bodyFor("sendC2CMessage", object), TypeError);
  assert.equal(getterCalls, 0);
  assert.throws(() => bodyFor("removeGroupMembers", { member_openids: sparse }), TypeError);
  assert.throws(() => build("getMe", { url: "https://outside.example/" }), TypeError);
  assert.throws(() => build("getMe", { body: {} }), TypeError);
  assert.throws(() => build("getMe", { query: null }), TypeError);
  const media = { file_info: "reference" };
  const result = bodyFor("sendC2CMessage", { msg_type: 7, media });
  media.file_info = "modified";
  assert.deepEqual(result?.media, { file_info: "reference" });
});
