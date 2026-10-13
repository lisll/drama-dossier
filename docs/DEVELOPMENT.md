# 开发说明

## 模块边界

| 模块 | 责任 | 禁止事项 |
|---|---|---|
| `crates/dossier-core` | 领域模型、目录扫描、ffprobe 解析、检查规则、报审包生成 | 不依赖 Tauri/GUI，不做网络请求 |
| `src-tauri` | 桌面窗口、IPC 命令、文件夹选择、打包 | 不把业务规则写在命令层 |
| `src` | 桌面 WebView UI | 不绕过 Rust 命令访问文件系统 |

## IPC 命令

| 命令 | 输入 | 说明 |
|---|---|---|
| `pick_folder` | 无 | 显示系统文件夹选择器 |
| `scan_library` | `root` | 扫描根目录下所有剧目 |
| `analyze_drama` | `root`, `name`, `ffprobePath` | 探测规格并返回检查报告 |
| `generate_package` | 剧目与包选项 | 重新探测后生成材料包 |

Rust 命令使用 snake_case，Tauri 会把 JavaScript 的 camelCase 参数映射到对应 Rust 参数。

## 数据流

```text
素材根目录
  → scanner::scan_library
  → UI 剧目列表
  → probe::probe_episode（ffprobe）
  → checklist::run_checks
  → package::generate_report_package
  → 本地报审包 + manifest.json
```

生成报审包时会重新扫描和探测，避免使用过期的前端状态；没有成功探测规格的分集不会写入 CSV。

## 质量门槛

提交前运行：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-features
python3 -m json.tool src-tauri/tauri.conf.json
python3 -m json.tool src-tauri/capabilities/default.json
node --check src/main.js
```

新增领域规则必须有单元测试；处理不可信文件名或文件系统错误时优先返回可操作的中文错误信息。
