# DramaDossier · 剧备

![CI](https://github.com/lisll/drama-dossier/actions/workflows/desktop.yml/badge.svg)
![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS-blue)
![Rust](https://img.shields.io/badge/Rust-1.85%2B-orange)
![Tauri](https://img.shields.io/badge/Tauri-2-24C8D8)
![License](https://img.shields.io/badge/license-MIT-green)

> 短剧报审材料自动生成工具 · Automated submission dossier builder for short dramas

DramaDossier 是一个本地优先的桌面应用，用于把散落在磁盘上的剧集素材整理成结构化、可人工复核、可提交的报审材料包。项目聚焦“素材扫描 → 媒体探测 → 合规检查 → 材料包生成”的完整链路，不提供网页版，也不会上传用户素材。

## 目录

- [项目定位](#项目定位)
- [功能特性](#功能特性)
- [系统架构](#系统架构)
- [目录结构](#目录结构)
- [技术栈](#技术栈)
- [核心模块](#核心模块)
- [数据流](#数据流)
- [快速开始](#快速开始)
- [素材目录规范](#素材目录规范)
- [开发规范](#开发规范)
- [CI / CD](#ci--cd)
- [发布与部署](#发布与部署)
- [安全与隐私](#安全与隐私)
- [合规责任](#合规责任)
- [Roadmap](#roadmap)
- [常见问题](#常见问题)
- [贡献](#贡献)
- [License](#license)

## 项目定位

短剧报审通常需要整理分集清单、封面、简介、成本说明、权属声明等多类材料。这些工作高度重复、格式分散，也容易因文件遗漏导致反复返工。DramaDossier 把这一过程工程化：

- 按约定扫描本机剧集目录；
- 调用 `ffprobe` 读取分集规格；
- 按可解释规则生成材料检查报告；
- 输出编号清晰的报审包和本地完整性清单；
- 强制保留人工填写与确认的责任边界。

项目适合作为短剧内容制作工具链、Tauri 2 桌面应用、Rust 领域工程和本地媒体处理管线的学习参考。

## 功能特性

- **剧目扫描**：以“素材根目录 / 一部剧一个子目录”为约定，支持一层视频子目录。
- **集号识别**：支持 `第01集`、`E01`、`01.mp4` 等常见命名，并按集号排序。
- **媒体探测**：通过外部 `ffprobe` 获取时长、分辨率、编码、帧率和码率。
- **材料检查**：区分 `pass` / `warn` / `fail` / `pending`，避免把人工判断伪装成自动结论。
- **报审包生成**：输出剧集信息、封面海报、分集清单、成本表、权属声明和 `manifest.json`。
- **桌面应用**：基于 Tauri 2 构建 Windows NSIS 安装包与 macOS `.app` / `.dmg`。
- **隐私优先**：素材、报告和生成结果都保留在本地文件系统。
- **合规边界明确**：不伪造签名、印章、时间戳或权属证明。

## 产品界面

应用主流程分为三个区域：

1. **素材路径区**：选择或输入剧集素材根目录，执行扫描。
2. **剧目列表**：展示剧目名称、分集数量和素材体积。
3. **检查与生成区**：查看检查结果，填写真实成本信息，生成报审包。

界面只负责状态展示和触发 Rust 命令，业务规则全部保存在 `dossier-core` 中。

## 系统架构

```text
+------------------------------------------------------+
|                    Tauri 2 Desktop                   |
|                                                      |
|  +------------------------------------------------+  |
|  |              WebView UI (src/)                 |  |
|  |  HTML / CSS / JavaScript state and forms       |  |
|  +---------------------+--------------------------+  |
|                        | invoke()                   |
|  +---------------------v--------------------------+  |
|  |           Tauri Commands (src-tauri/)          |  |
|  |  scan_library / analyze_drama / generate       |  |
|  +---------------------+--------------------------+  |
|                        | typed structs              |
|  +---------------------v--------------------------+  |
|  |          Domain Core (crates/dossier-core)     |  |
|  |  scanner -> probe -> checklist -> package      |  |
|  +---------+---------------------+----------------+  |
|            |                     |                  |
+------------|---------------------|------------------+
             |                     |
             v                     v
     Local media files        ffprobe process
```

架构原则：

- **UI 不实现业务规则**：界面只渲染状态、收集表单并调用命令。
- **命令层不做规则**：Tauri command 负责参数适配、错误转换和进程桥接。
- **核心库保持纯净**：`dossier-core` 不依赖 GUI、HTTP 服务或网络能力。
- **外部能力显式声明**：`ffprobe` 不捆绑，由用户安装或指定路径。
- **错误可操作**：路径、权限、外部进程错误都应提示下一步如何处理。
- **本地优先**：除启动外部 `ffprobe` 进程外，不发起网络请求。

## 目录结构

```text
.
├── crates/
│   └── dossier-core/            # 纯 Rust 领域核心
│       └── src/
│           ├── scanner.rs       # 目录扫描与集号解析
│           ├── probe.rs         # ffprobe 调用与 JSON 解析
│           ├── checklist.rs     # 报审材料检查规则
│           ├── package.rs       # 报审包生成与 manifest
│           └── types.rs         # 共享领域模型
├── src/                         # Tauri WebView 前端
│   ├── index.html
│   ├── main.js
│   └── styles.css
├── src-tauri/                   # Tauri 桌面壳
│   ├── src/lib.rs               # IPC commands 与应用入口
│   ├── tauri.conf.json          # 窗口、bundle 和打包配置
│   ├── capabilities/            # Tauri capability 配置
│   └── icons/                   # Windows / macOS 图标
├── docs/
│   ├── BUILD.md                 # 构建与发布说明
│   ├── DEVELOPMENT.md           # 开发规范与模块边界
│   └── MATERIAL_SPEC.md         # 素材与报审包数据规格
└── .github/workflows/           # CI 和桌面包构建
```

## 技术栈

| 层级 | 技术 | 说明 |
|---|---|---|
| 桌面壳 | Tauri 2 | 窗口、WebView、IPC、打包和 capability 管理 |
| 后端核心 | Rust / edition 2021 | 领域逻辑、文件处理和进程调用 |
| 序列化 | Serde / serde_json | 领域模型与前端 JSON 协议 |
| 哈希 | SHA-2 | 生成分集和材料包的本地校验值 |
| 文件遍历 | walkdir | 读取剧目目录结构 |
| 前端 | HTML / CSS / JavaScript | 无构建框架的轻量 WebView UI |
| 媒体探测 | FFmpeg `ffprobe` | 外部依赖，不随应用重新分发 |
| CI/CD | GitHub Actions | 多平台质量检查和安装包构建 |
| 打包 | NSIS / DMG | Windows 与 macOS 安装包 |

## 核心模块

### `scanner` — 素材扫描

- 扫描用户选择的素材根目录；
- 每个非隐藏一级子目录识别为一部剧；
- 读取剧目目录及一层子目录中的视频、封面和简介；
- 从文件名解析集号；
- 排序分集并返回 `Drama` 模型。

支持的视频扩展名：

```text
mp4, mov, mkv, flv, ts, m4v, webm
```

封面识别关键字：

```text
封面, cover, poster
```

简介识别关键字：

```text
简介, intro, synopsis
```

集号解析优先级：

1. `第12集` / `第12话` / `第12回`
2. 词边界形式的 `E12` / `e03`
3. 文件名主干中的独立数字，例如 `show_12`

### `probe` — ffprobe 探测

- 不捆绑 FFmpeg 二进制；
- 默认从 `PATH` 查找 `ffprobe`；
- 支持用户指定 `ffprobe` 完整路径；
- 使用 `--show_format --show_streams --print_format json` 获取数据；
- 将结果解析为 `EpisodeSpec`；
- 探测逻辑与进程调用分离，便于使用固定 JSON 样本测试。

提取字段包括：

- 时长；
- 宽高；
- 视频编码；
- 音频编码；
- 平均帧率；
- 总码率。

### `checklist` — 材料检查

检查器不修改文件，只生成可解释的检查报告。

| 检查项 | 默认规则 | 级别 |
|---|---:|---|
| 分集数量 | 0 集失败；少于 4 集警告 | `fail` / `warn` |
| 封面 | 缺失失败 | `fail` |
| 简介 | 缺失失败；少于 30 字警告 | `fail` / `warn` |
| 单集时长 | 31–480 秒之外警告 | `warn` |
| 码率 | 超过 8 Mbps 建议规范化 | `warn` |
| 画幅 | 高大于宽为竖屏建议 | `warn` |

`ready = true` 表示没有 `fail` 项；`warn` 仍需要人工确认。

### `package` — 报审包生成

默认输出到剧目目录下的 `报审包/`：

```text
报审包/
├── 01_剧集信息/
│   ├── 剧名.txt
│   └── 简介.txt
├── 02_封面海报/
│   └── 封面.jpg
├── 03_分集清单/
│   └── 分集清单.csv
├── 04_成本配置表/
│   └── 成本配置表.md
├── 05_权属声明/
│   └── 内容权属声明.txt
├── 06_分集视频/          # 仅选择复制时生成
│   └── 第01集.mp4
└── manifest.json
```

`manifest.json` 记录：

- schema 版本；
- 剧目名称；
- UTC 生成时间；
- 是否使用 AI 辅助生成；
- 是否复制分集视频；
- 每个文件的相对路径、大小和 SHA-256。

manifest 是本地完整性清单，不是官方审核回执，也不能替代签名、公章或可信时间戳。

### `src-tauri` — Tauri IPC 桥

| Command | 作用 |
|---|---|
| `pick_folder` | 打开系统文件夹选择器 |
| `scan_library` | 扫描素材根目录下的所有剧目 |
| `analyze_drama` | 探测指定剧目并返回检查报告 |
| `generate_package` | 重新扫描、探测并生成报审包 |

Rust command 使用 snake_case，JavaScript 调用时使用 camelCase 参数。命令层返回中文可读错误，前端只做展示和用户输入适配。

### `src` — WebView UI

- 无 React、Vue 或 Vite 构建链，降低桌面示例复杂度；
- 使用原生 HTML/CSS/JavaScript；
- 通过 `window.__TAURI__.core.invoke` 调用命令；
- 页面 CSP 与 Tauri security 配置保持本地资源边界。

## 数据流

```text
素材根目录
  ↓ scanner::scan_library
剧目列表
  ↓ 用户选择剧目
scanner::scan_drama_dir
  ↓ EpisodeFile[]
probe::probe_episode
  ↓ EpisodeSpec[]
checklist::run_checks
  ↓ ChecklistReport
用户确认并填写真实数据
  ↓
package::generate_report_package
  ↓
报审包 + manifest.json
```

生成报审包时会重新扫描和探测，避免使用过期前端状态。没有成功探测规格的分集不会写入分集清单。

## 快速开始

### 1. 安装依赖

通用依赖：

- Rust 1.85 或更新版本；
- Tauri CLI；
- 包含 `ffprobe` 的 FFmpeg。

macOS：

```bash
brew install rust ffmpeg
cargo install tauri-cli --locked
```

Windows：

1. 安装 [Rust](https://rustup.rs/)；
2. 安装 Visual Studio Build Tools 与 Windows SDK；
3. 安装 WebView2 Runtime；
4. 安装 [FFmpeg](https://ffmpeg.org/download.html)；
5. 安装 Tauri CLI：

```powershell
cargo install tauri-cli --locked
```

### 2. 获取源码

```bash
git clone https://github.com/lisll/drama-dossier.git
cd drama-dossier
```

### 3. 开发运行

```bash
cargo test --workspace --all-features
cargo tauri dev
```

### 4. 构建安装包

```bash
cargo tauri build
```

产物位置：

- Windows：`target/release/bundle/nsis/*.exe`
- macOS：`target/release/bundle/macos/*.app`、`target/release/bundle/dmg/*.dmg`

## 素材目录规范

推荐的素材组织方式：

```text
素材根目录/
└── 示例短剧/
    ├── 第01集.mp4
    ├── 第02集.mp4
    ├── 第03集.mp4
    ├── 第04集.mp4
    ├── 封面.jpg
    └── 简介.txt
```

推荐命名：

| 类型 | 推荐命名 |
|---|---|
| 分集 | `第01集.mp4` |
| 封面 | `封面.jpg` |
| 简介 | `简介.txt` |

简介建议不少于 30 字，且说明故事主题、题材、主要人物和受众定位。详细规则见 [`docs/MATERIAL_SPEC.md`](docs/MATERIAL_SPEC.md)。

## 开发规范

### 模块边界

| 模块 | 允许 | 禁止 |
|---|---|---|
| `dossier-core` | 领域模型、扫描、探测、检查、包生成 | 依赖 GUI、HTTP 服务或全局可变状态 |
| `src-tauri` | 参数适配、IPC、文件夹选择、进程边界 | 把业务规则散落在 command 中 |
| `src` | 表单、状态、渲染、用户提示 | 直接访问文件系统或复制核心逻辑 |

### 质量门槛

提交或合并前必须通过：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-features
python3 -m json.tool src-tauri/tauri.conf.json
python3 -m json.tool src-tauri/capabilities/default.json
node --check src/main.js
```

推荐本地执行：

```bash
cargo fmt
cargo clippy --fix --allow-dirty --allow-staged
cargo test
cargo tauri build
```

### 代码风格

- 领域模型使用 Serde 的 camelCase JSON 表示；
- 错误信息应说明原因和下一步处理方式；
- 文件路径和用户输入必须做边界处理；
- 核心规则必须配单元测试；
- 不在库代码中使用 `unwrap()` 处理可失败 I/O；
- 不引入未经讨论的网络能力或遥测。

### Git 工作流

- `master` 保持可构建状态；
- 功能分支命名：`feature/<topic>`；
- 缺陷修复命名：`fix/<topic>`；
- 文档修改命名：`docs/<topic>`；
- 发布 tag 命名：`app-v<semver>`，例如 `app-v0.1.0`。

提交信息使用 Conventional Commits：

```text
feat: add transcode suggestion
fix: preserve episode order when numbers are duplicated
docs: expand packaging checklist
refactor: isolate ffprobe parser
test: add malformed media fixture
ci: add macOS Intel bundle job
```

### Pull Request 要求

PR 需要说明：

1. 变更目的；
2. 用户可见行为变化；
3. 数据或报审包结构是否变化；
4. 已执行的测试；
5. 对 Windows / macOS 的兼容性影响。

涉及以下内容时必须额外说明迁移方案：

- `manifest.json` schema；
- 分集清单 CSV 字段；
- IPC command 参数或返回值；
- 默认检查规则；
- 报审包目录结构。

## CI / CD

GitHub Actions 工作流：

- 在 Linux、Windows、macOS 上执行格式检查、Clippy 和测试；
- 在 tag `app-v*` 或手动触发时构建桌面包；
- Windows x64 构建 NSIS 安装包；
- macOS Apple Silicon 构建 `.app` / `.dmg`；
- macOS Intel 构建 `.app` / `.dmg`。

构建配置见 [`.github/workflows/desktop.yml`](.github/workflows/desktop.yml)。

## 发布与部署

### 版本规则

项目使用语义化版本：

- `MAJOR`：报审包结构、manifest schema 或 IPC 协议发生不兼容变化；
- `MINOR`：新增能力或检查规则，保持兼容；
- `PATCH`：缺陷修复、文档和构建优化。

### 发布检查

发布前确认：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-features
cargo tauri build
```

同时人工验证：

1. Windows 安装、启动、卸载；
2. macOS 安装、启动、拖入 Applications；
3. 空目录、无权限目录和 ffprobe 缺失时的提示；
4. 真实剧目扫描与检查结果；
5. 生成包能否打开且 manifest 哈希正确。

### GitHub Release

1. 更新 `Cargo.toml`、`src-tauri/Cargo.toml` 和 `src-tauri/tauri.conf.json` 中的版本；
2. 更新文档中的平台要求或产物说明；
3. 创建 PR 并合入 `master`；
4. 打 tag：

```bash
git tag app-v0.1.0
git push origin app-v0.1.0
```

5. 等待 CI 产物构建完成；
6. 在 GitHub Release 中附上安装包和变更说明。

### Windows 分发

未签名安装包首次运行可能触发 SmartScreen。正式分发建议：

- 使用 OV 或 EV Code Signing 证书；
- 记录证书私钥的保管方式；
- 不将签名凭据写入仓库或 CI 日志。

### macOS 分发

未签名应用首次打开需要在 Finder 中右键选择“打开”。正式分发建议：

- 加入 Apple Developer Program；
- 使用 Developer ID Application 证书签名；
- 执行 notarization；
- 在文档中说明 Gatekeeper 行为。

### FFmpeg 分发边界

DramaDossier 通过外部 `ffprobe` 读取媒体信息，不重新分发 FFmpeg。这样可以：

- 减小应用体积；
- 避免 FFmpeg / LGPL / GPL 组合的许可证复杂度；
- 允许用户使用系统或自有 FFmpeg 构建。

若未来提供便携包，需要单独审查对应 FFmpeg 构建的许可证。

## 安全与隐私

- 应用不上传素材；
- 不收集用户剧目、联系方式或成本数据；
- 不包含遥测；
- 不执行下载的可执行文件；
- 只读取用户显式选择的目录及其子目录；
- 生成文件只写入用户确认或约定的输出位置。

报告安全问题时请勿直接创建公开 issue。可以通过仓库维护者的安全联系方式私下报告。

## 合规责任

DramaDossier 只做真实材料的整理、格式化和完整性提示：

- 不绕过版权保护；
- 不识别或清洗盗版素材；
- 不去除水印；
- 不伪造公章、签名或可信时间戳；
- 不生成虚假投资或成本数据；
- 不代替用户确认著作权或授权状态。

使用者必须确保提交材料的真实性、合法性和授权完整性。AI 辅助生成的剧名、简介、封面等内容必须如实标识并经过人工审核。

## Roadmap

- [x] Rust 领域核心
- [x] Tauri 2 桌面壳
- [x] Windows / macOS 构建目标
- [x] GitHub Actions 质量与打包流水线
- [ ] 转码建议与批量规范化
- [ ] 剪映草稿导入 / 导出
- [ ] 平台材料要求知识库
- [ ] 检查规则自定义配置
- [ ] 报审包差异对比
- [ ] macOS 签名与公证示例
- [ ] 应用内更新通道

## 常见问题

### 为什么需要安装 FFmpeg？

DramaDossier 使用 `ffprobe` 读取媒体元数据。应用本身不解析视频容器，也不捆绑 FFmpeg 二进制。

### 为什么部分项目显示警告但可以生成？

`warn` 表示需要人工判断，例如集数偏少、码率偏高或横屏画幅。应用不会替用户隐藏这些风险。

### 为什么生成包时会重新探测？

前端状态可能已经过期。重新扫描和探测可以减少使用旧规格或旧路径生成清单的风险。

### 为什么不提供网页版？

本项目需要稳定访问本地文件系统、运行外部 `ffprobe`，并把输出写入用户选择的位置。桌面边界比浏览器更合适，也更符合本地优先的数据处理模型。

## 贡献

欢迎 issue 和 pull request。提交前请：

1. 阅读 [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md)；
2. 保持模块边界清晰；
3. 补充必要测试；
4. 运行完整质量门槛；
5. 说明对数据和兼容性的影响。

文档类 PR 也欢迎，包括平台差异、报审实践、构建排错和术语校对。

## License

本项目基于 [MIT License](LICENSE) 开源。
