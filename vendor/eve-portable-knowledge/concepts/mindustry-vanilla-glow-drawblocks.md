# Mindustry 原版泛光 DrawBlock 记录

> Source: Local source inspection of `mindustry.world.draw.DrawGlowRegion` and `mindustry.content.Blocks`

## Key Points

- 原版里最明确的“泛光” `DrawBlock` 是 `mindustry.world.draw.DrawGlowRegion`。
- 它使用 `Blending.additive` 和 `Layer.blockAdditive`，并在 `warmup > 0` 时绘制带发光的贴图区域。
- `Blocks.java` 里原版方块常把它塞进 `DrawMulti(...)` 里，用来做 glow / ventglow / power-glow 之类的视觉。

## Confirmed Vanilla Uses

以下都是在 `mindustry.content.Blocks` 里实际出现的 `DrawGlowRegion` 用法：

- `-` 具体方块名待按上下文继续定位，但位置已确认：`Blocks.java:1382`
- `-` 位置：`Blocks.java:1568`
- `-` 位置：`Blocks.java:2023`
- `-` 位置：`Blocks.java:2732`
- `-` 位置：`Blocks.java:2754`
- `-` 位置：`Blocks.java:2805`
- `-` 位置：`Blocks.java:2868`

## DrawGlowRegion Core Behavior

`mindustry.world.draw.DrawGlowRegion`:

- `blending = Blending.additive`
- `suffix = "-glow"`
- `layer = Layer.blockAdditive`
- `draw(build)` 内部：
  - `Draw.blend(blending)`
  - `Draw.color(color)`
  - `Draw.alpha(...)`
  - `Draw.rect(region, ...)`

## Related

- [Mindustry 自定义 Shader 接入](../concepts/mindustry-custom-shader.md) — 如果你想把泛光从贴图层升级成 shader 效果
- [Arc Draw 如何在屏幕上画东西](../concepts/arc-draw-screen-rendering.md) — 解释 additive blending 和 Draw 层级
