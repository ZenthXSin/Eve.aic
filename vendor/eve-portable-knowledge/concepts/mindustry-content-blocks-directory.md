# Mindustry content/block 目录拼写错误

> Source: QQ 群 AC.zip 附件分析；Mindustry `mindustry.mod.Mods` 源码验证。

## 结论

Mindustry JSON/HJSON 内容目录按 `ContentType.name()` 加复数 `s` 搜索：`block` 会变成 `blocks`，所以方块文件必须放在 `content/blocks/`。

`content/block/` 不会被加载器扫描。zip 能通过 `unzip -t` 只能说明压缩包没坏，不代表 mod 内容路径有效。

## 源码依据

`源码和示例/Mindustry/core/src/mindustry/mod/Mods.java` 的内容加载逻辑：

```java
String lower = type.name().toLowerCase(Locale.ROOT);
Fi folder = contentRoot.child(lower + (lower.endsWith("s") ? "" : "s"));
```

对 `ContentType.block`，目录名为 `blocks`。

## 相关

- [JSON 问题解决流程](json-problem-solving-flow.md) — Mindustry JSON 路径和类型问题的排查顺序
- [JSON 常见问题](json-common-issues.md) — JSON mod 常见结构错误
