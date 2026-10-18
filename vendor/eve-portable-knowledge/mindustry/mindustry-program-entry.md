# Mindustry 程序入口

## 结论
Mindustry 的程序入口分为两条线：

- **客户端入口**：`mindustry.ClientLauncher`
- **服务端入口**：`mindustry.server.ServerLauncher#main(String[] args)`

## 客户端启动链路片段
源码里客户端大致是这样串起来的：

```java
public class ClientLauncher extends ApplicationCore{
    @Override
    public void init(){
        setup();
    }

    public void setup(){
        // 初始化窗口、内容、UI、输入、渲染等
    }

    @Override
    public void update(){
        // 主循环
    }
}
```

核心理解：

- `init()` 是生命周期入口
- `setup()` 才是客户端真正开始搭建游戏环境的地方
- `update()` 负责帧循环

## 服务端启动链路片段
服务端入口可以概括成：

```java
public class ServerLauncher{
    public static void main(String[] args){
        // 设置平台和服务器环境
        // 创建无头应用
        // 进入服务器初始化流程
    }
}
```

更关键的是：

- `main()` 是 JVM 入口
- 之后会走无头服务器初始化
- 最后进入服务器运行循环

## 结构理解
可以把它理解成：

```text
客户端：ClientLauncher
  └─ init()
      └─ setup()
          └─ update()

服务端：ServerLauncher.main()
  └─ HeadlessApplication
      └─ init()
```

## 一句话版
- **游戏客户端**：从 `ClientLauncher.init()` / `setup()` 开始
- **专用服务器**：从 `ServerLauncher.main()` 开始

如果你要，我可以继续往下补 **Vars 初始化、内容加载、Core/Net/UI 挂载** 的源码链路。