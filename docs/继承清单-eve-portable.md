# 继承清单：eve-portable → Eve.aic

旧包：https://gitee.com/zenthxsin/eve-portable，固定版本 `e7e43b4db4de38775a8d28f59dd4ec69e5e1686e`。
相关 PR：[#204](https://github.com/ZenthXSin/Eve.aic/pull/204)（执行规则与语言风格）、[#205](https://github.com/ZenthXSin/Eve.aic/pull/205)（Mindustry 资料）。

本清单只记录**已经落地并验证**的继承内容；旧包里其余部分逐项标注“未继承”及原因，不把整理过的文档当成已装配的能力。

## 已继承

| 能力 | 旧包来源 | Eve 中的落点 | 许可证 | 验证 |
| --- | --- | --- | --- | --- |
| Mindustry 资料 228 篇（含长期记忆技术节选） | `knowledge/mindustry/`、`knowledge/mod-making-guide/`、`knowledge/mod-making/`、`knowledge/concepts/`（Mindustry/Arc/JSON 主题）、`knowledge/analysis/` 与 `knowledge/entities/` 中的 Mindustry 与模组技术记录、`memory/MEMORY.md` 技术小节 | `vendor/eve-portable-knowledge/`，经 `--research-corpus` 与 `eve-corpus://` 入口进入受控研究 | AGPL-3.0（副本自带 `LICENSE`） | `knowledge-plugin` 资料源单元测试；`research_test.py` 真实进程选文、逐字引用、不访问网络 |
| 执行规则中与当前身份一致的部分 | `rules/RULE.md`：冲突取舍顺序、回复不写出内部判断、能验证就实际验证、Mindustry 结论要有出处 | `AGENT.md`“沿用旧 Eve 的做法”一节 | 改写而非复制，MIT | QQ 进程测试使用根目录 AGENT.md 装配 |
| 语言风格中与当前身份一致的部分 | `language-style/LANGUAGE_STYLE.md`：先说结论、短句、偶尔轻语气词；代码问题先给可行性与关键几行 | 同上 | 同上 | 同上 |
| Mindustry 模组验证器（无头版） | `project/mindustry-mod-validator/`：Java/Gradle 独立项目，在无头 Mindustry 中导入模组、创建并初始化内容，再放置方块、生成单位、跑更新帧，输出文本或 JSON 报告 | `vendor/mindustry-mod-validator/`，不含预编译 jar 与重复的 zip 示例；构建改为依赖官方 v160.7 服务端 jar（JitPack 上的旧依赖已无法获取），见[模组验证器](./Mindustry模组验证器.md) | 旧包 AGPL-3.0（副本自带 `LICENSE`）；项目 README 声明 MIT；`src/main/resources/` 中的地图、基地蓝图与图标来自 Mindustry（GPL-3.0） | CI `Mindustry 真实运行验收 / 继承的模组验证器`：从源码构建，示例模组正确的通过、写错的失败并指出出错内容 |

## 未继承（及原因）

| 旧包内容 | 原因 | 后续条件 |
| --- | --- | --- |
| `knowledge/` 其余部分（`mindustry-source-files/` 1769 篇源码摘录、`mindustry-package-analysis/` 自动生成的包摘要、`sources/` 58 MB 含许可未说明的第三方教程副本、其他主题的 `concepts/`、`analysis/` 与 `entities/` 中的非 Mindustry 内容）、正文命中敏感模式的 15 篇；`memory/` 的身份、关系与环境小节；`data/` 向量索引 | 体积大且以源码摘录或模板摘要为主，许可不明，或混有旧环境与非 Mindustry 内容；旧向量索引依赖旧的嵌入模型 | 按同样的导入规则逐类评估；语义检索沿用 Eve 自己的 `--semantic-recall` |
| `skills/*/SKILL.md` 及其余脚本（含蓝图解析、地图生成、API 解析等其他 Mindustry 工具）；`AGENTS.eve.md`、`SELF_TEST.md`、`USER.md`、`identity/`、`affection/`、地图配置与模组压缩包 | 大多是流程文档或依赖旧环境（固定路径、xvfb、Gradle、旧账号）；Mindustry 相关只迁移验证器；`AGENTS.eve.md` 是身份、规则、风格与主人资料的汇编，已继承的部分见上，其余含旧主人资料；两个模组压缩包不是有效的 zip | 每项单独评估是否有可执行接入点 |
| `源码和示例/Mindustry`、`Arc` 源码副本 | 体积大且是特定 commit 的快照 | 需要时通过 `github_api` 按 commit 查证，而不是复制进仓库 |
| 无头 UI 测试与完整 GL 验证器（`project/mindustry-mod-validator-full/`、`skills/mindustry-headless-ui-test`）；旧包中的预编译验证器 jar | 依赖 xvfb/GL 环境；预编译 jar 来源无法核对，改为从迁入的源码构建 | 与现有 `Mindustry 真实运行验收` 作业合并评估 |
| 语言风格中的粗口、高频梗与 6～15 字硬性上限；人格设定、情感系统 | 与当前稳定身份“直接、自然、不刻意卖萌”冲突，或与动态状态、表达学习重叠 | 不继承；表达节奏由会话内表达学习按用户原话调整 |
| 执行规则中的管理员 ID、旧工作区路径、`[null]` 回复、`qq_sender`、AnySearch、`eve_vector_search` | 属于旧宿主的账号、环境与工具，Eve.aic 没有对应装配；身份使用当前宿主绑定 | 不继承具体账号与工具；主人/管理员/普通用户的管理边界已按现有契约接入，见[管理权限](./管理员绑定与默认权限.md) |
| Mindustry JSON 校验规则（`tools/mindustry_json_validator.py`）及其预检接线 | 曾在 [#201](https://github.com/ZenthXSin/Eve.aic/pull/201)、[#203](https://github.com/ZenthXSin/Eve.aic/pull/203) 继承，现已删除：规则是 `ContentParser.java` 的手写复刻，会误报合法字段，且需要额外的 Python 解释器；实践是否通过本来就以真实服务端加载为准 | 不再继承；草稿问题由真实运行与[工具锻造](./工具锻造.md)从实践中得到的检查发现 |
| `bootstrap.sh`、定时任务、自动恢复脚本 | 旧包安装流程会改动环境 | 不运行、不移植 |

## 复现方式

```sh
python3 scripts/import_portable_knowledge.py <eve-portable 固定版本检出>
cargo test -p eve-knowledge-plugin --locked
```
