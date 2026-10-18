# Mindustry Placement/HUD UI 修改

## 结论
Mindustry 进入世界后的右下角建造列表，本质上属于 `PlacementFragment` / `hudfrag.blockfrag` 这一类 HUD 结构。对这类 UI 的修改，优先思路不是直接改设置页，而是在客户端加载后 hook 到 HUD，再做替换、重排、隐藏或重建。

## 从示例 mod 得到的实现线索
在 `Helium` 的 `HePlacementFrag` 中，可以看到它并不直接改原版逻辑，而是：
- 通过 `Vars.ui.hudfrag.blockfrag` 相关字段接管原版块选择区域
- 使用 `PlacementFragment` 的私有 `toggler` 字段控制显示状态
- 在 `WorldLoadEvent` 后重新构建分类与槽位
- 用自定义 `Table` / `Collapser` / `ScrollPane` 组合出新的底部面板
- 通过 `Vars.control.input.block` 维持当前选中方块

这说明：想改“右下角建造列表 UI”，更稳的路线是**接管 HUD 中对应 fragment，再自定义布局**。

## 可用切入点
1. **客户端加载后再改**
   - `Events.on(ClientLoadEvent::class.java)`
   - 或世界加载后 `WorldLoadEvent`

2. **定位原版建造列表 fragment**
   - `Vars.ui.hudfrag.blockfrag`
   - `PlacementFragment`
   - 与 `toggler` / `rebuild` / `shown` 相关字段或方法

3. **三种常见改法**
   - 只改外观：调整按钮大小、间距、布局
   - 改内容：筛选/排序/隐藏某些 block
   - 完全替换：隐藏原版列表，自己叠一层 UI

## 实战建议
- 若只是想“改样式”，先找原版容器再修改其 children。
- 若想“稳定可维护”，建议像 Helium 一样**自建面板 + 控制原版 fragment 显隐**。
- 若要改逻辑（分类、默认选项、排序），通常要同时处理 `Vars.control.input.block` 和分类重建逻辑。

## 相关源码参考
- `源码和示例/mods/Helium-master/src/main/kotlin/helium/ui/fragments/placement/HePlacementFrag.kt`

## 相关页面
- [Helium](../entities/helium.md)
- [Mindustry JSON 可用接口概览](mindustry-json-interfaces.md)（如需对 UI/内容接口做延伸理解）
