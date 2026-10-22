# 编译型 Java mod 特效做成 JSON 可配置（注入/覆盖 mod 思路）

> 来源：群聊任务 2026-08-13（把第三方编译 mod "Flame Out" 的 despondency 特效做成 JSON 可配置）
> 验证版本：原版 Mindustry 159.7

## 场景

用户给一个**已编译 jar（无源码、无 content HJSON 目录）**的第三方 Java mod，要求把特定特效做成可配置 JSON 接口。

## 可行性前提（已源码印证）

- Mindustry `Effect` 的字段：`lifetime`、`clip`、`layer`、`renderer`。
- `renderer` 是 **`public Cons<EffectContainer>`**（可读可写），`lifetime`/`clip` 也是 public。
- 如果特效是 `public static Effect` 字段（如 `FlameFX.desNuke`），可在运行时用反射读取原特效的 `renderer`，构造新的 `Effect` 只改 `lifetime/clip/layer` 再覆盖回静态字段 —— **不破坏原绘制 lambda**。
- 效果在运行时各调用点是 `FlameFX.desXxx` 这种静态引用，运行时覆盖字段即可全局生效。

## 边界（诚实告知）

- **通用参数覆盖（可行）**：`enabled`（屏蔽，替换为 `Fx.none`）、`lifeScale`、`clipScale`、`layer`。
- **绘制级参数（不可直接 JSON 化）**：改颜色/粒子大小/数量属于特效内部 lambda 的魔法数，必须把每个 lambda 逐帧翻译成参数化代码（工作量数量级差异）。编译 jar 无源码，只能用 `javap -p -c` 反编译——lambda 体在私有静态 `lambda$static$N` 方法里（javap 能看到），需按 static 块顺序确定每个特效绑定哪个 lambda。

## 反编译要点

- `javap -p -c` 输出大（本案例 7514 行），lambda 通过 invokedynamic 绑定，映射需按 static 块字节码顺序确定特效 ↔ lambda 编号。
- 关注 `life/clip/layer` 的构造参数，可在一开始就整理出全部特效的参数表（本案例 15 个 `des*`/`end*` 特效）。

## Source

- `mindustry/type/Effect`（`lifetime`/`clip`/`layer`/`renderer` 字段）
- `Fx.none`（`mindustry/type/Fx`）
- 反编译工具：`javap -p -c`
