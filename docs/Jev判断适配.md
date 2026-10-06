# Jev / SystemOne 消息判断

`eve-jev` 是实现公开 `RelationJudge` 的独立 HTTP 适配器，协议依据 TypeSafe 官方 Python SDK 固定版本 [`f078f1e`](https://github.com/typesafe-ai/typesafe-sdk-python/tree/f078f1e208a0d885154dc758344ae4fce77ac168)：`POST /v1/systemone`，Bearer 凭据，输入 `state/model/questions`。适配器只判断消息，不执行工具或控制动作。

本分支增加 QQ 宿主装配，保持实验功能默认关闭。终端入口仍未接入自然消息判断；本地替身验收只证明接线和恢复边界，真实 Jev 的分类质量、概率校准及延迟仍待评估，不据此开启正式训练。

## QQ 开关与独立配置

`eve-qqbot --message-judge off|primary|jev`：

| 模式 | 在途任务收到普通文字 | 判断顺序 |
| --- | --- | --- |
| `off`（默认） | 排队开始新轮 | 显式命令继续使用规则 |
| `primary` | 绑定接收时的当前代进行判断 | 明确规则 → 当前主模型 → 澄清 |
| `jev` | 绑定接收时的当前代进行判断 | 明确规则 → Jev → 当前主模型 → 澄清 |

没有在途任务时，普通文字仍开启新轮。`--self-learning` 不会自动开启消息判断。完整斜杠命令直接使用规则；任何一行以 `/` 开头但格式错误或混合未解释文字时，保留澄清，不交给模型重解。QQ 桥接尚未提供可信澄清引用，所以 `answer` 不能仅凭自然文字执行。

Jev 模式需要宿主环境中的 `EVE_JEV_API_KEY`，不会读取主模型密钥作为备用，不把密钥写进参数、普通配置或 Node 环境。主模型继续使用 `EVE_OPENAI_API_KEY`。Jev 的非敏感配置如下，配置文件覆盖环境，环境覆盖默认：

| 配置字段 | 环境变量 | 约束 |
| --- | --- | --- |
| `provider.jev.base_url` | `EVE_JEV_BASE_URL` | 默认 `https://api.typesafe.ai`；HTTPS 或环回 HTTP |
| `runtime.models.jev_enabled` | `EVE_MODELS_JEV_ENABLED` | 必须显式 `true` |
| `runtime.models.jev_provider` | `EVE_MODELS_JEV_PROVIDER` | 必须为 `jev` |
| `runtime.models.jev_model` | `EVE_MODELS_JEV_MODEL` | 显式模型名，例如 `jev-latest` |
| `runtime.models.jev_credential_ref` | `EVE_MODELS_JEV_CREDENTIAL_REF` | 空或 `env:EVE_JEV_API_KEY`；不解析任意环境引用 |
| `runtime.models.jev_timeout_ms` | `EVE_MODELS_JEV_TIMEOUT_MS` | 1–60000 毫秒，仍受消息总期限约束 |
| `runtime.models.jev_max_concurrent_requests` | `EVE_MODELS_JEV_MAX_CONCURRENT_REQUESTS` | 1–32，满时立即回退 |
| `runtime.models.jev_max_output_tokens` | `EVE_MODELS_JEV_MAX_OUTPUT_TOKENS` | 保持 0；当前 Jev 协议适配不支持此选项 |

启动只预检并构造客户端，不发送健康探测请求。缺凭据、角色关闭或配置错误会拒绝启动；运行中的 Jev 配置变坏则返回不可用并进入主模型回退。每次判断捕获同一修订的角色与 Provider 配置，最多重读四次；在途请求固定选择，新请求使用新配置，宿主只缓存最近一个客户端选择。凭据在宿主启动时读取，变更凭据需重启。

## 回退、原文与取消

复用 `RelationPlugin::with_fallback`，不另造动作执行器。`runtime.messages.judge_timeout_ms`（环境 `EVE_MESSAGE_JUDGE_TIMEOUT_MS`，默认 2000 毫秒）限制整次判断，Jev 至多使用链内预算的一半，低置信度、含混、非法结构、HTTP 失败或超时后至多调用主模型一次。主模型由 `LlmRelationJudge::with_resolver` 在本次回退开始时解析，单次请求固定 Provider 和其期限。主模型也失败、超时或低置信度时，路由器给出澄清，不因判断失败取消任务。消息准入时固定的外层期限始终优先，不会因回退延长。

Jev 一次请求同时询问 `choice` 单意图与 `noul` 完整单意图条件。只有无需裁剪、整条用户原文可直接使用时，才在本地绑定 UTF-8 全文范围；不生成修订文字。多意图、需要切分或缺失可信引用时交给主模型或澄清。分数取 choice 自评、选中概率和 noul 概率的最小值再向下取整，未经校准，不代表真实正确率。宿主继续校验阈值、范围、作用域、代际及工具副作用。

开启自然判断后，不同会话可并行推进，同会话的旧回复和后续任务等待自己的判断收尾。显式命令撤销同会话尚未完成的自然判断；路由器在判断、等待动作锁和动作前检查通道关闭，并检查权威快照的取消状态/展示退休状态。取消已准入后仍等待原代工具析构与最终提交，再决定是否允许替代代。已经执行或无法确认工具数的修订只澄清，绝不重放旧工具；明确 `/new` 保留开启独立任务的语义。底层条件提交仍以完整代际为原子比较条件，不将布尔取消状态描述为跨调用者的原子版本号。

HTTP 不跟随重定向、不自动重试，JSON state 不超过 65536 字节，响应不超过 16384 字节。只发送当前任务和最新消息的必要文字，不带路由 ID、epoch、完整历史或工具参数/结果；文字自身仍可能含敏感信息。错误不输出密钥、正文或原始 Provider 诊断。丢弃请求 Future 释放本地请求和额度，不保证远端停止或费用撤销。

## 保存、恢复与验收范围

消息判断和澄清不持久化，QQ 回执格式保持版本 1。接收处理中的消息写入既有回执账本；重启保留失败/未完成记录，不补发旧回复、不重放旧判断或工具。停止先关闭准入并撤销判断，等待已准入的路由、原代提交和本地执行器收尾，再停止桥接子进程。保存失败/提交未知继续阻塞，不能清空状态后假装成功。

自然修订产生的替代轮次当前不计入普通聊天的已送达学习证据；自主偏好学习沿用原观察器边界。普通用户文字的表达统计仍遵守原训练开关与去重规则。

本地验收入口：`cargo test -p eve-jev -p eve-message-plugin -p eve-qqbot-plugin --locked`、`cargo test -p eve-runtime --test relation_judge --test messages --locked`、`cargo test -p eve-app --lib --locked`；构建 `eve-qqbot` 后运行 `python3 connectors/qqbot/test/message_judge_test.py`。进程测试只连接环回替身，只停止自己创建的子进程，覆盖独立凭据、回退、期限、原文修订、工具保护、取消抢占、隔离及重启去重；执行结果以本次 PR 记录为准。

后续用同一公开标注集比较规则、主模型和 Jev 的误取消、纠正遗漏、原文匹配及延迟，再单独验证真实任务完成率和请求/回退成本。没有真实接口证据时保持草稿和默认关闭；实际 QQ 验收在本地进行，不在工作流里连接正式 QQ。
