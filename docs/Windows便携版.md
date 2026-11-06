# Windows 支持已停止

从 2026-11-06 的平台范围调整起，Eve 不再维护 Windows CI、生成 Windows x64 / ARM64 发行包或提供新的 Windows 更新。当前支持 Linux x64 / ARM64，安装与发行见 [Linux 双架构发行](./跨平台发行.md)。macOS 也不在支持范围内。

已经发布的 Windows Release 资产及原用户目录保持原样。旧启动器仍按原平台匹配资产；新版本缺少 Windows 资产时不会选择 Linux 包，不会转换或清空配置、会话和训练数据。这不代表旧 Windows 版本仍有兼容性或修复保证。

历史资料保存在停止支持前的主线提交 `c57f35b0f156a3c2144ac2d31a498a9e13f4f47c`：[原构建与验收说明](https://github.com/ZenthXSin/Eve.aic/blob/c57f35b0f156a3c2144ac2d31a498a9e13f4f47c/docs/Windows便携版.md)、[原安装说明](https://github.com/ZenthXSin/Eve.aic/blob/c57f35b0f156a3c2144ac2d31a498a9e13f4f47c/packaging/windows/使用说明.md)。这些资料只描述当时的版本。
