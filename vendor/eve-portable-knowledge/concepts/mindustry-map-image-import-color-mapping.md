# Mindustry 地图导入图片颜色映射

## 导入链路

```
MapEditorDialog "Import Image"
  → platform.showFileChooser("png")
  → Pixmap pixmap = new Pixmap(file)
  → editor.beginEdit(pixmap)
    → MapIO.readImage(pixmap, tiles)
      → ColorMapper.get(color) → block
```

## ColorMapper 注册

```java
// ColorMapper.java
for(Block block : Vars.content.blocks()){
    color2block.put(block.mapColor.rgba(), block);
}
color2block.put(Color.rgba8888(0, 0, 0, 1), Blocks.air);
```

`mapColor` 的来源（`Block.createIcons`）：
```java
// Block.java:1521
mapColor.set(image.get(image.width/2, image.height/2));
```
即从该方块第一张贴图的中心像素采样。

OreBlock 特殊处理（`OreBlock.java:22`）：
```java
this.mapColor.set(ore.color);
```

`rgba()` 返回 `int` 格式：`(r << 24) | (g << 16) | (b << 8) | a`
`rgba8888()` 返回 `int` 格式：`(a << 24) | (b << 16) | (g << 8) | r`

## readImage 限制

```java
// 只处理 environment 方块，跳过 building
if(block.hasBuilding()) continue;
```

## 完整颜色映射表

见 [mindustry-floor-color-map.md](mindustry-floor-color-map.md)