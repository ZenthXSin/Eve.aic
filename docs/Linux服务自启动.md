# Linux 服务自启动与恢复

`scripts/eve-service.py` 是已有 Eve 部署的宿主启动入口，读取部署记录、已验证候选和私有配置，复用两层守护。它不创建空白业务状态，也不把凭据写入命令行。

```sh
python3 scripts/eve-service.py \
  --service-record /path/to/qq-service.json \
  --private-directory /path/to/private start
```

私有目录沿用部署的 `developer-features.json` 和 `features-environment.json`；服务记录提供守护脚本、控制目录、状态目录和进程身份。`status` 输出进程与 QQ 连通状态；`stop` 先禁用自动恢复，再正常停止守护；`start` 重新启用。`ensure --background` 仅在已启用且没有相同身份的守护时后台恢复，适合登录钩子或平台启动脚本。

启动采用进程锁，重复调用接管同一个守护。判断进程必须同时核对 boot ID、PID、启动时刻和脚本路径；旧 boot 的进程元数据归档后重新建立，不向本次启动中碰巧同号的进程发信号。候选摘要、已有状态、端口和孤立监督器存在冲突时停止，不强占、不重复启动。

每次真实启动先备份业务状态和控制记录。内层监督器恢复 Eve，外层守护恢复监督器，沿用原有次数上限和退避；已被标记为 `blocked` 的恢复不由启动入口绕过，损坏配置也不静默覆盖。历史工具的结果未知状态继续由原运行时保留，不因自启动重放操作。

## 容器与开机触发的区别

PID 1 没有运行 systemd 的容器，不能只安装一个 service 文件就宣称开机启动生效。登录钩子只在登录 shell 或首次进入工作环境时执行；不保证容器刚启动、尚未有会话时执行。

当前部署安装的 `~/.local/bin/eve-service` 封装了实际路径，`~/.profile` 和交互式 `~/.bashrc` 调用：

```sh
"$HOME/.local/bin/eve-service" ensure --background
```

若平台提供真正的启动脚本入口，应在平台配置中加入：

```sh
/home/agent/.local/bin/eve-service start --background
```

平台入口的配置与启动记录必须单独确认；登录恢复测试不能代替开机触发验收。首次安装不停止正在运行的 Eve，实际部署脚本从固定副本执行，避免工作区后续开发直接改变宿主启动逻辑。
