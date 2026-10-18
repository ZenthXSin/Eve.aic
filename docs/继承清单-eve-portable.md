# 继承清单：eve-portable → Eve.aic

旧包：https://gitee.com/zenthxsin/eve-portable，固定版本 `e7e43b4db4de38775a8d28f59dd4ec69e5e1686e`。
相关 PR：[#201](https://github.com/ZenthXSin/Eve.aic/pull/201)（JSON 校验规则）、[#203](https://github.com/ZenthXSin/Eve.aic/pull/203)（接入 QQ 宿主）、执行规则与语言风格（本 PR）。

本清单只记录**已经落地并验证**的继承内容；旧包里其余部分逐项标注“未继承”及原因，不把整理过的文档当成已装配的能力。

## 已继承

| 能力 | 旧包来源 | Eve 中的落点 | 许可证 | 验证 |
| --- | --- | --- | --- | --- |
| Mindustry JSON 内容校验规则 | `tools/mindustry_json_validator.py`（逐字节副本，sha256 `0c7bdf38…e11ba7`） | `vendor/eve-portable-mindustry-json-validator/vendor.py` | AGPL-3.0（副本自带 `LICENSE`） | `cargo test -p eve-practice-mindustry`；`scripts/check_mindustry_json.py` 实跑 |
| hjson / 注释 / 尾随逗号展开 | 旧版校验器只吃 JSON，旧 SKILL.md 只写了“stdin、`--type`”用法 | `vendor/.../eve_modcheck.py`（Eve 写的调用层，不含规则） | 同上目录，AGPL-3.0 | 同上 + `tests/json_precheck.rs` |
| 运行前预检 | 旧包 `skills/mindustry-json-validator/SKILL.md` 的“先校验再加载”流程 | `crates/practice-mindustry/src/precheck.rs`（`JsonPrecheck`） | MIT（Eve 新代码） | `tests/precheck_wiring.rs` 用真实内核与账本跑通 |
| QQ 宿主接入 | 同上 | `--practice-mindustry-json-rules`、`--practice-python`（`crates/eve-app/src/qqbot.rs`）；`JsonDraftCheck` 只按错误拦下运行 | MIT | `connectors/qqbot/test/practice_test.py` 真实进程验收 |
| 规则文件摘要核对 | 旧包没有 | `crates/practice-mindustry/src/rules.rs`（`MindustryJsonRules`） | MIT | `rules::tests::accepts_the_pinned_copy_and_rejects_any_modified_file` |
| 目录级命令行入口 | 旧包 `scripts/validator.py <file.json>` | `scripts/check_mindustry_json.py` | MIT | 对含坏 JSON 的样例模组实跑，退出码 1 |
| 来源与边界文档 | — | `docs/继承-eve-portable-json校验.md`、`vendor/.../README.md` | — | 文档本身 |
| 执行规则中与当前身份一致的部分 | `rules/RULE.md`：冲突取舍顺序、回复不写出内部判断、能验证就实际验证、Mindustry 结论要有出处 | `AGENT.md`“沿用旧 Eve 的做法”一节 | 改写而非复制，MIT | QQ 进程测试使用根目录 AGENT.md 装配 |
| 语言风格中与当前身份一致的部分 | `language-style/LANGUAGE_STYLE.md`：先说结论、短句、偶尔轻语气词；代码问题先给可行性与关键几行 | 同上 | 同上 | 同上 |

## 未继承（及原因）

| 旧包内容 | 原因 | 后续条件 |
| --- | --- | --- |
| `knowledge/`、`memory/`、`data/` 旧知识库与记忆 | 有版本与来源，但格式不匹配 Eve 的知识契约；直接灌入会把旧会话事实当成当前事实 | 需要按知识插件契约逐条导入并标来源/版本 |
| `skills/*/SKILL.md` 及其余脚本 | 大多是流程文档或依赖旧环境（固定路径、xvfb、Gradle、旧账号） | 每项单独评估是否有可执行接入点 |
| `源码和示例/Mindustry`、`Arc` 源码副本 | 体积大且是特定 commit 的快照 | 需要时通过 `github_api` 按 commit 查证，而不是复制进仓库 |
| 无头 UI 测试与完整 GL 验证器 | 依赖 xvfb/GL 环境与 jar，宿主没有装配 | 与现有 `Mindustry 真实运行验收` 作业合并评估 |
| 语言风格中的粗口、高频梗与 6～15 字硬性上限；人格设定、情感系统 | 与当前稳定身份“直接、自然、不刻意卖萌”冲突，或与动态状态、表达学习重叠 | 不继承；表达节奏由会话内表达学习按用户原话调整 |
| 执行规则中的管理员 ID、旧工作区路径、`[null]` 回复、`qq_sender`、AnySearch、`eve_vector_search` | 属于旧宿主的账号、环境与工具，Eve.aic 没有对应装配；主人身份已由 #202 按宿主绑定 | 不继承 |
| `bootstrap.sh`、定时任务、自动恢复脚本 | 旧包安装流程会改动环境 | 不运行、不移植 |

## 尚未完成（本项能力的剩余工作）

- 与真实游戏的一致性：上游规则是 `ContentParser.java` 的手写复刻，个别合法字段（如方块 `targetAir`/`targetGround`）会误报未知字段。按上游版本保留，不擅自改规则；如要修正，应同时更新副本、摘要和文档。
- 上游规则的单元测试：旧包没有可复用的测试资产，现由调用层集成测试覆盖。

## 复现方式

```sh
cargo fmt --all -- --check
cargo clippy -p eve-practice-mindustry --all-targets --locked -- -D warnings
cargo test -p eve-practice-mindustry --locked
python3 scripts/check_mindustry_json.py <模组目录>
```
