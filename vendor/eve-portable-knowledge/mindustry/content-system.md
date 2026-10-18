# Mindustry 内容系统源码验证

> Source: `源码和示例/Mindustry/core/src/mindustry/ctype/` (4个文件, 430行)

## Content 基类

### 字段
| 字段 | 类型 | 说明 |
|------|------|------|
| id | short | 自增ID（构造时自动分配） |
| minfo | ModContentInfo | Mod来源信息 |

### ModContentInfo 内部类
| 字段 | 类型 | 说明 |
|------|------|------|
| mod | LoadedMod | 加载此内容的Mod（null=原版） |
| sourceFile | Fi | 源文件 |
| error | String | 加载错误信息 |
| baseError | Throwable | 错误异常 |

### 生命周期
```
构造函数（自动注册到ContentLoader）
→ init()        // 所有内容和模块创建后调用，不要在此加载纹理！
→ postInit()    // init()之后调用
→ loadIcon()    // load()之前调用，仅非headless
→ load()        // 所有内容创建后调用，仅非headless，用于加载纹理
→ afterPatch()  // 被patch后调用
```

### 关键方法
| 方法 | 说明 |
|------|------|
| getContentType() | 返回ContentType枚举 |
| isVanilla() | 是否原版内容（minfo.mod == null） |
| isModded() | 是否Mod内容 |
| hasErrored() | 加载是否出错 |

## ContentType 枚举（18个）

| 枚举值 | 内容类 | 状态 |
|--------|--------|------|
| item | Item.class | 正常 |
| block | Block.class | 正常 |
| mech_UNUSED | null | 已废弃 |
| bullet | BulletType.class | 正常 |
| liquid | Liquid.class | 正常 |
| status | StatusEffect.class | 正常 |
| unit | UnitType.class | 正常 |
| weather | Weather.class | 正常 |
| effect_UNUSED | null | 已废弃 |
| sector | SectorPreset.class | 正常 |
| loadout_UNUSED | null | 已废弃 |
| typeid_UNUSED | null | 已废弃 |
| error | null | 特殊 |
| planet | Planet.class | 正常 |
| ammo_UNUSED | null | 已废弃 |
| team | TeamEntry.class | 正常 |
| unitCommand | UnitCommand.class | 正常 |
| unitStance | UnitStance.class | 正常 |

**有效内容类型**：item, block, bullet, liquid, status, unit, weather, sector, planet, team, unitCommand, unitStance（共12个）

## UnlockableContent 基类

### 继承链
```
Content → MappableContent → UnlockableContent
```

### 核心字段
| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| localizedName | String | 自动从bundle加载 | 本地化名称 |
| description | String | null | 本地化描述 |
| details | String | null | 详细描述 |
| credit | String | null | 制作人 |
| alwaysUnlocked | boolean | false | 是否总是解锁 |
| inlineDescription | boolean | true | 研究界面是否显示描述 |
| hideDetails | boolean | true | 自定义游戏中未解锁是否隐藏详情 |
| hideDatabase | boolean | false | 是否在数据库中隐藏 |
| generateIcons | boolean | true | 是否生成图标 |
| selectionSize | float | 24f | 选择菜单中的大小 |

### 图标字段
| 字段 | 类型 | 说明 |
|------|------|------|
| uiIcon | TextureRegion | UI图标 |
| fullIcon | TextureRegion | 完整图标（未缩放） |
| fullOverride | String | 完整图标覆盖（用于重复图标） |

### 数据库分类字段
| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| allDatabaseTabs | boolean | false | 是否在所有数据库标签页显示 |
| shownPlanets | ObjectSet\<Planet\> | empty | 目标星球 |
| databaseTabs | ObjectSet\<UnlockableContent\> | empty | 数据库标签页 |
| databaseCategory | String | getContentType().name() | 主分类 |
| databaseTag | String | "default" | 子分类标签 |

### 科技树字段
| 字段 | 类型 | 说明 |
|------|------|------|
| techNode | TechNode | 当前科技树节点（null=不在树中） |
| techNodes | Seq\<TechNode\> | 所有科技树节点 |
| unlocked | boolean | 解锁状态（从settings加载） |

### 图标加载顺序（loadIcon）
```
fullOverride → contentType-name-full → name-full → name → contentType-name → name1
uiIcon = contentType-name-ui → fullIcon
```

### 解锁系统
| 方法 | 说明 |
|------|------|
| unlock() | 解锁并触发UnlockEvent |
| quietUnlock() | 静默解锁（不触发事件） |
| unlocked() | 联机时检查host解锁状态，单机检查本地 |
| unlockedNow() | unlocked() || 非战役模式 |
| unlockedHost() | 联机host检查rules.researched |
| locked() | !unlocked() |
| clearUnlock() | 重新锁定 |

### Stats 系统
- stats 字段按需初始化（checkStats()）
- setStats() 只在首次显示时调用一次
- afterPatch() 重置stats

## 硬性规则

1. **Content 构造函数自动注册** — 不需要手动添加到ContentLoader
2. **id 是 short 类型** — 不是int，最大32767
3. **init() 不要加载纹理** — 源码注释明确说明
4. **load() 只在非headless调用** — 服务端不调用
5. **loadIcon() 在 load() 之前** — 图标先于纹理加载
6. **ContentType 不要重排序** — 源码注释"Do not rearrange, ever!"
7. **有效ContentType只有12个** — 6个UNUSED不算
8. **UnlockableContent 继承 MappableContent** — 不是直接继承Content
9. **localizedName 自动从bundle加载** — 格式：`contentType.name.name`
10. **unlocked 从 Core.settings 加载** — 不是存档，是全局设置
11. **联机时 unlocked() 检查 state.rules.researched** — 不是本地settings
12. **allDatabaseTabs 默认 false** — StatusEffect默认true
13. **databaseCategory 默认 getContentType().name()** — postInit()中设置
14. **shownPlanets 为空时用 requirements 推断** — 只对block有意义
15. **fullOverride 覆盖任何其他full icon** — 源码注释说明

## 关联
- [Modding API](modding-api.md)
- [Mod Loading System](mod-loading-system.md)
- [Content Loading Chain](../concepts/mindustry-content-loading-chain.md)
- [Research Tech Tree System](research-tech-tree-system.md)
