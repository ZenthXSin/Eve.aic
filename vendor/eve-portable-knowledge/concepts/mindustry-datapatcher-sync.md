# Mindustry DataPatcher 同步机制

> Source: GameState.java:47, NetworkIO.java:53/87, DataPatcher.java

## 结论

`DataPatcher` 补丁**仅本地挂载**于 `GameState.patcher`，不在 `Packet` 中传输。多人生存模式下补丁走存档区域且读档时重放，联机中修改**不会自动同步**给已在线玩家。

## 关键源码证据

```java
// GameState.java:47
public DataPatcher patcher = new DataPatcher();

// NetworkIO.java:53 — 新玩家连接时通过存档流发送
SaveIO.getSaveWriter().writeDataPatches(stream);

// NetworkIO.java:87 — 读取时反序列化
SaveIO.getSaveWriter().readDataPatches(stream);
```

## 同步行为

| 场景 | 行为 |
|------|------|
| 新玩家连接 | 通过 `WorldStream` 存档区域发送补丁（`writeDataPatches`） |
| 读取存档 | 补丁随存档重放（`readDataPatches`） |
| 已在线玩家修改补丁 | **不会**自动推送给其他玩家 |
| 切地图/重载 | 补丁被清除 |
| 联机实时同步 | 需自行在 `Packet` 中传递 patch 字符串并调用 `apply()` |

## 硬性规则

1. `DataPatcher` 挂载在 `Vars.state.patcher`，是 `GameState` 的字段
2. 补丁不走标准 `Packet` 系统，走存档流
3. 联机实时同步需要自定义 `Packet` 实现
4. `DataPatcher.patchFormatVersion = 1`（当前版本）
5. `maxImageSize = 1024`（补丁图片最大尺寸）
6. 补丁是叠加的但 `apply()` 会先 `unapply()` 再重新应用，不堆叠

## 关联

- [Networking System](../mindustry/networking-system.md)
- [Save and Load System](../mindustry/save-load-system.md)
- [GameState Vars](../mindustry/vars-game.md)
