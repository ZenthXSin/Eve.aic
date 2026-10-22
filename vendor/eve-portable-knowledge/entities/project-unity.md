# ProjectUnityPublic

- **仓库**: [Avant-Team/ProjectUnityPublic](https://github.com/Avant-Team/ProjectUnityPublic)
- **版本**: 0.2-youngcha.3.3 (分支 `4775723`)
- **作者**: Avant Team（多人大协作）
- **语言**: Java（576 源文件，~148,916 行）
- **类型**: Java mod（`unity.Unity`）
- **minGameVersion**: 157
- **本地路径**: `源码和示例/mods/ProjectUnityPrivate_V8`

## 定位

Mindustry mod 社区最大的协作项目，由多位作者共同创作，涵盖图系统、模块化单位、3D 模型渲染、过场动画、经验系统等几乎所有 Mindustry 子系统的深度扩展。

## 包结构（58 个包）

| 包 | 源文件数 | 代码行 | 内容 |
|---|---------|--------|------|
| `unity/gen/` | 105 | 77,003 | 注解处理器 + JavaPoet 自动生成的实体类（WormUnit、MonolithUnit 等） |
| `unity/world/` | 147 | 17,427 | 方块实现（defense、distribution、effect、environment、light、power、production、units） |
| `unity/content/` | 25 | 15,893 | 内容定义（UnityBlocks 3395行、UnityUnitTypes 5388行） |
| `unity/entities/` | 120 | 14,609 | 实体（bullet、abilities、legs、units、effects） |
| `unity/type/` | 28 | 3,934 | 自定义类型（weapons、decal） |
| `unity/ai/` | 18 | 2,647 | 自定义 AI（kami 包） |
| `unity/ui/` | 14 | 1,620 | UI（dialogs、canvas） |
| `unity/graphics/` | 9 | 1,476 | 渲染 |
| `unity/mod/` | 8 | 857 | mod 系统（AntiCheat、Faction、MusicHandler、TimeStop） |
| `unity/map/` | — | — | 地图系统（cinematic、objectives、planets） |
| `unity/logic/` | — | — | 逻辑扩展 |
| `unity/sync/` | 2 | 56 | 网络同步 |

## 核心特色

### Graph<T> 图系统
- 通用图数据结构，支持：
  - TorqueGraph（力学）
  - HeatGraph（热力）
  - CrucibleGraph（冶金）
- 物理量守恒的图合并
- 帧 ID 去重更新机制
- Gauss-Seidel 扩散算法

### 模块化单位
- `unity.parts.*` — ModularPartType + ModularUnitAssembler
- 可视化编辑器

### 3D 模型渲染
- `unity/assets/type/g3d/` — Model、ModelLoader
- 3D 相机（Camera3D）
- 带光照属性的材质系统

### 过场动画系统
- `unity/map/cinematic/` — Cinematics、Cutscene、Speeches
- CinematicDialog、CinematicEditor

### 经验系统
- ExpOrbs 实现为 BulletType
- 完整经济链
- FactionMeta 派系元数据

### 自定义子弹（25+ 种）
- 按类别分子包：anticheat、energy、kami、laser、misc、monolith、physical
- 代表：SingularityBulletType（黑洞）、GluonOrbBulletType（胶子球）、HealingNukeBulletType

### 自定义炮塔（17 种）
- Monolith 系列巨型炮塔
- 防御、分配、效果、环境、发电、生产、沙盒等多类方块

### 自定义 AI（9 种）
- kami 包下为不同单位提供定制 AI

### 注解处理器代码生成
- `unity/gen/` 包含 105 个自动生成的实体类文件
- 使用 @EntityDef、@Merge、@Dupe 注解
- JavaPoet 代码生成

### 反作弊系统
- `unity.mod.AntiCheat` — 反作弊验证
- `AntiCheatBulletTypeBase` — 反作弊子弹基础类

## 值得学习的模式
- 注解处理器 + JavaPoet 代码生成架构
- Graph<T> 通用图数据结构设计
- 3D 模型渲染管线
- 过场动画系统
- 模块化单位组件系统
- 帧 ID 去重更新
- Gauss-Seidel 扩散算法
- 派系（Faction）系统

## 参考
- [ProjectUnityPublic GitHub](https://github.com/Avant-Team/ProjectUnityPublic)
- [ProjectUnity README.txt](../../源码和示例/mods/ProjectUnityPrivate_V8/README.txt)
