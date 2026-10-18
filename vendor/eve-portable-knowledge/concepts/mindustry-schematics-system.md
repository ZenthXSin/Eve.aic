# Mindustry 蓝图系统源码验证

> Source: `源码和示例/Mindustry/core/src/mindustry/game/Schematics.java` 直接验证，2026-06-11

## 核心常量

```java
private static final byte[] header = {'m', 's', 'c', 'h'}; // 蓝图文件头 "msch"
private static final byte version = 1;                       // 蓝图版本
private static final int padding = 2;                        // 预览图边距
private static final int maxPreviewsMobile = 32;             // 移动端最大预览数
private static final int resolution = 32;                    // 预览图分辨率
```

## 核心字段

```java
private Seq<Schematic> all = new Seq<>();                           // 所有蓝图
private OrderedMap<Schematic, FrameBuffer> previews = new OrderedMap<>(); // 预览缓存
private ObjectSet<Schematic> errored = new ObjectSet<>();           // 错误蓝图
private ObjectMap<CoreBlock, Seq<Schematic>> loadouts = new ObjectMap<>(); // 部署方案
private ObjectMap<CoreBlock, Schematic> defaultLoadouts = new ObjectMap<>(); // 默认部署
```

## 蓝图格式

### 文件结构

```
[header: 4 bytes "msch"]
[version: 1 byte]
[compressed data (deflate)]
  → [tags: StringMap]
  → [width: short, height: short]
  → [block count: int]
  → [blocks: array of (block_id, x, y, config, rotation)]
```

### Base64 编码

```java
// 蓝图 Base64 前缀
Vars.schematicBaseStart = "bXNjaA"

// 编码流程
1. 写入 header "msch"
2. 写入 version 1
3. 写入 tags（名称、描述等）
4. 写入尺寸和方块数据
5. Deflate 压缩
6. Base64 编码
```

## 蓝图尺寸限制

```java
Vars.maxSchematicSize = 64;  // 最大 64×64 瓦片

// 限制检查
if(limitSchematicSize) {
    // 检查方块是否超出范围
}
```

## 部署方案（Loadout）

```java
// 每个核心类型有多个部署方案
loadouts: ObjectMap<CoreBlock, Seq<Schematic>>
defaultLoadouts: ObjectMap<CoreBlock, Schematic>

// 部署方案用于战役模式
// 选择核心后显示可用的部署方案
```

## 常见误区

- **蓝图文件头是 "msch"**：不是 "msav"（那是存档/地图）
- **版本目前是 1**：没有版本 2
- **maxSchematicSize = 64**：不是 128 或无限
- **蓝图使用 Deflate 压缩**：不是 GZIP
- **Base64 前缀是 "bXNjaA"**：可以用来识别蓝图
- **loadouts 按 CoreBlock 分类**：不同核心有不同部署方案
- **预览图分辨率 32**：不是无限精度
- **移动端限制 32 个预览**：性能考虑
- **tags 是 StringMap**：包含名称、描述、作者等

## Related

- [Mindustry Schematic](mindustry-item-team-attribute-schematic-techtree.md)
- [Mindustry Vars System](mindustry-vars-system.md)
