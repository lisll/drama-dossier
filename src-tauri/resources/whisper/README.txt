这个目录由 CI（.github/workflows/desktop.yml）在打包时预置：

- `whisper-cli`（Windows 为 `whisper-cli.exe`）：由 whisper.cpp 源码本地编译，
  Windows 为 CPU 版，macOS 为 arm64 + x86_64 通用二进制且已嵌入 Metal。
- `models/ggml-base.bin`：官方 base 模型（约 141 MB），下载后按 sha256 校验。

「一键安装内容模型」会把它们复制到应用数据目录，用户端因此不需要 Python、
venv、pip 或任何联网下载。本地开发与未预置的构建会退回在线下载流程。

为什么不用官方预编译包：whisper.cpp 的稳定版 release 不附带任何二进制，
macOS 侧只发 xcframework（没有 CLI 可执行文件）；Windows 的 CUDA 包 285~685 MB，
不适合内置——有 NVIDIA 显卡的用户由客户端检测到之后后台自动下载。
