# Core Package Overview

> Source: `源码和示例/Mindustry/core/src/mindustry/core/` (13个类，6475行)

## 核心模块清单

| 类名 | 行数 | 职责 | 平台 |
|------|------|------|------|
| NetServer | 1300 | 服务端逻辑、连接管理、同步 | 全平台 |
| UI | 832 | 界面管理、Fragment、Dialog | 非无头 |
| NetClient | 766 | 客户端网络、加载、重连 | 非无头 |
| Control | 763 | 输入、存档、音效、键位 | 非无头 |
| World | 664 | 地图加载、Tile管理、路径 | 全平台 |
| Renderer | 629 | 渲染管线、图层、特效 | 非无头 |
| Logic | 586 | 逻辑更新、全局规则、队列 | 全平台 |
| ContentLoader | 382 | 内容加载、注册、初始化 | 全平台 |
| Platform | 188 | 平台能力抽象 | 全平台 |
| GameState | 122 | 游戏状态、模式、统计 | 全平台 |
| FileTree | 102 | 资源文件树 | 全平台 |
| Version | 87 | 版本信息 | 全平台 |
| PerfCounter | 54 | 性能计数器 | 全平台 |

## 模块初始化顺序

```
ClientLauncher.init()
  → Vars.init()           // 静态字段初始化
  → ContentLoader.init()  // 内容注册
  → World.init()
  → Control.init()
  → Renderer.init()
  → UI.init()
  → Logic.init()
  → NetClient.init()
```

## 关键依赖关系

- **Control** → Saves, SoundControl, InputHandler
- **World** → Tiles, Pathfinder, BlockIndexer
- **Logic** → World, GameState
- **Renderer** → World, Camera, Shaders
- **UI** → Scene (Arc), Fragments
- **NetServer/NetClient** → Net (Arc), Packets

## 头部服务端排除

以下类在无头服务端不创建：
- Control（无输入/音效）
- Renderer（无渲染）
- UI（无界面）
- NetClient（服务端不需要）

## 关联

- [Project Structure](project-structure.md)
- [Arc Framework Overview](arc-framework.md)
- [Vars System](vars-system.md)
