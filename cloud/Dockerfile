# 部署镜像（M7-7）。同一个镜像跑两个服务，靠 command 区分。
#
# 为什么一个镜像而不是两个：两个服务是同一个 Python 包
# （`drama_cloud` 同时提供 `drama-commerce` 与 `drama-cloud` 两个入口），
# 拆两个镜像只是把同一个东西装两遍。

FROM python:3.12-slim-bookworm

# 媒体控制面要调 ffmpeg/ffprobe；commerce 用不到但不影响。
# 字体给报审包 PDF 用（Noto CJK）。
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ffmpeg \
        fonts-noto-cjk \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# 先只拷依赖声明，让 pip 层能复用缓存。
COPY pyproject.toml /app/pyproject.toml
COPY src /app/src
RUN pip install --no-cache-dir -e /app

# 非 root 运行：容器被攻破时影响面更小。
# 数据目录 /data 与密钥目录在 compose 里挂卷，属主由卷决定；
# 这里只保证进程不需要 root。
RUN useradd --create-home --uid 10001 appuser \
    && mkdir -p /data && chown -R appuser:appuser /data /app
USER appuser

ENV PYTHONUNBUFFERED=1

# 默认跑媒体控制面；commerce 在 compose 里覆盖 command。
CMD ["drama-cloud", "serve", "--host", "0.0.0.0", "--port", "8000"]
