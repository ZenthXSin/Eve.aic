// @satorijs/adapter-qq 4.14.1, MIT; extracted SDK internal request core and complete route tables.
// Only adaptation: inline makeArray/name helpers; ESM exports. See upstream.json.
// src/internal/internal.ts
const makeArray = (value) => value == null ? [] : Array.isArray(value) ? value : [value];
const __name = (value, name) =>
  Object.defineProperty(value, "name", { value: name, configurable: true });
var Internal = class {
  constructor(bot, http) {
    this.bot = bot;
    this.http = http;
  }
  bot;
  http;
  static {
    __name(this, "Internal");
  }
  static define(isGuild, routes, preset) {
    for (const path in routes) {
      for (const key in routes[path]) {
        const method = key;
        for (const name of makeArray(routes[path][method])) {
          (isGuild ? GuildInternal : GroupInternal).prototype[name] = async function (...args) {
            const raw = args.join(", ");
            const url = path.replace(/\{([^}]+)\}/g, () => {
              if (!args.length) throw new Error(`too few arguments for ${path}, received ${raw}`);
              return args.shift();
            });
            const config = { ...preset };
            if (args.length === 1) {
              if (method === "GET" || method === "DELETE") {
                config.params = args[0];
              } else {
                config.data = args[0];
              }
            } else if (args.length === 2 && method !== "GET" && method !== "DELETE") {
              config.data = args[0];
              config.params = args[1];
            } else if (args.length > 1) {
              throw new Error(`too many arguments for ${path}, received ${raw}`);
            }
            const http = this.http();
            try {
              this.bot.logger.debug(`${method} ${url} request: %o`, config);
              const response = await http(url, { ...config, method });
              this.bot.logger.debug(
                `${method} ${url} response: %o, trace id: %s`,
                response.data,
                response.headers.get("x-tps-trace-id"),
              );
              return response.data;
            } catch (error) {
              if (!http.isError(error) || !error.response) throw error;
              this.bot.logger.debug(
                `${method} ${url} response: %o, trace id: %s`,
                error.response.data,
                error.response.headers.get("x-tps-trace-id"),
              );
              throw error;
            }
          };
        }
      }
    }
  }
};
var GroupInternal = class extends Internal {
  static {
    __name(this, "GroupInternal");
  }
};
var GuildInternal = class extends Internal {
  static {
    __name(this, "GuildInternal");
  }
};

// src/internal/group.ts
GroupInternal.define(false, {
  "/v2/groups/{channel.id}/info": {
    GET: "getGuildInfo",
  },
  "/v2/groups/{channel.id}/bot_state": {
    GET: "getGuildBotState",
  },
  "/v2/groups/{channel.id}/members/{user.id}": {
    GET: "getGuildMember",
  },
  "/v2/groups/{channel.id}/join_request_list": {
    GET: "getGuildJoinRequestList",
  },
  "/v2/groups/{channel.id}/approval_join_request/{user.id}": {
    POST: "approveGuildJoinRequest",
  },
  "/v2/groups/{channel.id}/restrict_chat_setting": {
    GET: "getRestrictChatSetting",
    POST: "updateRestrictChatSetting",
  },
  "/v2/groups/{channel.id}/messages": {
    POST: "sendMessage",
  },
  "/v2/groups/{channel.id}/messages/{message.id}": {
    DELETE: "deleteMessage",
  },
  "/v2/users/{user.id}/messages": {
    POST: "sendPrivateMessage",
  },
  "/v2/users/{user.id}/stream_messages": {
    POST: "sendPrivateStreamMessage",
  },
  "/v2/users/{user.id}/messages/{message.id}": {
    DELETE: "deletePrivateMessage",
  },
  "/v2/users/{user.id}/files": {
    POST: "sendFilePrivate",
  },
  "/v2/groups/{channel.id}/files": {
    POST: "sendFileGuild",
  },
  "/v2/users/{user.id}/upload_prepare": {
    POST: "uploadPreparePrivate",
  },
  "/v2/groups/{channel.id}/upload_prepare": {
    POST: "uploadPrepareGuild",
  },
  "/v2/users/{user.id}/upload_part_finish": {
    POST: "uploadPartFinishPrivate",
  },
  "/v2/groups/{channel.id}/upload_part_finish": {
    POST: "uploadPartFinishGuild",
  },
  "/gateway": {
    GET: "getGateway",
  },
  "/gateway/bot": {
    GET: "getGatewayBot",
  },
});
GroupInternal.define(false, {
  "/interactions/{interaction.id}": {
    PUT: "acknowledgeInteraction",
  },
}, { responseType: "text" });

// src/internal/guild.ts
GuildInternal.define(true, {
  "/users/@me": {
    GET: "getMe",
  },
  "/users/@me/guilds": {
    GET: "getGuilds",
  },
  "/guilds/{guild.id}": {
    GET: "getGuild",
  },
  "/guilds/{guild.id}/channels": {
    GET: "getChannels",
    POST: "createGuildChannel",
  },
  "/channels/{channel.id}": {
    GET: "getChannel",
    PATCH: "modifyChannel",
    DELETE: "deleteChannel",
  },
  "/channels/{channel.id}/online_nums": {
    GET: "getChannelOnlineNums",
  },
  "/guilds/{guild.id}/members": {
    GET: "getGuildMembers",
  },
  "/guilds/{guild.id}/roles/{role.id}/members": {
    GET: "getGuildRoleMembers",
  },
  "/guilds/{guild.id}/members/{user.id}": {
    GET: "getGuildMember",
    DELETE: "removeGuildMember",
  },
  "/guilds/{guild.id}/roles": {
    GET: "getGuildRoles",
    POST: "createGuildRole",
  },
  "/guilds/{guild.id}/roles/{role.id}": {
    PATCH: "modifyGuildRole",
    DELETE: "removeGuildRole",
  },
  "/guilds/{guild.id}/members/{user.id}/roles/{role.id}": {
    PUT: "addGuildMemberRole",
    DELETE: "removeGuildMemberRole",
  },
  "/channels/{channel.id}/members/{user.id}/permissions": {
    GET: "getChannelMemberPermissions",
    PUT: "modifyChannelMemberPermissions",
  },
  "/channels/{channel.id}/roles/{role.id}/permissions": {
    GET: "getChannelRole",
    PUT: "modifyChannelRole",
  },
  "/channels/{channel.id}/messages/{message.id}": {
    GET: "getMessage",
    DELETE: "deleteMessage",
  },
  "/channels/{channel.id}/messages": {
    POST: "sendMessage",
  },
  "/dms/{guild.id}/messages": {
    POST: "sendDM",
  },
  "/guilds/{guild.id}/messages/setting": {
    GET: "getMessageSetting",
  },
  "/users/@me/dms": {
    POST: "createDMS",
  },
  "/dms/{guild.id}/messages/{message.id}": {
    DELETE: "deleteDM",
  },
  "/guilds/{guild.id}/mute": {
    PATCH: "muteGuildOrMembers",
  },
  "/guilds/{guild.id}/members/{user.id}/mute": {
    PATCH: "muteGuildMember",
  },
  "/guilds/{guild.id}/announces": {
    POST: "createGuildAnnounce",
  },
  "/guilds/{guild.id}/announces/{message.id}": {
    DELETE: "removeGuildAnnounce",
  },
  "/channels/{channel.id}/pins/{message.id}": {
    PUT: "createPinsMessage",
    DELETE: "removePinsMessage",
  },
  "/channels/{channel.id}/pins": {
    GET: "getPinsMessage",
  },
  "/channels/{channel.id}/schedules": {
    GET: "getSchedules",
    POST: "createSchedule",
  },
  "/channels/{channel.id}/schedules/{schedule.id}": {
    GET: "getSchedule",
    PATCH: "modifySchedule",
    DELETE: "removeSchedule",
  },
  "/channels/{channel.id}/messages/{message.id}/reactions/{type}/{id}": {
    PUT: "createReaction",
    DELETE: "deleteReaction",
    GET: "getReactions",
  },
  "/channels/{channel.id}/threads": {
    GET: "listThreads",
    PUT: "createPost",
  },
  "/channels/{channel.id}/threads/{thread.id}": {
    DELETE: "removePost",
  },
  "/guilds/{guild.id}/api_permissions": {
    GET: "getGuildApiPermissions",
  },
  "/guilds/{guild.id}/api_permissions/demand": {
    POST: "createGuildApiPermissionDemand",
  },
});

export { GroupInternal, GuildInternal };
