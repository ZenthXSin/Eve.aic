# Mindustry JSON 工厂活塞运动动画

> 记录如何在 JSON 工厂里做出类似活塞往复的机械动画。

## 核心思路
Mindustry 的 JSON 工厂动画通常不是“骨骼动画”，而是通过 `draw` 里的多个 `DrawBlock` 组件叠加实现：

- 底座/外壳：静态图层
- 活塞杆：用可移动的 `Draw` 组件，根据 `progress` 或 `warmup` 做偏移
- 旋转件/连杆：用旋转或摆动的 `Draw` 组件配合插值曲线
- 发光/烟雾：用粒子或额外图层增强机械感

因此，“活塞运动”的本质是：**让某个绘制层沿轴向做周期性位移**。

## 适合的实现方式
### 1. 用 `DrawLiquidRegion` / `DrawDefault` 类似的位移参数
如果你的 block 解析器支持位置偏移字段，可以让某个图层随 `progress` 改变 `x` / `y`。

### 2. 用 `DrawGlowRegion` / `DrawRegion` 的自定义偏移
把活塞杆单独拆成一层，设置它的绘制偏移，使其在运行时沿固定方向伸缩。

### 3. 用 `warmup` 或 `progress` 驱动
最常见的是：

- `warmup`：0 到 1，表示工厂当前工作强度
- `progress`：0 到 1，表示一次生产周期进度

如果想要“来回运动”，一般需要将 `progress` 映射成三角波或正弦波式的位移，而不是单向线性推进。

## 动画公式建议
### 三角波式活塞
让活塞在一个周期内“前进—后退”：

```text
p = progress
motion = 1 - abs(p * 2 - 1)
```

这个值会在 0→1→0 间变化，适合做往复运动。

### 正弦波式活塞
更平滑：

```text
motion = sin(progress * PI)
```

如果要更像机械活塞，可以再做一点硬切：

```text
motion = pow(sin(progress * PI), 1.5)
```

## JSON 结构思路
如果你的 JSON 版本/解析器支持可配置 `DrawBlock`，可以把活塞拆成独立图层：

```json
{
  "type": "GenericCrafter",
  "name": "piston-crafter",
  "draw": {
    "type": "DrawMulti",
    "draws": [
      { "type": "DrawDefault" },
      {
        "type": "DrawRegion",
        "region": "piston-rod",
        "x": 0,
        "y": 0
      }
    ]
  }
}
```

然后让 `piston-rod` 这一层跟随生产进度做位移。具体字段名是否支持“按 progress 偏移”，取决于你当前使用的 JSON 反序列化实现。

## 更稳妥的落地方案
如果你发现纯 JSON 无法直接把某一层绑定到 `progress`，最稳的是：

1. **JSON 只负责静态结构**：基础外壳、活塞杆图层、发光层
2. **Java/Kotlin 自定义 DrawBlock**：在 `draw(...)` 里根据 `warmup/progress` 计算位移
3. **JSON 里仅引用这个 DrawBlock 类型**

这在 Mindustry 里通常是最可控、最像“真正活塞动画”的做法。

## 可直接尝试的方向
如果你现在只是想先在 JSON 里试效果，建议：
- 把活塞杆单独做成一个 region
- 用 `DrawMulti` 叠层
- 优先尝试 `progress` 驱动的位移/插值
- 若不生效，再转 Java/Kotlin 自定义绘制逻辑

## 结论
**JSON 工厂的活塞动画，本质是“拆层 + 进度驱动位移”。**
最常见的实现是让某个绘制层沿固定轴根据 `progress` 或 `warmup` 往复偏移；如果 JSON 解析能力不够，就用自定义 `DrawBlock` 补上。
