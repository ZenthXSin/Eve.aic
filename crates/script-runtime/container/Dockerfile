# 仅由可信管理员构建；部署时配置 docker image inspect 返回的完整 sha256 ID。
# 插件运行永远 --pull=never，不在运行路径下载依赖。
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends python3 nodejs coreutils \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /work
