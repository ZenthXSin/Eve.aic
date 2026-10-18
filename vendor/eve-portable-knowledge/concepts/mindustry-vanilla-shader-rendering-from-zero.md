# Mindustry 原版着色器渲染从零复刻路线

> Source: Conversation synthesis on 2026-05-11; verified against local Mindustry source `core/src/mindustry/core/Renderer.java`, `core/src/mindustry/graphics/Shaders.java`, `core/src/mindustry/graphics/Layer.java`, and local VoidShield shader sample `src/voidshield/shader/ShaderManage.kt` / `src/voidshield/world/shaders/TestShader.kt`.

## Core Conclusion

To reproduce vanilla Mindustry shader rendering, implement a **rendering pipeline**, not just a fragment shader. Vanilla combines `Renderer.draw()` layer scheduling, `Layer` z constants, `Draw.drawRange(...)`, `FrameBuffer` capture, `FrameBuffer.blit(shader)`, and shader classes in `Shaders.java` that update uniforms every frame.

## Verified Source Evidence

- `mindustry.core.Renderer` owns `effectBuffer = new FrameBuffer()` and resizes it when animated water or shields are active.
- `mindustry.core.Renderer.draw()` calls `graphics.clear(clearColor)`, `Draw.reset()`, `Draw.proj(camera)`, `Draw.sort(true)`, schedules layers, then `Draw.flush()` and `Draw.sort(false)`.
- `mindustry.graphics.Layer` defines rendering order: floor `0`, block `30`, blockBuilding `40`, bullet `100`, effect `110`, buildBeam `122`, shields `125`, light `140`, fogOfWar `155`, space `160`.
- `mindustry.core.Renderer.draw()` wraps animated shields and build beams with `Draw.drawRange(..., () -> effectBuffer.begin(Color.clear), () -> { effectBuffer.end(); effectBuffer.blit(Shaders.shield/buildBeam); })`.
- `mindustry.graphics.Shaders.ShieldShader` and `BuildBeamShader` extend `LoadShader`, use `screenspace.vert`, and set `u_dp`, `u_time`, `u_offset`, `u_texsize`, `u_invsize`.
- `mindustry.graphics.Shaders.SurfaceShader` loads `screenspace.vert` plus `<effect>.frag`, sets camera/time/resolution uniforms, and binds noise textures when `u_noise` exists.
- VoidShield local sample currently demonstrates raw shader loading and mesh rendering, but not the full vanilla-style `FrameBuffer` + `Layer` post-processing pipeline.

## Minimal Architecture

1. `ShaderManager`: centralizes shader file lookup, construction, and uniform updates.
2. `RenderPass`: owns one `FrameBuffer`, one `Shader`, and a target `Layer` or trigger pair.
3. `Layer registration`: schedules begin/end through `Draw.drawRange` for world-layer effects.
4. `Settings toggles`: reads `Core.settings` or custom mod settings to enable/disable passes.
5. `Asset fallback`: local mod shaders first, vanilla `Shaders.getShaderFi(...)` as fallback for shared vertex shaders.

## Implementation Notes

- World-space post-processing uses camera dimensions: `Core.camera.width`, `Core.camera.height`, and `Core.camera.position - half size`.
- Screen-space effects use pixel dimensions: `Core.graphics.getWidth()`, `Core.graphics.getHeight()`.
- Do not capture UI with world shader passes unless the intended effect is a whole-screen filter.
- Do not call `FrameBuffer.blit(shader)` without first capturing meaningful content into that buffer.
- Use `screenspace.vert` for framebuffer blit shaders and `default.vert` for texture-region/world batch shaders.

## Related

- [Rendering System](../mindustry/rendering-system.md)
- [Mindustry 原版着色器渲染实现路线](mindustry-vanilla-shader-rendering.md)
- [Mindustry 自定义 Shader 接入](mindustry-custom-shader.md)
- [Helium 屏幕采样与着色器渲染](helium-screen-sampling-rendering.md)
