# crystal-works mod 验证流程与方法

## 概述

本页面记录 Mindustry mod 开发中的完整验证流程，特别是通过 **无头服务器版（Headless Server）** 进行模组加载验证的方法。

## 验证流程

### 1. 静态检查（本地）

```bash
# 检查文件结构
unzip -l crystal-works.zip | grep -E "bundle|mod.json|content"

# 检查 bundle 键名格式
unzip -p crystal-works.zip "crystal-works/bundles/bundle_zh_CN.properties" | grep -E "^(item|block|liquid|status|mod)\."

# 检查 content JSON 是否有 type 声明
unzip -p crystal-works.zip "crystal-works/content/items/crystal.hjson"
```

### 2. 启动无头服务器验证

**前提条件**：
- 需要 `server-XXXX.jar`（Mindustry 服务器版，无需 OpenGL）
- 需要 Java 运行环境

**操作步骤**：

```bash
# 1. 确保 mod 已替换到 mods 目录
cp crystal-works.zip ~/文档/Mindustry/server/config/mods/crystal-works.zip

# 2. 启动服务器（后台运行，捕获日志）
cd ~/文档/Mindustry/server/config && \
  nohup java -jar ~/文档/Mindustry/server/server-2025.07.X12.jar > /tmp/mindustry-server.log 2>&1 &

# 3. 等待启动完成（约10-15秒）
sleep 15

# 4. 读取日志检查错误
cat /tmp/mindustry-server.log
```

### 3. 日志分析 - 关键错误模式

#### 3.1 致命错误（Fatal Errors）

```
arc.util.serialization.SerializationException: Serialization trace:
{}.drawer.drawers
drawers (mindustry.world.draw.DrawMulti)
...
Caused by: java.lang.RuntimeException: 'drawLiquid' in DrawLiquidTile is missing!
```

**含义**：`DrawLiquidTile` 的 `drawLiquid` 字段未设置 → 需要添加 `drawLiquid: <liquid_name>`

#### 3.2 字段名错误（Unknown Field）

```
[W] [crystal-turret.hjson]: Unknown field 'shootShake' for class 'ItemTurret'
[W] [crystal-turret.hjson]: Unknown field 'bulletWidth' for class 'BasicBulletType'
[W] [crystal-turret.hjson]: Unknown field 'bulletHeight' for class 'BasicBulletType'
```

**含义**：JSON 中定义的字段在该类中不存在 → 需要查找源码确认正确字段名

#### 3.3 不存在的 StatusEffect 引用

```
[E] Error for crystal-works-crystal-fluid / content/liquids/crystal-fluid.hjson:
Caused by: java.lang.IllegalArgumentException: Unknown status effect: 'cryocooling'
```

**含义**：`effect: cryocooling` 引用了不存在的状态效果 → 改为 `effect: none` 或定义该状态效果

#### 3.4 字段不存在（NoSuchFieldException）

```
[E] Error for crystal-works-crystal-shock / content/status/crystal-shock.hjson:
Caused by: java.lang.NoSuchFieldException: shock
```

**含义**：`effect: shock` 尝试引用不存在的字段 → `StatusEffect.effect` 应引用 `Effect` 类，而非 `StatusEffect` 的字段

### 4. 源码查询验证

```bash
# 查找类定义
find ~/文档/Mindustry/源码 -name "*.java" | xargs grep -l "class DrawLiquidTile"

# 查看完整字段列表
cat ~/文档/Mindustry/源码/core/src/mindustry/world/draw/DrawLiquidTile.java

# 查找所有 StatusEffect 可用的字段
cat ~/文档/Mindustry/源码/core/src/mindustry/type/StatusEffect.java | grep "public"
```

## 常见字段名对照表

| Hjson 字段 | 对应 Java 字段 | 类 | 备注 |
|-----------|---------------|-----|------|
| `drawLiquid` | `drawLiquid` | `DrawLiquidTile` | 必须设置 |
| `width` | `width` | `BasicBulletType` | 不是 `bulletWidth` |
| `height` | `height` | `BasicBulletType` | 不是 `bulletHeight` |
| `shake` | `shake` | `ItemTurret` | 不是 `shootShake` |
| `damageMultiplier` | `damageMultiplier` | `StatusEffect` | 不是 `armorMultiplier` |
| `effect` | `effect` | `StatusEffect` | 类型是 `Effect` 不是 `StatusEffect` |
| `type` | （无） | - | 某些类型不需要显式 `type` |
| `mass` | （无） | `Liquid` | 已被移除 |

## 验证成功标志

```
[05-18-2026 21:33:35] [I] [MindustryX] Hooks.beforeInit
[05-18-2026 21:33:35] [I] MDTX: bundle has been loaded.
[05-18-2026 21:33:38] [I] Server loaded. Type 'help' for help.
[05-18-2026 21:33:38] [I] [MindustryX] Hooks.init
```

**关键特征**：
- ✅ 没有 `[E]`（错误）开头的行
- ✅ 没有 `SerializationException`
- ✅ 没有 `NoSuchFieldException`
- ✅ 有 `Server loaded` 消息
- ❌ 警告 `[W]` 可能仍可接受（不影响加载）

## 注意事项

1. **GUI 显示 ≠ 加载成功**：服务器版只验证加载，不验证 GUI 显示
2. **中文显示需要桌面版验证**：中文显示依赖 GUI 和 `bundle_zh_CN.properties`
3. **bundle 键名格式**：
   - ❌ `item.crystal-works-crystal.name`（多余前缀）
   - ✅ `item.crystal.name`（与 content 名称匹配）
   - 需要 `mod.crystal-works.name` 定义模组名称

4. **type 字段争议**：
   - `Item`, `Liquid`, `StatusEffect`, `Block` 等类型**不需要**显式 `type` 字段
   - ContentParser 通过 `ContentType` 推断类型
   - 添加 `type` 会导致 "Unknown field" 警告

---

## 相关页面

- [Mindustry Content Loading Chain](../concepts/mindustry-content-loading-chain.md)
- [JSON 问题解决流程](../concepts/json-problem-solving-flow.md)
- [JSON 常见问题](../concepts/json-common-issues.md)
- [Mindustry JSON 接口真实存在性检查](../concepts/mindustry-json-interface-existence-check.md)