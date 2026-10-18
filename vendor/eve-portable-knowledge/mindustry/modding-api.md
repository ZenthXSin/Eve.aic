# Mindustry Modding API 源码验证

> Source: `源码和示例/Mindustry/core/src/mindustry/mod/` (9个文件, 4561行)

## Mod 基类

### 生命周期方法
| 方法 | 说明 |
|------|------|
| init() | 所有插件创建和命令注册后调用 |
| loadContent() | 客户端Mod加载内容时调用 |
| packSprites(MultiPacker) | 纹理打包时添加自定义纹理 |
| registerServerCommands(CommandHandler) | 注册服务器命令 |
| registerClientCommands(CommandHandler) | 注册客户端命令 |

### 配置方法
| 方法 | 说明 |
|------|------|
| getConfigFolder() | 获取配置文件夹 |
| getConfig() | 获取配置文件（mods/[name]/config.json） |

## ContentParser (1461行)

### 关键常量
| 常量 | 值 | 说明 |
|------|------|------|
| ignoreUnknownFields | true | 忽略未知字段 |
| typesToSearch | block, item, unit, liquid, planet | 搜索时的类型顺序 |
| implicitNullable | TextureRegion及其数组 | 可隐式为null的类型 |

### 类型解析器注册
ContentParser 为每种 ContentType 注册 TypeParser：
- ContentType.block → Block解析器
- ContentType.unit → UnitType解析器
- ContentType.weather → Weather解析器
- ContentType.item → Item解析器
- ContentType.liquid → Liquid解析器
- ContentType.status → StatusEffect解析器
- ContentType.sector → SectorPreset解析器
- ContentType.planet → Planet解析器
- ContentType.team → TeamEntry解析器

### 字段解析器（FieldParser）
特殊类型的自定义解析：
| 类型 | 解析逻辑 |
|------|----------|
| Effect | 字符串→Fx字段，数组→MultiEffect，对象→type解析 |
| TextureRegion | 字符串→atlas查找，icon-开头→图标查找 |
| Color | 字符串→Color.valueOf() |
| StatusEffect | 字符串→locate查找，对象→新建 |
| BuildVisibility | 字符串→字段查找 |
| Schematic | 字符串→base64或文件加载 |
| Attribute | 字符串→存在则返回，不存在则添加 |
| Attributes | 对象→键值对映射 |

### readFields() 方法
- 遍历JSON对象的所有字段
- 通过反射设置字段值
- 支持类型转换和特殊解析
- 处理嵌套对象和数组

### resolve() 方法
- 解析类名为Class对象
- 支持简写（如"Block"→"mindustry.world.Block"）
- allowClassResolution 控制是否允许任意类解析

## Mods.java (1526行)

### 核心功能
| 方法 | 说明 |
|------|------|
| loadMod() | 加载单个Mod |
| loadContent() | 加载所有Mod内容 |
| loadScripts() | 加载JS脚本 |
| parseMod() | 解析Mod元数据 |
| handleContent() | 处理加载的内容 |

### Mod 加载流程
```
1. 扫描mods/目录
2. 解析每个mod.json/mod.hjson
3. 加载Java类或JS脚本
4. 调用Mod.loadContent()
5. 解析JSON内容文件
6. 触发ModContentLoadEvent
7. 初始化所有内容（Content.init()）
8. 触发ContentInitEvent
```

## Scripts.java (142行)

### JS脚本系统
- 基于Rhino引擎
- 支持content/scripts/*.js
- 提供Java API的JS绑定
- 支持事件监听和内容注册

## ClassMap.java (517行)

### 类映射
- 提供字符串到Class的映射
- 用于JSON中的type字段解析
- 包含所有可序列化的游戏类

## 硬性规则

1. **Mod.init() 在所有内容加载后调用** — 不是loadContent()之后立即调用
2. **Mod.loadContent() 是客户端调用** — 服务端不调用
3. **ContentParser 忽略未知字段** — ignoreUnknownFields = true
4. **类型搜索顺序固定** — block → item → unit → liquid → planet
5. **TextureRegion 解析通过 atlas** — 字符串直接查找
6. **Color 必须是字符串** — 不是数字，否则警告
7. **StatusEffect 支持字符串引用和对象定义** — 两种方式
8. **Attribute 支持动态添加** — 不存在时自动创建
9. **resolve() 支持简写** — "Block" 自动补全包名
10. **allowClassResolution 控制类解析** — 安全考虑
11. **implicitNullable 类型可省略** — TextureRegion等
12. **readFields() 通过反射设置** — 字段必须是public或有setter
13. **ClassMap 包含所有可序列化类** — 用于type字段解析
14. **JS脚本在 content/scripts/*.js** — 不是其他目录
15. **ModContentLoadEvent 在内容加载后触发** — 不是init后

## JSON Mod 内容定义示例

### 定义新方块
```json
{
  "type": "Block",
  "name": "my-mod-my-block",
  "health": 100,
  "size": 2,
  "requirements": [
    {"item": "copper", "amount": 10}
  ]
}
```

### 定义新单位
```json
{
  "type": "UnitType",
  "name": "my-mod-my-unit",
  "health": 500,
  "speed": 1.5,
  "flying": true,
  "weapons": [
    {
      "type": "Weapon",
      "name": "my-weapon",
      "bullet": {
        "type": "BasicBulletType",
        "damage": 10
      }
    }
  ]
}
```

### 定义新状态效果
```json
{
  "type": "StatusEffect",
  "name": "my-mod-my-effect",
  "damage": 0.5,
  "speedMultiplier": 0.8,
  "color": "ff0000"
}
```

## 关联
- [Content System](content-system.md)
- [Content Loading Chain](../concepts/mindustry-content-loading-chain.md)
- [JSON Problem Solving Flow](../concepts/json-problem-solving-flow.md)
- [JSON Common Issues](../concepts/json-common-issues.md)
