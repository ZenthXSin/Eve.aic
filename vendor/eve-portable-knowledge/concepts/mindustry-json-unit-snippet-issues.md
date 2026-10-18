# Mindustry JSON Unit 片段常见问题

## 目标
记录一段 `MechUnit` 单位 JSON 中常见的结构性问题，便于快速排错。

## 结论
这段 JSON **至少有 5 类问题**，其中部分是**必错**，部分是**强烈可疑/高概率无效**。

## 问题清单

### 1. `type: "MechUnit"` 很可能不是可直接用的 JSON 入口类型
Mindustry JSON 里通常需要确认 `type` 对应的类是否真实存在、是否是该 JSON 入口支持的对象。
如果入口类型不匹配，解析可能回退或直接不按预期构建。

### 2. `weapons` 的中文 `name` 可疑
`weapons` 里的 `name: "主炮"` 是否可接受，要看该字段是否只是调试名/标识名。一般不建议依赖中文名作为内部标识。

### 3. 武器字段里 `spacing` 对 `BasicBulletType` 武器未必有效
`spacing` 常见于特定开火模式或多管配置，并不是所有 weapon 配置都需要或支持。

### 4. `bullet.type: "BasicBulletType"` 需要确认该 JSON 接口是否真实存在且字段可被反序列化
子弹 JSON 也必须匹配真实可用的类/接口结构，不能只看名字像就写。

### 5. `requirements` 里的 `Cm` 物品名高度可疑
如果 `Cm` 不是已注册物品名，会直接导致需求无效或报错。

## 备注
- `localizedName` / `description` 这类字段通常没问题。
- 这类 JSON 问题应优先做**存在性验证**：先确认 `type`、武器类型、子弹类型、物品名是否真实存在，再判断结构是否合理。

## 关联
- [Mindustry JSON 中的 `type` 是否真实存在](mindustry-json-type-real-existence.md)
- [Mindustry JSON 接口真实存在性检查](mindustry-json-interface-existence-check.md)
- [JSON 问题解决流程](json-problem-solving-flow.md)
