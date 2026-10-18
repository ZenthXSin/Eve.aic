# Mindustry ContentParser 源码验证

> Source: `源码和示例/Mindustry/core/src/mindustry/mod/ContentParser.java` (1300+ 行)
> 验证版本：v146 (本地源码)

## 核心方法

### readFields（字段读取）
```java
void readFields(Object object, JsonValue jsonMap){
    JsonValue research = jsonMap.remove("research");  // 特殊处理
    var fields = parser.getFields(type);
    
    for(JsonValue child = jsonMap.child; child != null; child = child.next){
        FieldMetadata metadata = fields.get(child.name().replace(" ", "_"));
        // 空格 → 下划线匹配
        
        if(metadata == null){
            if(ignoreUnknownFields){
                warn(...);  // 警告但继续
                continue;
            }else{
                throw SerializationException;  // 抛异常
            }
        }
        field.set(object, readField);  // 反射设置
    }
}
```

### research 字段特殊处理
```java
// research 在 readFields 开头被 remove
JsonValue research = jsonMap.remove("research");

// 后续处理：
if(research.isString()){
    researchName = research.asString();  // 字符串格式 → 父节点名
    customRequirements = null;
}else{
    researchName = research.getString("parent", null);  // 对象格式
    customRequirements = research.has("requirements") ? 
        parser.readValue(ItemStack[].class, research.get("requirements")) : null;
}

// 创建 TechNode
TechNode node = new TechNode(null, unlock, customRequirements);
```

### Seq/ObjectSet 追加模式
```json
{
    "someSeq": {"add": [1, 2, 3]}
}
```
- 检测到 `add` 字段 → 读取值后 addAll 到已有列表
- 不替换，只追加

### Map 合并模式
```json
{
    "someMap": {"add": true, "key1": "val1", "key2": "val2"}
}
```
- 检测到 `add: true` → 读取后 putAll 到已有 Map
- 不替换，只合并

## type 解析

### 类型映射
1. `type` 字段值 → `ClassMap.get(name)` 获取 Java 类
2. 实例化：`parser.newInstance(type)`
3. 调用 `readFields(result, data)` 填充字段
4. 特殊类型（Block, UnitType, BulletType）有额外后处理

### Block 特殊处理
```java
// Block 类型创建后：
result = (Block)parser.readValue(Block.class, value);
// 然后根据具体 type 字段值创建子类实例
```

## 忽略未知字段

```java
// ContentParser 有一个标志：
boolean ignoreUnknownFields = true;  // 默认忽略未知字段
```

**关键**：默认忽略未知字段（只警告），不会因为 JSON 中有未知字段而崩溃。但建议移除未知字段避免警告。

## 常见解析顺序

1. 读取 `type` 字段确定 Java 类
2. 实例化对象
3. `readFields()` 填充所有 JSON 字段
4. 特殊后处理（如 Block 的 consumes、research 等）
5. 调用 `content.init()` 初始化
6. 调用 `content.postInit()` 后初始化

## 关键规则

| 规则 | 说明 |
|------|------|
| 空格→下划线匹配 | JSON 字段名中的空格被替换为下划线 |
| research 被特殊处理 | 在 readFields 开头被 remove |
| Seq 支持 add 追加 | 不替换整个数组 |
| Map 支持 add:true 合并 | 不替换整个 Map |
| 未知字段默认只警告 | ignoreUnknownFields=true |
| type 通过 ClassMap 查找 | 不是直接类名反射 |
| research 支持字符串/对象 | 字符串=父节点名，对象=parent+requirements |
| Block type 创建子类实例 | 不是直接实例化 Block |

## JSON 常见误区

| 误区 | 正确 |
|------|------|
| 认为未知字段会崩溃 | 默认忽略（只警告） |
| 忘记 research 被特殊处理 | 在 readFields 中 remove |
| JSON 字段名用空格 | 自动转下划线匹配 |
| 认为 type 直接反射 | 通过 ClassMap 查找 |
| 认为 Seq 只能整体替换 | 支持 add 追加模式 |

## 关联

- [JSON Mod 反序列化流程](../concepts/mindustry-json-mod-deserialization.md) — 完整加载链路
- [Content System](../mindustry/content-system.md) — Content 生命周期
- [Block and Building System](../mindustry/block-building-system.md) — Block 字段与 init
