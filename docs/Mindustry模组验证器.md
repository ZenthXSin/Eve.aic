# Mindustry 模组验证器

从 eve-portable 迁入的独立 Java/Gradle 项目，位于 `vendor/mindustry-mod-validator/`。它在无头 Mindustry 环境中实际导入一个模组，报告内容解析错误、加载日志中的错误与警告，并做动态测试：逐个放置模组方块、生成模组单位，各跑 60 个更新帧，捕获运行时崩溃。

## 来源与许可

- 来源：https://gitee.com/zenthxsin/eve-portable 的 `project/mindustry-mod-validator/`，固定版本 `e7e43b4db4de38775a8d28f59dd4ec69e5e1686e`。
- 去掉了旧包附带的预编译 jar（来源无法核对）和与示例目录重复的 zip。除下面两处外与旧包逐字节相同：
  - `build.gradle`：JitPack 上 Mindustry v159.5 依赖的 Arc 构件已无法获取（CI 与本地均返回 404），改为只依赖官方发布的 Mindustry v160.7 服务端 jar（与实践运行器同一版本、同一摘要），它已包含 core、server、Arc、Rhino、LZ4 与 JLine。
  - `HeadlessTestEnvironment.java`：目标主版本常量由 159 改为 160，与所用服务端一致。
- 旧包整体为 AGPL-3.0（随副本提供 `LICENSE`），项目 README 声明 MIT；`src/main/resources/` 下的地图、基地蓝图和图标来自 Mindustry，属 GPL-3.0。这个目录不属于本仓库的 MIT 许可范围。

## 构建与使用

需要 JDK 17 及以上、Gradle 9.3.1 和 Mindustry v160.7 的 `server-release.jar`（CI 下载两者的官方发行包并校验 SHA-256）。

```sh
cd vendor/mindustry-mod-validator
gradle --no-daemon fatJar -PmindustryServerJar=<server-release.jar 路径>
java -jar build/libs/mindustry-mod-validator-1.0.0-all.jar <模组目录或 zip> --json --output 报告.json
```

退出码：0 通过，1 发现错误，2 环境初始化等致命错误。JSON 报告含 `status`、`summary`（错误、警告、信息条数）和 `issues`（`severity`、`category`、`message`、`detail`）。报告中的字符串没有转义制表符，解析时需要宽松模式。

## 验收

CI 工作流“Mindustry 真实运行验收”中的“继承的模组验证器”作业从源码构建，再运行 `scripts/mod_validator_acceptance.py`：

- `good-json-mod`、`test-json-mod` 必须通过且没有错误；
- `bad-json-mod` 必须失败，报告要指出出错的内容；
- 其余示例只要求给出报告，不能出现致命错误。

## 与 Eve 实践运行器的关系

Eve 的实践运行器（`eve-practice-mindustry`）用固定版本 v160.7 的官方服务端加载纯数据模组，并按探测项核对内容属性。验证器补上运行器没有的动态测试，但两者有几点不同：

- 两者使用同一个 v160.7 服务端 jar；运行器启动完整服务端并按探测项核对属性，验证器只初始化内容与世界，做放置、生成与更新帧测试。
- 验证器会加载模组中的脚本，它不是沙箱。只对已经通过实践运行器结构检查的纯数据草稿使用，或在隔离环境中运行。
- 验证器只把测试数据写在 `java.io.tmpdir/modvalidator-test-data`，每次启动先整体删除该目录；不保存任何状态，也没有需要恢复的数据。

目前它是一个独立工具，Eve 还不会自动调用。下一步是把它接入实践：缺少验证器或运行环境时，Eve 自己构建或下载并校验，而不是直接放弃这一步。
