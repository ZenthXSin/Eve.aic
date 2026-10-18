# Mindustry UI 系统源码验证

> 来源: `core/src/mindustry/core/UI.java`(832行)、`core/src/mindustry/ui/` 全目录约4400+行

## 概述

UI 基于 Arc scene2d 框架，采用 Fragment(13个) + Dialog(20+个) 双层架构。UI.java 在 **`mindustry.core`** 包，不在 `mindustry.ui` 包。

## UI.java 核心类

**包路径**: `mindustry.core.UI`  
**实现接口**: `ApplicationListener`, `Loadable`

### 生命周期

| 阶段 | 方法 | 作用 |
|------|------|------|
| 构造 | `UI()` | `Fonts.loadFonts()` |
| 异步加载 | `loadAsync()` | 加载纹理/图标/样式 |
| 同步加载 | `loadSync()` | |
| 初始化 | `init()` | 创建所有 Fragment 和 Dialog，构建场景树 |

### 关键字段（8 Fragments + 20+ Dialogs）

```java
// === Fragments ===
public MenuFragment menufrag;        // 主菜单
public HudFragment hudfrag;           // 游戏内 HUD（1235行，最大 Fragment）
public ChatFragment chatfrag;         // 聊天
public ConsoleFragment consolefrag;   // 控制台
public MinimapFragment minimapfrag;   // 小地图
public PlayerListFragment listfrag;   // 玩家列表
public LoadingFragment loadfrag;      // 加载画面
public HintsFragment hints;           // 新手提示

// === Dialogs（20+个） ===
public PlanetDialog planet;           // 1462行，最大的 Dialog
public SettingsMenuDialog settings;
public ResearchDialog research;
public SchematicsDialog schematics;
public ModsDialog mods;
public LogicDialog logic;
public MapEditorDialog editor;
public JoinDialog join;
public HostDialog host;
public PausedDialog paused;
public GameOverDialog restart;
// ... 10+ 个更多
```

### 场景树结构

```
Core.scene.root
├── menuGroup        ← menufrag.build()
└── hudGroup         ← hudfrag.build()
    ├── PlacementFragment (blockfrag)  ← 建造面板
    ├── ChatFragment                   ← 聊天
    ├── MinimapFragment                ← 小地图
    ├── PlayerListFragment             ← 玩家列表
    └── ConsoleFragment                ← 控制台
```

### 关键 UI 方法

| 方法 | 作用 | 备注 |
|------|------|------|
| `showInfo(String)` | 普通信息弹窗 | |
| `showErrorMessage(String)` | 红色错误弹窗 | |
| `showInfoFade(String, float)` | 浮动提示 | 默认 7 秒 |
| `showInfoToast(String, float)` | Toast 提示 | |
| `showInfoPopup(...)` | 带定位弹窗 | align/边距参数 |
| `showLabel(String, int, float, float, float)` | 世界坐标标签 | id 用于管理 |
| `showConfirm(String, Runnable)` | 确认对话框 | |
| `showTextInput(...)` | 文本输入 | 支持数字模式、允许空 |
| `announce(String, float)` | 全屏公告 | 默认 3 秒 |
| `showUnlock(UnlockableContent)` | 解锁提示动画 | |
| `showToast(Drawable, float, String)` | 带图标 Toast | |
| `loadAnd(String, Runnable)` | 显示加载画面+异步执行 | |
| `addPopup(String, Table)` | 按 ID 管理弹窗 | 可更新/移除 |
| `addLabel(int, Table)` | 按 ID 管理标签 | 可更新/移除 |
| `newMenuDialog(...)` | 创建菜单对话框 | 选项数组 |

## Fragments（13个）

### HudFragment（1235行）— 最大 Fragment

- `shown` 字段：控制 HUD 显示/隐藏
- `blockfrag` (PlacementFragment)：建造面板引用
- `setHudText(String)`：设置 HUD 文本
- `toggleHudText(boolean)`：切换 HUD 文本显示
- `showToast(...)`：Toast 提示
- `showUnlock(UnlockableContent)`：解锁动画
- `showPauseDisabled()`：暂停禁用提示
- `setPlayerTeamEditor(Player, Team)`：编辑器设置玩家队伍

### PlacementFragment（800行）— 建造面板

- `currentCategory`：当前分类，默认 `Category.distribution`
- `getUnlockedByCategory(Category)`：获取某分类已解锁方块
- `getSelectedBlock(Category)`：获取某分类选中方块
- `selectedBlocks` (ObjectMap)：每分类记住最后选中方块
- `scrollPositions` (ObjectFloatMap)：每分类记住滚动位置
- 分类切换：`Category.all[i]` 遍历，跳过空分类
- 鼠标滚轮切换分类：`currentCategory.prev()` / `.next()`

### 其他 Fragments

| Fragment | 行数 | 作用 |
|----------|------|------|
| MenuFragment | 348 | 主菜单界面 |
| ChatFragment | 353 | 聊天输入/显示 |
| ConsoleFragment | 272 | 开发者控制台 |
| PlayerListFragment | 275 | 玩家列表 |
| BlockInventoryFragment | 249 | 方块库存显示 |
| MinimapFragment | 183 | 小地图 |
| LoadingFragment | 136 | 加载画面 |
| HintsFragment | 404 | 新手提示系统 |
| PlanConfigFragment | 82 | 计划配置 |
| FadeInFragment | - | 淡入效果 |
| BlockConfigFragment | - | 方块配置面板 |

## Menus.java（网络菜单系统）

**注释**: "Unstable API!" — 不稳定 API

### 注册机制

```java
// 注册全局菜单监听器 → 返回 menuId
public static int registerMenu(MenuListener listener)
// 注册文本输入监听器 → 返回 textInputId  
public static int registerTextInput(TextInputListener listener)
```

### 显示方法

| 方法 | 作用 |
|------|------|
| `menu(int, String, String, String[][])` | 显示网络同步菜单 |
| `followUpMenu(int, String, String, String[][])` | 后续菜单（不替换当前） |
| `hideFollowUpMenu(int)` | 隐藏后续菜单 |
| `textInput(int, String, String, int, String, boolean)` | 文本输入框 |
| `setHudText(String)` | 设置 HUD 文本 |
| `announce(String)` | 全屏公告 |
| `infoMessage(String)` | 信息消息 |
| `infoPopup(...)` | 信息弹窗（支持定位/边距） |
| `label(String, float, float, float)` | 世界坐标标签 |
| `infoToast(String, float)` | Toast |
| `warningToast(int, String)` | 警告 Toast |
| `openURI(String)` | 打开链接 |
| `copyToClipboard(String)` | 复制到剪贴板 |
| `removeWorldLabel(int)` | 移除世界标签 |

### 回调机制

```java
// 菜单选择回调 → 触发 MenuOptionChooseEvent
public static void menuChoose(@Nullable Player player, int menuId, int option)
// 文本输入结果回调
public static void textInputResult(@Nullable Player player, int textInputId, @Nullable String text)
```

## Bar.java（进度条组件，136行）

```java
public class Bar extends Element {
    private Floatp fraction;        // 进度值（0~1）
    private CharSequence name;      // 标签名
    private float value, lastValue, blink;
    private Color blinkColor, outlineColor;
}
```

| 构造/方法 | 作用 |
|-----------|------|
| `Bar(String, Color, Floatp)` | 固定名称/颜色 |
| `Bar(Prov<CharSequence>, Prov<Color>, Floatp)` | 动态版本（可变名/色） |
| `set(Prov<String>, Floatp, Color)` | 重设 |
| `flash()` | 闪烁 |
| `blink(Color)` | 设置闪烁颜色 |
| `outline(Color, float)` | 描边 |
| `snap()` | 立即跳到目标值 |

## Dialogs（20+个，按行数排序）

| Dialog | 行数 | 作用 |
|--------|------|------|
| PlanetDialog | 1462 | 星球视图（最大） |
| SettingsMenuDialog | 963 | 设置菜单 |
| SchematicsDialog | 816 | 蓝图管理 |
| ResearchDialog | 797 | 科技树 |
| JoinDialog | 794 | 加入游戏 |
| ModsDialog | 777 | Mod 管理 |
| CustomRulesDialog | 565 | 自定义规则 |
| MapListDialog | 381 | 地图列表 |
| FileChooser | 304 | 文件选择 |
| CanvasEditDialog | 282 | 画布编辑 |
| LoadDialog | 253 | 存档加载 |
| DatabaseDialog | 244 | 数据库查看 |
| EffectsDialog | 235 | 特效查看 |
| LaunchLoadoutDialog | 230 | 发射负载 |

## 硬性规则

1. **UI.java 在 `mindustry.core` 包**，不是 `mindustry.ui`
2. **Fragments 共 13 个**（不是 8 个）
3. **Dialogs 共 20+ 个**，PlanetDialog 最大（1462行）
4. **HudFragment 是最大 Fragment**（1235行），内含 blockfrag (PlacementFragment)
5. **PlacementFragment 默认分类 `Category.distribution`**
6. **Menus.java 官方标注 "Unstable API"**
7. **Bar.fraction 是 Floatp（0~1）**，不是 0~100
8. **Bar 动态版用 `Prov<>` 供应商**，支持运行时变名变色
9. **scene2d 树**：menuGroup + hudGroup 两个顶层 Group
10. **showConfirm 最多 4 参数**（title, text, hide条件, confirmed回调）
11. **showTextInput 支持 numeric 和 allowEmpty 标志**
12. **Menus.followUpMenu 是叠加显示**，不替换当前菜单
13. **Popup/Label 用 ID 管理**，可更新/移除
14. **announce 默认 3 秒**

### 常见误区

1. ❌ "UI.java 在 mindustry.ui 包" → ✅ 在 `mindustry.core`
2. ❌ "Fragment 只有 8 个" → ✅ 有 13 个
3. ❌ "Bar 的值是 0-100" → ✅ 是 0-1（Floatp）
4. ❌ "Menus 是稳定 API" → ✅ 标注 Unstable
5. ❌ "PlacementFragment 默认战斗分类" → ✅ 默认 distribution
6. ❌ "followUpMenu 替换当前菜单" → ✅ 叠加显示
