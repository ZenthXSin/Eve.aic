# mod-tools

- **仓库**: [I-hope1/mod-tools](https://github.com/I-hope1/mod-tools)
- **版本**: 1.5.8
- **作者**: I hope...
- **语言**: Java（237 源文件，37,919 行）
- **类型**: Java mod（`modtools.ModTools`）— **开发工具 mod**
- **minGameVersion**: 154
- **本地路径**: `源码和示例/mods/mod-tools`
- **WIKI**: https://i-hope1.github.io/mod-tools

## 定位

Mindustry v7 的开发工具 mod，为 mod 开发者提供调试、反射、性能分析、热替换、UI 审查、语法高亮编辑等专业工具。**不同于游戏内容 mod**，这是一个纯开发辅助工具。

## 核心系统

### HotSwap 热替换
- `unsupported/HotSwapController.java` + `HotSwapManager.java`
- 基于 JDWP 和自定义 Agent（hotswap-agent, jni-agent, utils-agent）
- 运行时代码替换，无需重启游戏

### 反射系统
- `jsfunc/reflect/REFLECT.java` / `UNSAFE.java`: 完整反射 + Unsafe API 封装暴露给 JS
- `utils/reflect/HopeReflect.java`, `FieldUtils.java`, `ClassUtils.java`: 反射工具
- `utils/ByteCodeTools.java`: ASM 9.5 字节码操作

### JS 函数扩展
- `JSFuncClass.java` 入口，`IScript.java` 脚本执行
- `REFLECT.java` / `UNSAFE.java` / `CAST.java` / `PTYPE.java`: JS 中的反射、类型转换

### 性能分析（火焰图）
- `content/world/Profiler.java` + `utils/profiler/SamplingProfiler.java`
- `ui/windows/profile/FlameGraphWindow.java`: 火焰图可视化

### UI 审查
- `content/ui/ReviewElement.java`: 运行时 UI 元素检查
- `ui/comp/review/ElementDetailsWindow.java` / `CellDetailsWindow.java`

### 语法高亮与代码补全
- `ui/comp/input/highlight/JSSyntax.java` / `JavaSyntax.java`
- `ui/comp/completion/CompletionPopup.java`

### 屏幕采样与特效
- `ui/effect/ScreenSampler.java` / `BufferCapturer.java` / `EBBlur.java`
- 8 个自定义着色器（毛玻璃、高斯模糊等）

### 子项目
- `annotations/`: 注解处理器
- `compiler/`: 编译器（ASM 9.5）
- `hotswap-agent/`: 热替换 Agent
- `jni-agent/`: JNI Agent
- `utils-agent/`: 工具 Agent
- `_libs/`: 依赖库

## 值得学习的模式
- HotSwap 的 JDWP + Agent 架构
- 反射 + Unsafe API 封装暴露给 JS 的设计
- 采样性能分析器 + 火焰图
- UI 元素运行时审查器
- ByteCodeTools / ASM 字节码操作
- 屏幕采样 + 后处理着色器管线
- Android Hidden API 访问
- 子项目分离构建架构

## 参考
- [mod-tools GitHub](https://github.com/I-hope1/mod-tools)
- [WIKI](https://i-hope1.github.io/mod-tools)
- [mod-tools README.txt](../../源码和示例/mods/mod-tools/README.txt)
