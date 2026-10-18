# Mindustry 版本号系统

## 版本号结构

Mindustry 的版本号由 `build.gradle` 定义：

```groovy
versionNumber = '8'          // 主版本号
versionModifier = 'release'  // 修饰符（默认 'release'）
versionType = 'official'     // 类型（默认 'official'）
```

## 版本字符串格式

- **完整版本**：`$versionNumber-$versionModifier-$buildVersion`（如 "8-release-158"）
- **显示版本**：`v$buildVersion`（如 "v158"）
- **对外名称**：`v$versionNumber Build $buildVersion`（如 "v8 Build 158"）

## 当前版本（截至 2026-06）

- **最新稳定版**：v8 Build 158.1
- **versionNumber**：8
- **GitHub Release tag**：v158.1

## 版本历史

- v7：旧版本（versionNumber=7）
- v8：当前版本（versionNumber=8），从 Build 146 开始

## 常见误区

- ❌ "v7 Build 146 是最新版" - 过时信息
- ❌ "v8 不存在" - 错误，v8 是当前版本
- ✅ v8 Build 158.x 是最新稳定版

## 源码位置

- 版本号定义：`build.gradle`（根目录）
- 版本属性加载：`core/assets/version.properties`（构建时生成）
- 版本检查：`mindustry.io.SaveIO`（存档兼容性检查）

## 相关概念

- `minModGameVersion`：mod 最低兼容游戏版本（Vars.java:53，当前值 136）
- `minModGameVersionJava`：Java mod 最低兼容版本（Vars.java:55，当前值 146）
