# Arc util 内容补全建议

> Source: 本次对话中关于 Mindustry/Arc 开发、`updateTile()`、反序列化崩溃排查的讨论

## 说明
本页记录目前知识库里与 `arc.util` 相关、但尚未系统整理的常用内容方向，便于后续按主题补全。

## 已知可补主题

### 1. 时间与计时工具
- `Time`：游戏内时间、延迟任务、时间推进相关
- `Timer` / `TaskQueue`：定时回调与任务队列
- `Time.run()`：延迟执行逻辑的常用入口

### 2. 序列化与反序列化
- `arc.util.serialization.Json`
- `JsonValue`
- `SerializationException`
- 与 Mindustry `JsonIO`、`SaveIO`、`SaveVersion` 的协作链路

### 3. IO 相关工具
- `arc.util.io.Reads`
- `arc.util.io.Writes`
- 常用于建筑状态写入/读取、存档数据保存

### 4. 日志与异常
- `Log`
- `Strings.neatError()`
- 崩溃时的 trace 定位方式

### 5. 函数式与集合辅助
- `Seq`
- `ObjectMap`
- `Eachable` / 遍历辅助
- 常见工具方法的用途和边界

### 6. 线程与并发辅助
- `Threads`
- 主线程/后台任务的基本约束
- Mindustry Mod 中什么时候不能直接开线程

## 建议的后续拆页
- [Arc Serialization & Json](../mindustry/arc-framework.md) — 先补序列化链路，再补异常定位
- [Arc Time & Timer](../mindustry/arc-framework.md) — 补定时与任务调度
- [Arc IO Basics](../mindustry/arc-framework.md) — 补 Reads/Writes 的存取约定

## 备注
当前这是一份“补全清单”，不是最终说明页。后续如果用户继续问到某个具体类，例如 `Json`、`Reads`、`Writes`、`Timer`，建议单独拆成知识页，再在索引中加入正式条目。
