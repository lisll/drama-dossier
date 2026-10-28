# 构建与发布

## 本地开发

```bash
# 授权形态 feature 互斥，不能用 --all-features（见 docs/LICENSING.md「构建形态」）
cargo test --workspace --features drama-dossier/license-test-mode
cargo clippy --workspace --all-targets --features drama-dossier/license-test-mode -- -D warnings
cargo install tauri-cli --locked
cargo tauri dev --features license-test-mode
cargo tauri build --features license-test-mode   # 内测包；正式包用 --features official-release
```

**Linux 系统依赖**（与 CI 一致，缺了会在编译 `src-tauri` 时失败，而不是 lint 报错）：

```bash
sudo apt-get install -y libwebkit2gtk-4.1-dev libgtk-3-dev \
  libayatana-appindicator3-dev librsvg2-dev libsoup-3.0-dev ffmpeg
```

提交前可一键跑与 CI 等价的门槛，并建议启用仓库自带的 Git hooks：

```bash
scripts/check.sh            # 全量：fmt + clippy + test + JSON + node --check
scripts/install-hooks.sh    # 启用 .githooks（pre-commit 快检 / pre-push 全量）
```

前端是静态 HTML/CSS/JS，由 Tauri 直接打包；没有网页服务器和浏览器入口。`src/main.js` 只能在桌面 WebView 中调用 `window.__TAURI__.core.invoke`。

## 构建矩阵

| 平台 | Runner | 目标 | 产物 |
|---|---|---|---|
| Windows x64 | `windows-latest` | `x86_64-pc-windows-msvc` | NSIS `.exe` |
| macOS Apple Silicon | `macos-14` 或更新 | `aarch64-apple-darwin` | `.app` / `.dmg` |
| macOS Intel | `macos-15-intel` | `x86_64-apple-darwin` | `.app` / `.dmg` |

> macOS Intel 使用 `macos-15-intel` 标签：`macos-13` 镜像已于 2025 年 12 月退役，使用该标签的任务会一直排队而无法启动。`macos-15-intel` 是 GitHub Actions 最后一个 x86_64 macOS 镜像，支持至 2027 年秋季。

GitHub Actions 工作流会先运行 Rust 测试和 Clippy，再使用 Tauri Action 构建三个平台的安装包。

发布 tag 形如 `app-v0.2.1` 时，`bundle` 三个平台全部构建成功后，`release` job 会自动创建
GitHub Release（标记为 prerelease），并挂上 Windows NSIS `.exe`、两个 macOS `.dmg`
以及 `SHA256SUMS.txt`。任一平台构建失败则不会创建 Release；产物缺失时 `release` job
会直接失败，不会发出残缺的 Release。

`release` job 只在推 tag 时运行：手动触发（`workflow_dispatch`）只构建、不发布，
推 `master` 只跑质量门与测试。

## 代码签名

仓库默认产出未签名包：

- **Windows**：用户首次运行可能看到 SmartScreen 提示，需要选择「仍要运行」。正式分发建议购买 OV 或 EV Code Signing 证书。
- **macOS**：未签名应用首次打开需要在 Finder 中右键选择「打开」。正式分发建议加入 Apple Developer Program，配置 Developer ID 签名与公证。

不要把证书、密码、`TAURI_SIGNING_PRIVATE_KEY` 或公证凭据提交到仓库。所有密钥只应作为 GitHub Actions Secrets 或本机受保护环境变量。

## FFmpeg 打包边界

`dossier-core` 只调用系统中的 `ffprobe`，不重新分发 FFmpeg 二进制。这避免 FFmpeg/LGPL/GPL 许可证和体积问题。若未来需要便携版，应单独审查所选 FFmpeg 构建的许可证，并把路径写入应用设置。
