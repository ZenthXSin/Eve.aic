# Mindustry Mod 搜索机制

## 数据源

Mindustry 的 mod 浏览器**不使用 mod.io**，而是从 GitHub 拉取一个 JSON 文件：

```java
// Vars.java:91
public static final String[] modJsonURLs = {
    "https://raw.githubusercontent.com/Anuken/MindustryMods/master/mods.json",
    "https://cdn.jsdelivr.net/gh/anuken/mindustrymods/mods.json"  // 备用 CDN
};
```

## 数据结构

`ModListing` 类（`mindustry.mod.ModListing`）包含以下字段：
- `repo` - GitHub 仓库路径（如 "Anuken/ExampleMod"）
- `name` - 显示名称
- `internalName` - 内部名称
- `author` - 作者
- `lastUpdated` - 最后更新时间（ISO 8601 格式）
- `description` - 描述
- `minGameVersion` - 最低游戏版本
- `hasScripts` - 是否包含脚本
- `hasJava` - 是否包含 Java 代码
- `iosCompatible` - 是否兼容 iOS
- `legacyCompatible` - 是否兼容旧版
- `contentTypes` - 内容类型数组
- `stars` - 星标数

## 搜索流程

1. `ModsDialog.getModList()` 从 `modJsonURLs` 数组依次尝试获取 JSON
2. 使用 `JsonIO.json.fromJson(Seq.class, ModListing.class, strResult)` 解析
3. 按 `lastUpdated` 降序排序
4. 搜索是客户端过滤（在已获取的列表中筛选）

## 导入流程

- **GitHub 导入**：`githubImportMod(repo, isJava, release)` 直接从 GitHub 仓库下载
- **Release 导入**：对于 Java mod，优先从 GitHub Releases 下载编译好的 jar
- **分支导入**：对于非 Java mod，直接下载仓库分支内容

## 与 mod.io 的区别

- mod.io 是独立的 mod 托管平台，有自己的 API
- Mindustry 使用 GitHub 作为 mod 列表源，mod 本身也托管在 GitHub
- `mods.json` 由社区维护，不是自动同步的

## 源码位置

- Mod 列表数据类：`mindustry.mod.ModListing`
- Mod 浏览器 UI：`mindustry.ui.dialogs.ModsDialog`
- URL 定义：`mindustry.Vars.modJsonURLs`
