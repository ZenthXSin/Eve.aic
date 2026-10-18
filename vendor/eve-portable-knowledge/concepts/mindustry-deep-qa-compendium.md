# Mindustry 深度问答总纲

> Source: 当前对话总结

本页汇总一组 Mindustry 深度问题的回答框架，涵盖同步机制、实体注解、原版多合成工厂、实体-享元结构、平台与后端、Rhino 选择、iOS JS 边界、按需评估式工厂、JSON 方块不显示排查、多视口 UI 方案，以及英文/中文语言文件特征。

## Key Points

- 原版同步机制可按“状态同步、事件同步、网络调用同步、存档同步、表现同步”来分层理解。
- `EntityDef` 的核心价值是把 ECS/实体样板生成与样板代码收敛到注解处理流程中。
- 多合成工厂可用原版类型组合实现：输入路由、配方选择、产物缓存、输出调度、UI 展示。
- 原版大量采用“Block/Building”“Unit/UnitType”“Ability/Unit”“Controller/Unit”等实体-享元对应关系。
- Mindustry 依赖 Arc 与平台后端，跨平台主要靠 libGDX/Arc 的抽象层和各平台运行时。
- iOS 上 JS 受到动态代码与原生桥接边界限制，通常不能指望完整脚本生态。
- Tick 驱动的生产模型可改造成按需评估，但要单独处理传送带等连续物流的缓存与事件化。
- JSON 核心方块不显示时，常见原因集中在 type、资源名、路径、注册顺序、加载失败、命名冲突和依赖缺失。
- 嵌入式双视口 UI 需要独立相机、独立输入分发与焦点管理。
- 语言文件差异通常体现在命名习惯、语序、占位符、缩写、术语统一与本地化粒度。

## Related

- [JSON 问题解决流程](../concepts/json-problem-solving-flow.md)
- [Mindustry JSON 中的 `type` 是否真实存在](../concepts/mindustry-json-type-real-existence.md)
- [Mindustry 多合成工厂](../concepts/mindustry-multi-recipe-crafter.md)
- [Mindustry Kotlin 多合成工厂](../concepts/mindustry-kotlin-multi-recipe-crafter.md)
- [Mindustry Native/Java Mod Structure](../mindustry/java-mod-structure.md)