# Mindustry Java Mod Structure

## 目标
本文整理一个标准的 Mindustry Java Mod 目录结构与最小入口写法，重点是：
- Java Mod 的常见文件布局
- `mod.json` 的作用
- 主类入口与生命周期
- `src` / `assets` / `libs` 的分工

## 1. 标准目录结构
一个常见的 Java Mod 大致长这样：

```text
MyMod/
├── mod.json
├── README.md
├── assets/
│   ├── sprites/
│   ├── sounds/
│   └── ...
├── src/
│   └── mymod/
│       ├── MyMod.java
│       ├── content/
│       │   ├── ModBlocks.java
│       │   ├── ModItems.java
│       │   └── ModUnits.java
│       ├── graphics/
│       ├── world/
│       ├── ui/
│       └── util/
├── libs/
└── build.gradle
```

## 2. 各目录的职责

### `mod.json`
Mod 元数据入口，通常包含：
- `name`
- `displayName`
- `author`
- `version`
- `main`
- `minGameVersion`
- `description`

其中 `main` 指向你的主类。

### `src/`
Java 源码目录，放：
- 主 Mod 类
- 内容注册类
- UI / 渲染 / 工具类
- 自定义块、单位、能力逻辑

### `assets/`
资源目录，放：
- sprite
- atlas 贴图
- 音效
- shader
- 语言文件
- 其他静态资源

### `libs/`
第三方依赖 jar（如果有）。

### `build.gradle`
Gradle 构建文件，负责：
- 依赖 Mindustry / Arc
- 编译打包
- 复制资源
- 输出 mod jar

## 3. 最小 `mod.json`

```json
{
  "name": "mymod",
  "displayName": "My Mod",
  "author": "you",
  "version": "1.0",
  "main": "mymod.MyMod",
  "minGameVersion": 146,
  "description": "A Mindustry Java mod."
}
```

## 4. 最小主类

```java
package mymod;

import mindustry.mod.Mod;

public class MyMod extends Mod {
    @Override
    public void loadContent() {
        // register blocks/items/units here
    }

    @Override
    public void init() {
        // ui / events / commands / runtime setup
    }
}
```

## 5. 常见分层方式

### 方案 A：按内容类型拆
- `content/ModItems.java`
- `content/ModBlocks.java`
- `content/ModUnits.java`

适合中小型 Mod，结构清晰。

### 方案 B：按功能模块拆
- `world/`
- `graphics/`
- `ui/`
- `util/`
- `content/`

适合逻辑较多的项目。

## 6. 推荐的内容注册模式
常见写法是把注册集中到一个类里：

```java
public class ModContent {
    public static void load() {
        ModItems.load();
        ModBlocks.load();
        ModUnits.load();
    }
}
```

然后主类里调用：

```java
@Override
public void loadContent() {
    ModContent.load();
}
```

## 7. 常见的扩展目录
如果 Mod 规模变大，通常还会加：
- `ui/dialogs/`
- `world/blocks/`
- `world/entities/`
- `graphics/shaders/`
- `input/`
- `net/`

## 8. 一个实用的判断标准
如果你在写 Java Mod，通常可以按这个思路分层：

- **入口**：`MyMod.java`
- **内容**：`content/`
- **业务逻辑**：`world/`、`logic/`
- **表现层**：`graphics/`、`ui/`
- **工具层**：`util/`

## 9. 结论
标准 Mindustry Java Mod 的核心不是“必须长得完全一样”，而是：
- 有清晰的 `mod.json`
- 有一个继承 `mindustry.mod.Mod` 的主类
- 有独立的 `src/` 与 `assets/`
- 内容注册与运行逻辑分离

## Related
- [Mindustry Mod Java/Kotlin Loading](mod-java-kotlin-loading.md)
- [Mindustry Mod Lifecycle](mod-lifecycle.md)
- [Mindustry Mod System Complete Outline](mod-system-complete-outline.md)