# Mindustry Content Loading Chain

> Source: Local Mindustry source analysis of `mindustry.core.ContentLoader`, `mindustry.mod.ContentParser`, `arc.util.serialization.Json`, and `mindustry.world.Block` / `mindustry.world.Tile`; reinforced by example mods `BetaMindy`, `ProjectUnityPrivate_V8`, and `MultiCrafterLib`.

## Conclusion

Mindustry content loading is a staged pipeline, not a single JSON parse step.

## Chain

1. `ContentLoader` creates base content and mod content.
2. `ContentParser` turns JSON into typed content objects and special helper objects.
3. `Json` performs the low-level value conversion and class resolution.
4. Content objects are then initialized and loaded in later passes.

## Key Roles

- `mindustry.core.ContentLoader` stores content, maps names to instances, and runs `init()` / `postInit()` / `load()`.
- `mindustry.mod.ContentParser` parses JSON content, resolves fields, and handles special structures such as consumes and nested objects.
- `arc.util.serialization.Json` provides the underlying reflection-based value reading.
- `mindustry.world.Block` and `mindustry.type.UnitType` expose the public fields that JSON and Java mod code usually fill.

## Practical Meaning

- Content is registered before many cross-references are resolved.
- Name lookup is a core part of the system.
- JSON mod and Java mod are both ways to feed the same content pipeline.
- Special fields often have dedicated parsing logic instead of plain reflection.

## Related

- [Mindustry JSON Mod 反序列化流程](mindustry-json-mod-deserialization.md)
- [Mindustry JSON and Java Hybrid Mod Pattern](mindustry-json-and-java-hybrid-mod-pattern.md)
- [Mindustry JSON 问题解决流程](json-problem-solving-flow.md)
- [Mindustry Block and Building System](../mindustry/block-building-system.md)

## Block / Building / Production Chain

从 `mindustry.world.Block`、`mindustry.entities.comp.BuildingComp`、`mindustry.world.blocks.production.GenericCrafter` 看，Mindustry 的方块执行链不是“方块对象自己工作”，而是：

1. `Block` 定义静态属性和展示逻辑。
2. `BuildingComp.create(Block, Team)` 把方块装配成实体，并按 `hasItems`、`hasLiquids`、`hasPower` 初始化模块。
3. `GenericCrafter` 之类的子类在 `updateTile()` / `craft()` 里做实际生产。
4. 示例 Mod 通过继承 `GenericCrafter` 验证这一模式。

这条链和 `ContentLoader` / `ContentParser` 的关系是上下游：前者管世界运行时，后者管内容对象生成。

## Related

- [Mindustry Content Loading Chain](mindustry-json-mod-deserialization.md)
- [Mindustry JSON and Java Hybrid Mod Pattern](mindustry-json-and-java-hybrid-mod-pattern.md)
- [Mindustry Source Content Analysis Start](../analysis/mindustry-source-content-analysis-start.md)
