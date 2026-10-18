# Mindustry JSON 星球生成器模板

> Source: conversation about a JSON planet generator template for Mindustry

这是一个用于 Mindustry JSON 内容定义时的“星球生成器模板”写法框架，适合先搭建结构，再根据具体星球内容逐步补全。

## Key Points

- 由于 Mindustry 的 JSON 内容通常依赖具体 `type` / 模块字段，星球生成器模板应先保留最小骨架，再根据目标数据补字段。
- 如果目标是做“星球”内容，通常需要围绕行星属性、关卡/区块、规则、背景视觉等信息组织模板。
- 对 JSON 问题应优先按 Mindustry JSON 解析习惯处理：先确认 `type`，再确认该类型支持的字段。
- 模板应保持“可改造性”优先，不要一次塞满所有字段。

## Template Skeleton

```json
{
  "type": "Planet",
  "name": "example-planet",
  "localizedName": "Example Planet",
  "description": "A custom planet template.",
  "generator": {
    "type": "PlanetGenerator",
    "sectorSeed": 1,
    "baseSeed": 12345
  }
}
```

## Usage Notes

- `type` 需要替换为实际可用的星球类型或对应内容类型。
- `generator` 内部字段应根据具体生成器实现调整。
- 若项目中存在现成星球/生成器 JSON，应优先参考已有写法，再扩展模板。
- 如果星球模板涉及地形、资源、规则等字段，建议分层补齐：先基础信息，再生成器，再视觉与玩法配置。

## Related

- [Mindustry JSON 技能约定](../concepts/mindustry-json-skill-rules.md) — JSON 查询与模板生成优先级。
- [Mindustry JSON 可用接口概览](../concepts/mindustry-json-interfaces.md) — 按对象类型整理的常见字段与特殊结构。
- [Mindustry JSON Mod 反序列化流程](../concepts/mindustry-json-mod-deserialization.md) — 内容加载与后处理链路。
