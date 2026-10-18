# Arc Draw 如何在屏幕上画东西

> Source: Conversation — 用户提问“原版Draw怎么实现在屏幕上画东西的”

## Key Points

- `Draw` 是高层门面，负责设置颜色、透明度、混色、shader、投影矩阵与图层顺序。
- 真正的绘制提交由 `Core.batch`（通常是 `SpriteBatch`）完成。
- 普通 2D 绘制走“顶点缓存 + batch flush + shader + GPU”链路。
- 相机通过正交投影矩阵把世界坐标转换到裁剪空间，最终映射到屏幕。
- `Draw.blit()` 走的是全屏四边形/后处理路线，不是普通 spritebatch 绘制。

## Related

- [Rendering System](../mindustry/rendering-system.md) — Mindustry 整体渲染系统入口
- [Arc Framework Overview](../mindustry/arc-framework.md) — Arc 框架的绘制与批处理基础
- [Arc Time & Timer](../concepts/arc-time-timer.md) — 与渲染帧节奏相关的时间系统
