# Mindustry 网络系统源码验证

> Source: `源码和示例/Mindustry/core/src/mindustry/net/Net.java`、`Packets.java`、`NetConnection.java`、`core/NetServer.java`、`core/NetClient.java` 直接验证，2026-06-11

## 1. Net（`net/Net.java`，431行）

### 核心字段

```java
private boolean server;      // 是否是服务器
private boolean active;      // 是否激活（多人游戏）
private boolean clientLoaded; // 客户端是否加载完成

private final Seq<Packet> packetQueue = new Seq<>();  // 加载期间包队列
private final ObjectMap<Class<?>, Cons> clientListeners = new ObjectMap<>();
private final ObjectMap<Class<?>, Cons2<NetConnection, Object>> serverListeners = new ObjectMap<>();
private final IntMap<StreamBuilder> streams = new IntMap<>();
```

### 包注册顺序

```java
static {
    registerPacket(StreamBegin::new);    // id=0
    registerPacket(StreamChunk::new);    // id=1
    registerPacket(WorldStream::new);    // id=2
    registerPacket(ConnectPacket::new);  // id=3
    Call.registerPackets();              // 生成的包从 id=4 开始
}
```

### 网络状态方法

```java
public boolean active()  // 是否多人游戏
public boolean server()  // 是否服务器（server && active）
public boolean client()  // 是否客户端（!server && active）
```

### 包处理逻辑

**客户端**（`handleClientReceived`）：
1. StreamBegin → 创建 StreamBuilder
2. StreamChunk → 追加数据，完成后递归处理
3. 高优先级包 → 立即处理
4. 客户端已加载 → 正常处理
5. 非低优先级 → 入队等待

**服务端**（`handleServerReceived`）：
1. 检查 `hasConnected` 或高优先级
2. 有自定义监听器 → 调用监听器
3. 无监听器 → 调用 `object.handleServer(connection)`
4. ValidateException → 忽略

### 常见误区

- **server() = server && active**：必须同时满足
- **包队列只在客户端加载期间使用**：加载完成后清空
- **StreamBuilder 是流式传输**：不是单个包
- **RegisterPacket 顺序影响 ID**：新增包必须追加在末尾

## 2. Packets（`net/Packets.java`，177行）

### KickReason 枚举

```java
kick, clientOutdated, serverOutdated, banned, gameover(true), recentKick,
nameInUse, idInUse, nameEmpty, customClient, serverClose, vote, typeMismatch,
whitelist, playerLimit, serverRestarting
```

- **gameover 是 quiet**：不会显示踢出消息

### 包结构

| 包 | 字段 | 说明 |
|----|------|------|
| Connect | addressTCP | 连接事件 |
| Disconnect | reason | 断开原因 |
| ConnectPacket | version, versionType, mods, name, locale, uuid, usid, mobile, color | 连接握手 |
| WorldStream | (继承 Streamable) | 世界数据流 |
| StreamBegin | id, total, type | 流开始标记 |
| StreamChunk | id, data | 流数据块 |

### ConnectPacket 序列化

```java
// 写入顺序
buffer.i(Version.build);        // 版本号
TypeIO.writeString(versionType); // 版本类型
TypeIO.writeString(name);        // 玩家名
TypeIO.writeString(locale);      // 语言
TypeIO.writeString(usid);        // 用户会话 ID
buffer.b(Base64Coder.decode(uuid)); // UUID（16字节）
buffer.l(CRC32(uuid));           // UUID 校验
buffer.b(mobile ? 1 : 0);       // 是否移动端
buffer.i(color);                 // 颜色
buffer.b((byte)mods.size);       // mod 数量
for(mod : mods) TypeIO.writeString(mod); // mod 列表
```

### 常见误区

- **ConnectPacket.mods 是 Seq<String>**：mod 名称列表，不是文件
- **uuid 是 Base64 编码的 16 字节**：不是字符串
- **color 是 int（RGBA）**：不是 Color 对象
- **StreamBegin.total 是总字节数**：用于进度条

## 3. NetConnection（`net/NetConnection.java`，125行）

### 字段

```java
public final String address;           // IP 地址
public String uuid = "AAAAAAAA";       // 用户 UUID
public String usid = uuid;             // 用户会话 ID
public boolean mobile, modclient;      // 客户端类型
public @Nullable Player player;        // 关联的玩家
public boolean kicked = false;         // 是否被踢出
public long syncTime;                  // 同步时间戳
public long connectTime;               // 连接时间
public int lastReceivedClientSnapshot = -1;
public int snapshotsSent;
public Seq<BuildPlan> rejectedRequests = new Seq<>(); // 被拒绝的建造请求
public Ratekeeper chatRate = new Ratekeeper();         // 聊天频率限制
public Ratekeeper packetRate = new Ratekeeper();       // 包频率限制
```

### 踢出机制

```java
// 踢出有 4 个重载
kick()                           // 标准踢出
kick(KickReason)                 // 带原因的踢出
kick(KickReason, long)           // 带原因和持续时间
kick(String)                     // 自定义消息
kick(String, long)               // 自定义消息和持续时间

// 默认踢出持续时间
kick/banned/vote → 30 秒
其他原因 → 0（不封禁）
```

### 常见误区

- **uuid 默认 "AAAAAAAA"**：不是 null
- **rejectedRequests 每个快照清空**：不是永久的
- **chatRate/packetRate 是频率限制器**：防刷屏
- **sendStream 使用 maxTcpSize（1100）分块**：不是一次性发送

## 4. NetServer（`core/NetServer.java`）

### 核心字段

```java
public Administration admins = new Administration();  // 管理系统
public CommandHandler clientCommands = new CommandHandler("/"); // 客户端命令
public TeamAssigner assigner;  // 队伍分配器
public ChatFormatter chatFormatter; // 聊天格式化
public @Nullable VoteSession currentlyKicking = null; // 当前投票踢人

// 投票系统
public static int kickDuration = 60 * 60;       // 踢出持续 1 小时
public static float voteDuration = 0.5f * 60;   // 投票持续 30 秒
public static int voteCooldown = 60 * 5;         // 投票冷却 5 分钟

// 同步
private static final float correctDist = tilesize * 14f; // 位置校正距离（112 像素）
private static final int maxSnapshotSize = 800;  // 最大快照大小
private static final Timekeeper blockSyncTime = Timekeeper.ofSeconds(6f);
private static final Timekeeper healthSyncTime = Timekeeper.ofSeconds(0.5f);
```

### 队伍分配逻辑

```java
// PvP 模式：找玩家最少的队伍
// 非 PvP：使用 state.rules.defaultTeam
// 排除：waveTeam（如果 waves=true）、无核心队伍、derelict、protectCores=false
```

### @Remote 注解系统

```java
@Remote(targets = Loc.client)                    // 客户端→服务器
@Remote(variants = Variant.both)                 // 双向
@Remote(targets = Loc.client, unreliable = true) // 不可靠 UDP
@Remote(variants = Variant.both, priority = PacketPriority.high)
```

- **Call 类是自动生成的**：通过 `@Remote` 注解处理器
- **`Call.registerPackets()`**：注册生成的包类型
- **`Variant.one`**：只生成一个方向
- **`Variant.both`**：生成双向调用

## 5. NetClient（`core/NetClient.java`）

### 核心字段

```java
private long ping;                    // 延迟
private boolean connecting = false;   // 正在连接
private boolean quiet = false;        // 静默断开
private boolean quietReset = false;   // 完全静默
private float timeoutTime = 0f;       // 超时计时器
private long lastSnapshotTimestamp;   // 最后快照时间

// 同步频率
private static final long entitySnapshotTimeout = 1000 * 20; // 20 秒
private static final float dataTimeout = 60 * 30;            // 30 分钟
private static final Timekeeper playerSyncTime = Timekeeper.ofMillis(66); // ~15 Hz
private static final Timekeeper planSyncTime = Timekeeper.ofSeconds(0.5f);
private static final Timekeeper pingTime = Timekeeper.ofSeconds(1f);
```

### 连接流程

```
1. Connect 包到达 → 发送 ConnectPacket（含 mods/uuid/name）
2. 服务器验证 → 发送 WorldStream
3. 客户端接收 → NetworkIO.loadWorld() 解压加载
4. 加载完成 → setClientLoaded(true) → 处理队列中的包
```

### 常见误区

- **playerSyncTime = 66ms**：约 15 Hz，不是每帧
- **dataTimeout = 30 分钟**：长时间无数据才超时
- **quiet 和 quietReset 不同**：quiet 只隐藏消息，quietReset 完全静默
- **ConnectPacket 在客户端构造**：不是服务器发送的

## 6. 流式传输机制

```
服务器: sendStream(Streamable)
  → 发送 StreamBegin（id, total, type）
  → 循环发送 StreamChunk（id, data[maxTcpSize=1100]）
  → 客户端接收 StreamBegin → 创建 StreamBuilder
  → 客户端接收 StreamChunk → 追加到 builder
  → builder.isDone() → build() → 递归处理完整包
```

### 常见误区

- **StreamBegin.type 是 byte**：区分不同流类型
- **maxTcpSize = 1100**：每个 chunk 最大 1100 字节
- **WorldStream 继承 Streamable**：不是 Packet
- **流完成后递归调用 handleClientReceived**：解码后的包走正常处理

## Related

- [Mindustry 渲染/输入/网络](mindustry-rendering-input-network.md)
- [Mindustry Vars 系统](mindustry-vars-system.md)
- [Mindustry GameState/Rules](mindustry-content-loading-gamestate-rules-save.md)
