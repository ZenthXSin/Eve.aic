# Mindustry JSON 多液体输入

> 记录：工厂/方块在 JSON 中如何声明多个液体输入。

## 结论
Mindustry 的 JSON 里，**多个液体输入**通常直接写在 `consumes` 下的 `liquids` 字段里：

```json
{
  "consumes": {
    "liquids": [
      "water/0.2",
      "cryofluid/0.1"
    ]
  }
}
```

含义通常是：**按列表中的液体及数量进行消耗**。具体是否生效，还要看目标方块类型和解析器支持情况。

## 需要注意
- 这不是通用“任意一种液体都可用”的写法。
- 如果某个工厂类型不支持 `liquids`，这个字段会无效或被忽略。
- 对于标准加工厂，是否支持多个液体输入，要以该 block 的 consume 解析实现为准。

## 相关经验
- **多个液体都要消耗**：优先尝试 `consumes.liquids`
- **想要多种液体任选其一**：通常需要代码端自定义逻辑，不一定能纯 JSON 完成

## 相关
- [Mindustry JSON 技能约定](mindustry-json-skill-rules.md)
- [Mindustry JSON 可用接口概览](mindustry-json-interfaces.md)
