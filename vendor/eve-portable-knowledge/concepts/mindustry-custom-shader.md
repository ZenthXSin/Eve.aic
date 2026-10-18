# Mindustry 自定义 Shader 接入

## 概要

在 Mindustry Java/Kotlin Mod 中使用自定义 shader，常见做法是把 `.frag` / `.vert` 放入 mod 的 `assets/shaders/` 目录，然后在代码中用 `arc.graphics.gl.Shader` 读取并编译。若只写片元着色器，通常可以复用 Mindustry 自带的 `screenspace.vert`。

## 文件位置

典型目录：

```text
assets/shaders/my-effect.frag
assets/shaders/my-effect.vert  # 可选
```

如果是屏幕空间/后处理效果，常复用：

```java
Shaders.getShaderFi("screenspace.vert")
```

源码中 `Shaders.getShaderFi(file)` 实际返回：

```java
return tree.get("shaders/" + file);
```

因此 mod 内读取时应从当前 mod 根目录下的 `shaders/` 或 `assets/shaders/` 对应路径定位文件。

## 加载方式

Mindustry 源码中的 `Shaders.LoadShader`：

```java
public static class LoadShader extends Shader{
    public LoadShader(String frag, String vert){
        super(getShaderFi(vert + ".vert"), getShaderFi(frag + ".frag"));
    }
}
```

自定义 Mod 可提供自己的 shader 文件定位函数，先找当前 mod 文件，找不到再回退 Mindustry 内置 shader：

```kotlin
fun shaderFi(file: String): Fi {
    val local = Vars.mods.getMod(MyMod::class.java).root.child("shaders").child(file)
    return if (local.exists()) local else Shaders.getShaderFi(file)
}
```

## 最小 Kotlin Shader 类

```kotlin
class MyScreenShader : Shader(
    shaderFi("screenspace.vert"),
    shaderFi("my-effect.frag")
) {
    override fun apply() {
        setUniformf("u_time", Time.time)
        setUniformf("u_resolution", Core.graphics.width.toFloat(), Core.graphics.height.toFloat())
        setUniformf("u_campos", Core.camera.position.x, Core.camera.position.y)
    }
}
```

## 最小 fragment shader

```glsl
#ifdef GL_ES
precision mediump float;
#endif

uniform sampler2D u_texture;
uniform vec2 u_resolution;
uniform float u_time;

varying vec2 v_texCoords;

void main(){
    vec2 uv = v_texCoords;
    vec4 color = texture2D(u_texture, uv);
    color.rgb += 0.08 * sin(u_time / 10.0 + uv.xyx * 8.0);
    gl_FragColor = color;
}
```

## 屏幕后处理流程

如果要让 shader 作用在整个画面上，需要用 `FrameBuffer` 捕获画面，再用 shader 绘制回屏幕。核心流程是：

1. 在某个绘制事件开始时 `buffer.begin()`。
2. 让游戏正常绘制内容。
3. `buffer.end()`。
4. 使用 `buffer.blit(shader)` 把捕获的纹理通过 shader 输出到屏幕。

简化写法：

```kotlin
val buffer = FrameBuffer()
lateinit var shader: MyScreenShader

Events.run(EventType.Trigger.draw) {
    // 注意：真实项目中通常要拆成 drawBegin/drawEnd，避免把 UI 或错误阶段也捕获进去。
    buffer.resize(Core.graphics.width, Core.graphics.height)
    buffer.blit(shader)
}
```

更稳的做法是参考带 begin/end 的屏幕后处理器，在指定 Layer 或 Trigger 中截取画面。

## uniform 传递

常见 uniform：

```kotlin
setUniformf("u_time", Time.time)
setUniformf("u_resolution", Core.graphics.width.toFloat(), Core.graphics.height.toFloat())
setUniformf("u_campos", Core.camera.position.x, Core.camera.position.y)
setUniformi("u_noise", 1)
```

纹理 uniform 需要先绑定纹理槽，再设置采样器编号。例如噪声贴图：

```kotlin
noiseTexture.bind(1)
Core.graphics.texture.bind(0)
setUniformi("u_noise", 1)
```

## 注意事项

- Shader 文件名必须和代码读取名一致。
- `.frag` 中的 uniform 名必须和 `apply()` 设置的名字一致。
- 如果只是屏幕空间效果，优先复用 `screenspace.vert`。
- 手机/OpenGL ES 对 GLSL 语法更严格，尽量使用 `precision mediump float;` 和旧式 `texture2D`。
- 后处理不要在所有阶段无脑执行，否则可能影响 UI、菜单或造成递归捕获。
- 报错时优先检查 shader 文件路径、uniform 拼写、GLSL 版本语法和纹理绑定顺序。

## 相关页面

- [Mindustry 电弧特效](mindustry-electric-arc-effect.md)
- [Mindustry Java 抛壳特效](mindustry-java-shell-ejection-effect.md)
