# Mindustry 深度验证总结与硬性规则

## 本轮验证覆盖系统（2026-03-24）

### 新增知识页
1. **content-system.md** - ContentType枚举12个有效类型、Content生命周期、UnlockableContent全字段
2. **unit-system.md** - UnitType 2065行、50+布尔字段、默认值全表
3. **events-system.md** - EventType 795行、Trigger枚举42个、Event类57个
4. **status-ability-system.md** - StatusEffect全字段、17个原版Ability
5. **research-tech-tree-system.md** - TechTree上下文栈构建、TechNode全字段
6. **modding-api.md** - Mod基类、ContentParser类型解析、Mods加载流程

### 验证统计
- 源码验证量：约25000+行
- 提取硬性规则：约500+条
- 更新占位页面：6个

## 高频幻觉点与硬性规则

### 1. 内容系统幻觉
- ❌ ContentType 有20+种 → ✅ 有效只有12个（6个UNUSED）
- ❌ Content.init() 可以加载纹理 → ✅ 源码注释明确禁止
- ❌ load() 在 headless 服务器调用 → ✅ 只在非headless调用
- ❌ id 是 int 类型 → ✅ 是 short 类型（最大32767）

### 2. 单位系统幻觉
- ❌ flying 默认 true → ✅ 默认 false（大多数是地面单位）
- ❌ canBoost 默认 true → ✅ 默认 false
- ❌ omniMovement 默认 false → ✅ 默认 true（全向移动）
- ❌ canDrown 默认 false → ✅ 默认 true（地面单位会溺亡）
- ❌ useUnitCap 默认 false → ✅ 默认 true（受上限限制）
- ❌ itemCapacity 是固定值 → ✅ -1 时按体型自动计算
- ❌ naval 手动设置 → ✅ 在 init() 中自动设置

### 3. 事件系统幻觉
- ❌ Trigger 是 Event 类 → ✅ 是枚举，用 Events.run() 不是 Events.on()
- ❌ ContentLoadEvent 存在 → ✅ 不存在，只有 ContentInitEvent 和 ModContentLoadEvent
- ❌ 所有事件可安全缓存 → ✅ BuildDamageEvent/TilePreChangeEvent/UnitDamageEvent 等是复用的
- ❌ PlayerChatEvent 客户端触发 → ✅ 服务器端，ClientChatEvent 才是客户端

### 4. 状态/能力系统幻觉
- ❌ StatusEffect 倍率默认 0 → ✅ 全部默认 1f
- ❌ effectChance 默认 1.0 → ✅ 默认 0.15f
- ❌ EnergyFieldAbility = ForceFieldAbility → ✅ 前者伤害，后者护盾
- ❌ Ability.data 无意义 → ✅ 是唯一同步字段

### 5. 科技树幻觉
- ❌ TechNode 是独立类 → ✅ 是 TechTree 的静态内部类
- ❌ 研究进度存存档 → ✅ 存 Core.settings（全局设置）
- ❌ 依赖需手动添加 → ✅ 自动从 content.getDependencies() 转换
- ❌ requiresUnlock 对所有节点有效 → ✅ 只对根节点有效

### 6. Modding API 幻觉
- ❌ Mod.init() 在 loadContent() 后立即调用 → ✅ 在所有内容加载后调用
- ❌ ContentParser 严格校验字段 → ✅ ignoreUnknownFields = true
- ❌ Color 可以用数字 → ✅ 必须是字符串，否则警告
- ❌ 类型解析无顺序 → ✅ 固定顺序：block→item→unit→liquid→planet

## 输出前自检清单

回答 Mindustry 问题前必须检查：

1. **字段默认值** — 布尔/数值字段的默认值是否正确？常见错误：flying/canBoost/omniMovement
2. **生命周期时序** — init/postInit/load/loadIcon 的调用顺序和条件
3. **事件类型** — 是 Trigger 还是 Event？是复用的吗？
4. **系统独立性** — 热系统独立于电力，载荷系统独立于普通方块
5. **JSON 解析** — ContentParser 忽略未知字段，Color 必须是字符串

## 相关知识页索引

- [Content System](mindustry/content-system.md)
- [Unit System](mindustry/unit-system.md)
- [Events System](mindustry/events-system.md)
- [Status and Ability System](mindustry/status-ability-system.md)
- [Research Tech Tree System](mindustry/research-tech-tree-system.md)
- [Modding API](mindustry/modding-api.md)
- [Block Full Fields](concepts/mindustry-block-full-fields.md)
- [Turret System](concepts/mindustry-turret-system.md)
- [Heat System](concepts/mindustry-heat-system.md)
- [Payload System](concepts/mindustry-payload-system.md)
