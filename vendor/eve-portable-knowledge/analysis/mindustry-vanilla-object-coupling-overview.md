# Mindustry 原版对象联动与耦合总览

> Source: conversation synthesis based on Mindustry v158 local source: `Vars.java`, `ContentLoader.java`, `World.java`, `Logic.java`, `Renderer.java`, `GroupDefs.java`, `BuildingComp.java`, `UnitComp.java`, `Team.java`.

## Core Pattern

Mindustry 原版不是干净的分层架构，而是以 `Vars` 全局服务定位器、`Events` 事件总线、`Groups` 实体集合、`GameState` 状态容器和 `ContentLoader` 内容注册表为中心的高耦合运行时。

主要耦合中心：

- `Vars`：持有 `content/world/state/renderer/control/logic/netServer/netClient/pathfinder/indexer/fogControl` 等全局单例。
- `ContentLoader`：先注册静态内容，再调用 `Content.init/postInit/loadIcon/load`，内容对象之间可互相引用。
- `World`：持有 `Tiles`，`Tile` 绑定 `floor/overlay/block/build`，地图加载结束后更新邻接、调整 `Groups` 空间边界并发 `WorldLoadEvent`。
- `Groups`：由注解生成，集中管理 `all/player/bullet/unit/build/sync/draw/fire/puddle/weather/label/powerGraph` 等运行时对象。
- `GameState` / `Rules` / `Teams`：决定游戏模式、队伍核心、AI、资源、科技与胜负。
- `Events`：将构建、摧毁、载入、波次、解锁、渲染、网络等系统松耦合串起来。

## Object Coupling Graph

- Content → Runtime：`Block` 生成 `Building`，`UnitType` 生成 `Unit`，`BulletType` 驱动 `Bullet` 行为。
- World → Building：`Tile` 持有 `Building`，`Building` 反查 `Tile/Block/World`，邻接通过 `nearby/front/back/left/right` 查询。
- Building → Modules：建筑按 `Block` 能力挂 `ItemModule/LiquidModule/PowerModule`，生产、传输、耗电、耗液都走模块。
- Building → Team：建筑有 `Team`，核心建筑进入 `Teams.TeamData.cores`，影响资源、单位上限、胜负。
- Unit → UnitType：单位行为参数来自 `UnitType`，控制器、武器、状态、能力、负载共同决定运行时行为。
- Unit/Building/Bullet → Groups：实体加入对应组后被逻辑更新、渲染、碰撞、同步和查询。
- Logic → State/World/Teams/Groups：每 tick 更新波次、AI、天气、队伍统计、胜负、解锁和事件。
- Renderer → World/Groups/Content：读取地图、建筑、单位、子弹、天气和内容贴图绘制。
- Net → Entity/State/Event：服务端权威处理玩家、同步实体与世界；客户端接收状态并触发 UI/事件。
- Save → Content/World/Entity：存档通过内容 ID 和实体序列化恢复世界。
- Campaign → Sector/Planet/Rules/TechTree：星球、区块、发射、资源与研究互相绑定。

## Modding Implications

- 修改 `Block` 字段通常影响同类型所有建筑实例；实例状态应放在 `Building`。
- 自定义建筑联动优先走邻接、模块、事件和 `Groups` 查询，不要硬改全局系统。
- 涉及渲染、同步、存档、科技树时，要同时检查内容注册、运行时实体、事件、IO 四条链路。

## Related

- [Mindustry Content Loading Chain](../concepts/mindustry-content-loading-chain.md)
- [Mindustry Mod 生命周期与解析器边界](../concepts/mindustry-mod-lifecycle-parser-boundary.md)
- [Block and Building System](../mindustry/block-building-system.md)
- [Entity and ECS System](../mindustry/entity-ecs-system.md)
