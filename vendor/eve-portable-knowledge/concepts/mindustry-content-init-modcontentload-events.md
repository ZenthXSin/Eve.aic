# Mindustry ContentInitEvent vs ModContentLoadEvent

> Source: `源码和示例/Mindustry/core/src/mindustry/game/EventType.java`，`源码和示例/Mindustry/core/src/mindustry/core/ContentLoader.java`，`源码和示例/Mindustry/core/src/mindustry/mod/Mods.java`，`源码和示例/Mindustry/core/src/mindustry/Vars.java`

## 两个事件的区分

### ContentInitEvent
- 定义：`mindustry.game.EventType.ContentInitEvent`
- 触发位置：`ContentLoader.init()` 第108行 `Events.fire(new ContentInitEvent())`
- 触发时机：所有内容的 `Content.init()` 和 `Content.postInit()` 执行完毕之后
- 用途：通知系统所有内容已初始化完成，可以安全访问全部 Content 实例
- 监听：`Vars.java` 第383行 `Events.on(ContentInitEvent.class, e -> {...})` 中设置滑条计算等

### ModContentLoadEvent
- 定义：`mindustry.game.EventType.ModContentLoadEvent`
- 触发位置：`Mods.java` 第977行 `Events.fire(new ModContentLoadEvent())`
- 触发时机：所有 Mod 加载完成后
- 用途：通知系统所有 Mod 内容已加载完毕
- **注意**：类名是 `ModContentLoadEvent`，不是 `ContentLoadEvent`

## 重要结论

**Mindustry 源码中不存在 `ContentLoadEvent` 这个类名**。有人提出将 `ContentInitEvent` 改为 `ContentLoadEvent` 的建议是误写，正确对应的是 `ModContentLoadEvent`。

两个事件语义不同：
- `ContentInitEvent`：内容初始化完成（所有 `Content.init()` 执行完）
- `ModContentLoadEvent`：Mod 内容加载完成（所有 Mod 的 `loadContent()` 执行完）

不应互换使用。