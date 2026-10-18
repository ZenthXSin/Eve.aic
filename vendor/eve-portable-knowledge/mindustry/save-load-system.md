# Save and Load System

> 详细源码验证见 [mindustry-save-system](../concepts/mindustry-save-system.md)
> 验证版本：v136.2

## 核心要点

### 存档格式
- **Header**: `{'M', 'S', 'A', 'V'}` (MSAV)
- **当前版本**: Save12
- **压缩**: InflaterInputStream
- **7个区域顺序**: meta → patches(v12+) → content → map → entities → markers(v8+) → custom

### 核心类
- **SaveIO** — 入口，save()/getStream()/isSaveValid()
- **SaveVersion** — 版本实现，customChunks管理
- **SaveMeta** — 元数据（version/build/timestamp/map/wave/rules/mods）
- **SaveSlot** — 存档槽位
- **Saves** — 存档管理器，load()并行加载

### CustomChunk
- mod自定义数据存入存档的custom区域
- `addCustomChunk(name, chunk)` 是静态方法
- write/read/shouldWrite/writeNet

## 硬性规则

1. 存档是二进制格式，不是JSON
2. mod数据通过CustomChunk存在custom区域
3. 保存时先备份再写入
4. fallback映射处理旧版本方块名
5. SaveMeta.mods是String[]不是Seq
6. patches区域是v12新增
