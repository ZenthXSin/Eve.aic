# Mindustry UI 系统源码验证

## UI 核心类：`mindustry.core.UI`

- 实现 `ApplicationListener, Loadable`
- 持有所有 Fragment 和 Dialog 引用
- 初始化顺序：`Fonts.loadFonts()` → `loadSync()` → `init()`

### Fragment（UI片段，嵌入 HUD/菜单）

| Fragment | 作用 |
|----------|------|
| `MenuFragment menufrag` | 主菜单 |
| `HudFragment hudfrag` | 游戏内 HUD（含 PlacementFragment） |
| `ChatFragment chatfrag` | 聊天 |
| `ConsoleFragment consolefrag` | 控制台 |
| `MinimapFragment minimapfrag` | 小地图 |
| `PlayerListFragment listfrag` | 玩家列表 |
| `LoadingFragment loadfrag` | 加载画面 |
| `HintsFragment hints` | 提示 |

### Dialog（弹出对话框）

| Dialog | 作用 |
|--------|------|
| `PausedDialog paused` | 暂停菜单 |
| `SettingsMenuDialog settings` | 设置 |
| `ResearchDialog research` | 科技树 |
| `SchematicsDialog schematics` | 蓝图 |
| `PlanetDialog planet` | 星球地图 |
| `LogicDialog logic` | 逻辑编辑器 |
| `ModsDialog mods` | Mod管理 |
| `MapEditorDialog editor` | 地图编辑器 |
| `ContentInfoDialog content` | 内容详情 |
| `DatabaseDialog database` | 数据库 |
| `CampaignCompleteDialog campaignComplete` | 战役完成 |

### 关键字段

- `followUpMenus: IntMap<Dialog>` — 跟进菜单（按 ID 索引）
- `popups: ObjectMap<String, Table>` — 按 ID 管理的弹出窗口
- `labels: IntMap<Table>` — 按 ID 管理的标签
- `drillCursor/targetCursor/repairCursor` — 自定义鼠标指针

### loadSync() 关键初始化

- `Core.scene = new Scene()` — scene2d 场景根
- `Tex.load()` / `Icon.load()` / `Styles.load()` — 样式资源
- `Dialog.setShowAction()` / `Dialog.setHideAction()` — 对话框动画
- `Tooltips.getInstance().animations = false` — 关闭工具提示动画

## PlacementFragment

- `rowWidth = 4` — 每行 4 个方块按钮
- `currentCategory = Category.distribution` — 默认分类
- 监听 `WorldLoadEvent` → 重置 `currentCategory = distribution`
- 监听 `UnlockEvent` → 解锁新方块时重建
- 监听 `ResetEvent` → 清空选中方块记录

## HudFragment

- `dsize = 65f` — 按钮尺寸
- `pauseHeight = 36f` — 暂停栏高度
- `blockfrag = new PlacementFragment()` — 内嵌建造面板
- `coreItems = new CoreItemsDisplay()` — 核心物品显示
- `shown = true` — HUD 默认可见
- 收藏方块系统：`favoriteBlocks: ObjectSet<String>`

## Bar（进度条）

- 继承 `Element`
- 两种构造函数：固定 name/color 或动态 Prov
- `outline(Color, stroke)` — 描边
- `blink(Color)` — 闪烁
- `flash()` — 触发闪烁

## Menus（网络菜单系统）

- `registerMenu(MenuListener)` → 返回 menuId
- `registerTextInput(TextInputListener)` → 返回 textInputId
- `@Remote` 注解实现网络同步
- `menu()` — 显示菜单，选项回调 `Call.menuChoose()`
- `textInput()` — 文本输入，回调 `Call.textInputResult()`
- `setHudText(String)` — 设置 HUD 文本
- `followUpMenu()` — 跟进菜单（不替换当前菜单）

## 硬性规则

1. UI 基于 arc.scene2d 框架，不是原生 Android/iOS UI
2. `Core.scene` 是场景根节点，`loadSync()` 中初始化
3. PlacementFragment 默认分类是 `distribution`，不是 `turret`
4. Bar 的 fraction 返回 0~1 的 float
5. Menus 是网络同步的，用 `@Remote` 注解
6. Dialog.show/hide 有淡入淡出动画（0.1s）
7. `followUpMenus` 用 IntMap 按 ID 索引
8. Tooltip 在 mobile 模式下 offsetY 增加 60px
9. HudFragment 包含 PlacementFragment，不是独立的
10. 自定义鼠标指针通过 `Core.graphics.newCursor()` 创建

## 关联

- [Vars UI](../mindustry/vars-ui.md)
- [Vars Renderer](../mindustry/vars-renderer.md)
- [Rendering System](../mindustry/rendering-system.md)
- [Block and Building System](../mindustry/block-building-system.md)
