# 继承自 eve-portable 的 Mindustry 资料

本目录是一份外部资料的固定副本，**不是本仓库 MIT 许可的一部分**。

- 来源：https://gitee.com/zenthxsin/eve-portable
- 版本：`e7e43b4db4de38775a8d28f59dd4ec69e5e1686e`
- 许可证：AGPL-3.0（见同目录 `LICENSE`，随副本一起提供）

内容由 `scripts/import_portable_knowledge.py` 从固定版本生成，重复运行结果相同；不要手工修改本目录。按固定版本 Mindustry 源码更正过的文件列在脚本的 `CORRECTED` 中，重新生成时保留现有内容。

| 目录 | 来源 | 说明 |
| --- | --- | --- |
| `mindustry/` | `knowledge/mindustry/` | 游戏各子系统的源码梳理 |
| `mod-making-guide/` | `knowledge/mod-making-guide/` | 模组制作指南与知名模组分析 |
| `concepts/` | `knowledge/concepts/` 中以 `mindustry-`、`arc-`、`json-`、`mdt-` 开头的文件 | 单个机制或问题的整理 |
| `mod-making/` | `knowledge/mod-making/` | 模组制作中的经验与反模式 |
| `analysis/` | `knowledge/analysis/` 中 Mindustry 与具体模组的技术分析（不含海报、旧助手框架与部署记录） | 崩溃诊断、迁移、校验与平衡分析 |
| `entities/` | `knowledge/entities/` 中的模组与工具条目（不含旧表情索引与 SCP 设定） | 知名模组与工具介绍 |
| `memory/technical-notes.md` | `memory/MEMORY.md` 的技术小节 | 节选，去掉了含本机路径、主机、凭据模板的行 |

只写着旧机器本地路径的引用说明行（`> ` 开头）会被去掉；其余命中敏感模式（本机路径、IP、长数字、令牌字样）的文件整篇排除，导入脚本会列出它们。`MANIFEST.json` 记录每个文件的 SHA-256；Eve 打开资料源时逐个核对，不符就拒绝启用。

Eve 通过受控研究读取这些文件，来源地址形如 `eve-corpus://eve-portable/mindustry/power-system.md`。这些资料是旧 Eve 整理的二手材料，结论对应的游戏版本以原文为准；研究摘录逐字引用原文，不把它们当作已经核实的源码证据。
