# Mindustry tools:pack 描边偏红问题

> Source: 2026-06-07 对话排查；源码：`源码和示例/Mindustry/tools/build.gradle`、`tools/src/mindustry/tools/ImagePacker.java`、`tools/src/mindustry/tools/Generators.java`、`core/src/mindustry/world/Block.java`、`core/src/mindustry/ctype/UnlockableContent.java`、`源码和示例/Arc/arc-core/src/arc/graphics/Pixmap.java`、`Pixmaps.java`

## 结论

`tools:pack` 不是 mod 打包链路，而是 Mindustry 自身素材生成/打包链路：复制 `core/assets-raw/sprites/` 到 `sprites_out/`，运行 `ImagePacker` 与 `Generators.run()` 生成图标、描边、队伍色等派生贴图，再可选执行 AA，最后 `TexturePacker.process(...)` 输出 atlas。

所有“需要描边”的贴图偏红时，优先怀疑 `tools/build.gradle` 中的 `antialias(File file)` 对已生成描边图再次处理，而不是 `TexturePacker` 本身。该 AA 算法会对像素做 premultiply/邻域混合：透明或半透明边缘附近的 RGB 会向周围主体颜色、队伍色或已有红橙像素偏移，导致深色描边出现粉/红 halo。

## 源码链路

- `tools/build.gradle` 的 `pack` task：
  - 删除 `../core/assets-raw/sprites_out/`
  - 复制 `../core/assets-raw/sprites/` 到 `sprites_out/`
  - `javaexec { mainClass = "mindustry.tools.ImagePacker" }`
  - `if(enableAA)` 遍历 `sprites_out/**/*.png` 并调用 `antialias(file.file)`
  - `TexturePacker.process(..., "sprites.aatls")`
- `ImagePacker.main()`：初始化 `ContentLoader`，加载 `sprites_out` 到临时 atlas，执行 `Vars.content.load(); Generators.run();`
- 描边生成入口：
  - `tools/src/mindustry/tools/Generators.java` 多处 `get(region).outline(block.outlineColor, block.outlineRadius)`
  - `core/src/mindustry/world/Block.java#createIcons(MultiPacker)` 中 `Pixmaps.outline(region, outlineColor, outlineRadius)` 后 `Drawf.checkBleed(out)`
  - `core/src/mindustry/ctype/UnlockableContent.java#makeOutline(...)` 同样 `Pixmaps.outline(...)` 后 `Drawf.checkBleed(result)`
- `Arc Pixmap.outline(Color,int)`：只在 alpha 为 0 的像素、邻域存在非空像素时写入指定 outline 色
- `Arc Pixmaps.bleed(...)`：只给 alpha=0 的透明像素扩散邻近 RGB，主要用于线性过滤防边缘黑/白边

## 排查建议

1. 先把 `tools/build.gradle` 顶部 `def enableAA = true` 临时改成 `false`，重新跑 `tools:pack`
2. 若偏红消失，根因就是 AA 后处理污染描边色
3. 若仍偏红，再比较 `sprites_out` 中生成的 `*-outline.png` 与最终 atlas，判断是生成阶段还是 pack 阶段
4. 对需要保持硬描边的贴图，可跳过 AA 或在 AA 后对 outline 色做阈值/掩码修正

## Related

- [Mindustry 回答验证与低幻觉策略](../analysis/mindustry-answer-validation-policy.md) — Mindustry 输出前必须先查源码/知识库验证
- [Mindustry 回答准确度强化规则](../analysis/mindustry-answer-accuracy-hardening.md) — 固化 LMM/知识库 → 源码 → 示例 mod 的验证顺序
