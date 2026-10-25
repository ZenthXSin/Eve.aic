# Mindustry Mod Validator

Mindustry 模组动态验证工具。在 headless 服务端环境中加载你的 mod，自动检测内容错误、运行时崩溃和兼容性问题。

## 功能

- **动态加载验证** — 在真实 headless 环境中导入并加载 mod，捕获内容解析错误
- **内容错误检测** — 自动识别 JSON/HJSON 字段类型错误、无效引用、尺寸超限等问题
- **运行时测试** — 放置方块、生成单位、执行 update tick，检测运行时崩溃
- **错误日志捕获** — 收集所有 err/warn 级别日志，不遗漏任何警告
- **多格式报告** — 支持纯文本和 JSON 格式输出，便于人工阅读和 CI 集成

## 环境要求

- JDK 17+
- Gradle 9.x

## 构建

```bash
./gradlew fatJar
```

构建产物位于 `build/libs/mindustry-mod-validator-1.0.0-all.jar`，包含所有依赖和 Mindustry 核心资源。

## 使用

```bash
# 验证一个 mod 目录
java -jar build/libs/mindustry-mod-validator-1.0.0-all.jar ./my-mod

# 验证一个 mod zip
java -jar build/libs/mindustry-mod-validator-1.0.0-all.jar ./my-mod.zip

# JSON 格式输出
java -jar build/libs/mindustry-mod-validator-1.0.0-all.jar ./my-mod --json

# 输出到文件
java -jar build/libs/mindustry-mod-validator-1.0.0-all.jar ./my-mod --json --output report.json
```

## 退出码

| 码 | 含义 |
|----|------|
| 0 | 验证通过 |
| 1 | 验证失败（发现错误） |
| 2 | 致命错误（环境初始化失败等） |

## 验证流程

1. 初始化 headless Mindustry 环境（对齐 `ServerLauncher.init()`）
2. 导入目标 mod（支持目录和 zip）
3. 创建 mod 内容（`content.createModContent()`）
4. 初始化内容（`content.init()`）
5. 报告内容解析错误
6. 运行动态测试：放置方块、生成单位、执行 update tick
7. 生成报告

## 项目结构

```
src/main/java/modvalidator/
├── ModValidatorCli.java          # CLI 入口
├── ModValidator.java             # 验证编排
├── ValidationResult.java         # 结果模型
├── core/
│   ├── HeadlessTestEnvironment.java  # Headless 环境管理
│   └── ContentTester.java            # 动态内容测试
└── report/
    └── ReportGenerator.java        # 报告生成
```

## 版本对应

当前基于 Mindustry **v159.5** 构建。不同版本的 Mindustry API 可能有差异，建议使用与目标服务器匹配的版本。

## License

MIT
