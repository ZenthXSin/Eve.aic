# DrawFlame.load() 要求 `-top` 贴图

> Source: `源码和示例/Mindustry/core/src/mindustry/world/draw/DrawFlame.java`

## 规则

`DrawFlame.load()` (第 24 行) 硬编码 `Core.atlas.find(block.name + "-top")`，无 fallback，无 null 检查：

```java
@Override
public void load(Block block){
    block.emitLight = true;
    top = Core.atlas.find(block.name + "-top");  // ← 没有 fallback！
    block.lightClipSize = Math.max(block.lightClipSize, (lightRadius + lightSinMag) * 2f * block.size);
}
```

如果 `-top` 贴图缺失，`top` 区域为 `TextureRegion.empty`（全白小图），draw 时火焰透明方块的半透明层无法正常显示。

**必要条件**: 使用 `DrawFlame` 的方块必须同时具备 `blockname-top` 贴图。