# Mindustry JSON 技能约定

> Source: conversation

## Key Points

- 处理 **JSON** 相关问题时，优先使用 [mindustry-api-parser](../../skills/mindustry-api-parser/SKILL.md) 技能。
- 只要用户问题里出现 **“json”**，就应命中该技能。
- 该技能适用于 Mindustry JSON API、模块结构、block 定义、consume 字段、反序列化与相关查询。
- 当用户在 JSON 语境下提到 **类 / 模块 / 接口 / type** 时，统一按“可解析目标”处理。
- 支持对类、模块、接口、type 的**模糊搜索**，而不是只做精确匹配。
- 当用户问“**有什么接口** / **对应接口**”时，应返回该对象下的嵌套接口、字段关系与用途说明。
- 当用户问“**生成模板**”时，应基于已解析字段、关系与示例，生成可直接参考的 JSON 模板。

## Related

- [Mindustry JSON Mod 反序列化流程](mindustry-json-mod-deserialization.md) — JSON mod 的加载与解析链路
- [Mindustry JSON 可用接口概览](mindustry-json-interfaces.md) — JSON 支持字段与结构概览

## Retrieval Priority

When a user asks a JSON-related question:
1. Check the JSON knowledge pages first.
2. If the knowledge base has no matching content, inspect the 饱和火力 mod JSON files.
3. If the mod JSON files still do not cover it, inspect the source code.
4. Use mindustry-api-parser to support fuzzy lookup for class/module/interface/type.
5. If needed, inspect nested APIs, relationships, and template structure.
6. Write the newly learned interface, type, or template back into the knowledge base.
7. Update persistent memory with the useful rule or decision.
