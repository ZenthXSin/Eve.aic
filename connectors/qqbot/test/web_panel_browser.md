# 本地面板浏览器验收

`web_panel_test.py` 与 `crates/web-panel/tests/http.rs` 已接入普通离线 CI。浏览器脚本是可选本地验收，不在 CI 自动安装 Chromium。

先构建 `eve-qqbot`，在独立工具目录安装 Playwright 和 Chromium（需要相应系统图形库）：

```bash
cargo build -p eve-app --bin eve-qqbot --locked
npm install --prefix /tmp/eve-web-playwright --no-audit --no-fund playwright
node /tmp/eve-web-playwright/node_modules/playwright/cli.js install chromium
# 系统库缺失时可由本机管理员执行：
# node /tmp/eve-web-playwright/node_modules/playwright/cli.js install-deps chromium
EVE_PLAYWRIGHT_MODULE=/tmp/eve-web-playwright/node_modules/playwright/index.mjs \
EVE_QQBOT_BINARY="$PWD/target/debug/eve-qqbot" \
EVE_WEB_ARTIFACTS=/tmp/eve-web-browser-artifacts \
node connectors/qqbot/test/web_panel_browser.mjs
# 认知目标页使用同样的环境变量：
node connectors/qqbot/test/web_panel_cognition_browser.mjs
node connectors/qqbot/test/web_panel_memory_browser.mjs
```

脚本自己启动真实 Eve 进程、环回模型和 QQ 替身，使用合成令牌与合成正文。验证登录、仅内存令牌、任务取消确认、历史分页、截断标注、正文按纯文本展示、判断诊断（明确命令规则的一次判断、不显示正文或身份）、移动端布局、退出与刷新重新登录；截图保存为 `desktop.png`、`judgments.png`、`mobile-judgments.png` 和 `mobile.png`。不读取实际 QQ/模型凭据，不接触已有状态目录，停止只作用于脚本持有的子进程。

`web_panel_cognition_browser.mjs` 以 `--cognition` 启动真实 Eve，经 QQ 替身保存一条含 HTML 文本的 `/goal`、等待第一份反思草稿，再提交 `/goal-feedback` 生成第二份草稿。浏览器验证目标列表只显示父目标、详情中当前草稿在前且历史草稿保留、“未验证”标注、HTML 按纯文本显示、浏览期间状态文件字节不变、手机布局与退出；截图保存为 `goals.png` 和 `mobile-goals.png`。主脚本另验证未开启认知或记忆时页面提示不可用。

`web_panel_memory_browser.mjs` 以 `--memory` 两次启动真实 Eve：第一次经 QQ 替身保存两条偏好（其一含 HTML 文本）并完成三轮对话；第二次加 `--memory-learning`，更正其中一条并等待一次提炼批次完成后打开面板。浏览器验证作用域列表、偏好与版本历史（新的在前）、显式打开来源后用户声明按纯文本显示、手动模式下的待确认学习候选与“决策不等于保存”说明、浏览期间状态文件字节不变、手机布局与退出；截图保存为 `memory.png` 和 `mobile-memory.png`。
