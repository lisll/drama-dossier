# 构建与发布

## 本地开发

```bash
cargo test --workspace --all-features
cargo clippy --workspace --all-targets -- -D warnings
cargo install tauri-cli --locked
cargo tauri dev
cargo tauri build
```

前端是静态 HTML/CSS/JS，由 Tauri 直接打包；没有网页服务器和浏览器入口。`src/main.js` 只能在桌面 WebView 中调用 `window.__TAURI__.core.invoke`。

## 构建矩阵

| 平台 | Runner | 目标 | 产物 |
|---|---|---|---|
| Windows x64 | `windows-latest` | `x86_64-pc-windows-msvc` | NSIS `.exe` |
| macOS Apple Silicon | `macos-14` 或更新 | `aarch64-apple-darwin` | `.app` / `.dmg` |
| macOS Intel | `macos-13` | `x86_64-apple-darwin` | `.app` / `.dmg` |

GitHub Actions 工作流会先运行 Rust 测试和 Clippy，再使用 Tauri Action 构建安装包。发布 tag 形如 `app-v0.1.0` 时会创建 GitHub Release。

## 代码签名

仓库默认产出未签名包：

- **Windows**：用户首次运行可能看到 SmartScreen 提示，需要选择「仍要运行」。正式分发建议购买 OV 或 EV Code Signing 证书。
- **macOS**：未签名应用首次打开需要在 Finder 中右键选择「打开」。正式分发建议加入 Apple Developer Program，配置 Developer ID 签名与公证。

不要把证书、密码、`TAURI_SIGNING_PRIVATE_KEY` 或公证凭据提交到仓库。所有密钥只应作为 GitHub Actions Secrets 或本机受保护环境变量。

## FFmpeg 打包边界

`dossier-core` 只调用系统中的 `ffprobe`，不重新分发 FFmpeg 二进制。这避免 FFmpeg/LGPL/GPL 许可证和体积问题。若未来需要便携版，应单独审查所选 FFmpeg 构建的许可证，并把路径写入应用设置。
