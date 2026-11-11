# QQ 官方接口核对与传输层状态

核对时间：2026-11-11。来源是实际读取的官方页面，SDK 只作为接入参考。

## 已确认的接口事实

- 当前统一 API 地址是 `https://api.bot.qq.com`，访问凭证通过 `/app/getAppAccessToken` 获取。
  此接口失败也可能返回 HTTP 200，必须检查业务 `code`；调用开放接口使用
  `Authorization: QQBot <access_token>`。
  [访问凭证](https://bot.q.qq.com/wiki/develop/api-v2/dev-prepare/access-token.html)、[API 调用指南](https://bot.q.qq.com/wiki/develop/api-v2/dev-prepare/api-call-guide.html)。
- “接收所有消息”开启后，群消息可通过 `GROUP_MESSAGE_CREATE`
  推送；是否已给当前机器人开启仍需真实接入确认。
  [全量群消息事件](https://bot.q.qq.com/wiki/develop/api-v2/autogen/event/group_message_create.html)。
- WebSocket 支持 Identify、Resume 和心跳。恢复序号应在事件处理之后记录，否则可能跳过尚未保存的事件。
  [WebSocket 接入](https://bot.q.qq.com/wiki/develop/api-v2/dev-prepare/event-emit/websocket.html)。
- 单聊与群聊分别使用 `/v2/users/{openid}/messages`、`/v2/groups/{openid}/messages`。 被动回复使用
  `msg_id` 或 `event_id`；`msg_seq` 关联回复去重，不能在结果未知时换序号重发。 引用展示使用
  `message_reference`，其索引与被动回复的 ID 不同。
  [发送单聊消息](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_users_user_openid_messages.post.html)、[发送群聊消息](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_groups_group_openid_messages.post.html)。
- 平台会限制回复次数、频率和主动消息，用户也可拒收主动消息。能力存在不证明当前机器人有权限或剩余额度。
  [消息收发规则](https://bot.q.qq.com/wiki/develop/api-v2/server-inter/message/overview.html)。

## 文档仍有不一致与未确认处

单聊发送页面顶部与概述写被动回复有效期为 60 分钟，参数表 `msg_id` 又写 5 分钟。
传输层不据此提前裁定过期，保留调用方指定的回复目标和序号，如实返回平台错误；不能假装已验证当前账号的有效期。
这里只记录现状，没有确立手机层展示和提示策略。

已读取页面没有提供通用的单聊/群聊历史拉取接口；这不证明整个官方平台都没有。
翻页与搜索仍需核对能力清单，再根据适配器入站日志实现可追溯范围，不能展示接入前未收到的历史。

## 目前实现范围

`adapters/qq/client.ts` 是独立 HTTP
传输组件，不依赖内核或插件。已接入凭证获取、机器人身份读取和文字发送。

- 只有成功响应里的实际消息编号才返回 `sent`。
- 认证失败会注明发送未开始；明确业务拒绝返回 `failed`。
- 发送超时、断线、无有效回包、无编号、异步审核和平台去重返回 `unknown`。
- 每次调用只尝试发送一次，不自动递增序号、改正文、改主动发送或追随重定向。
- 凭据只留在传输实例；不向调用方返回 access_token，不记录平台任意错误正文。
- 并发读取共享一次凭证刷新，接近过期时刷新；只有本机测试地址允许 HTTP。

本地验证通过真实回环 HTTP 服务进行，不能当作线上 QQ 送达。

```bash
deno test --allow-net=127.0.0.1 tests/qq_client_test.ts
deno check adapters/qq/client.ts tests/qq_client_test.ts
```

还未接通：独立适配器服务、持久动作记录、外部所有权令牌、入站日志、WebSocket、手机插件、图片/文件以及真实
QQ 联调。 直接调用此组件跨调用没有持久去重；服务层接入前不能宣称整个消息链路已可靠。
当前真实认证返回业务错误 `100016`，配置确认见 Issue #548；此文档不含凭据。
