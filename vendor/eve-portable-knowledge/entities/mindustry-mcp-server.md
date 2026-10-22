# Mindustry MCP Server

## 作用
这是一个 **MCP 服务器**，作用是把 **MindustryX 的 AI bridge** 包装成标准 MCP 工具接口，让支持 MCP 的客户端可以直接调用 Mindustry 游戏能力。

## 它能做什么
### 读取类工具
- `get_state`
- `list_maps`
- `get_tiles`
- `get_buildings`
- `get_units`
- `get_content`

### 写入/控制类工具
- `load_map`
- `join_game`
- `leave_game`
- `place_block`
- `break_block`
- `write_logic`
- `spawn_unit`
- `set_speed`
- `pause`

### 其它
- `await_op`：等待异步操作完成

## 部署前提
- 先有 **MindustryX**，并且 **AI bridge 已启用**
- Node.js **20+**
- 服务器会自动从系统临时目录读取 `bridge-info.json`，并连接本地 bridge

## 部署流程
1. 安装依赖：`npm install`
2. 编译：`npm run build`
3. 启动：`npm start`

## 入口与运行方式
- `src/index.ts` 使用 `@modelcontextprotocol/sdk`
- 通过 `StdioServerTransport` 运行
- 这意味着它通常是作为 **MCP 子进程** 被上层客户端拉起，而不是一个网页服务

## 注意点
- 它不是独立的 Mindustry 服务器
- 没有 MindustryX + AI bridge，就没法工作
- 如果 `bridge-info.json` 没生成，说明 bridge 侧没起来

## 关联
- MindustryX 原始仓库: https://github.com/TinyLake/MindustryX （TinyLake，262 stars）
- MindustryX BEK 分支: https://github.com/BEK-Group/MindustryX （BEK-Group/风，基于 TinyLake 继续开发）
- MCP server: https://github.com/DeterMination-Wind/Mindustry-MCP
