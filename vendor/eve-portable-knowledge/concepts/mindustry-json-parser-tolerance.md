# Mindustry JSON 解析器容错特性

> Source: conversation (用户补充的 JSON 解析规则)

Mindustry 的 JSON 解析器对部分常见文本问题具有容错能力，因此在排查 JSON 问题时，不要把某些格式噪音误判为核心错误。

## Key Points

- 逗号问题在 Mindustry JSON 解析中通常可以被忽略。
- 注释问题在 Mindustry JSON 解析中通常也可以被忽略。
- 因此，排查 JSON 失败时，优先检查 `type`、接口是否真实存在、字段名是否正确、层级是否正确，而不是先怀疑逗号或注释。
- 这类容错不代表所有格式问题都安全，真正影响解析的仍可能是字段结构、类型不匹配或内容定义错误。

## Related

- [Mindustry JSON 接口真实存在性检查](mindustry-json-interface-existence-check.md) — 先确认类型和接口是否真实存在
- [JSON 问题解决流程](json-problem-solving-flow.md) — JSON 问题的推荐排查顺序
- [JSON 常见问题](json-common-issues.md) — Mindustry JSON 中常见误区、失败模式与快速检查清单
