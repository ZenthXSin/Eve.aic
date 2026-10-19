# GitHub 公开访问配置

在 `EVE_DEVELOPER_CONFIG` 指向的宿主 JSON 内配置：

```json
{
  "github": true,
  "github_all_users": true,
  "github_all_repositories": true
}
```

这是已有开发工具配置的补充字段，仍需提供绝对工作目录、`operators` 和凭据环境引用等原有字段；配置内不填写 token。修改后重启运行器生效，现有状态、记忆和操作账本保留。

`github_all_users` 允许具有宿主绑定身份的所有聊天用户使用 `github_api`，包括旧会话；文件、命令及自身升级仍按 `operators` 授权。`github_all_repositories` 取消 GitHub 写入仓库白名单，实际访问受宿主 token 的 GitHub 授权约束。缺少 token 时无法写入。

两个字段默认均为 `false`，旧配置的操作者和仓库白名单语义保持不变。任意 URL、自报凭据、账户/密钥管理、Actions secrets/variables 仍不属于工具提供的接口；请求取消、凭据脱敏和操作账本继续生效。公开模式会让聊天用户触发宿主 token 允许的仓库写操作，应由宿主明确启用。
