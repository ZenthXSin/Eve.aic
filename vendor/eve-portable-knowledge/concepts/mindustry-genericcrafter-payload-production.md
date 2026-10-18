# GenericCrafter 与 Payload 生产

## 结论

**原生 `GenericCrafter` 不能直接变成真正的 payload 输入/输出工厂。**

如果目标是“把载荷搬进工厂加工，再把载荷搬出去”，正确路线通常是：

- **继承 `PayloadBlock`**，而不是 `GenericCrafter`
- 在 `Building` 里实现 `acceptPayload()`、`handlePayload()`、`updateTile()`、必要时的输出逻辑
- 如果还要保留普通物品/液体消耗，可以把 `PayloadBlock` + consumes 逻辑组合起来

## 为什么 `GenericCrafter` 不够

`GenericCrafter` 主要面向：

- 物品输入
- 液体输入
- 电力消耗
- 输出物品/液体/部分实体效果

它没有天然的 payload 承载、移动、接收、转交机制。payload 逻辑属于 `PayloadBlock` 体系。

## 可行方案

### 方案 A：改成 `PayloadBlock`

这是最推荐的方案。

适合：
- 输入 `UnitPayload`
- 输入 `BuildPayload`
- 输入/输出搬运型结构
- 做装配、改造、拆解、拼装类工厂

核心点：
- 用 `PayloadBlock` 作为基类
- 让建筑自身持有 `payload`
- 通过 `acceptPayload()` 判断能否接收
- `handlePayload()` 把载荷存起来
- `updateTile()` 推进处理进度
- 完成后把 payload 替换成新结果并尝试输出

### 方案 B：保留 `GenericCrafter`，额外做 payload 侧车

不推荐，但可做。

思路：
- 主体仍是 `GenericCrafter`
- 旁边加一个 `PayloadConveyor` / `PayloadBlock` 作为入口/出口
- 用逻辑或自定义方块做中转

缺点：
- 结构绕
- 交互差
- 更难维护

### 方案 C：自定义一个“像 GenericCrafter 的 PayloadBlock”

这是实际开发里最常见的折中：

- 继承 `PayloadBlock`
- 复用 `GenericCrafter` 的“配方、进度、状态条、特效”思路
- 自己写 payload 输入/输出

这样既有工厂体验，又能处理 payload。

## 为什么不能直接在 GenericCrafter 上硬加 payload

从源码看，`GenericCrafterBuild` 只有：

- `progress`
- `warmup`
- `craft()` / `dumpOutputs()`
- `outputItems` / `outputLiquids`

它没有：

- `payload` 成员
- `acceptPayload()`
- `handlePayload()`
- payload 的位置推进、旋转、输出传递

这些能力都在 `PayloadBlock.PayloadBlockBuild` 里。

## 最小实现结构

### Kotlin 伪代码
```kotlin
class PayloadCrafter(name: String) : PayloadBlock(name) {

    init {
        update = true
        sync = true
        acceptsPayload = true
        outputsPayload = true
    }

    inner class PayloadCrafterBuild : PayloadBlockBuild<Payload>() {
        var progress = 0f
        var craftTime = 60f

        override fun acceptPayload(source: Building, payload: Payload): Boolean {
            return this.payload == null
        }

        override fun handlePayload(source: Building, payload: Payload) {
            this.payload = payload
            progress = 0f
        }

        override fun updateTile() {
            super.updateTile()
            if (payload == null) return

            // 把 payload 慢慢送到工厂内部
            if (!hasArrived()) {
                moveInPayload()
                return
            }

            progress += edelta() / craftTime
            if (progress >= 1f) {
                progress = 0f
                // 这里把当前 payload 替换成结果 payload
                // 然后 moveOutPayload() 或 dumpPayload()
            }
        }
    }
}
```

## 如果你要的是“像 GenericCrafter 一样有配方”的 payload 工厂

建议这样做：

- 仍然继承 `PayloadBlock`
- 自己定义配方数据结构
- 每个配方声明：
  - 输入 payload 类型
  - 可选附加资源（items/liquids/power）
  - 输出 payload 类型
  - craftTime
- 在 `updateTile()` 中判断当前 payload 是否匹配配方
- 完成后替换为输出 payload

## 实战建议

1. **别在 `GenericCrafter` 上硬塞 payload**，成本高且容易踩版本坑
2. 想做 payload 工厂，直接抄 `UnitAssembler` / `PayloadBlock` / `PayloadAmmoTurret` 的结构
3. 如果你愿意保留配方风格，可以把“配方选择”逻辑移到 `PayloadBlockBuild` 中
4. `outputItems` / `outputLiquids` 这套可以继续保留，但载荷本身必须走 `PayloadBlock`

## 相关页面

- [Mindustry 工厂输入/输出载荷生产](mindustry-load-production.md)
- [Mindustry Payload 输入/输出关键片段](mindustry-payload-io-snippets.md)
- [Mindustry 多合成工厂](mindustry-multi-recipe-crafter.md)
- [Mindustry Kotlin 多合成工厂](mindustry-kotlin-multi-recipe-crafter.md)
