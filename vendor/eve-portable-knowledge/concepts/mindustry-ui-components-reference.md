# Mindustry UI 组件完整用法参考

## 基础框架

Mindustry UI 基于 `arc.scene2d`，场景根是 `Core.scene`。
所有组件继承 `Element`，使用 `Table` + `Cell` 布局系统。

---

## 一、布局组件（Arc scene2d）

### Table
- **作用**：核心布局容器，类似 HTML table，用 `add()` / `add(row)` / `add(column)` 添加子元素
- **用法**：
```java
Table table = new Table(Styles.defaultPane);
table.add(label).width(100).height(50);
table.add(button).colspan(2);
```
- **Cell 属性**：`.width()` `.height()` `.margin()` `.pad()` `.padTop()` `.space()` `.colspan()` `.rowspan()` `.fill()` `.expand()` `.center()` `.left()` `.right()` `.top()` `.bottom()` `.growX()` `.growY()` `.left().top().right().bottom()` 边距链式调用

### Spacer
- **作用**：弹性空白占位
- **用法**：`add(Spacer.spacer())` 或 `add(Table.spacer())`

### Stack
- **作用**：层叠布局（子元素重叠，顶层可见）
- **用法**：`new Stack(layer1, layer2, ...)`

### ScrollPane
- **作用**：可滚动容器
- **用法**：
```java
ScrollPane scroll = new ScrollPane(container, Styles.defaultPane);
scroll.setScrollY(0.5f); // 0~1
scroll.setOvershoot(0);  // 禁止弹性滚动
```
- **样式**：`Styles.defaultPane` / `horizontalPane` / `smallPane` / `noBarPane`

### Dialog
- **作用**：弹出对话框，带淡入淡出动画
- **用法**：
```java
Dialog d = new Dialog("Title", Styles.defaultDialog);
d.defaults().pad(10f);
d.button("OK", e -> d.hide());
d.show(Core.scene);
```
- **样式**：`Styles.defaultDialog` / `fullDialog`

### Collapser
- **作用**：可折叠容器，点击收缩/展开

---

## 二、显示组件（Arc scene2d）

### Label
- **作用**：文本标签
- **用法**：
```java
Label label = new Label("Text", Styles.defaultLabel);
label.setFontScale(1.2f);   // 字号缩放
label.setColor(Colors.white);
label.truncate(Truncate.ellipsis); // 超出显示 ...
label.setAlignment(Align.left, 0.5f);
```
- **样式**：`defaultLabel` / `outlineLabel` / `techLabel` / `monoLabel`

### Image
- **作用**：显示纹理/图片
- **用法**：
```java
Image img = new Image(Atlas.atlas.find("region-name"));
img.setDrawable(drawable);
img.setColor(Colors.red);
img.setSize(32, 32);
```
- **九宫格**：用 `NinePatchDrawable` 实现自适应描边

### ColorImage
- **作用**：纯色矩形（不依赖贴图）
- **用法**：`new ColorImage(width, height, Colors.red)`

---

## 三、交互组件（Arc scene2d）

### Button
- **作用**：图片按钮（无文本）
- **用法**：
```java
Button btn = new Button(Styles.defaultb);
btn.setChecked(true);
btn.addListener(new ClickListener() {
    @Override public void clicked(InputEvent e, float x, float y) {
        // 点击回调
    }
});
```
- **样式**：`Styles.defaultb` / `underlineb`

### TextButton
- **作用**：带文本的按钮
- **用法**：
```java
TextButton btn = new TextButton("按钮文字", Styles.defaultt);
btn.getLabel().setFontScale(1f);
btn.addListener(ClickListener);
```
- **样式**：`defaultt` / `nonet` / `flatt` / `grayt` / `logict` / `flatBordert` / `cleart`

### ImageButton
- **作用**：图片 + 文本组合按钮
- **用法**：
```java
ImageButton btn = new ImageButton(Atlas.find("icon"), "Text", Styles.defaulti);
```
- **样式**：`defaulti` / `flat` / `nonet` / `gray` / `logic` / `flatBorder` / `clear`

### TextField
- **作用**：单行文本输入
- **用法**：
```java
TextField field = new TextField("placeholder", Styles.defaultField);
field.setText("default");
field.setInputFilter(InputFilters.alphanumeric());
field.setTextChangeDeferred(false); // 即时触发变更
field.addListener(new TextFieldListener() { /*...*/ });
```
- **样式**：`defaultField` / `nodeField` / `areaField` / `nodeArea`

### TextArea
- **作用**：多行文本输入
- **用法**：`new TextArea("text", Styles.defaultField)`（复用 TextFieldStyle）

### CheckBox
- **作用**：复选框
- **用法**：
```java
CheckBox cb = new CheckBox("label", Styles.defaultCheck);
cb.setChecked(true);
cb.addListener(ClickListener);
```
- **样式**：`Styles.defaultCheck`

### ButtonGroup
- **作用**：单选按钮组
- **用法**：
```java
ButtonGroup<PushButton> group = new ButtonGroup<>();
group.add(btn1, btn2, btn3);
group.setMinCheckedAmount(1);
```

### Slider
- **作用**：滑动条
- **用法**：
```java
Slider slider = new Slider(0f, 100f, 1f, false, Styles.defaultSlider);
slider.setValue(50f);
slider.addListener(new ChangeListener.Adapter() {
    @Override public void changed(ChangeEvent e, Element target) {
        // slider.setValuePct() 时触发
    }
});
```
- **样式**：`Styles.defaultSlider`

### Touchpad
- **作用**：虚拟摇杆
- **用法**：`new Touchpad(Styles.defaultTouchpad)`，`getKnobPercentX/Y()` 获取 -1~1 方向

### ProgressBar
- **作用**：带刻度的进度条
- **用法**：
```java
ProgressBar bar = new ProgressBar(0, 100, 1, false, Styles.defaultProgress);
bar.setValue(60f);
```

---

## 四、Mindustry 专用 UI 组件

### Bar（进度条）
- **源码**：`mindustry.ui.Bar.java`
- **构造**：`new Bar(name, color)` 或 `new Bar(drawable, color, size)`
- **用法**：
```java
Bar bar = new Bar("bar", Colors.red);
bar.setFraction(0.5f);  // 0~1
bar.outline(Colors.black, 2); // 描边
bar.blink(Colors.yellow);     // 闪烁
bar.flash();                  // 触发闪烁
```

### TreeElement（树形目录）
- **作用**：可展开/折叠的树节点
- **用法**：
```java
TreeElement tree = new TreeElement("Root", Styles.defaultTree);
tree.setCollapsed(true);
tree.addListener(ClickListener); // 点击展开/折叠
```
- **样式**：`Styles.defaultTree`

### Tooltip（悬停提示）
- **用法**：
```java
Tooltip tooltip = new Tooltip("提示文字");
tooltip.setDelay(0.5f);
tooltip.setDisplayTime(5f);
tooltip.showFor(element);
```

### GridImage / MultiReqImage / ReqImage
- **作用**：带需求高亮的图片（满足条件白、不满足灰）
- **用法**：`new MultiReqImage(drawable, requirements...)`

### MobileButton
- **作用**：移动端虚拟按键

---

## 五、Mindustry UI 架构（Fragment + Dialog）

### Fragment（UI 片段）
嵌入到 HUD 或菜单中，不弹窗：

| Fragment | 作用 |
|----------|------|
| `PlacementFragment` | 建造面板（每行4个方块） |
| `HudFragment` | 游戏内 HUD（含建造面板） |
| `ChatFragment` | 聊天窗口 |
| `ConsoleFragment` | 控制台 |
| `MinimapFragment` | 小地图 |
| `PlayerListFragment` | 玩家列表 |
| `LoadingFragment` | 加载画面 |
| `HintsFragment` | 提示信息 |

### Dialog（弹出对话框）

| Dialog | 作用 |
|--------|------|
| `PausedDialog` | 暂停菜单 |
| `SettingsMenuDialog` | 设置 |
| `ResearchDialog` | 科技树 |
| `SchematicsDialog` | 蓝图管理 |
| `PlanetDialog` | 星球地图 |
| `LogicDialog` | 逻辑编辑器 |
| `ModsDialog` | Mod 管理 |
| `MapEditorDialog` | 地图编辑器 |
| `ContentInfoDialog` | 内容详情 |
| `DatabaseDialog` | 数据库 |
| `MenuFragment` | 主菜单 |

---

## 六、事件系统

| 监听器 | 触发时机 |
|--------|----------|
| `ClickListener` | 点击 |
| `DragListener` | 拖拽 |
| `ChangeListener` | 属性变更（Slider/CheckBox） |
| `ResizeListener` | 尺寸改变 |
| `FocusListener` | 焦点变化 |
| `DragScrollListener` | 拖拽滚动 |

---

## 七、样式与资源

- **Styles**：`mindustry.ui.Styles` 管理所有按钮/标签/文本框样式
- **Tex**：`mindustry.ui.Tex` 管理通用 UI 贴图
- **Icon**：`mindustry.ui.Icon` 管理图标
- **Fonts**：`mindustry.ui.Fonts` 管理字体（默认/mono/outline 等）

## 八、Mod 中自定义 UI

```java
// 在 Mod.load() 或 init() 中
Table myPanel = new Table(Styles.defaultPane);
myPanel.add(new Label("Mod Content", Styles.defaultLabel));
myPanel.add(new TextButton("Click", Styles.defaultt, e -> {
    // 点击处理
}));
// 添加到 HUD
Core.scene.add(myPanel);
```

---

## 关键规则

1. UI 基于 arc.scene2d，不是原生 Android UI
2. `Core.scene` 是场景根，UI 必须 `add()` 到它或其子 `Group`
3. Table 使用 `add()` 逐行布局，`row()` 换行
4. `Cell` 的属性链式调用决定元素在 Table 中的位置
5. 所有 Style 对象在 `Styles.load()` 中初始化，必须在 `loadSync()` 后可用
6. Dialog.show() 有 0.1s 淡入动画，hide() 同理
7. Dialog 必须 `show(Core.scene)`，不能直接添加到其他容器
8. 自定义 Fragment 实现 `Fragment` 接口，通过 `UI.addFragment()` 注册
