# JSON 问题解决流程

> Source: conversation update on JSON troubleshooting workflow

本文整理 Mindustry JSON 问题的推荐排查流程，适用于字段含义、渲染顺序、接口可用性、类型反序列化、解析器容错等常见场景。

## Key Points

- 先确认问题类型：是字段不存在、字段无效、渲染异常、加载报错，还是效果不符合预期。
- 优先查知识库中的 JSON 相关页面，再查源码，最后看示例 mod。
- 如果问题涉及具体 `type`、接口或类名，优先用 `mindustry-api-parser` 以及已有 JSON 接口概览页。
- **第一步先确认接口/类型是否真实存在，并且名称要完全匹配。**
- **判断接口是否合规必须有证据，不能靠猜。**
- **证据优先级**：mindustry api parser 匹配结果 > 源码实现 > 示例 mod > 其他推断。
- 如果 `type` 不存在于 Mindustry 的 JSON 解析器中，优先怀疑会被兜底定向到 `Block`。
- Mindustry JSON 解析器对**逗号**和**注释**通常具有容错能力，排查时不要把它们优先当作核心问题。
- 渲染类问题不要只看字段名，要结合执行顺序、覆盖关系和父子对象的绘制链路判断。
- 反序列化类问题要区分“声明字段”与“真正生效的目标字段”，不要把 JSON 入口字段误认为最终成员。
- 若是 data patch / JSON mod 争议，先判断是修改已有对象，还是创建/定义新内容。
- 结论要落到可验证的最小 JSON 片段，避免只停留在概念解释。

## Recommended Workflow

1. **确认现象**
   - 报错信息
   - 期望效果
   - 实际效果

2. **判断类别**
   - 内容定义
   - 渲染绘制
   - 逻辑/AI
   - 科技树/解锁
   - 资源/纹理
   - 反序列化/接口

3. **查证顺序**
   - 先看知识库已有页面
   - 再用 `mindustry-api-parser` 做匹配搜索，确认类型、类、接口或字段相关名称是否真实存在
   - 再看源码实现
   - 再看本地示例 mod
   - 必要时再补网页资料

4. **验证最小样例**
   - 写一个最小 JSON
   - 逐个字段排除影响
   - 观察输出或报错

5. **沉淀结论**
   - 记录问题本质
   - 记录可复用模板
   - 补充相关概念页面链接

## Related

- [Mindustry JSON 可用接口概览](mindustry-json-interfaces.md) — 常见接口与字段入口参考
- [Mindustry JSON 中的 `type` 是否真实存在](mindustry-json-type-real-existence.md) — 解释 `type` 的反序列化入口含义
- [Mindustry JSON Mod 反序列化流程](mindustry-json-mod-deserialization.md) — JSON 内容加载与后处理链路
- [Mindustry Data Patch vs JSON Mod](mindustry-data-patch-vs-json-mod.md) — 区分修改已有对象与定义新内容
- [Mindustry JSON 技能约定](mindustry-json-skill-rules.md) — JSON 问题检索优先级与技能约定
- [Mindustry Mod 问题处理 SOP](../analysis/mindustry-mod-question-processing-sop.md) — 更宏观的 mod 问题处理顺序
