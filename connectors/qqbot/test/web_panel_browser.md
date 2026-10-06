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
```

脚本自己启动真实 Eve 进程、环回模型和 QQ 替身，使用合成令牌与合成正文。验证登录、仅内存令牌、任务取消确认、历史分页、截断标注、正文按纯文本展示、判断诊断（明确命令规则的一次判断、不显示正文或身份）、移动端布局、退出与刷新重新登录；截图保存为 `desktop.png`、`judgments.png`、`mobile-judgments.png` 和 `mobile.png`。不读取实际 QQ/模型凭据，不接触已有状态目录，停止只作用于脚本持有的子进程。
