# 本地 PostgreSQL 状态后端

`eve-state-postgres` 实现公开的同步 `StateStore` 字节接口，插件仍通过 `PluginContext::state_get` / `state_set` 访问自己的命名空间。`eve-cognition`、终端 `eve` 与 `eve-qqbot` 均通过 `--database-config` 显式选择该后端；默认文件状态保持兼容。

## 配置与启动

连接配置使用独立 JSON 文件，必填且仅接受以下五个字段：

```json
{
  "hostname": "127.0.0.1",
  "port": 5433,
  "database": "<数据库名称>",
  "user": "<数据库用户>",
  "password": "<宿主数据库密码>"
}
```

可将文件放在仓库外的 `~/.config/eve-local/database.json`。Unix 建议将文件权限设为 `0600`；当前读取器不强制检查权限位。文件必须是至多 16 KiB 的普通文件，符号链接、额外字段、缺字段和无效 JSON 均拒绝。应用错误不回显密码或原始连接配置；凭据不写入插件状态、普通配置或目标正文。

本切片使用 `NoTls`，只接受本机 loopback IP、`localhost` 或 Unix socket 路径；`localhost` 连接时按 `127.0.0.1` 处理。远程主机被拒绝，不提供远程 TLS、连接池或自动主从切换。宿主负责准备数据库、账户及创建 `eve_state` schema 和表所需的权限，后端不会创建数据库或用户。

首次从文件后端改用数据库时，选择没有 `state.json` 的新状态目录。以下命令均在仓库根目录运行；模型凭据沿用[核心对话](./核心对话.md)的宿主环境，`add`、`status`、`show` 不请求模型。

```sh
export EVE_DB_CONFIG="$HOME/.config/eve-local/database.json"
cargo run -p eve-app --bin eve-cognition --locked -- --state-dir ./.eve-cognition-pg --database-config "$EVE_DB_CONFIG" add --id sql-goal --text "明确当前学习目标的差距，给出可验证的最小下一步。"
cargo run -p eve-app --bin eve-cognition --locked -- --state-dir ./.eve-cognition-pg --database-config "$EVE_DB_CONFIG" status
cargo run -p eve-app --bin eve-cognition --locked -- --state-dir ./.eve-cognition-pg --database-config "$EVE_DB_CONFIG" run --seconds 30 --max-executions 1
cargo run -p eve-app --bin eve-cognition --locked -- --state-dir ./.eve-cognition-pg --database-config "$EVE_DB_CONFIG" show --id sql-goal
# 终端与 QQ 使用相同存储选择；同一数据库同时只允许一个 Eve 宿主。
cargo run -p eve-app --bin eve --locked -- --state-dir ./.eve-console-pg --database-config "$EVE_DB_CONFIG"
cargo run -p eve-app --bin eve-qqbot --locked -- --state-dir ./.eve-qqbot-pg --database-config "$EVE_DB_CONFIG"
```

认知状态和 Session 草稿进入同一数据库，父目标保持 Waiting、反思子目标独立完成及去重语义不变，见[内生差距发现与反思记录](./认知循环.md#内生差距发现与反思记录)。`--state-dir` 仍用于本地排他锁、数据库绑定标记和 `configuration/` 普通配置；AGENT 文件和 Provider 凭据继续由宿主提供。

QQ 的 Session 历史、`eve.channel.qqbot/receipts.v1` 回执、训练开关和表达统计共用该宿主的同一个 SQL 后端。完成轮次重启后恢复为历史；旧平台消息 ID 不重新调用模型或补发，Processing/ReplyPending 仍保留待诊断，训练停止和表达去重跨重启保持。终端共享底层数据库但沿用自己的会话标识，不会自动接管 QQ 会话。两个入口都拒绝已有文件状态的静默迁移；现有 QQ 文件数据应继续使用原目录与默认后端，或另行显式迁移，不能仅改启动参数后当作旧历史已进入数据库。

## 状态目录与数据库绑定

应用在成功连接数据库后创建 `state.backend.json`，记录版本 1 及 `hostname`、`port`、`database`、`user`，不保存密码。Unix 新建标记权限为 `0600`。该文件用于防止同一目录在不同启动参数下悄悄读取另一套状态：

- 已有数据库绑定时，漏传 `--database-config` 会失败，不退回空文件库。
- 连接目标四个字段必须与标记完全一致；更换数据库、用户、端口或主机表示方式均拒绝。密码不参与目标绑定，可由宿主更新独立凭据文件后重启。
- 目录中已有 `state.json` 时拒绝切换到 PostgreSQL，保留原文件。新目录只选择数据库，不迁移旧文件状态。
- 标记损坏、未知版本、非普通文件或不可读时拒绝启动，不覆盖或删除标记来绕过检查。

同一目录的本地锁与数据库锁同时持有。不同目录指向同一数据库也只能有一个遵守协议的 Eve 宿主；共享宿主内应复用同一个 `Arc<dyn StateStore>`。这些锁不阻止数据库管理员或其他不遵守协议的程序修改表，运行时不能用外部 SQL 改写业务状态。

## 存储格式与同步接口

后端占用 `eve_state` schema：

| 表 | 内容 |
| --- | --- |
| `eve_state.metadata` | 唯一的 `singleton=true` 记录与 `format_version=1` |
| `eve_state.entries` | `namespace BYTEA`、`key BYTEA`、`value BYTEA`，主键为 `(namespace, key)` |

命名空间和键按原始字节匹配，值保持插件提供的字节，不把 JSON 或 UTF-8 作为数据库后端契约。插件仍负责各自格式、修订、权限与恢复校验；数据库格式版本与插件数据版本独立。

初始化先取得数据库内的 session advisory lock，再在事务中创建缺失 schema 或检查已有版本、列和主键。已有 schema 不兼容时返回错误，不自动修复、迁移或清空。一个专用普通线程独占同步 PostgreSQL 客户端，所有 `get` / `set` 串行请求该线程，避免在宿主 Tokio 任务里嵌套客户端 runtime。`StateStore` 调用仍同步等待结果，这不是异步数据库 API；连接、语句和锁等待均有有限超时。

`get` 直接查询数据库，不依赖本地值缓存。单次 `set` 用事务提交一个键的插入或替换；接口不提供多键事务、`get` 后 `set` 的原子修改，也不提供认知与 Session 的跨插件事务。正常释放后端时关闭连接、等待专用线程退出并释放 advisory lock。

## 提交失败与恢复

PostgreSQL 的提交响应失败可能发生在服务器已经提交之后。因此 `set` 返回错误不能证明数据库仍是旧值；与文件后端不同，不能把插件保留旧内存快照解释为数据库写入已回滚。

任意 SQL 读取、写入或提交操作失败后，该后端实例永久关闭，后续 `get` / `set` 都返回错误，不自动重连、重试、返回不存在或切换存储。初始化提交失败也不能直接当作空库；宿主应停止并保留连接目标，下一次显式启动重新读取实际持久化状态。

业务恢复遵循已保存的状态机：

- 认知 Executing 在重启时转为 Blocked/Interrupted；已有 Blocked 保留，不自动提交旧尝试。
- Session Pending 在重启时转为 Interrupted；完成会话只恢复历史，不重放工具。
- 反馈保存报错时，数据库可能已保存终态，也可能仍保留 Executing。重启以实际字节和验证结果为准，不能根据旧进程缺失反馈推断模型或工具未执行。
- 若在执行标记、会话提交或反馈之间崩溃，没有跨插件事务来一起回滚；不确定执行保持阻塞，不能自动另建目标或重新调用模型来补写状态。

连接错误、未知格式、损坏业务字节和恢复失败均保留现有数据库与文件，由宿主诊断。状态后端没有通用自动修复或恰好执行一次保证，也不承诺替代数据库备份策略。

## 独立测试库验收

集成测试显式读取仓库外的 `EVE_POSTGRES_TEST_CONFIG`，字段同上；只允许本机且名称以 `_test` 结尾的专用数据库。测试会清理该库的 `eve_state` schema、注入未知格式版本并终止属于测试实例的连接，不能指向正式库。三个数据库测试必须串行运行，CI 仅使用临时测试库；QQ 套件需要 Node 22，但不安装 SDK、不连接 QQ 或真实模型。

```sh
export EVE_POSTGRES_TEST_CONFIG="$HOME/.config/eve-local/database-test.json"
cargo test -p eve-state-postgres --test persistence --locked -- --ignored --test-threads=1
cargo test -p eve-app --test cognition_postgres --locked -- --ignored --test-threads=1
cargo test -p eve-app --test qq_postgres --locked -- --ignored --test-threads=1
```

后端测试覆盖原始字节、命名空间隔离、覆盖写、独占锁、重开、未知格式保留以及断连后永久关闭；认知测试启动真实独立 `eve-cognition` 进程和 loopback 模型，检查 SQL 草稿恢复、父目标 Waiting、再次启动零请求以及漏参数/更换目标/已有文件状态的拒绝路径。2026-10-04 在合并主线 `fdabc3c` 后以本机专用 `eve_test` 串行复验：后端测试通过（4.28 秒），认知进程测试通过（3.69 秒）；没有使用真实模型或 QQ。

`qq_postgres` 启动实际 `eve-qqbot`、离线 Node 桥接与 `eve` 终端进程，检查聊天/训练/回执的 SQL 保存、训练停止后跨重启保持、重复消息零重发、历史恢复以及两个入口的后端绑定拒绝路径。进程清理只操作测试自身 `spawn` 返回的 `Child`，不读取 PID 文件发送信号。独立工作流 `.github/workflows/postgres.yml` 在 Ubuntu 临时 PostgreSQL 18 服务中生成权限 `0600` 的临时连接文件，并以 Rust 1.89 串行执行三项数据库测试；普通工作区测试跳过需要数据库的 `#[ignore]` 用例。远程结果以对应 PR 工作流为准。
