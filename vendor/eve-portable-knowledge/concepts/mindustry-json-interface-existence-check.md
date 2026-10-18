# Mindustry JSON 接口真实存在性检查

> Source: conversation (用户补充的 JSON 判断规则)

在 Mindustry JSON 解析中，第一步应先判断目标接口或类型是否**真实存在**，并且名称需要与解析器可识别的类型名**完全一致**。

## Key Points

- JSON 判断的第一步是检查目标接口/类型是否真实存在。
- 需要使用 `mindustry-api-parser` 进行匹配搜索。
- 匹配应以**完全相同的名称**为准，不能只靠相似词或大概意思。
- 如果 `type` 在 Mindustry 的 JSON 解析器中不存在，解析器通常会自动定向到 `Block`。
- 因此，遇到 JSON 不生效时，先确认 `type` 是否命中了真正的目标类型，而不是被兜底到 `Block`。

## Recommended Check

1. 先用 `mindustry-api-parser` 搜索目标名称。
2. 确认是否存在完全一致的类名或接口名。
3. 再确认该类型是否支持 JSON 反序列化。
4. 如果找不到对应类型，检查是否被解析器默认映射到了 `Block`。
5. 最后再继续排查字段、层级和行为逻辑。

## Related

- [Mindustry JSON 技能约定](mindustry-json-skill-rules.md) — JSON 问题检索优先级与技能约定
- [Mindustry JSON 中的 `type` 是否真实存在](mindustry-json-type-real-existence.md) — `type` 入口字段的含义说明
- [Mindustry JSON 可用接口概览](mindustry-json-interfaces.md) — 常见接口与字段入口参考
- [JSON 问题解决流程](json-problem-solving-flow.md) — JSON 问题的推荐排查顺序
