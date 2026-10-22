# Mindustry 版本号系统

> 依据：Mindustry 源码 `v160.7`（提交 `17012d61d9`）的 `build.gradle` 与
> `core/src/mindustry/Vars.java`。此前的“最新稳定版 v158.1”“Java mod 最低版本 146”
> 等结论已过时，本文件按 160.7 更正。

## 版本号结构

Mindustry 的版本号由根目录 `build.gradle` 的 `allprojects.ext` 定义：

```groovy
versionNumber = '8'          // 主版本号
versionModifier = 'release'  // 修饰符（默认 'release'）
versionType = 'official'     // 类型（默认 'official'）
```

`buildversion` 由构建时通过 `-Pbuildversion=<n>` 传入；没有传时 `getBuildVersion()`
返回 `"custom build"`。`core/assets/version.properties` 是构建时才生成的产物，
仓库里没有这个文件。

## 版本字符串格式

- **完整版本**：`$versionNumber-$versionModifier-$buildVersion`（如 `8-release-160`）
- **显示版本**：`v$buildVersion`（如 `v160`）
- **对外名称**：`v$versionNumber Build $buildVersion`（如 `v8 Build 160`）
- **发布 tag**：`v$buildVersion.$revision`（如 `v160.7`，`revision` 用于热修）
- **运行时字段**：`Version.number`、`Version.build`、`Version.revision`、
  `Version.modifier`、`Version.type`、`Version.commitHash`、`Version.buildDate`
  （`core/src/mindustry/core/Version.java:19`）

## 当前版本

- **最新稳定版**：v8 Build 160.7（GitHub release `v160.7`，2026-10-08 发布）
- **versionNumber**：8
- **release 目标分支**：master
- **本仓库固定快照**：Mindustry 源码提交 `17012d61d9`，对应 `v160.7`；
  实践运行器使用的 `server-release.jar` 摘要为
  `f7b899a4038b95b14a824d5f8993984f79d73baa3254c385b6538327f9c0befd`
- **配套 Arc 版本**：`gradle.properties` 的 `archash=7445105cd2`

## 版本兼容下限（160.7 实际值）

- `minModGameVersion`：**136**（`core/src/mindustry/Vars.java:53`），普通内容 mod 的最低兼容版本
- `minJavaModGameVersion`：**154**（`core/src/mindustry/Vars.java:55`），Java mod 的
  最低兼容版本更高，因为 Java mod 更容易受破坏性改动影响

旧资料里把它写成 `minModGameVersionJava` 且值为 146，名称和数值都不对。
判定逻辑见 `core/src/mindustry/mod/Mods.java:1180` 与 `Mods.java:1330`。

## 版本历史

- v7：旧主版本（versionNumber=7）
- v8：当前主版本（versionNumber=8）

## 常见误区

- ❌ “最新版是 v158.1” —— 过时，当前是 v8 Build 160.7
- ❌ “Java mod 最低兼容 146” —— 过时，160.7 中是 154
- ❌ “v8 不存在” —— 错误，v8 是当前版本
- ❌ 把 `Version.build` 当成主版本号 —— 它是构建号，主版本看 `Version.number`

## 源码位置

- 版本号定义：`build.gradle`（根目录，`allprojects.ext`）
- 版本属性加载：`core/assets/version.properties`（构建时生成，不在仓库中）
- 版本读取与比较：`core/src/mindustry/core/Version.java`
- mod 兼容下限：`core/src/mindustry/Vars.java:53`、`Vars.java:55`

## 相关概念

- mod 元数据里的 `minGameVersion` 写的是这个兼容下限量级的构建号，例如 `136`
- 服务端兼容检查用 `Version.build`；mod 版本兼容用 `ModMeta.getMinMajor()`
