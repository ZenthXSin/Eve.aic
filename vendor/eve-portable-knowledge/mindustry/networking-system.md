# Networking System

## 概述
Mindustry 网络系统基于 arc.net 库，采用 TCP 协议通信。核心架构为 Net（协议层）+ NetServer（服务端逻辑）+ NetClient（客户端逻辑）+ Call（RPC 调用）。

## 核心类

### Net.java（432行）
- 包路径：`mindustry.net.Net`
- 职责：协议层，管理 packet 注册、listener、连接状态
- 关键字段：
  - `packetProvs: Seq<Prov<? extends Packet>>` — packet 工厂列表
  - `packetClasses: Seq<Class<? extends Packet>>` — packet 类列表
  - `packetToId: ObjectIntMap<Class<?>>` — 类→ID 映射
  - `clientListeners: ObjectMap<Class<?>, Cons>` — 客户端监听器
  - `serverListeners: ObjectMap<Class<?>, Cons2<NetConnection, Object>>` — 服务端监听器
  - `packetQueue: Seq<Packet>` — 加载完成前的包队列
  - `clientLoaded: boolean` — 客户端是否加载完成
- 关键方法：
  - `registerPacket(Prov<T>)` — 注册 packet 类型（静态方法）
  - `getPacketId(Packet)` — 获取 packet 的 byte ID
  - `newPacket(byte)` — 根据 ID 创建 packet 实例
  - `handleClientReceived(Packet)` — 处理客户端收到的包
  - `setClientLoaded(boolean)` — 设置客户端加载状态，加载完成后处理队列中的包

### NetServer.java（1300行）
- 包路径：`mindustry.core.NetServer`
- 职责：服务端逻辑，处理连接、验证、同步
- 关键字段：
  - `maxSnapshotSize = 800` — 快照最大字节数
  - `blockSyncTime = 6秒` — 方块同步间隔
  - `healthSyncTime = 0.5秒` — 血量同步间隔
  - `correctDist = tilesize * 14f` — 玩家位置校正距离
  - `admins: Administration` — 管理系统
  - `clientCommands: CommandHandler` — 客户端命令处理器（"/"前缀）
  - `chatFormatter: ChatFormatter` — 聊天消息格式化
  - `kickDuration = 3600秒` — 踢出时长
  - `voteDuration = 30秒` — 投票时长
  - `voteCooldown = 300秒` — 投票冷却
- 关键机制：
  - Connect 处理：IP 封禁检查、UUID 验证、mod 兼容性检查
  - ConnectPacket 处理：验证→分配队伍→发送世界数据
  - 玩家同步：位置校正、快照发送、血量同步

### NetConnection.java
- 包路径：`mindustry.net.NetConnection`
- 职责：单个客户端连接抽象
- 关键字段：`player`, `address`, `uuid`, `usid`, `mobile`, `hasBegunConnecting`

### Packets.java（176行）
- 包路径：`mindustry.net.Packets`
- 职责：定义所有 packet 类型
- 关键类型：
  - `Connect` — TCP 连接建立
  - `Disconnect` — 断开连接
  - `ConnectPacket` — 客户端连接请求（含 uuid/name/mods）
  - `WorldStream` — 世界数据流
  - `StreamBegin/StreamChunk` — 流传输控制
  - `InvokePacket` — RPC 调用包

### Call 类（自动生成）
- 由注解处理器根据 `@Remote`/`@Local` 注解生成
- 不是手写代码，不要在源码中查找完整实现
- 提供客户端→服务端和服务端→客户端的 RPC 调用

### NetworkIO.java
- 包路径：`mindustry.net.NetworkIO`
- 职责：网络数据序列化/反序列化
- 关键方法：`writeSnapshot()`, `readSnapshot()`, `writeWorld()`, `readWorld()`

## 连接流程

```
客户端                    服务端
  |                         |
  |-- Connect ------------->|  TCP 连接建立
  |-- ConnectPacket ------->|  发送 uuid/name/mods
  |                         |  验证：封禁/UUID/mod兼容
  |<-- WorldStream ---------|  发送世界数据（压缩）
  |                         |  客户端解压并加载
  |-- StreamBegin/Chunk --->|  发送地图流
  |<-- StreamBegin/Chunk ---|  
  |                         |
  |  [clientLoaded=true]    |  客户端加载完成
  |<-- InvokePacket --------|  开始接收 RPC 调用
  |-- InvokePacket -------->|  开始发送 RPC 调用
```

## 关键机制

### Packet 注册
```java
// 静态初始化块中注册
static {
    registerPacket(StreamBegin::new);
    registerPacket(StreamChunk::new);
    registerPacket(WorldStream::new);
    registerPacket(ConnectPacket::new);
    Call.registerPackets(); // 注册生成的 packet 类
}
```

### 客户端加载队列
- 客户端未加载完成时，收到的普通包会被放入 `packetQueue`
- `setClientLoaded(true)` 后，队列中的包会被依次处理
- 流相关包（StreamBegin/Chunk/WorldStream）不受此限制

### 位置校正
- 服务端检测玩家位置偏差超过 `correctDist`（14*tilesize=112）时强制校正
- 防止玩家瞬移作弊

### 投票系统
- `VoteSession` 管理投票状态
- `kickDuration = 3600秒`（1小时）
- `voteDuration = 30秒`
- `voteCooldown = 300秒`（5分钟）

## 关联
- [NetClient](vars-network.md) — 客户端网络逻辑
- [UI System](ui-system.md) — 网络错误显示
- [Logic System](logic-system.md) — 逻辑处理器网络同步
