# Mindustry JS 恒星生成模板

> Source: conversation about generating a custom stellar planet in JavaScript for Mindustry

这是一个面向 Mindustry Java/JS mod 场景的恒星外观模板，目标是快速生成一颗带有冰蓝、雾白、淡紫和浅青极光风格的恒星。

## Key Points

- 设计目标可以拆成三层：主体底色、外层大气、两极极光。
- 主体建议使用冰蓝色与浅青灰的低饱和组合，避免过于纯白导致发灰或过曝。
- 大气层适合用雾白、淡紫、浅青做渐变或分层混合，以突出“冷光感”。
- 极光更适合放在两极区域，使用浅蓝或青蓝的发光色，并与云层/壳层叠加。
- 生成器模板最好先能跑起来，再逐步调外观参数。

## Template Notes

- `Planet` 的基础参数包括名字、所属恒星、尺寸、层级/位置等。
- 视觉部分通常通过 `meshLoader` / `cloudMeshLoader` / 大气颜色等参数塑造。
- 如果具体 Java/JS 环境对某些字段不兼容，应优先参考源码里实际可用的 `Planet` 构造和渲染实现。
- 这类模板应与实际资源图和生成器逻辑配套，而不是只靠一份脚本完成全部表现。

## Related

- [Mindustry JSON 星球生成器模板](mindustry-json-planet-generator-template.md) — JSON 形式的星球模板骨架。
- [Mindustry JSON 可用接口概览](mindustry-json-interfaces.md) — 相关接口与字段整理入口。
- [Mindustry JSON 技能约定](mindustry-json-skill-rules.md) — JSON/内容查询优先级规则。
