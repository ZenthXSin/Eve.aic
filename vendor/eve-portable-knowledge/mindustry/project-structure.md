# Mindustry 项目结构

> Source: `源码和示例/Mindustry/` 目录结构分析

## Mindustry 模块结构

```
Mindustry/
├── core/              # 核心游戏逻辑（跨平台共享）
│   ├── src/mindustry/
│   │   ├── ai/        # AI控制器（16个子类）
│   │   ├── async/     # 异步任务（PathfindQueue等）
│   │   ├── audio/     # 音频控制（SoundControl, MusicControl）
│   │   ├── content/   # 内容定义（Items, Liquids, Blocks, UnitTypes...）
│   │   ├── core/      # 核心模块（Vars, World, Logic, Control, UI, Renderer...）
│   │   ├── ctype/     # 内容类型基类（Content, UnlockableContent）
│   │   ├── editor/    # 地图编辑器
│   │   ├── entities/  # 实体系统（Bullet, Unit, Effect, abilities）
│   │   ├── game/      # 游戏规则、事件、目标、队伍
│   │   ├── graphics/  # 渲染、着色器、绘制
│   │   ├── input/     # 输入处理（InputHandler, 移动端/桌面端）
│   │   ├── io/        # 存档IO（SaveIO, SaveVersion, TypeIO）
│   │   ├── logic/     # 逻辑处理器（LExecutor, LStatements, LogicBlock）
│   │   ├── maps/      # 地图加载、MapIO
│   │   ├── mod/       # Mod加载器（Mods, Mod, ContentParser）
│   │   ├── net/       # 网络（Net, NetServer, NetClient, Packets）
│   │   ├── service/   # 平台服务（成就、统计）
│   │   ├── type/      # 内容类型定义（Block, UnitType, Item, Liquid...）
│   │   ├── ui/        # UI组件（Fragment, Dialog, 表单）
│   │   └── world/     # 世界系统（Tile, Building, Block, 生成器）
│   └── assets/        # 游戏资源（sprites, sounds, bundles）
├── annotations/       # 编译时注解处理器（@Remote, @EntityDef, @Component）
├── server/            # 无头服务端（ServerLauncher）
├── desktop/           # 桌面端（ClientLauncher, DesktopLauncher）
├── android/           # Android端
├── ios/               # iOS端
├── tests/             # 测试
└── tools/             # 工具（资源打包、纹理处理）
```

## Arc 模块结构

```
Arc/
├── arc-core/          # 核心框架（跨平台）
│   └── src/arc/
│       ├── assets/    # AssetManager资源管理
│       ├── audio/     # Audio, Sound, Music
│       ├── files/     # Fi文件抽象
│       ├── func/      # 函数接口（Prov, Cons, Floatp...）
│       ├── graphics/  # Camera, Batch, Texture, Shader, Color
│       ├── input/     # Input接口
│       ├── math/      # Mathf, Vec2, Rect, Geometry
│       ├── scene/     # Scene2d UI框架
│       ├── struct/    # Seq, IntSeq, ObjectMap, OrderedSet
│       └── util/      # Time, Timer, Log, Strings, IO
├── backends/          # 平台后端
│   ├── backend-sdl/   # SDL桌面后端（主要）
│   ├── backend-sdl3/  # SDL3桌面后端
│   ├── backend-android/
│   ├── backend-headless/  # 无头后端（服务器）
│   └── backend-robovm/    # iOS后端
├── extensions/        # 扩展模块
│   ├── arcnet/        # 网络库（基于KryoNet）
│   ├── freetype/      # FreeType字体渲染
│   ├── g3d/           # 3D渲染
│   ├── packer/        # 纹理打包器
│   └── ...
└── natives/           # 原生库（桌面/移动端）
```

## 源码位置

| 项目 | 本地路径 |
|------|----------|
| Mindustry源码 | `源码和示例/Mindustry/` |
| Arc源码 | `源码和示例/Arc/` |
| Mindustry版本 | v7 (build 146) |
| Arc版本 | 对应Mindustry v7 |

## 关键构建入口

- `Mindustry/build.gradle` — 主构建脚本
- `Arc/build.gradle` — Arc框架构建
- `annotations/` — 编译时处理（@Remote→Call类，@EntityDef→Entity类）

## 关联

- [Core Package Overview](core-packages.md)
- [Arc Framework Overview](arc-framework.md)
- [Key Classes Analysis](key-classes.md)
