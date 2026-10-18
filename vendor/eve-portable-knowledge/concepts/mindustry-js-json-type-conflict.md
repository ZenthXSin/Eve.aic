# JS 扩展 JSON 内容时不能写 type 的原因

> Source: ContentParser.java 源码分析（v8）

## 问题

JS 用 `extend(GenericCrafter, ...)` 创建了自定义 block 后，JSON 里如果写了 `"type": "GenericCrafter"`，直接报错崩溃。

## 根因

`ContentParser.java` 解析 block 的逻辑（第 562-575 行）：

```java
if(locate(ContentType.block, name) != null){
    // JS extend 已经注册了同名 block
    if(value.has("type")){
        // JSON 同名 + 写了 type → 崩溃
        warn("re-declares a type...");
        block = make(resolve(value.getString("type", ""), Block.class), mod + "-" + name);
    }else{
        // JSON 同名 + 没写 type → 正常复用
        block = locate(ContentType.block, name);
    }
}else{
    // 没有同名，正常创建
    block = make(resolve(value.getString("type", ""), Block.class), mod + "-" + name);
}
```

**崩溃条件**：JS `extend("my-block", ...)` 已注册同名内容，JSON 里又写了 `"name": "my-block"` + `"type": "GenericCrafter"` → `locate()` 找到了 JS 的实例，但 `value.has("type")` 为 true → 走 `make(resolve(...))` 试图重新创建 → 与已注册实例冲突 → 崩溃。

**不崩溃的情况**：JSON 同名但不写 `type` → `locate()` 找到 → 走 else → 复用 JS 实例，正常工作。

## 正确做法

**JS 负责创建，JSON 负责配置，JSON 不写 type**：

JS 创建类：
```js
extend(GenericCrafter, "multi-crafter", {
    // 自定义方法...
});
```

JSON 配置数值（name 必须一致，不写 type）：
```json
{
  "name": "multi-crafter",
  "size": 2,
  "hasItems": true,
  "itemCapacity": 20
}
```

## 反模式

```json
// 错误：name 与 JS extend 的名称相同，且写了 type → 崩溃
{
  "name": "multi-crafter",
  "type": "GenericCrafter"
}
```

## 适用范围

所有 JS `extend` 的内容类型都有这个问题：Block、UnitType、BulletType、Planet、SectorPreset 等。

## Related

- [Mindustry JSON 接口真实存在性检查](mindustry-json-interface-existence-check.md) — JSON 解析器对 type 的验证
- [Mindustry JSON 不存在的 Block type 回退为 Block](mindustry-json-invalid-block-type-fallback.md) — type 不存在时的回退行为
