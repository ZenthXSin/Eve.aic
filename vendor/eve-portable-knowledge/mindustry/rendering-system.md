# Rendering System

> Source: Mindustry source code and existing knowledge pages

Mindustry 的画面渲染核心由 `mindustry.core.Renderer` 驱动，底层依赖 Arc 的 `Draw`、`Camera`、`FrameBuffer`、`Bloom` 等图形设施。整体流程可以概括为：**更新相机与渲染状态 → 按图层顺序提交绘制任务 → 执行特效/后处理 → 输出到屏幕**。

## 核心入口
- `Renderer.update()`：每帧更新摄像机缩放、屏幕抖动、设置开关，并决定走普通渲染还是像素化渲染。
- `Renderer.draw()`：真正组织一帧中各个图层的绘制顺序。
- `Renderer.drawBackground()`：绘制规则背景图、星球背景或自定义背景。

## 一帧是怎么画出来的
1. **准备阶段**
   - 更新 `camera.width / height`，让视口跟随缩放变化。
   - 清屏，重置 `Draw` 状态。
   - 更新 `blocks`、`floor`、`minimap` 等会影响画面的系统。

2. **提交图层绘制**
   - 通过 `Draw.draw(Layer.xxx, runnable)` 把不同内容挂到对应 z 层。
   - 常见顺序大致是：
     - `Layer.background`：背景
     - `Layer.floor`：地表
     - `Layer.block`：方块、阴影、墙体等
     - `Layer.light` / `Layer.darkness`：光照与黑暗
     - `Layer.effect`：子弹、粒子、特效
     - `Layer.overlayUI` / `Layer.fogOfWar`：覆盖层、战争迷雾
     - `Layer.space`：核心起降、太空动画
     - `Layer.plans` / `Layer.shields` / `Layer.buildBeam`：计划、护盾、建造光束等

3. **特殊效果与后处理**
   - `bloom`：启用时先 `capture` 再 `render`，做泛光。
   - `effectBuffer`：用于护盾、建造光束等需要先渲染到缓冲区再套 shader 的内容。
   - `pixelator`：像素化模式时走单独渲染路径。
   - `lights.draw()`：光照贴图/遮罩。
   - `fog.drawFog()`：战争迷雾。

4. **最终提交**
   - `Draw.flush()` 把批处理里的内容真正发到 GPU。
   - `Draw.sort(false)` 做最后的排序/收尾。

## 关键机制

### 1) 图层系统
Mindustry 不会把所有东西乱画，而是把绘制逻辑拆成很多层。`Layer` 决定谁先画、谁后画，保证地面、建筑、特效、UI 不互相遮挡错乱。

### 2) 批处理
大量精灵不会逐个立即提交，而是先进入 `Draw` 的批处理队列，最后统一 flush，减少状态切换和 draw call。

### 3) FrameBuffer 与 shader
像护盾、建造光束、星球背景、泛光这些内容，往往先画进 `FrameBuffer`，再用 shader 处理后贴回屏幕。

### 4) 相机与缩放
渲染尺寸由 `camera.position`、`camera.width`、`camera.height` 决定。缩放来自 `Renderer.targetscale`、`camerascale`，再结合设置项和剧情过场状态动态调整。

### 5) 背景绘制
`drawBackground()` 会按规则优先级选择：
- 普通背景纹理 `backgroundTexture`
- 星球背景 `planetBackground`
- 自定义背景回调 `customBackgroundCallback`

## 简化理解
可以把 Mindustry 的画面理解成：
- **Renderer 负责调度**
- **Layer 负责排队**
- **Draw 负责批量提交**
- **FrameBuffer / Shader 负责特效**
- **Camera 负责看哪里、看多大**

## 关联
- [UI System](../mindustry/ui-system.md)
- [Vars Renderer](../mindustry/vars-renderer.md)
- [Mindustry Knowledge Structure](../mindustry/structure-overview.md)
- [Mindustry 自定义 Shader 接入](mindustry-custom-shader.md)
- [Mindustry 持续激光伤害宽度](mindustry-continuous-laser-width.md)
- [Mindustry 原版发射台评估系统](mindustry-launch-pad-evaluation-system.md)