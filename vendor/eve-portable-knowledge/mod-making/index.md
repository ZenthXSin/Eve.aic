# Mod 制作经验

> 这个目录记录开发 Mindustry Mod 过程中学到的**经验、教训、最佳实践和反模式**。
>
> 按主题分页，每个经验一个章节。持续积累，每次遇到新坑/新发现就追加。

---

## 项目结构与构建

### Kotlin Mod 的最小骨架

- `mod.hjson` 必须指定 `main` 指向入口类和 `name`（唯一内部名）
- `build.gradle` 的 `mindustryVersion` 要和服务器 jar 版本匹配（当前 `v157.4`）
- 入口类继承 `Mod`，重写 `loadContent()` 注册内容，`init()` 做初始化（注册命令、启动服务等）
- 源码放 `src/`，资源放 `assets/`
- 编译产物 jar 丢到 `server/config/mods/`，重启服务端加载

### `gradle jar` 打包要点

- `archiveFileName` 决定输出 jar 名
- `duplicatesStrategy = DuplicatesStrategy.EXCLUDE` 避免资源冲突
- 如果有 Android 端需求，需要 `d8` 工具链；纯服务端 mod 可以只打 Desktop jar
- jar 里要包含 `mod.hjson` 在根目录（不是 META-INF 里）

### 服务端部署

- 服务端 jar：`~/文档/Mindustry/server/server-2025.07.X12.jar`（v157.4）
- mod 路径：`~/文档/Mindustry/server/config/mods/`
- 支持 `.jar` 和 `.zip` 两种格式
- 重启服务端后 mod 自动加载，日志中有 `[Mods] Loaded mod "xxx"` 表示成功

---

## 内容注册

### Block 注册

```kotlin
// 推荐写法：用 object 注册表统一管理
object ModBlocks {
    val myBlock: MyBlock by lazy {
        MyBlock("my-block").apply {
            requirements(Category.turret, ItemStack.with(Items.copper, 50))
            // ...
        }
    }
}

override fun loadContent() {
    ModBlocks.myBlock.load()
    // 逐个 load()
}
```

### 科技树挂接

- `Block` 的 `research` 字段可以直接指向父节点：`research = parentBlock`
- 自定义科技树需要关注 JSON 的挂接方式，详见 `knowledge/concepts/mindustry-block-tech-tree-json.md`

---

## JSON 注意事项

### 常见坑

- `type` 字段必须真实存在（是 Mindustry 源码中的类名），不存在则静默回退到基础 `Block`
- JSON 结尾允许多余逗号，这是解析器容错
- JS mod 的 `extend` 创建内容后，JSON 里再加 `type` 会覆盖 JS 实例，导致自定义方法丢失
- 中文翻译不显示通常是因为 bundle 键名前缀和 `mod.hjson` 的 `name` 不一致

---

## 调试与验证

### 无头服务端验证

```bash
cd ~/文档/Mindustry/server
java -jar server-2025.07.X12.jar
```

看启动日志中的 mod 加载信息确认内容注册成功。如果有错，日志会打出 stack trace 和分析原因。

### 常用验证路径

1. 服务端日志 → 看 mod 是否加载
2. `/eve list blocks [name]` → 看方块是否注册
3. `/eve info [name]` → 看字段值
4. 进游戏实际测试

---

## 反模式

### ❌ 不改 Block 实例级字段

Mindustry 中 `Block` 是**类级单例**，同一 `Block` 实例被所有 `Building` 共享。
如果 JS 里直接 `block.someField = value` 或 Java/Kotlin 里修改 `Block` 的字段，会影响该类型**所有**建筑实例。

正确做法：
- 配置字段在 `loadContent()` 时设好就不要改了
- 运行时状态放在 `Building` 子类的实例字段里

### ❌ 猜 API

不要根据"感觉"写代码。不确定某方法是否存在时，优先查源码，再看示例 mod，最后才试。

**已知踩过的坑**：
- `LVar.numfWorld()` 在某些版本不存在（手机 beta 155.4），改为 `World.unconv(LVar.numf())`
- `Shader` 的 varying 名称必须 vertex 和 fragment 完全一致，拼写错了链接就炸

### ❌ 服务端不加 UI 代码

服务端没有 UI 系统（没有 `Scene`、`Element`、`Dialog`），任何 UI 操作在服务端都会崩。
条件编译用 `!Vars.headless` 包裹。

---

## 运行时诊断与逆向

- [Item 网络传输与接收端不输出](item-network-transport.md) — acceptItem/pushItemsToNetwork/outputToNearbyBlocks 链路、"收但不吐"根因排查、三种修复方向
- [编译型 Java mod 特效做成 JSON 可配置](effect-json-config-injection.md) — 无源码 jar 用 javap 反编译定位 lambda、Effect.renderer 反射注入覆盖思路、可配置边界（通用参数 vs 绘制级）
- 建造分区 `Category` 是 enum，mod 无法新增分区，只能归入现有分区（需改游戏源码重编译才能加新分区）