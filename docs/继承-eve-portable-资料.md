# 继承 eve-portable 的 Mindustry 资料

旧 Eve（eve-portable）整理过大量 Mindustry 资料：各子系统的源码梳理、模组制作指南、单个机制的问题记录，以及长期记忆里的项目经验。本文说明 Eve.aic 怎样把它们接入[受控研究](./受控研究与领域知识.md#本地资料来源)。

## 导入了什么

`scripts/import_portable_knowledge.py` 从固定版本 `e7e43b4db4de38775a8d28f59dd4ec69e5e1686e` 生成 `vendor/eve-portable-knowledge/`，重复运行结果相同，共 186 篇：

| 目录 | 来源 | 篇数 |
| --- | --- | --- |
| `mindustry/` | `knowledge/mindustry/` | 48 |
| `mod-making-guide/` | `knowledge/mod-making-guide/` | 12 |
| `concepts/` | `knowledge/concepts/` 中以 `mindustry-`、`arc-`、`json-`、`mdt-` 开头的文件 | 125 |
| `memory/technical-notes.md` | `memory/MEMORY.md` 的技术小节 | 1 |

- 命中敏感模式（本机路径、IP、长数字、令牌字样）的文件整篇排除，这次排除 7 篇，导入脚本会列出它们。
- 长期记忆只保留 Mindustry 机制与各项目的技术经验；身份、关系、环境小节不导入，并逐行去掉含路径、主机或凭据模板的行。
- 资料按 AGPL-3.0 单独授权，随副本附 `LICENSE` 与 `README.md`，不属于本仓库的 MIT 许可，也不被编译进可执行文件。

## 怎么用

```sh
./target/debug/eve-qqbot --interest-learning \
  --research-corpus vendor/eve-portable-knowledge \
  --research-source eve-corpus://eve-portable/mindustry/ \
  --research-source eve-corpus://eve-portable/mod-making-guide/
```

`eve-corpus://` 入口可以和网页入口混用，合计至多 8 个。学习目标进入研究后，Eve 从资料目录里选文、逐字引用，知识的来源地址记为 `eve-corpus://eve-portable/...`。

一次研究最多看到 40 个候选、选 3 篇，所以入口宜指向具体目录（例如 `mindustry/`），而不是整个资料根目录；`concepts/` 有 125 篇，目录页分两页，第二页要靠第一页末尾的链接进入候选。

## 持久化与恢复

- 资料本身只读，不写入状态目录；研究抓到的资料文档、知识条目与研究记录照常保存在知识账本中，恢复语义与网页来源相同：重启不重新抓取、不重放研究。
- 启动时核对清单摘要，资料被改动或缺失就拒绝启动，不会清空或改写知识账本里已有的记录。
- 资料更新只能通过导入脚本生成新副本并走 PR；文档摘要随内容变化，旧研究保存的文档保留原摘要，可以分辨来自哪个版本。

## 边界

- 这些是旧 Eve 整理的二手资料，不是源码本身。按 AGENT.md，Mindustry 结论要有可核对的出处；研究得到的“来源原文”只说明资料这样写，不代表已经核实或实践验证。
- 个别资料对应的游戏版本与现在不同，引用时以原文写明的版本为准。
- `mindustry-source-files/`（1769 篇源码摘录）、`sources/`（58 MB）、`entities/`、`analysis/` 和不以上述前缀开头的 `concepts/` 文件这次没有导入：前两者体积大且以源码摘录为主，后几类混有旧环境与非 Mindustry 内容，命中敏感模式的比例也高。需要时按同样的规则单独评估。
