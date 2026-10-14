# 开发说明

## 模块边界

| 模块 | 责任 | 禁止事项 |
|---|---|---|
| `crates/dossier-core` | 领域模型、目录扫描、ffprobe 解析、检查规则、报审包生成 | 不依赖 Tauri/GUI，不做网络请求 |
| `crates/dossier-inpaint` | 硬字幕与角标擦除管线（学习原型）：帧 IO、掩码、检测/修复后端编排 | 不接入桌面 UI；不做 DRM 绕过，不处理他人作品的版权标识 |
| `tools/model_worker` | 模型侧 worker：DBNet 检测、E2FGVI 修复 | 不决定「改哪些像素」；不碰整帧回贴 |
| `src-tauri` | 桌面窗口、IPC 命令、文件夹选择、打包 | 不把业务规则写在命令层 |
| `src` | 桌面 WebView UI | 不绕过 Rust 命令访问文件系统 |

`dossier-inpaint` 与报审材料链路相互独立，是命令行运行的学习原型，接入方式与
交换机协议见 [SUBTITLE_REMOVAL.md](SUBTITLE_REMOVAL.md)；
角标 / 水印场景的 ROI 取法、掩码规则与编码策略见
[WATERMARK_REMOVAL.md](WATERMARK_REMOVAL.md)。

## IPC 命令

| 命令 | 输入 | 说明 |
|---|---|---|
| `pick_folder` | 无 | 显示系统文件夹选择器 |
| `scan_library` | `root` | 扫描根目录下所有剧目 |
| `load_app_settings` | 无 | 读取本机设置文件，返回设置、告警与文件路径 |
| `save_app_settings` | `settings` | 校验检查规则并写入本机设置文件 |
| `analyze_drama` | `root`, `name`, `ffprobePath`, `rules` | 探测规格并返回检查报告 |
| `generate_package` | 剧目与包选项，含 `rules` | 重新探测后生成材料包 |

Rust 命令使用 snake_case，Tauri 会把 JavaScript 的 camelCase 参数映射到对应 Rust 参数。

`rules` 为可选的 `ChecklistRules`；缺省时使用内置默认阈值。设置持久化由 `dossier-core` 的 `settings` 模块负责：文件缺失或损坏时回退默认值并带回告警，写入采用临时文件替换。

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

`src-tauri` 需要 Tauri 的系统依赖（Linux 上要求 `glib >= 2.70`），在不满足的机器上
`cargo test --workspace` 会失败。只想验证核心与擦除管线时可以按 crate 跑：

```bash
cargo test -p dossier-core
cargo test -p dossier-inpaint
cargo clippy -p dossier-inpaint --all-targets -- -D warnings
cd tools/model_worker && python3 -m unittest discover -s tests -v
```

擦除管线的一键体验脚本（造测试素材 → 跑链路）：`tools/prototype_demo.sh`。
