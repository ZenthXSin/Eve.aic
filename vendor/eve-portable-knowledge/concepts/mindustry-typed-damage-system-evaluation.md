# Mindustry 伤害类型系统设计评价

> Source: QQ discussion synthesis; checked against existing damage knowledge and Mindustry source paths.

## 结论

给伤害添加类型有价值，但不适合侵入原版 `damage(float)` 主链路。更合理的做法是做一层 mod 自己的伤害上下文，例如 `TypedDamageContext`，在子弹命中、状态效果、建筑受击回调等入口转发。

## 评价

- 创新性：中高。Mindustry 原版伤害主要是数值、护甲、穿甲、状态效果，没有通用元素/类型克制系统。
- 可行性：中。能在自定义 `BulletType`、`StatusEffect`、`Ability`、`Building` 中落地；但全局覆盖原版所有伤害需要 patch 或大量桥接。
- 玩法多样性：高。可做火/冰/电/腐蚀/动能/能量等类型，配合抗性、弱点、环境、科技树和单位装甲形成构筑。
- 可拓展性：高，但前提是数据驱动。类型表、抗性表、反应规则应集中注册，不要散落在每个子弹类里。

## 推荐边界

第一版只处理 mod 自己的伤害来源：自定义子弹、状态、炮塔、能力。不要试图一开始重写原版 `HealthComp`/`BuildingComp` 伤害链路。

## Related

- [Mindustry 百分比伤害](mindustry-percent-damage.md)

## 剧情 Boss 特殊伤害处理

对于剧情 Boss 的阶段性特殊处理，不建议把规则写死进通用伤害表，也不建议改原版 `HealthComp.damage(float)`。推荐在 mod 自己的 `TypedDamageResolver` 中增加一层 `DamageRule` / `BossDamagePolicy`：

- 输入：`Unit target`、`TypedDamageContext ctx`、当前剧情 flag / phase / wave / sector state。
- 输出：修改后的 `amount`、是否免疫、是否转化为治疗、是否触发反击/护盾/剧情事件。
- 挂载位置：自定义 Boss 单位的运行时状态组件或外部 `ObjectMap<Unit, BossState>`，不要直接依赖原版 entity 生成类结构。
- 命中入口：仍从自定义 `BulletType.hitEntity`、Ability、Status 等 mod 伤害源进入 resolver，最终再调用原版 `Healthc.damage` / `damagePierce`。

这样 Boss 特判属于规则层，普通抗性属于数据层，二者不会互相污染。
