# iOS Java Mod 加载方案

> Source: 2026-05-08 对话，源码分析

## 前提

Mindustry iOS 版基于 RoboVM，`IOSLauncher.hasJava()` 返回 `false`，原生不支持运行时加载 Java/Kotlin mod jar。

## 方案：AOT 编译进 IPA

1. 将 mod 源码 implementation 进原版工程
2. 修改 `IOSLauncher.hasJava()` 返回 `true`
3. `Mods.loadContent()` 中通过 `Class.forName()` 替代 jar 扫描来加载 mod 类
4. 用 RoboVM AOT 将 mod 代码编译进 IPA

## 代价

- 每次 mod 更新需重新编译 IPA
- 无法运行时热插拔（不能像 desktop/Android 那样直接丢 jar）
- 仅适合"内嵌"式预编译发布

## 结论

iOS 可以跑 Java mod，但发布流程和 Java mod 的标准热插拔模式完全不同，近似于把 mod 写死进客户端。