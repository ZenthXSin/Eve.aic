# Mindustry 源码内容分析启动

> Source: 本地源码 `源码和示例/Mindustry`、`源码和示例/Arc`；已核对包级页 `knowledge/mindustry-package-analysis/overview.md`、文件级页 `knowledge/mindustry-source-files/`、示例 Mod 抽样。

## 当前分析入口

这次从源码内容本身开始，而不是继续扩目录。优先入口选择：

1. `mindustry.world` — 世界/方块/地块核心，直接关联 Mod 方块开发。
2. `mindustry.entities.bullet` — 子弹与伤害系统，直接关联炮塔和武器开发。
3. `mindustry.type` / `mindustry.content` — 内容注册和原版内容定义。
4. `mindustry.mod` — Mod 加载、脚本、依赖与错误处理。
5. `arc.struct` / `arc.util.serialization` — Arc 集合与 JSON 序列化，影响 Mindustry JSON、存档和运行时数据结构。

## 已确认的第一批源码证据

- `mindustry.world.Block`：源码路径 `源码和示例/Mindustry/core/src/mindustry/world/Block.java`，声明 `class Block extends UnlockableContent implements Senseable`，关键方法线索包括 `drawBase`、`drawPlace`、`setStats`、`setBars`、`canPlaceOn`。
- `mindustry.world.Tile`：源码路径 `源码和示例/Mindustry/core/src/mindustry/world/Tile.java`，声明 `class Tile implements Position, QuadTreeObject, Displayable`，关键方法线索包括 `pos`、`relativeTo`、`worldx`、`worldy`、`isDarkened`。
- `arc.struct.Seq`：源码路径 `源码和示例/Arc/arc-core/src/arc/struct/Seq.java`，声明 `class Seq<T> implements Iterable<T>, Eachable<T>`，关键方法线索包括 `add`、`remove`、`each`、`mapInt`、`allMatch`。
- `arc.util.serialization.Json`：源码路径 `源码和示例/Arc/arc-core/src/arc/util/serialization/Json.java`，声明 `class Json`，关键方法线索包括 `setIgnoreUnknownFields`、`addClassTag`、`getClass`、`getTag`。

## 初步判断

- Mindustry 的 Mod API 不是独立 DSL，而是围绕 `UnlockableContent`、`Block`、`UnitType`、`BulletType` 等 Java 类型直接暴露字段和生命周期方法。
- 方块系统核心要从 `Block` 的静态配置字段和 `Building`/派生类的运行时行为分开看。
- JSON 和 JS Mod 的很多问题，最终会落到 Arc `Json` 反序列化、Mindustry `ContentLoader`、以及具体内容类的 public 字段上。
- `Seq` 等 Arc 容器不是普通 JDK 集合替代品，而是 Mindustry 全局内容表、实体集合和 UI 构建中高频使用的基础结构。

## Related

- [Mindustry / Arc Package-Oriented File Analysis](../mindustry-package-analysis/overview.md)
- [Mindustry / Arc Per-File Knowledge Pages](../mindustry-source-files/overview.md)
- [Block and Building System](../mindustry/block-building-system.md)
- [Mindustry JSON Mod 反序列化流程](../concepts/mindustry-json-mod-deserialization.md)

## 第二轮分析：内容加载链

从 `mindustry.core.ContentLoader`、`mindustry.mod.ContentParser`、`arc.util.serialization.Json` 三条线交叉看，内容定义不是孤立对象，而是：

1. `ContentLoader` 负责把内容表装进 `Vars.content`。
2. `ContentParser` 负责把 JSON 内容反序列化成具体的 `BulletType`、`Consume*`、`Draw*`、`UnitReq` 等对象。
3. `Json.readValue(...)` 是底层入口。
4. 具体类型最后回到 `Block`、`BulletType`、`UnitType` 的 public 字段和生命周期方法。

## 第二轮分析：示例 Mod 对照

- `源码和示例/mods/BetaMindy/src/betamindy/content/MindyBullets.java`：证明自定义 `BulletType` 常见做法是直接 new 具体子类并填字段。
- `源码和示例/mods/ProjectUnityPrivate_V8/src/unity/content/UnityBullets.java`、`UnityUnitTypes.java`：说明大型 Mod 会集中定义内容表，再在初始化阶段注册。
- `源码和示例/mods/MultiCrafterLib/java/src/WithJsonMod.java`：说明 JSON 与 Java 混用时，常见策略是 Java 提供扩展点，JSON 提供结构化配置。

## 下一步继续解析

优先级不变：

1. `mindustry.world.Block` / `Tile` / `Building`
2. `mindustry.entities.bullet.BulletType` 及其派生类
3. `mindustry.type.UnitType` / `Item` / `Liquid`
4. `mindustry.mod.ContentParser` / `ContentLoader`
5. 对照示例 Mod 的内容注册入口


## 第三轮分析：Tile、Block、ContentParser 三点联动

从 `Tile`、`Block`、`ContentParser` 这三处交叉看，Mindustry 的内容系统有一个很明确的分层：

- `Tile` 解决世界位置与格子状态。
- `Block` 解决“这个方块是什么、能做什么、怎么展示”。
- `ContentParser` 解决“JSON 如何变成这些对象，以及如何把名字绑定回内容实例”。

这意味着 JSON mod 并不是独立数据驱动层，而是对原生 Java 内容对象的字段级填充与后处理。

## 第三轮分析：示例 Mod 的实际写法

- `源码和示例/mods/BetaMindy/src/betamindy/content/MindyBullets.java` 中，`BasicBulletType` 被直接 new 出来后，覆盖 `draw(Bullet)`、`update(Bullet)` 等行为，说明 Mod 常通过匿名子类快速替换原版弹体表现。
- `源码和示例/mods/ProjectUnityPrivate_V8/src/unity/content/UnityUnitTypes.java` 中，单位内容集中定义，且内部武器大量使用 `BasicBulletType`，说明大型 Mod 倾向先集中声明内容，再统一注册。
- `源码和示例/mods/MultiCrafterLib/java/src/WithJsonMod.java` 证明 Java Mod 里也会直接在 `loadContent()` 中构造内容对象，和 JSON 配置不是互斥关系。

## 第三轮分析：目前可以落成的知识结论

1. `mindustry.world.Block` 是所有可放置方块的核心静态定义入口。
2. `mindustry.world.Tile` 是地形格子、世界坐标和建筑挂载的基础。
3. `mindustry.entities.bullet.BulletType` 是炮塔/武器伤害系统的抽象核心。
4. `mindustry.type.UnitType` 决定单位生成、控制、AI、显示和武器组合。
5. `mindustry.mod.ContentParser` + `arc.util.serialization.Json` 是 JSON mod 的实际解析入口。
6. 示例 Mod 验证了这些类在真实项目里都是“直接 new + 填字段 + 覆盖少量方法”的主流模式。


## 第四轮分析：BulletType 与 UnitType 的对象模型

从 `mindustry.entities.bullet.BulletType` 和 `mindustry.type.UnitType` 看，Mindustry 的内容对象不是纯配置表，而是带大量 public 字段的“半行为对象”。

- `BulletType` 直接暴露 lifetime、speed、damage、pierce、splash、status、effect、sound 等字段，说明弹体定义本质上就是可序列化的行为参数集合。
- `UnitType` 直接暴露 speed、health、armor、range、environment、payload、build/mine 参数与图形层参数，说明单位类型同样是运行期对象配置核心。
- `BetaMindy` 里常见 `new BasicBulletType(...) {{ ... }}` 这种写法，说明 Java Mod 倾向用匿名子类+字段填充快速表达弹体差异。
- `ProjectUnityPrivate_V8` 把单位与弹体集中放在内容类里统一声明，说明大 Mod 会把内容注册和行为定制放在同一层组织。

## 第四轮分析：目前可以追加的知识结论

1. `BulletType` 与 `UnitType` 都是面向内容系统的 runtime content class，不是单纯的配置结构。
2. `BulletType` 更像弹体行为参数总表，`UnitType` 更像单位生存/移动/建造/战斗参数总表。
3. Java 与 JSON 只是两种创建和填充这些对象的入口。
4. 当字段不够时，Mod 才通过覆盖方法补足行为。



## 第五轮分析：Building 与 GenericCrafter 的运行时链路

从 `mindustry.world.Block`、`mindustry.entities.comp.BuildingComp`、`mindustry.world.blocks.production.GenericCrafter` 交叉看，方块到建筑实例的链路可以明确拆成三层：

1. `Block` 保存静态定义：生命、容量、可否产物、drawer、stats、bars。
2. `BuildingComp.create(Block block, Team team)` 负责把 `Block` 绑定到建筑实体，并按 `hasItems`、`hasLiquids`、`hasPower` 创建对应模块。
3. `GenericCrafter.GenericCrafterBuild` 在 `updateTile()` / `craft()` 里执行实际生产行为。

### 已核对的源码证据

- `mindustry.world.Block`：`源码和示例/Mindustry/core/src/mindustry/world/Block.java`
- `mindustry.entities.comp.BuildingComp`：`源码和示例/Mindustry/core/src/mindustry/entities/comp/BuildingComp.java`
- `mindustry.world.blocks.production.GenericCrafter`：`源码和示例/Mindustry/core/src/mindustry/world/blocks/production/GenericCrafter.java`

### 关键观察

- `BuildingComp.create(Block block, Team team)` 会根据 `block.hasItems`、`block.hasLiquids`、`block.hasPower` 初始化 `ItemModule`、`LiquidModule`、`PowerModule`，说明建筑模块不是随便挂的，是由方块静态字段驱动。
- `GenericCrafter` 构造器默认设置 `update = true`、`solid = true`、`hasItems = true`、`sync = true`，说明它不是“纯显示方块”，而是明确的运行时生产节点。
- `GenericCrafter.init()` 会把 `outputItem` / `outputItems`、`outputLiquid` / `outputLiquids` 互相归并，说明 JSON 或 Java 入口都只是给同一套运行时对象补字段。
- 示例 mod `BetaMindy` 的 `LiquidRefiner extends GenericCrafter` 证明大型 mod 通常直接继承原版生产基类，只覆盖少量行为。

### 结论

- `Block` 决定建筑类型的静态边界。
- `BuildingComp` 决定建筑实例的模块装配。
- `GenericCrafter` 决定生产类方块的实际工艺循环。
- JSON / Java / 示例 mod 最终都落到同一条运行时链路上。

## Related

- [Mindustry Content Loading Chain](../concepts/mindustry-content-loading-chain.md)
- [Mindustry JSON and Java Hybrid Mod Pattern](../concepts/mindustry-json-and-java-hybrid-mod-pattern.md)
- [Mindustry BulletType and UnitType Content Pattern](../concepts/mindustry-bullet-unit-content-pattern.md)
- [Block and Building System](../mindustry/block-building-system.md)


## 第六轮分析：Mod 生命周期与 JSON 解析器的分工

从 `ContentLoader`、`ContentParser`、`BetaMindy`、`ProjectUnityPrivate_V8`、`MultiCrafterLib` 对照看，Mindustry JVM Mod 的内容进入游戏通常分成两条互补路径：

1. Java/Kotlin 路径：继承 `mindustry.mod.Mod`，在 `loadContent()` 中 `new` 内容对象并填字段。
2. JSON 路径：由 `mindustry.mod.ContentParser` 解析 JSON，按字段 `type` 或默认类型创建对象，再通过反射/专用 parser 填字段。

### 已核对源码证据

- `mindustry.core.ContentLoader`：`源码和示例/Mindustry/core/src/mindustry/core/ContentLoader.java`
  - `createBaseContent()` 先加载原版内容：Items、Liquids、Bullets、UnitTypes、Blocks 等。
  - `createModContent()` 调用 `mods.loadContent()`。
  - `init()` 调用 `Content::init`、`Content::postInit` 并触发 `ContentInitEvent`。
  - `load()` 调用 `Content::loadIcon`、`Content::load`。
- `mindustry.mod.ContentParser`：`源码和示例/Mindustry/core/src/mindustry/mod/ContentParser.java`
  - `BulletType`：字符串走 `Bullets` 字段查找，数组生成 `MultiBulletType`，对象用 `type` 解析到具体 BulletType，默认 `BasicBulletType`。
  - `DrawBlock`：字符串尝试实例化，数组是 `DrawMulti` 简写，对象用 `type` 解析，默认 `DrawDefault`。
  - `DrawPart` 默认 `RegionPart`，`Weapon` 默认 `Weapon`，`Consume` 有专用解析入口。
- `BetaMindy`：`源码和示例/mods/BetaMindy/src/betamindy/BetaMindy.java`
  - 构造器注册事件和设置。
  - `init()` 做评分、UI、Trigger 更新逻辑。
  - `loadContent()` 依次加载 StatusEffects、Liquids、Items、Bullets、Blocks、Planets、TechTree。
- `ProjectUnityPrivate_V8`：`源码和示例/mods/ProjectUnityPrivate_V8/src/unity/Unity.java`
  - 使用 `Events.on(ContentInitEvent/FileTreeInitEvent/ClientLoadEvent)` 做后续初始化。
  - `loadContent()` 是内容注册入口。
- `MultiCrafterLib`：`源码和示例/mods/MultiCrafterLib/java/src/WithJsonMod.java`
  - 在 `loadContent()` 中直接 `new MultiCrafter("java-crafter")` 并用 Java 填 `resolvedRecipes`，证明 Java 配置和 JSON 配置最终服务同一对象模型。

### 结论

- `loadContent()` 是 JVM Mod 内容对象进入 `ContentLoader` 的关键阶段。
- `init()` 更适合做内容已存在之后的索引、评分、UI、事件绑定和运行期系统初始化。
- JSON `type` 不是随意字符串，而是 `ContentParser.resolve(...)` 的类解析入口；不同字段有不同默认类型。
- 大型 Mod 通常混用：Java/Kotlin 负责复杂行为和生命周期，JSON/数据结构负责可配置内容。

## 第七轮分析：GenericCrafter 生产循环源码确认

`mindustry.world.blocks.production.GenericCrafter.GenericCrafterBuild` 的实际循环已经确认：

1. `updateTile()` 在 `efficiency > 0` 时增加 `progress` 和 `warmup`。
2. 液体输出在生产过程中按 `getProgressIncrease(1f)` 连续写入 `LiquidModule`。
3. `progress >= 1f` 时调用 `craft()`。
4. `craft()` 先 `consume()`，再按 `outputItems` 调 `offload(...)` 输出物品，最后播放 `craftEffect` 并 `progress %= 1f`。
5. `dumpOutputs()` 按 `timerDump` 输出物品，并按 `liquidOutputDirections` 输出液体。
6. `write()` / `read()` 保存和恢复 `progress`、`warmup`。

这说明自定义工厂若继承 `GenericCrafter`，最小改法通常不是重写整个方块，而是选择性覆盖 `canConsume()`、`getProgressIncrease()`、`craft()`、`dumpOutputs()` 或子类 `Build` 的状态读写。
