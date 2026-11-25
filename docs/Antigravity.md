# Antigravity 模型通道

Panel V2 的 `Antigravity · Google 原生登录` 是可选模型通道，不是 QQ 或 OneBot 消息通道。保存不等于激活。桥只监听生产机 `127.0.0.1:21986`，面板使用的密钥是桥自己的，不是 Google 凭据。`antigravity-default` 是本地别名，由官方 CLI 选择其已登录账户的默认模型，不代表某个固定 Gemini 版本。

## 操作

此桥同时接受 Chat Completions 和 Eve 使用的无状态 Responses 请求（文本和函数工具，返回完整 JSON）。不支持图片或通过 `previous_response_id` 续接。可选择 `antigravity-default`，也可填写 `eve-antigravity models` 实际列出的精确模型 ID；例如账号提供的 `gemini-4-neon-high` 包含档位后缀。面板测试使用已保存的 Antigravity 通道协议、密钥、模型及推理强度，不发送 Eve 的历史。

推荐入口：Panel V2 → 设置 → 模型通道 → 在「已保存的连接」里选 `Antigravity · Google 原生登录`。点击「连接 Google 账号」，打开 Google 授权页，完成后把一次性授权码粘回面板提交。等待显示登录完成，点击「测试连接」；只有需要正式切换时才点「设为当前通道」。已通过 SSH 登录的账号可直接测试，不必重新授权。面板不显示密码、保存授权码或把授权码写到命令行；只允许发起授权的管理员会话提交，刷新页面可恢复，支持取消，等待超过五分钟需重开。下列 SSH 命令仍可作为备用入口。

1. SSH 登录生产机，在有终端的会话运行 `sudo eve-antigravity login`，按官方 CLI 提示自行授权，完成后退出 CLI。不要把授权码贴到 QQ、面板或日志。不要直接用自己的 Unix 用户运行 `agy` 代替此步骤。
2. `sudo eve-antigravity models` 查询该账户可用的模型。`sudo eve-antigravity test` 只发送独立测试文本，不带 Eve 历史、不调用 Eve 工具；拿到 `PASS EVE_OK` 再从 Panel V2 的模型通道页激活。切回此前通道即可停止把新请求发给 Antigravity。激活后，实际推理会向 Google 提交 Eve 当前会话与工具定义，受账户权限和配额约束。
3. 桥的 `/healthz` 只表示进程可用，不查询账号；`503 antigravity_login_required` 表示需要交互登录。`401` 指桥接密钥问题；`429` 表示正在处理其他请求；`504` 表示超时。不要因为未知错误自动重放带工具的会话。

## 边界与部署

- 网页登录由面板启动官方 CLI，子进程继承管理服务的挂载权限。使用 `ProtectSystem=strict` 时，需将 `deployment/eve-rebuild-admin.service.d/antigravity-login.conf` 安装为管理服务的 systemd drop-in；保留沙箱并仅额外允许 Antigravity 专用账号目录可写。不要把凭据文件复制到面板目录。

- 此版本接收文本对话和 Eve function tools，完整 JSON 一次性返回，不转发 CLI 中间事件、思考内容或命令输出。图片输入明确报错。模型输出预算作为提示目标，不是 CLI 硬 token 上限；桥另设请求/输出字节和运行时间上限。仅允许本次声明且参数通过校验的函数，工具由 Eve 执行。
- 相同 `prompt_cache_key`、协议、模型和工具配置下，桥验证历史前缀和上一条回复；匹配时用 CLI `--conversation` 只发新增消息。压缩、改写、重启、过期或缺少会话键时全量重建。失败不重放。最多保留 4 个短期会话，空闲 20 分钟过期，缓存位于服务私有运行目录。只记录路径模式、字节数和 token 数，不记录消息内容。
- CLI 报告会话累计用量；续接先取差值，输入总量按未缓存输入加缓存读取计算。缺失或回退的计数不冒充命中，不改写旧账本。字节发送减少不等于供应商 token 命中率。
- 当前实例在每次请求末尾附加一条临时提示，但不把它写入历史。桥只识别该版本三种模板的完整 SHA-256、末尾位置和消息结构：原文随当轮请求送出，不参与下一轮的历史匹配。任何未知模板仍按普通消息处理。真实消息、压缩或投影变化不忽略；日志中的 `reason` 区分冷启动、历史或回复不匹配与成功续接。
- 网页登录、命令行登录和模型桥共用生产机的 HTTP 代理 `http://127.0.0.1:7897`；这里的 `127.0.0.1` 指运行 Eve 的主机。模型桥只允许 loopback IP，不修改全机或 QQ 的代理设置。
- 部署依赖：已校验的官方 Linux x64 CLI、系统 Python 3 及 `jsonschema`、systemd。`deployment/eve-antigravity.service` 配置工作目录、凭据与临时会话目录。私有密钥 `/etc/eve-antigravity/bridge.key`，只读代理配置 `/etc/eve-antigravity/proxy.env`，桥位于 `/opt/eve-antigravity`。`deployment/antigravity-register.py` 通过现有面板 API 保存并读回通道，不编辑账本、不激活。
