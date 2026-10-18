# Mindustry Mod Lifecycle and Parser Boundary

> Source: 本地源码 `源码和示例/Mindustry/core/src/mindustry/core/ContentLoader.java`、`源码和示例/Mindustry/core/src/mindustry/mod/ContentParser.java`，以及示例 Mod `BetaMindy`、`ProjectUnityPrivate_V8`、`MultiCrafterLib`。

## Key Points

- `mindustry.core.ContentLoader.createBaseContent()` 先创建原版内容，`createModContent()` 再进入 mod 内容加载。
- JVM Mod 的内容注册入口是 `mindustry.mod.Mod.loadContent()`。
- `ContentLoader.init()` 统一调用 `Content::init`、`Content::postInit` 并触发 `ContentInitEvent`。
- `ContentLoader.load()` 统一调用 `Content::loadIcon`、`Content::load`。
- `mindustry.mod.ContentParser` 负责 JSON 到运行时对象的边界转换。
- JSON 中的 `type` 是类解析入口，不是自由标签；不同字段有不同默认类型，例如：
  - `BulletType` 默认 `BasicBulletType`，数组生成 `MultiBulletType`。
  - `DrawBlock` 默认 `DrawDefault`，数组生成 `DrawMulti`。
  - `DrawPart` 默认 `RegionPart`。
  - `Weapon` 默认 `Weapon`。

## Example Mod Evidence

- `源码和示例/mods/BetaMindy/src/betamindy/BetaMindy.java`：`loadContent()` 依次加载状态、液体、物品、子弹、方块、星球、科技树；`init()` 做评分、UI、Trigger。
- `源码和示例/mods/ProjectUnityPrivate_V8/src/unity/Unity.java`：使用 `ContentInitEvent`、`FileTreeInitEvent`、`ClientLoadEvent` 做后续初始化，`loadContent()` 负责内容注册。
- `源码和示例/mods/MultiCrafterLib/java/src/WithJsonMod.java`：在 `loadContent()` 中直接构造 `MultiCrafter` 并填 `resolvedRecipes`，说明 Java 配置和 JSON 配置服务同一对象模型。

## Practical Reading

- Java/Kotlin 适合写复杂行为、生命周期和方法覆盖。
- JSON 适合填公开字段和嵌套对象。
- 当 JSON 字段不工作时，先查 `ContentParser` 有没有专用 parser，再查目标类字段是否真实存在。
- 当 Java 内容初始化失败时，先看注册是否在 `loadContent()`，再看交叉引用是否需要等到 `init()` / `ContentInitEvent`。

## Related

- [Mindustry Content Loading Chain](mindustry-content-loading-chain.md)
- [Mindustry JSON and Java Hybrid Mod Pattern](mindustry-json-and-java-hybrid-mod-pattern.md)
- [Mindustry JSON Mod 反序列化流程](mindustry-json-mod-deserialization.md)
