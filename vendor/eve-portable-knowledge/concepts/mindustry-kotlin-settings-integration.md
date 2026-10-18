# Mindustry Kotlin Mod 设置项接入

## 目的
在 Kotlin Mod 中把自己的配置项接入 Mindustry 原版设置界面，并将值持久化到 `Core.settings`。

## 核心思路
1. 使用一个稳定的 settings key，例如 `mymod-enabled`。
2. 通过 `Core.settings` 读写设置值。
3. 在 `Vars.ui.settings` 对原版设置面板追加控件。
4. 在合适的初始化时机注册 UI 扩展，避免 UI 尚未创建就访问。

## 常见写法
### 1. 保存/读取设置值
```kotlin
val enabled = Core.settings.getBool("mymod-enabled", true)
Core.settings.put("mymod-enabled", enabled)
```

### 2. 向设置界面添加开关
```kotlin
override fun init() {
    Vars.ui.settings.addCheckPref(
        "mymod-enabled",
        true
    ) { enabled ->
        Core.settings.put("mymod-enabled", enabled)
    }
}
```

## 适用场景
- 开关类设置
- 简单数值配置
- 需要跟随原版设置菜单显示的 mod 配置

## 注意事项
- 不同 Mindustry 版本的设置 UI API 可能有差异，方法名可能不是 `addCheckPref`。
- 如果版本不支持直接追加原版设置项，可以改为在某个设置分组里插入自定义 `Table`。
- `Core.settings.put(...)` 只是写值，是否显示在 UI 里还需要额外挂接界面。

## 相关页面
- [Vars System](../mindustry/vars-system.md)
- [Vars UI](../mindustry/vars-ui.md)
- [Mindustry Java/Kotlin Mod加载链](../mindustry/mod-java-kotlin-loading.md)
