# 继承 eve-portable 的 Mindustry JSON 内容校验

本文说明 Eve 从旧包 `eve-portable` 继承的第一项能力：在把 Mindustry 内容草稿交给真实服务端之前，先用旧包的 JSON 校验规则查一遍。它记录来源、许可证、接线位置、验证证据和尚未完成的部分，便于复核与继续推进。

## 为什么选这一项

旧包里有大量技能文档（`skills/*/SKILL.md`）、人格与知识库、无头测试脚本和一份完整的 Mindustry 源码副本。其中多数要么依赖旧环境（固定路径、旧账号、旧群、xvfb、Gradle），要么只是流程文档，不能直接当作 Eve 现有插件契约里的可执行能力。

`tools/mindustry_json_validator.py` 是例外：它是纯 Python 数据校验器，不含网络、账号或环境路径，输入是内容文件、输出是错误与警告，正好能接在 Eve 已有的实践流程上——`eve-practice-mindustry` 的运行器本来就在全新目录里实际加载模组，缺的只是“运行前先用游戏自身的规则查一遍”。所以第一项继承是它，而不是把旧包整套文件塞进系统提示。

## 来源与许可证

| 项目 | 值 |
| --- | --- |
| 旧仓库 | https://gitee.com/zenthxsin/eve-portable |
| 固定版本 | `e7e43b4db4de38775a8d28f59dd4ec69e5e1686e` |
| 上游文件 | `tools/mindustry_json_validator.py`（复刻 `ContentParser.java` v8 / build 251） |
| 上游许可证 | AGPL-3.0（见同目录 `LICENSE`） |
| 本仓库内位置 | `vendor/eve-portable-mindustry-json-validator/vendor.py`（逐字节副本） |
| 副本摘要 | `sha256:0c7bdf382f5b4c690b7e04e765e272d1d45c2d6552d461398df043a609e11ba7` |

副本按 AGPL-3.0 单独授权，不是本仓库 MIT 许可的一部分，也不被链接进任何可执行文件：运行器只在运行时启动 Python 解释器执行它。`vendor/README.md` 与 `vendor/LICENSE` 记录同样的事实；`crates/practice-mindustry/src/rules.rs` 里的常量在装配时逐字节核对摘要，文件被替换或损坏就拒绝启用，不会把改过的规则当成原版继续用。

## 做了什么

1. **原样副本**：`vendor.py` 与上游文件摘要一致；把副本与上游内容一起交付，便于复核差异为零。
2. **调用层** `vendor/eve-portable-mindustry-json-validator/eve_modcheck.py`：读标准输入里的文件块，把 Mindustry 的 hjson 写法（裸 `#`、`//`、`/* */` 注释、尾随逗号、无引号键、省略花括号、数组里的 `key: value`、不带引号的字符串）展开成 JSON，再调用上游规则，输出一行 JSON。规则本身一行没改，转换只发生在调用层。
3. **运行前检查** `crates/practice-mindustry/src/precheck.rs`：用操作者给定的 `python3` 在规则包目录里执行调用层，`env_clear` 后只保留 `PATH` 等必要变量，禁用字节码缓存，单次 60 秒超时、输出上限 1 MiB；只把 `mod.hjson` / `mod.json` 与 `content/` 下的文件送检，脚本、贴图和包文件永远不进入解释器。
4. **摘要核对** `crates/practice-mindustry/src/rules.rs`：`MindustryJsonRules::open` 核对 `vendor.py` 与调用层的 SHA-256，不符就返回原因，不启用。
5. **脚本入口** `scripts/check_mindustry_json.py`：对任意模组目录跑一遍，打印 `ERROR` / `WARN` 与文件路径，有错误时退出码为 1。可以直接在开发或运维时用。
6. **文档**：本文与 `vendor/README.md`；README 与 `docs/对话工具.md` 补一句“预检只表示值得实际运行”。

预检结果按 Eve 既有的 `DraftCheck` 语义处理：问题加上 `[预检] ` 前缀写进本次尝试的记录，草稿器据此修正；预检问题不进能力缺口，不改变“只有独立实际运行通过才算验证”的判定。

## 验证证据

本机实际执行（Python 3.12.14、Rust 1.89.0）：

- `cargo test -p eve-practice-mindustry --locked` 全部通过，其中：
  - `rules::tests::accepts_the_pinned_copy_and_rejects_any_modified_file`：原样副本通过，改一个字节后拒绝；
  - `tests/json_precheck.rs`：hjson 展开、类型分派（block/unit/item/liquid）、非法 JSON 报错、清单只做解析检查、脚本与贴图不送检；
  - `tests/precheck_wiring.rs`：用真实内核、真实实践账本和真实规则跑一次实践，确认问题带 `[预检] ` 前缀、不进入能力缺口、锻造检查同时在场不受影响。
- `python3 scripts/check_mindustry_json.py <模组目录>`：对一份含方块、物品、炮塔、液体与坏 JSON 的样例模组输出逐文件结果，坏 JSON 报 `ERROR` 且退出码为 1。
- `python3 vendor/eve-portable-mindustry-json-validator/eve_modcheck.py --mod-name <name>` 直接调用：同一批文件结果一致。

真实游戏侧仍由既有 CI 作业（`Mindustry 真实运行验收`）在 v160.7 服务端上复验：预检通过不等于加载通过，最终证据仍来自实际运行。

## 接入 QQ 宿主

QQ 宿主用 `--practice-mindustry-json-rules <目录>` 显式开启预检，默认关闭。它只能和 `--practice-mindustry-server` 一起用，`--practice-python <程序>` 指定解释器（默认 `python3`，Windows 为 `python`）。开启后，后台实践和对话里的草稿检查都在锻造检查之前先跑预检。

- **启动**：先核对 `vendor.py` 与 `eve_modcheck.py` 的摘要，再用最小清单实跑一次规则。摘要不符、解释器无法启动或输出不合约定时拒绝启动，不会带着不可用的预检继续运行。
- **只有错误会拦下运行**：规则的 `ERROR` 以 `[预检] JSON 预检错误：…` 记入这次尝试，这次不启动服务端，草稿器据此修正。`WARN`（例如没写 `name`、按文件名推断）游戏照常加载，不交给草稿器，否则正确的草稿会一直跑不到真实运行。
- **运行中不可用**：启动后解释器失败、超时或输出不合约定时，这次尝试不加预检问题、照常实际运行，并在宿主日志里写一行“JSON 预检本次不可用，照常实际运行：原因”。不会因此让实践失败，也不把这种情况当作“没有发现问题”。
- **持久化**：预检不写自己的状态。它的问题随尝试写进实践账本，恢复语义与原有的 `[预检] ` 记录相同，重启不重放。

验收：`connectors/qqbot/test/practice_test.py` 用真实 `eve-qqbot` 进程和服务端替身跑一次。第一次草稿把方块尺寸写成 9，被规则拦下，服务端没有启动；第二次修正后实际加载并验证通过。另外两种情况会拒绝启动：规则文件被改动、解释器不存在。

## 尚未完成

- **不是全部规则都等价于游戏**：上游规则是 `ContentParser.java` 的手写复刻，个别字段（例如方块的 `targetAir`、`targetGround`）会被报成“未知字段”，而它们在当前版本里是合法字段。这类误报按上游版本如实保留，不自行改写规则；需要时先对照源码版本再更新上游副本与摘要。
- **其它旧包能力没有继承**：源码查证（旧包的 Mindustry 源码副本与 `Source: path:line-line` 引用习惯）、无头 UI 测试、知识库与记忆数据、语言风格与情感设定都还在旧包里。它们需要各自的可执行接入方式，不能靠复制文件完成；后续按同样标准逐项评估，本清单不为它们背书。
- **上游规则的单元测试没有搬**：旧包 `skills/mindustry-json-validator/` 只有 `SKILL.md` 与脚本，没有可复用的测试资产，因此调用层用自己的集成测试覆盖。
