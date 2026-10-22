# VoidShield shader linking error: `v_texCoords` missing from vertex output

> Source: Crash log shared by user on 2026-05-11; verified against [Mindustry 自定义 Shader 接入](../concepts/mindustry-custom-shader.md) and [Mindustry 原版着色器渲染从零复刻路线](../concepts/mindustry-vanilla-shader-rendering-from-zero.md).

## Symptom

Mod load fails during `voidshield.VoidShieldMod.init(VoidShieldMod.kt:24)`.

Immediate failure chain:

- `voidshield.other.VsVars.<clinit>(VsVars.kt:29)`
- `voidshield.other.V1Shaders.<init>(VsVars.kt:58)`
- `voidshield.shader.v1.DefaultShader.<init>(DefaultShader.kt:31/33)`
- `voidshield.shader.ShaderManage.getShader(ShaderManage.kt:88)`
- `arc.graphics.gl.Shader.<init>(Shader.java)`

Driver/compiler error:

```text
Failed to compile shader: Error: input v_texCoords not declared in output from previous stage.
Error: Linking failed.
```

## Verified conclusion

This is a vertex/fragment interface mismatch, not a generic "shader syntax" issue.

The fragment shader `shaders/test.frag` reads `varying vec2 v_texCoords;`, but the vertex shader paired with it (`shaders/default.vert`) does not output a matching varying to the fragment stage.

According to existing knowledge:

- screen/framebuffer post-processing should usually pair custom `.frag` with `screenspace.vert`
- `default.vert` is for different batch/world drawing usage
- if a fragment shader samples `u_texture` with `v_texCoords`, the chosen vertex shader must declare and write `v_texCoords`

## Fix directions

1. If this shader is a framebuffer/screen effect:
   - replace `default.vert` with `screenspace.vert`
   - keep `varying vec2 v_texCoords;` in the fragment shader

2. If this shader really must use a custom/default-style vertex shader:
   - edit the vertex shader so it declares and writes the same varying name/type:

```glsl
attribute vec4 a_position;
attribute vec2 a_texCoord0;

uniform mat4 u_projTrans;

varying vec2 v_texCoords;

void main(){
    v_texCoords = a_texCoord0;
    gl_Position = u_projTrans * a_position;
}
```

3. If the fragment shader does not need texture coordinates:
   - remove `v_texCoords` usage from the fragment shader
   - but then texture sampling logic must also be rewritten

## Fast triage checklist

- Check `voidshield/shader/v1/DefaultShader.kt` constructor arguments: which vert/frag files are actually paired.
- Open `assets/shaders/default.vert` and confirm whether it contains `varying vec2 v_texCoords;` and assigns it.
- Open `assets/shaders/test.frag` and confirm it reads `v_texCoords`.
- If `test.frag` is post-process style, switch to `screenspace.vert` first. That is the highest-probability fix.

## Related

- [Mindustry 自定义 Shader 接入](../concepts/mindustry-custom-shader.md)
- [Mindustry 原版着色器渲染从零复刻路线](../concepts/mindustry-vanilla-shader-rendering-from-zero.md)
