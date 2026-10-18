# Mindustry SectorPreset JSON 接口

> Source: 对话整理 + Mindustry 155.4 源码 `ContentParser.java`、`SectorPreset.java`、`UnlockableContent.java`

这页整理 Mindustry 中 `SectorPreset`（战役区块/预设扇区）在 JSON 里的常见可用字段，以及哪些字段虽然能写，但有额外语义要求。

## Key Points

- `SectorPreset` 的 JSON 内容类型是 `ContentType.sector`，解析入口在 `ContentParser` 的 `ContentType.sector` 分支。
- `sector` 是必填核心字段；源码明确要求它存在且是数字。
- `planet` 是合法字段，值需要能解析到已有 `Planet` 内容；不存在会直接报 `Planet 'xxx' not found.`
- `captureWave`、`difficulty`、`alwaysUnlocked`、`localizedName`、`description`、`details` 都来自 `SectorPreset` 本类或其父类 `UnlockableContent` / `Content` 的字段反射读取，接口本身成立。
- `research` 不是 `SectorPreset` 类字段，但属于所有 `UnlockableContent` 通用的科技树挂接入口，`SectorPreset` 也能使用。
- `research.parent` 必须指向科技树里已存在的内容节点，否则不会正确挂上；源码会警告该内容不在 tech tree 中。
- `details` 中的 `[purple]` 这类标记从字段接口角度不是问题，它只是普通字符串；是否按富文本显示取决于 UI 渲染位置。

## 字段判断

### 明确支持

- `localizedName`
- `description`
- `details`
- `alwaysUnlocked`
- `planet`
- `sector`
- `captureWave`
- `difficulty`
- `research`
  - `parent`
  - `requirements`
  - `objectives`
  - `planet`
  - `root`
  - `name`
  - `requiresUnlock`

### 需要注意

- `planet`: 必须是内容名，不是显示名。
- `research.parent`: 必须是 tech tree 中可找到的内容名，不是任意文本。
- `name`: 作为内容内部名，最好使用英文/稳定 ID；`localizedName` 才适合显示中文名。
- `alwaysUnlocked: true` 和 `research` 同时写并不冲突，但语义上可能显得重复：一个是默认已解锁，一个是挂科技树节点。

## 对应截图那类写法的接口结论

像下面这种区块定义，从“接口是否存在”角度看：

- `localizedName`：对
- `name`：对
- `description`：对
- `details`：对
- `planet`：对，但值必须是已注册星球内部名
- `sector`：对，必须是数字
- `captureWave`：对
- `difficulty`：对
- `alwaysUnlocked`：对
- `research.parent`：接口对，但父节点内容名必须真实存在于科技树

真正更可能出问题的不是这些字段名本身，而是：

1. `planet` 写成了显示名/中文名，而不是内部内容名
2. `research.parent` 写成了显示名/翻译名，而不是科技树节点对应的内容名
3. `name` 使用中文导致资源、存档回退名、贴图命名等兼容性变差；不是必然报错，但不推荐

## 源码依据

### SectorPreset 解析分支

`ContentParser` 中 `ContentType.sector` 分支会：

- 要求 `sector` 为数字
- 读取 `planet`
- 调用 `preset.initialize(planet, sector)`
- 再用 `readFields(preset, value)` 反射读入其余字段

这意味着 `captureWave`、`difficulty` 等类字段都可直接从 JSON 赋值。

### SectorPreset 类字段

`SectorPreset.java` 中直接定义了：

- `Planet planet`
- `Sector sector`
- `int captureWave`
- `float difficulty`
- 以及多个布尔配置字段

### UnlockableContent 通用字段

`UnlockableContent.java` 中定义了：

- `String localizedName`
- `String description`
- `String details`
- `boolean alwaysUnlocked`

所以这些字段对 `SectorPreset` 也成立。

### research 通用挂树逻辑

`ContentParser` 在读取字段前会先把 JSON 里的 `research` 取出，随后若对象是 `UnlockableContent`，就统一按科技树节点逻辑处理。

因此 `SectorPreset` 使用 `research` 是合法的，不是专属字段但能生效。

## Related

- [Mindustry Block 科技树 JSON 定义](../concepts/mindustry-block-tech-tree-json.md) — `research` 的通用挂树结构
- [Mindustry JSON 技能约定](../concepts/mindustry-json-skill-rules.md) — JSON 问题的检索与源码核对规则
