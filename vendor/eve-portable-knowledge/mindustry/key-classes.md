# Key Classes Analysis

> Source: Mindustry/Arc 源码关键类速查

## 全局入口类

| 类 | 路径 | 职责 |
|----|------|------|
| Vars | `mindustry.Vars` | Mindustry全局静态字段（476行） |
| Core | `arc.Core` | Arc全局静态引用 |

## 游戏循环核心

| 类 | 路径 | 行数 | 职责 |
|----|------|------|------|
| Logic | `mindustry.core.Logic` | 586 | 游戏更新循环、波次、胜利条件 |
| GameState | `mindustry.core.GameState` | 122 | 游戏状态（mode, sector, rules） |
| World | `mindustry.core.World` | 664 | 地图加载、Tile管理、寻路 |
| Control | `mindustry.core.Control` | 763 | 输入、存档、音效（非无头） |

## 世界系统

| 类 | 路径 | 职责 |
|----|------|------|
| Tile | `mindustry.world.Tile` | 瓦片（floor, overlay, block, building） |
| Block | `mindustry.world.Block` | 方块定义（500+字段） |
| Building | `mindustry.gen.Building` | 建筑实体（编译时生成） |

## 内容系统

| 类 | 路径 | 职责 |
|----|------|------|
| Content | `mindustry.ctype.Content` | 内容基类 |
| ContentLoader | `mindustry.core.ContentLoader` | 内容加载、注册 |
| ContentParser | `mindustry.mod.ContentParser` | JSON/JS解析 |
| Block | `mindustry.type.Block` | 方块类型（继承链最长） |
| UnitType | `mindustry.type.UnitType` | 单位类型 |
| Item | `mindustry.type.Item` | 物品 |
| Liquid | `mindustry.type.Liquid` | 液体 |

## 实体系统

| 类 | 路径 | 职责 |
|----|------|------|
| Unit | `mindustry.gen.Unit` | 单位实体（编译时生成） |
| Bullet | `mindustry.gen.Bullet` | 子弹实体 |
| Groups | `mindustry.gen.Groups` | 实体分组管理 |

## 网络系统

| 类 | 路径 | 行数 | 职责 |
|----|------|------|------|
| Net | `mindustry.net.Net` | 432 | 协议层（TCP/UDP） |
| NetServer | `mindustry.core.NetServer` | 1300 | 服务端逻辑 |
| NetClient | `mindustry.core.NetClient` | 766 | 客户端逻辑 |
| Call | `mindustry.gen.Call` | 自动生成 | 远程调用（@Remote注解） |

## Mod系统

| 类 | 路径 | 职责 |
|----|------|------|
| Mod | `mindustry.mod.Mod` | Mod基类 |
| Mods | `mindustry.mod.Mods` | Mod管理器 |
| ContentParser | `mindustry.mod.ContentParser` | JSON内容解析 |

## 渲染系统

| 类 | 路径 | 行数 | 职责 |
|----|------|------|------|
| Renderer | `mindustry.core.Renderer` | 629 | 渲染管线 |
| BlockRenderer | `mindustry.graphics.BlockRenderer` | - | 方块渲染 |
| FloorRenderer | `mindustry.graphics.FloorRenderer` | - | 地面渲染 |
| Shaders | `mindustry.graphics.Shaders` | - | 着色器管理 |

## 关联

- [Project Structure](project-structure.md)
- [Core Package Overview](core-packages.md)
- [Vars System](vars-system.md)
