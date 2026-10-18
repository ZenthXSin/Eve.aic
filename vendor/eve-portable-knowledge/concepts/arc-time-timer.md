# Arc Time & Timer

> Source: 本次对话中关于 `arc.util.Time` 的问题；结合现有知识库中 `arc.util` 补全清单整理

## 说明
`arc.util.Time` 是 Arc / Mindustry 中非常核心的时间工具。它既不是“普通的系统时钟”，也不只是“延迟执行器”，而是连接了**帧推进、定时调度、动画节奏、延迟逻辑**的一层基础设施。

在 Mindustry Mod 开发里，你会经常在这些场景碰到它：
- 延迟触发一次效果
- 若干 tick 后恢复状态
- 控制动画节奏
- 做冷却、闪烁、抖动、过渡、缓动
- 把“稍后做”的逻辑交给框架，而不是自己手写 while 轮询

---

## 1. Time 的角色

`Time` 属于 Arc 的时间基础工具。它的价值不在于提供复杂 API，而在于提供一种统一的时间语义：

- **时间是按游戏推进的**，不是按真实世界独立运行的
- **调度是和主循环绑定的**，不是随便开线程跑
- **很多逻辑以 tick/frame 为单位**，而不是秒

这意味着：
- 游戏暂停时，很多基于游戏时间的逻辑也会停住或减速
- 你在 Mod 里安排的延迟动作，通常会和游戏帧同步
- 不要把它当成 `java.util.Timer` 那种完全独立的后台定时器来理解

---

## 2. Time.run()

`Time.run()` 是最常见的入口之一，用来安排延迟执行。

### 基本理解
你可以把它理解成：

> “过一段游戏时间后，再执行这段逻辑。”

### 典型用途
- 视觉效果延迟触发
- 建筑状态短暂变化后恢复
- 攻击命中后延迟做二次处理
- 一段时间后清除标记、闪烁、重置参数

### 使用思路
不要把它用于高频、重复、持续维护型逻辑；那类逻辑通常更适合：
- `update()` / `updateTile()`
- `Timer` 驱动的重复事件
- 显式状态机

`Time.run()` 更适合“**一次性的、带延迟的、轻量的动作**”。

---

## 3. Timer / TaskQueue 的关系

在 Arc / Mindustry 的语境里，`Time`、`Timer`、`TaskQueue` 往往一起出现，但职责并不完全一样：

### `Time`
- 提供时间语义
- 常用于安排延迟逻辑
- 更像时间工具层

### `Timer`
- 更偏“按间隔触发”的定时辅助
- 常用于周期性检查、间隔刷新
- 适合做重复动作节流

### `TaskQueue`
- 更偏任务排队与执行
- 适合把要在主线程/合适时机执行的任务排进去
- 让执行节奏更可控

如果你想记一个简单判断：
- **一次性延迟** → 优先想到 `Time.run()`
- **周期性触发** → 优先想到 `Timer`
- **任务排队/顺序执行** → 优先想到 `TaskQueue`

---

## 4. 在 Mindustry Mod 中常见的使用场景

### 4.1 延迟效果
比如命中后 0.5 秒再爆一下、闪一下、抖一下。

### 4.2 冷却恢复
比如方块临时失效，过几秒后恢复工作。

### 4.3 状态回退
比如临时改了某个字段，延迟后恢复原值。

### 4.4 过渡动画
比如门、炮台、机械臂、指示灯、UI 状态的缓动。

### 4.5 逻辑分段
比如把“立刻做”和“稍后做”拆开，降低单次更新压力。

---

## 5. 设计上的注意点

### 5.1 不要滥用
如果一个逻辑本质上每帧都要检查，别硬塞进大量 `Time.run()`。

### 5.2 注意对象生命周期
延迟执行时，目标对象可能已经：
- 被销毁
- 失效
- 离开场景
- 被替换

因此延迟回调里通常要再次检查对象是否仍然有效。

### 5.3 注意时间单位
Mindustry 里很多逻辑更关心 tick/frame，而不是现实秒数。写代码时要确认你传的是哪种单位语义。

### 5.4 注意主线程约束
不要把游戏状态修改逻辑随便扔到后台线程。时间工具解决的是“何时执行”，不是“在哪个线程执行”。

---

## 6. 快速判断指南

如果你在写 Mod 代码时犹豫要不要用 `Time`，可以直接问自己：

1. 这是一次性的延迟动作吗？
2. 这是必须跟游戏帧同步的吗？
3. 这段逻辑需要在“稍后”执行，而不是现在执行吗？

如果答案基本都是“是”，那就很可能适合 `Time.run()` 或相关调度机制。

---

## 7. 常见误区

### 误区 1：把 Time 当成系统定时器
`Time` 的语义通常和游戏时间强绑定，不要默认它等于独立后台计时。

### 误区 2：把一次性延迟逻辑写成轮询
很多人会不自觉地把“几秒后执行一次”写成每帧检查一次，这通常更重。

### 误区 3：忽略对象状态变化
延迟执行时最容易出 bug 的地方，就是回调触发时对象已经不在了。

---

## 8. 源码层面的关键点

基于 `源码和示例/Arc/arc-core/src/arc/util/Time.java`，`Time` 里最关键的几个成员是：

- `public static final float toSeconds = 60f`
- `public static final float toMinutes = 60f * 60f`
- `public static final float toHours = 60f * 60f * 60f`
- `public static float delta = 1f`
- `public static float time, globalTime`

这说明 Arc 默认把 **1 秒视为 60 tick** 的语义来组织很多时间值。

### `delta`
`delta` 不是“真实秒数”，而是**按 60fps 归一化后的帧增量**。默认实现：

- `Core.graphics.getDeltaTime() * 60f`
- 并用 `Math.min(..., 3f)` 限制最大值

所以：
- 正常 60fps 时，`delta ≈ 1`
- 30fps 时，`delta ≈ 2`
- 掉帧很严重时也会被截断，避免单帧推进过猛

### `time` 和 `globalTime`
- `time`：由 `Time.update()` 用 `delta` 推进
- `globalTime`：由 `updateGlobal()` 用 `Core.graphics.getDeltaTime() * 60f` 推进

可简单理解为：
- `time` 更偏当前时间推进值
- `globalTime` 更偏全局持续推进值

### `Time.run(float delay, Runnable r)`
源码注释写得很直接：

> Runs a task with a delay of several ticks. If Time.clear() is called, this task will be cancelled.

也就是：
- 这里的 `delay` 单位是 **tick**，不是秒
- 它把任务放进 `runs` 队列里
- 每次 `Time.update()` 时用 `run.delay -= delta`
- 当 `delay <= 0` 时执行
- `Time.clear()` 会直接把这些延迟任务清掉

因此 `Time.run(60f, ...)` 基本可以理解成：
- **大约 1 秒后执行一次**

### `Time.runTask(float delay, Runnable r)`
源码：

```java
return Timer.schedule(r, delay / 60f);
```

这说明：
- 传进来的 `delay` 仍然按 **tick** 理解
- 但它实际转成秒后交给 `Timer`
- 与 `Time.run()` 不同，注释说明它**除非应用关闭，否则最终都会完成**

这是 `Time` 和 `Timer` 之间很关键的桥接点。

### `mark()` / `elapsed()`
这两个方法不是调度器，而是**简单计时器**：

- `mark()`：记录当前 `System.nanoTime()`
- `elapsed()`：返回距离上次 `mark()` 过去了多少毫秒

适合：
- 简单性能测量
- 局部耗时统计
- 快速打点排查

不适合：
- 代替游戏逻辑调度

---

## 9. `Time` 和 `Timer` 的真正区别

结合 `源码和示例/Arc/arc-core/src/arc/util/Timer.java`：

### `Time`
- 主要靠主循环里的 `Time.update()` 推进
- 延迟单位偏 **tick / 帧语义**
- `Time.clear()` 会取消 `Time.run()` 的任务
- 更适合和游戏更新强绑定的短延迟逻辑

### `Timer`
- 内部有单独的 `TimerThread`
- 调度单位是 **秒**（内部换算到毫秒）
- 任务最终通过 `task.app.post(task)` 回到主循环线程执行
- 支持：
  - 单次延迟
  - 固定间隔
  - 指定重复次数
  - 无限重复

所以可以简单记：

- **短延迟、tick 语义、跟帧同步感强** → `Time.run()`
- **定时任务、周期任务、秒语义更直观** → `Timer.schedule()`

---

## 10. Mod 里怎么选

### 用 `Time.run()` 的典型情况
- 10 tick 后播一个特效
- 30 tick 后恢复一个临时状态
- 想写“若干帧后执行一次”的逻辑

例如：

```java
Time.run(30f, () -> {
    if(build != null && build.isValid()){
        build.enabled = true;
    }
});
```

### 用 `Timer.schedule()` 的典型情况
- 每 0.5 秒检查一次
- 2 秒后执行，并每 1 秒重复
- 更想按“秒”思考，而不是按 tick

例如：

```java
Timer.schedule(() -> {
    // periodic logic
}, 2f, 1f);
```

---

## 11. 常见坑

### 1）把 `delay` 当秒
`Time.run(1f, ...)` 不是 1 秒后，而是**约 1 tick 后**。

如果你想 1 秒后执行：

```java
Time.run(60f, ...);
```

### 2）回调时对象已经失效
延迟执行时，建筑、单位、子弹可能已经被移除，所以回调里最好重查：

- 是否为 `null`
- 是否还有效
- 是否还在世界里

### 3）把高频逻辑塞进大量 `Time.run()`
如果逻辑本质上每帧都要检查，还是应该放到：

- `update()`
- `updateTile()`
- 状态机
- 周期性定时器

而不是不停堆积延迟回调。

---

## 12. 一句话总结

`arc.util.Time` 本质上是 **按 60tick 语义推进的时间工具**：`delta` 是归一化帧增量，`Time.run()` 适合做按 tick 延迟的一次性逻辑，而更通用的秒级/周期调度则更适合交给 `Timer`。

---

## 相关
- [Arc util 内容补全建议](arc-util-overview.md) — `arc.util` 待补主题总览
- [Mindustry Mod 问题处理 SOP](../analysis/mindustry-mod-question-processing-sop.md) — Mindustry 问题的处理顺序
- [Arc Framework Overview](../mindustry/arc-framework.md) — Arc 框架概览占位页
