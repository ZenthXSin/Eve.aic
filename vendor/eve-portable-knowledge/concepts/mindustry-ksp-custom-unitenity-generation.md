# Mindustry CustomUnitEntity KSP 生成思路

> 来源：本地知识库 `concepts/mindustry-mod-lifecycle-parser-boundary.md`、`sources/learn-mindustry-mod-github-io/full/java/7-program-structure/B-compiletime-ecs-gpt.md`

## 结论

Mindustry 的实体不是运行时手搓类，而是编译期把 `*Comp` 组件拼成 `mindustry.gen.*` 成品。`CustomUnitEntity` 这类东西，KSP 最稳的做法不是生成继承链，而是生成一个 `@EntityDef`/组件声明片段，交给现有注解处理器出最终类。

## 最简 KSP 产物

生成一个文件，比如：
```kotlin
@EntityDef(Unitc::class)
@Component
abstract class CustomUnitComp : UnitComp
```

如果你真要“生成方法”，那就让 KSP 只吐一个工厂壳：
```kotlin
fun customUnit(): Unitc = ...
```
但最终实体实现还是得走 Mindustry 的 EntityProcess，不然你是在跟现有 APT 打架，没必要。

## 关键点

- `loadContent()` 管内容注册，实体生成是编译期注解流程
- `Unitc` / `UnitComp` 这条命名映射是硬规则
- 真要自定义字段、方法，优先做 `*Comp`，别硬造 `CustomUnitEntity`
