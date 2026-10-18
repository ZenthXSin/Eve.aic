# Arc Framework Overview

> Source: `源码和示例/Arc/arc-core/src/arc/` (12个包)

## Arc 是什么

Arc 是 Mindustry 的自研跨平台框架，替代 LibGDX。提供：
- 跨平台应用抽象（桌面/移动/无头）
- 2D渲染管线（SpriteBatch, Camera, Shader）
- UI框架（Scene2d）
- 输入、音频、文件、网络基础能力
- 数据结构（Seq, IntSeq, ObjectMap）

## 核心类

| 类 | 路径 | 职责 |
|----|------|------|
| Core | `arc.Core` | 全局静态引用（app, graphics, audio, input, camera, batch, scene） |
| ApplicationCore | `arc.ApplicationCore` | 模块化应用基类（add模块, 按序update） |
| Application | `arc.Application` | 应用接口（平台相关实现） |
| ApplicationListener | `arc.ApplicationListener` | 生命周期接口（init, update, dispose） |

## 包职责

| 包 | 内容 |
|----|------|
| arc.assets | AssetManager资源加载 |
| arc.audio | Sound, Music, Audio |
| arc.files | Fi文件抽象（跨平台IO） |
| arc.func | 函数接口（Prov<T>, Cons<T>, Floatp, Boolp） |
| arc.graphics | Camera, Batch, Texture, Shader, Color, FrameBuffer |
| arc.input | Input接口, KeyCode |
| arc.math | Mathf, Vec2, Rect, Geometry, Interp |
| arc.scene | Scene2d UI框架（Element, Group, Widget） |
| arc.struct | Seq, IntSeq, ObjectMap, OrderedSet, Queue |
| arc.util | Time, Timer, Log, Strings, IO, Reflect |

## Mindustry 如何使用 Arc

```java
// Mindustry 继承 ApplicationCore
public class ClientLauncher extends ApplicationCore {
    public void setup() {
        add(module = new Logic());      // 逻辑
        add(control = new Control());   // 控制
        add(renderer = new Renderer()); // 渲染
        add(ui = new UI());            // 界面
    }
}

// 全局访问 Arc 核心
Core.app        // 应用
Core.graphics   // 图形
Core.audio      // 音频
Core.input      // 输入
Core.camera     // 相机
Core.batch      // 批处理
Core.scene      // UI场景
```

## 关键设计模式

1. **模块化**：ApplicationCore.add() 按数组存储，顺序update
2. **静态全局**：Core.xxx 和 Vars.xxx 是全局访问点
3. **跨平台**：接口+后端实现（SDL/Android/Headless/iOS）
4. **编译时处理**：注解处理器生成代码（@Remote→Call）

## 关联

- [Project Structure](project-structure.md)
- [Core Package Overview](core-packages.md)
- [Vars System](vars-system.md)
