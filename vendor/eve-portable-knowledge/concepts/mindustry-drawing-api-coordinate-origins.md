# Mindustry/Arc 绘制 API 坐标原点分类

## 核心结论

Arc 的绘制 API 坐标含义分三层，根本原因是 Draw/Fill 在 SpriteBatch 之上做了自己的封装

来源：Arc 源码 `arc-core/src/arc/graphics/g2d/` 下 Draw.java、Fill.java、Lines.java、SpriteBatch.java

---

## 底层接口

### SpriteBatch.draw(region, x, y, originX, originY, w, h, rotation)
- x,y 是**左下角**（bounding box 左下）
- constructVertices: `worldOriginX = x + originX`，x 就是左边缘
- 默认投影：y 轴向上，原点屏幕左下角

---

## 中心点 x,y 的方法

### Draw.rect(region, x, y, w, h)
核心实现：`Core.batch.draw(region, x - w/2f, y - h/2f, 0, 0, w, h, 0)`
自动减去半宽半高，把中心点转成底层的左下角

全部 Draw.rect 重载（带 originX/originY 的、带 rotation 的、带 String 的）都保持中心语义

### Fill.rect(x, y, w, h) → 委托 Draw.rect → **中心**
### Fill.circle(x, y, radius) → Draw.rect(center, 2r, 2r) → **中心**
### Fill.square(x, y, radius) → Fill.rect → **中心**
### Lines.circle/arc/poly(x, y, ...) → **中心**
### Lines.square(x, y, rad) → 转成 Lines.rect(x-rad, y-rad) → 也用中心语义

---

## 左下角 x,y 的方法

### Fill.crect(x, y, w, h) — "c" = corner
核心实现：`Draw.rect(atlas.white(), x + w/2f, y + h/2f, w, h)`
反向操作，把左下角转成中心给 Draw.rect

### Lines.rect(x, y, w, h)
内部用 Fill.crect 画四条边 → **左下角**

---

## 特殊方法

### Lines.line(x1, y1, x2, y2) — 两个端点，无原点概念

---

## y 轴方向

| 坐标系 | y 轴方向 | 原点 | 使用场景 |
|--------|---------|------|---------|
| SpriteBatch 默认投影 | 向上 | 屏幕左下角 | 游戏世界绘制（Draw/Fill/Lines） |
| Scene2d UI | 向下 | 屏幕左上角 | UI 组件定位 |

关键源码：`projectionMatrix.setOrtho(0, 0, Core.graphics.getWidth(), Core.graphics.getHeight())`

---

## 记忆口诀

- Draw.rect / Fill.rect (无c) → **中心**
- Fill.crect / Lines.rect / SpriteBatch.draw → **左下角**
- Lines.line → **两个端点**
- UI scene2d 的 Element.draw() → **左上角**

## 常见坑

在 draw() 或 drawLight() 中混用 Draw.rect（中心）和 Fill.crect（左下角）会导致偏移半宽半高的错位

参考：[Mindustry Building 与 Tile 坐标关系](mindustry-building-tile-coordinate-relationship.md) 、 [Arc Draw 如何在屏幕上画东西](arc-draw-screen-rendering.md)
