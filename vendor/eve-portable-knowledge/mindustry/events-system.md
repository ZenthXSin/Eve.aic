# Mindustry 事件系统源码验证

> Source: `源码和示例/Mindustry/core/src/mindustry/game/EventType.java` (795行)

## 事件总线机制

- **Events.java** (`arc.Events`) — 全局静态事件总线
- 注册：`Events.on(EventType.XxxEvent.class, e -> {...})`
- 触发：`Events.fire(new EventType.XxxEvent(...))`
- **Trigger 枚举** — 高频触发，用 `Events.run(Trigger.update, () -> {...})`
- **Event 类** — 低频事件，用 `Events.on(XxxEvent.class, e -> {...})`
- **Javalin 异步事件**：`Events.on(UnitDestroyEvent.class, event -> ctx.async(() -> {...}))`

## Trigger 枚举（42个）

### 游戏循环
| Trigger | 说明 |
|---------|------|
| update | 每游戏帧 |
| beforeGameUpdate | 游戏更新前 |
| afterGameUpdate | 游戏更新后 |
| newGame | 新游戏开始 |
| tutorialComplete | 教程完成 |

### 渲染
| Trigger | 说明 |
|---------|------|
| draw | 主绘制 |
| drawOver | 绘制覆盖层 |
| preDraw | 绘制前 |
| postDraw | 绘制后 |
| uiDrawBegin | UI绘制开始 |
| uiDrawEnd | UI绘制结束 |
| universeDrawBegin | 宇宙绘制开始（bloom/skybox前） |
| universeDraw | skybox已绘制、bloom启用 |
| universeDrawEnd | 行星绘制完、bloom禁用 |
| enablePixelation | 启用像素化 |

### 战斗
| Trigger | 说明 |
|---------|------|
| shock | 震击 |
| blastFreeze | 冰爆冻结 |
| impactPower | 冲击发电 |
| blastGenerator | 爆炸发电机 |
| forceProjectorBreak | 力场投影仪破碎 |
| thoriumReactorOverheat | 钍反应堆过热 |
| neoplasmReact | 新瘤反应 |
| fireExtinguish | 灭火 |
| flameAmmo | 火焰弹药 |
| resupplyTurret | 补给炮塔 |
| turretCool | 炮塔冷却 |
| teamCoreDamage | 核心受伤 |
| suicideBomb | 自杀炸弹 |
| exclusionDeath | 排斥死亡 |

### 单位
| Trigger | 说明 |
|---------|------|
| unitCommandChange | 单位指令变更 |
| unitCommandPosition | 单位位置指令 |
| unitCommandAttack | 单位攻击指令 |
| unitCommandBoost | 单位推进指令 |
| cannotUpgrade | 无法升级 |

### 系统
| Trigger | 说明 |
|---------|------|
| openConsole | 打开控制台 |
| openWiki | 打开Wiki |
| socketConfigChanged | Socket配置变更 |
| importMod | 导入Mod |
| acceleratorUse | 加速器使用 |

## Event 类完整清单（57个）

### 生命周期
| Event | 字段 | 说明 |
|-------|------|------|
| ContentInitEvent | 无 | 所有Content.init()+postInit()完成后 |
| ModContentLoadEvent | 无 | 所有Mod的loadContent()完成后 |
| FileTreeInitEvent | 无 | mod文件加入Vars.tree后 |
| MusicRegisterEvent | 无 | SoundControl注册音乐后 |
| ClientLoadEvent | 无 | 客户端首次加载 |
| ClientCreateEvent | 无 | 客户端创建 |
| ServerLoadEvent | 无 | 服务器加载 |
| DisposeEvent | 无 | 资源释放 |
| AtlasPackEvent | multiPacker | 所有内容加入atlas后（纹理打包） |

### 世界
| Event | 字段 | 说明 |
|-------|------|------|
| WorldLoadBeginEvent | 无 | 开始加载世界（generating=true） |
| WorldLoadEndEvent | 无 | 世界瓦片初始化完成（proximity前） |
| WorldLoadEvent | 无 | 世界加载完成（generating=false） |
| PlayEvent | 无 | 开始游玩 |
| ResetEvent | 无 | 重置 |
| WinEvent | 无 | 胜利 |
| LoseEvent | 无 | 失败 |
| GameOverEvent | winner(Team) | 游戏结束 |
| ResizeEvent | 无 | 窗口大小变化 |
| StateChangeEvent | from, to(State) | 游戏状态变更 |
| TurnEvent | 无 | 回合 |

### 战役
| Event | 字段 | 说明 |
|-------|------|------|
| SectorLoseEvent | sector | 扇区被摧毁 |
| SectorInvasionEvent | sector | 扇区被入侵 |
| SectorCaptureEvent | sector, initialCapture | 扇区被征服 |
| LaunchItemEvent | stack(ItemStack) | 发射物品 |
| SectorLaunchEvent | sector | 扇区发射 |
| SectorLaunchLoadoutEvent | sector, from, loadout | 扇区发射载荷 |

### 玩家操作
| Event | 字段 | 说明 |
|-------|------|------|
| TapEvent | player, tile | 玩家点击瓦片 |
| LineConfirmEvent | 无 | 玩家确认放置线 |
| BlockInfoEvent | 无 | 玩家打开方块信息 |
| WithdrawEvent | tile, player, item, amount | 玩家取物品 |
| DepositEvent | tile, player, item, amount | 玩家存物品 |
| ConfigEvent | tile, player, value | 建筑配置变更 |
| SchematicCreateEvent | schematic | 创建蓝图 |

### 建造
| Event | 字段 | 说明 |
|-------|------|------|
| BlockBuildBeginEvent | tile, team, unit, breaking | 开始建造（ConstructBlock） |
| BlockBuildEndEvent | tile, team, unit, breaking, config | 建造完成 |
| BuildSelectEvent | tile, team, builder, breaking | 选中建造 |
| BuildRotateEvent | build, unit, previous | 旋转建筑 |
| BlockDestroyEvent | tile | 方块被摧毁 |
| BuildDamageEvent | build, bullet | 建筑被子弹伤害（**复用！不能嵌套**） |
| TilePreChangeEvent | tile | 瓦片变更前（**复用！**） |
| TileChangeEvent | tile | 瓦片变更后（**复用！**） |
| TileFloorChangeEvent | tile, previous, floor | 地板变更（**复用！**） |
| TileOverlayChangeEvent | tile, previous, overlay | 覆盖物变更（**复用！**） |
| BuildTeamChangeEvent | previous, build | 建筑队伍变更（**复用！**） |
| CoreChangeEvent | core | 核心放置/移除/队伍变更 |
| GeneratorPressureExplodeEvent | build | 压力反应堆爆炸 |

### 单位
| Event | 字段 | 说明 |
|-------|------|------|
| UnitCreateEvent | unit, spawner, spawnerUnit | 单位被工厂创建 |
| UnitSpawnEvent | unit | 单位被波次生成 |
| UnitDestroyEvent | unit | 单位被摧毁 |
| UnitBulletDestroyEvent | unit, bullet | 单位被子弹杀死 |
| UnitDamageEvent | unit, bullet | 单位被子弹击中（**复用！**） |
| UnitDrownEvent | unit | 单位溺亡 |
| UnitUnloadEvent | unit | 单位从载荷卸载 |
| UnitControlEvent | player, unit | 玩家控制单位 |
| UnitChangeEvent | player, unit | 玩家切换单位 |
| PickupEvent | carrier, unit/build | 拾取载荷 |
| PayloadDropEvent | carrier, unit/build | 放下载荷 |
| BuildingCommandEvent | player, building, position | 建筑指令 |

### 联网
| Event | 字段 | 说明 |
|-------|------|------|
| PlayerChatEvent | player, message | 玩家聊天（服务器端） |
| ClientChatEvent | message | 客户端发送聊天（仅客户端） |
| ConnectionEvent | connection | 连接建立 |
| ConnectPacketEvent | connection, packet | 收到连接包 |
| PlayerConnectionConfirmed | player | 玩家连接确认 |
| PlayerJoin | player | 玩家加入 |
| PlayerConnect | player | 玩家连接 |
| PlayerLeave | player | 玩家离开 |
| PlayerBanEvent | player | 玩家被封禁 |
| PlayerUnbanEvent | player | 玩家解封 |
| PlayerIpBanEvent | ip | IP封禁 |
| PlayerIpUnbanEvent | ip | IP解封 |
| AdminRequestEvent | action, player, other | 管理员请求 |
| MenuOptionChooseEvent | player, menuId, option | 菜单选项选择 |
| TextInputEvent | player, textInputId, text | 文本输入 |
| ClientPreConnectEvent | host | 客户端预连接 |
| ClientServerConnectEvent | ip, port | 客户端连接服务器 |
| HostEvent | 无 | 主持 |
| MapMakeEvent | 无 | 制作地图 |
| MapPublishEvent | 无 | 发布地图 |
| SaveWriteEvent | 无 | 写入存档 |
| SaveLoadEvent | isMap | 加载存档 |
| ContentPatchLoadEvent | patches, images | 加载内容补丁 |

## 复用事件警告

以下事件的对象是**复用**的，监听器中**禁止**：
- 缓存事件对象引用
- 在监听器中再次触发同一事件（嵌套）
- `BuildDamageEvent`、`TilePreChangeEvent`、`TileChangeEvent`、`TileFloorChangeEvent`、`TileOverlayChangeEvent`、`BuildTeamChangeEvent`、`UnitDamageEvent`

## JSON Mod 可用事件

JSON mod 通过 `content/scripts/` 中的 JS 监听：
```javascript
Events.on(EventType.BlockBuildEndEvent, e => {
    if(e.block == Blocks.router) print("router built!");
});
```

## 硬性规则

1. **ContentInitEvent ≠ ModContentLoadEvent** — 前者在所有Content.init()后，后者在Mod加载后
2. **不存在 ContentLoadEvent** — 源码中无此事件类
3. **Trigger 是枚举，不是Event类** — 用 `Events.run()` 而非 `Events.on()`
4. **复用事件不能嵌套** — BuildDamageEvent/TilePreChangeEvent/TileChangeEvent/UnitDamageEvent
5. **PlayerChatEvent 服务器端，ClientChatEvent 仅客户端** — 不要搞混
6. **BlockBuildBeginEvent 的 tile.block 几乎总是 ConstructBlock** — 不是目标方块
7. **UnitCreateEvent ≠ UnitSpawnEvent** — 前者是工厂创建，后者是波次生成
8. **CoreChangeEvent 同时触发于放置、移除和队伍变更** — 不只是放置
9. **SaveLoadEvent.isMap 区分地图和存档** — true=地图，false=存档
10. **SectorLaunchLoadoutEvent 有3个字段** — sector, from, loadout
11. **AtlasPackEvent 提供 MultiPacker** — 可在此事件中处理纹理打包
12. **ContentPatchLoadEvent** — 存档加载自定义补丁时触发
13. **MenuOptionChooseEvent 建议改用 Menus.registerMenu** — 源码注释推荐

## 关联
- [ContentInitEvent vs ModContentLoadEvent](../concepts/mindustry-content-init-modcontentload-events.md)
- [Vars Game](vars-game.md)
- [Modding API](modding-api.md)
- [Unit System](unit-system.md)
