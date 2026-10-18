# Mindustry 原版着色器渲染实现路线

> Source: Mindustry 源码 `mindustry.core.Renderer`、`mindustry.graphics.Shaders`，以及知识页 [Rendering System](../mindustry/rendering-system.md)、[Mindustry 自定义 Shader 接入](mindustry-custom-shader.md)、[Helium 屏幕采样与着色器渲染](helium-screen-sampling-rendering.md)

## 结论

从零复现原版着色器渲染效果，核心不是“写一个 shader”而是复刻 Mindustry 的渲染管线：**Layer 排序 + FrameBuffer 捕获 + Shader 绑定 + 目标阶段 blit + 受设置项控制的开关**。

## 原版关键点

- `mindustry.core.Renderer` 负责一帧的渲染调度。
- `mindustry.graphics.Shaders` 集中管理原版 shader。
- `Renderer.effectBuffer` 用于护盾、建造光束等后处理。
- `Renderer.draw()` 中通过 `Draw.draw(...)` 和 `Draw.drawRange(...)` 把不同效果挂到固定层。
- `Shaders.LoadShader` 通过 `getShaderFi(vert + ".vert")` 和 `getShaderFi(frag + ".frag")` 读取 shader 文件。

## 从零实现步骤

1. **先复刻层级调度**
   - 定义类似 `Layer.background / floor / block / light / darkness / effect / shields / buildBeam / ui` 的绘制顺序。
   - 所有内容不要乱画，必须按层提交。

2. **建立主渲染器**
   - 一帧里先更新 camera，再清屏，再 `Draw.reset()`。
   - 用一个中心类统一控制是否启用 bloom、pixelate、light、darkness、shields。

3. **建立 Shader 管理器**
   - 把 shader 的加载、实例化、uniform 传递集中到一个 `Shaders` 或 `ShaderManage` 类。
   - 屏幕空间 shader 优先复用 `screenspace.vert`。

4. **建立 FrameBuffer 管线**
   - 需要后处理的内容先画进 `FrameBuffer`。
   - 再调用 `blit(shader)` 回贴到屏幕或目标 buffer。
   - 护盾、建造光束、模糊、扭曲都走这条路。

5. **补齐 uniform 更新**
   - 常用 uniform：时间、分辨率、相机位置、缩放、纹理槽、颜色参数。
   - 每帧在 shader `apply()` 中更新。

6. **让效果受配置控制**
   - 参考原版 `settings.getBool(...)`、`settings.getInt(...)`。
   - 把动画护盾、光照、像素化、特效开关做成运行时选项。

## 原版实现证据

### `Renderer.draw()` 的关键结构

- `graphics.clear(clearColor)` 清屏。
- `Draw.reset()` 重置绘制状态。
- `effectBuffer.resize(...)` 为后处理准备 buffer。
- `Draw.draw(Layer.background, this::drawBackground)` 画背景。
- `Draw.draw(Layer.floor, blocks.floor::drawFloor)` 画地表。
- `Draw.draw(Layer.light, lights::draw)` 画光照。
- `Draw.draw(Layer.darkness, blocks::drawDarkness)` 画黑暗。
- `Draw.draw(Layer.bullet - 0.02f, bloom::capture)` / `bloom::render` 做泛光。
- `Draw.drawRange(Layer.shields, 1f, () -> effectBuffer.begin(Color.clear), () -> { effectBuffer.end(); effectBuffer.blit(Shaders.shield); })` 做护盾后处理。
- `Draw.drawRange(Layer.buildBeam, 1f, () -> effectBuffer.begin(Color.clear), () -> { effectBuffer.end(); effectBuffer.blit(Shaders.buildBeam); })` 做建造光束后处理。

### `Shaders.LoadShader`

```java
public static class LoadShader extends Shader{
    public LoadShader(String frag, String vert){
        super(getShaderFi(vert + ".vert"), getShaderFi(frag + ".frag"));
    }
}
```

说明原版 shader 加载就是“vert + frag”组合，不是某种神秘框架。

## 最小复刻架构

- `RendererLike`
  - 管相机、清屏、层级绘制、buffer 切换。
- `Layer`
  - 定义绘制顺序。
- `ShaderManager`
  - 管 shader 实例和 uniform。
- `BufferPass`
  - 负责 capture / blit。
- `FeatureToggle`
  - 控制 bloom、pixelate、shadows、shields、build beam。

## 常见坑

- 把 UI 和世界画面混进同一个 buffer，最后一定乱。
- shader 文件名和代码读取名不一致。
- 没有在正确阶段 `begin/end`，导致递归捕获。
- 忽略 `settings`，效果会和原版不一致。
- 只写 fragment，不复刻 layer 和 buffer，结果只是一个滤镜，不是原版渲染系统。

## Related

- [Rendering System](../mindustry/rendering-system.md)
- [Mindustry 自定义 Shader 接入](mindustry-custom-shader.md)
- [Helium 屏幕采样与着色器渲染](helium-screen-sampling-rendering.md)
