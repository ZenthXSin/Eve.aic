# 跨平台容器隔离

Windows/macOS 需要可信配置的 Linux 容器 daemon（如 Docker Desktop Linux 模式或 Podman machine），并开启工作目录的只读共享。只安装 CLI 不代表隔离器可用；daemon 缺失、镜像缺失、挂载失败均拒绝执行，绝不降级原生 exec。不要把 daemon socket、宿主凭据或业务目录加入镜像。

管理员在受授权的构建环境构建本目录 Dockerfile，核对基础镜像和软件来源，获取 `docker image inspect --format '{{.Id}}' 镜像名`。Eve 可信启动配置提供 `EVE_SCRIPT_CONTAINER_CLI`（CLI绝对路径）与 `EVE_SCRIPT_CONTAINER_IMAGE`（完整 `sha256:...` 镜像ID）。两项必须同时提供。模型不能改变镜像、命令参数、挂载或容器权限。运行时不拉取镜像，网络构建不属于脚本执行权限。

隔离配置固定：UID/GID 65534的非root进程、无网络、只读根、capabilities全撤、禁止提权，256MiB内存、最多32 PID、3秒CPU、单文件1MiB，16MiB临时tmpfs。源码与本次调用工作目录在宿主私有暂存父目录内复制为只读可读文件；每次RPC返回前同步有界文件，宿主原文件不改变权限；该暂存视图只读挂载，不挂宿主根、Docker socket或凭据。入口清空环境（包括Docker自动注入的代理变量）。容器内固定30秒硬超时，宿主取消会销毁客户端并发送强制容器清理；daemon不可达时进程仍最多存活到硬期限。与Linux bubblewrap的禁止fork不同，容器后端允许上限内的子进程，所有子进程仍在同一无网络/只读/资源边界内。

跨平台真实验收命令：配置 `EVE_TEST_CONTAINER_CLI` 与 `EVE_TEST_CONTAINER_IMAGE` 后执行 `cargo test -p eve-script-runtime container_backend_real -- --ignored --nocapture`。该用例实际启动Python与JavaScript、验证无socket/宿主目录/代理环境/外网、只读工作目录以及状态迁移。没有这些环境时测试保持显式ignored，不能将普通CI通过解读为Windows/macOS真实执行通过。Linux本地容器通过也不代表macOS路径共享或Windows daemon集成已验收。
