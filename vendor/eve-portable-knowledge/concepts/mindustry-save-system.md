# Mindustry 存档系统源码验证

## 核心类

### SaveIO（入口）
- **路径**: `mindustry.io.SaveIO`
- **header**: `{'M', 'S', 'A', 'V'}`
- **版本**: 1~12，当前最新 Save12
- **getSaveWriter()**: 返回最新版本
- **save(Fi file)**: 保存时先备份再写入
- **getStream(Fi file)**: `InflaterInputStream` 解压读取
- **isSaveValid(Fi file)**: 验证文件有效性

### SaveVersion（抽象基类）
- **路径**: `mindustry.io.SaveVersion`
- **version**: int，版本号
- **customChunks**: `OrderedMap<String, CustomChunk>`，mod 自定义数据
- **addCustomChunk(String name, CustomChunk chunk)**: 注册自定义数据块

### 存档格式（区域顺序）
1. **meta**: 元数据（版本、时间、波次、规则、mods）
2. **patches**: 数据补丁（v12+）
3. **content**: 内容头（临时映射）
4. **map**: 地图数据
5. **entities**: 实体数据
6. **markers**: 标记（v8+）
7. **custom**: 自定义数据块

### SaveMeta
- **字段**: version, build, timestamp, timePlayed, map, wave, rules, mods
- **mods**: `String[]`，从 tags 读取

### CustomChunk（接口）
- **write(DataOutput)**: 写入
- **read(DataInput)**: 读取
- **shouldWrite()**: 默认 true
- **writeNet()**: 默认 true（是否同步到客户端）

### fallback 映射
- **用途**: 旧版本方块名 → 新版本名
- **示例**: `"dart-mech-pad" → "legacy-mech-pad"`

### Saves（存档管理器）
- **saves**: `Seq<SaveSlot>`
- **load()**: 并行加载所有存档
- **clearOldMegabaseSectors()**: 清理旧版巨型基地扇区

## 硬性规则

1. **存档 header 是 MSAV**，不是其他值
2. **当前最新版本是 Save12**
3. **存档格式有 7 个区域**，按顺序读写
4. **CustomChunk 用于 mod 自定义数据**
5. **addCustomChunk 是静态方法**，在 mod 初始化时调用
6. **存档使用 InflaterInputStream 压缩**
7. **fallback 映射处理旧版本方块名**
8. **Saves.load() 并行加载存档**
9. **SaveMeta.mods 是 String[]**，不是 Seq
10. **存档保存时先备份再写入**

## 常见误区

### 误区 1: 存档格式是 JSON
- **错误**: 存档是 JSON 格式
- **正确**: 存档是二进制格式，有固定区域顺序

### 误区 2: mod 数据存在 JSON 文件
- **错误**: mod 数据存在单独的 JSON 文件
- **正确**: mod 数据通过 CustomChunk 存在存档的 custom 区域

### 误区 3: 存档版本是固定的
- **错误**: 存档格式不会变化
- **正确**: 有 12 个版本，新版本可能添加区域（如 v12 添加 patches）

### 误区 4: fallback 映射是装饰
- **错误**: fallback 映射只是历史记录
- **正确**: fallback 映射用于加载旧版本存档时的方块名转换
