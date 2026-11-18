# QQ Bot 平台目录：哪些请求能构造，哪些能力有证据

核对日期：**2026-11-18**。目录有 **107 个命名操作**，不是 107 项已开通权限。101
个按腾讯平台文档建模（含两个同路径但作用域不同的禁言操作），6 个仅有旧官方 SDK 证据。`access` 是 Eve
的本地门禁；普通聊天脚本仍只能 `platformRead(getMe)`，不因目录中出现 `GET` 就获得接口。

| 证据                                                                                        | 能说明什么                                                                                                                                            | 不能说明什么                                                                                              |
| ------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| 腾讯[平台变更记录](https://bot.q.qq.com/wiki/develop/api-v2/changelog.html)及下列 REST 页面 | 方法、路径、线上协议和准入说明；`v2-restricted` / `guild-restricted` 表示另有关系、白名单、管理员、私域或机器人类型等条件                             | 本机 bot 获准、当前配额充足、消息已送达                                                                   |
| `sdk`                                                                                       | 旧 SDK 确有该方法：频道单条/列表读取、文本编辑、语音成员列表、频道/频道私信设置引导；这里均限本地 `admin`                                             | 现行平台承诺、QQ 群/C2C 历史能力、账号获准                                                                |
| 本机历史实证                                                                                | [接口核对](QQ接口核对.md)记录过取令牌和读取自身身份成功；[整体验收](整体验收.md)记录 2026-11-12 一条真实回复取得平台 `sent`；另一条超时仍为 `unknown` | 不外推到全部消息格式、会话或任一管理权限。本次只使用文档和离线/回环测试，没有新的账号授权、送达或部署证据 |

## 路由与条件

- **单聊、群聊、频道私信不可互换。** 单聊用 `/v2/users/{user_openid}`，群聊用
  `/v2/groups/{group_openid}`。频道私信先 `POST /users/@me/dms`（双方有共同频道），再向
  `/dms/{返回的私信 guild_id}/messages` 发送，不是源频道
  ID。[单聊发送](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_users_user_openid_messages.post.html)
  ·
  [群聊发送](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_groups_group_openid_messages.post.html)
  ·
  [频道私信](https://bot.q.qq.com/wiki/develop/api-v2/server-inter/channel/message/dms.html)。2026-11-16
  新增的[用户资料](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_users_user_openid_info.get.html)需要好友关系，不是任意
  QQ 号查询。
- **消息不是通用 JSON 透传。** 文本、Markdown、媒体和图文卡片按 `msg_type` 匹配；输入状态 6
  和[流式发送](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_users_user_openid_stream_messages.post.html)仅单聊。互动确认是
  `PUT /interactions/{原始 interaction_id}`，不是事件名加前缀的
  ID。[确认](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/interactions_interaction_id.put.html)
  ·
  [收发规则](https://bot.q.qq.com/wiki/develop/api-v2/server-inter/message/overview.html)。单聊被动窗口概述为
  60 分钟，而发送表的 `msg_id` 写 5
  分钟，不据此断言账号实效。单聊/群聊撤回均有两分钟限制；[群管理员](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_groups_group_openid_messages_message_id.delete.html)还能撤回普通成员消息，因此群撤回定为
  `admin`。频道消息及频道私信撤回另有私域限制。
- **群管理不是建群即开。**
  [群资料](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_groups_group_openid_info.get.html)、机器人群状态及[成员列表](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_groups_group_openid_members.get.html)有白名单限制（`11253`）；[批量移除](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_groups_group_openid_batch_remove_members.post.html)单次至多
  20
  人，还依赖群主和管理端开关。审批/禁言依赖管理员身份。[设置群禁言](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_groups_group_openid_restrict_chat_setting.post.html)写的是成员列表，不允许把查询结果中的全员配置写回。自动审批[创建策略](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_groups_join_approval_strategy.post.html)必须在
  `group_ids` 和 `group_openids` 中选一种。
- **菜单与面板作用域不同。**
  [菜单](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_menu.put.html)是覆盖式、全局
  C2C；[面板列表](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_panels.get.html)必传
  `scope`，最多 50
  条；[指定对象面板](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_panels.post.html)只支持
  C2C 或群，不给频道/频道私信伪造用户级配置。
- **频道管理。**
  成员列表与单个成员详情的准入不能等同；频道身份组、授权、精华、公告、日程及子频道权限仍受管理员或频道设置限制。角色
  `5` 的增删必须附
  `{channel:{id}}`。[角色授权](https://bot.q.qq.com/wiki/develop/api-v2/server-inter/channel/role-group/put_guild_member_role.html)
  ·
  [成员列表](https://bot.q.qq.com/wiki/develop/api-v2/server-inter/channel/role/member/get_members.html)。`muteGuildMembers`
  必须给非空 `user_ids`；全频道必须另选 `muteGuild`，不再接受含混的
  `muteGuildOrMembers`。[批量禁言](https://bot.q.qq.com/wiki/develop/api-v2/server-inter/channel/speak/patch_guild_mute_multi_member.html)。[表态](https://bot.q.qq.com/wiki/develop/api-v2/server-inter/message/trans/emoji.html)仅频道，首请求
  `limit` 最多
  50；[论坛](https://bot.q.qq.com/wiki/develop/api-v2/server-inter/channel/content/forum/put_thread.html)限私域，[音频](https://bot.q.qq.com/wiki/develop/api-v2/server-inter/channel/content/audio/put_mic.html)限音频机器人。
- **上传不是发送。**
  [富媒体](https://bot.q.qq.com/wiki/develop/api-v2/server-inter/message/rich-media.html)按同一会话类型预上传、逐片
  PUT 到签名地址、确认分片，再 `POST …/files` 合并（不是 `…/upload_complete`）。`srv_send_msg:true`
  可直接发消息。2026-10-21 新增的
  [Markdown 图片素材](https://bot.q.qq.com/wiki/develop/api-v2/server-inter/message/material-management.html)另用
  `/v2/media-images/prepare`、`/complete`、列表及 `DELETE /v2/media-images/{image_id}/delete`，不是
  `/files`。分片页中的大小是十进制字符串，[官方 SDK](https://github.com/tencent-connect/qqbot-nodejs/blob/429a0f1b798f3b0fcef3679e207dbc0469a1df2d/src/protocol/api/media-chunked.ts)也发送数值；两者均校验后保留原类型，分片序号沿用服务端返回值。

## 兼容性与未覆盖项

- [现代官方 OpenSDK 1.0.5](https://github.com/tencent-connect/qqbot-nodejs/tree/429a0f1b798f3b0fcef3679e207dbc0469a1df2d)（2026-10-12）有
  REST、WebSocket/Webhook 双接入、C2C
  流式和分片上传。不要用[旧 BotGo README](https://github.com/tencent-connect/botgo/blob/fe31c0dfe469001e0f783d2f07e7de7bd08b403f/README.md)关于
  2024 年底的计划判定今天所有 WebSocket 已下线，也不要反推所有账号均获准。2026-08-10 平台变更记录将
  API 域名统一为 `api.bot.qq.com`；SDK/本地传输仍有 `api.sgroup.qq.com`
  默认值，本目录只构造相对路径，不改主机或认证。Webhook
  在[管理端](https://bot.q.qq.com/wiki/develop/api-v2/dev-prepare/event-emit/webhook.html)配置；不提供旧
  `/gateway/webhook/sessions` 控制、令牌签发或任意 URL 转发。
- **已废弃与没证据是两回事。**
  [子频道公告](https://bot.q.qq.com/wiki/develop/api-v2/server-inter/channel/content/announces/post_channel_announces.html)注明
  2022-03-15 后废弃，使用 `/channels/{id}/pins`；`/guilds/{id}/announces`
  是另一个仍有文档的接口。频道单条消息采用[官方 Node SDK](https://github.com/tencent-connect/bot-node-sdk/blob/998051528aaa21fd952c31bcf60389ea8094e361/src/openapi/v1/message.ts)的带消息
  ID 路径，不照抄
  [BotGo](https://github.com/tencent-connect/botgo/blob/fe31c0dfe469001e0f783d2f07e7de7bd08b403f/openapi/v1/message.go)单条函数误用的集合
  URI。频道设置引导传 `content`
  提及；[频道私信设置引导](https://github.com/tencent-connect/botgo/blob/fe31c0dfe469001e0f783d2f07e7de7bd08b403f/openapi/v1/direct_message.go)是
  `setting_guide.guild_id`。未找到可推广为
  C2C/群历史、全量好友/群列表或群流式发送的官方路由；本目录不提供，也不复制 inbox/channel。
- **本地刻意收窄。** 所有嵌套对象只收列明字段；分页数值、布尔 `hidetip` 可以传原生 JSON
  标量，未发现需传 JSON
  对象的查询参数。首页省略空游标即可（目录接受文档的空串，独立执行器可能更严）。只收文件基名；平台抓取资源仅允许无凭据、查询串或片段的
  HTTPS 腾讯图片 CDN / COS 对象地址。内联 base64 最多 16,384 字符，整个 JSON 体 64
  KiB；白名单单次本地限 256。仅安全 Markdown 子集、已申请的键盘模板、纯文本发帖；不透传
  ARK、自定义按钮、原始 HTML/富文本、multipart、本机路径或签名
  URL。这些是本地边界，不是腾讯统一上限。
- **回执不是完整 REST 响应。** 通用执行器只投影安全字段，不返回上传凭据、资源
  `file_info`、预签名地址或分享链接；目录能构造相关请求，不代表可用它完成多步上传、获取链接或确认客户端渲染。需要附件时继续用现有专用发送链路。无账号权限实证的项目保持“未验证”，不为填表发真实请求。

离线复核：`deno test --allow-read --allow-write tests/qq_platform_catalog_test.ts`。测试用独立方法/路径/报文样例逐项经过真实目录和执行器，传输为内存函数；不需要网络、生产配置或机器人凭据。
