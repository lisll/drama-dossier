这个目录由 CI（.github/workflows/build-windows.yml）在构建 Windows 安装包时
预置 ffmpeg.exe / ffprobe.exe，让「一键配置运行环境」在用户机器上零下载。
本地开发与 mac 构建无需此目录内容，运行时会自动退回在线下载流程。
