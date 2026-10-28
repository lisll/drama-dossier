# 开发说明

## 模块边界

| 模块 | 责任 | 禁止事项 |
|---|---|---|
| `crates/dossier-core` | 领域模型、目录扫描、ffprobe 解析、检查规则、报审包生成、转码建议、报审包差异、平台档案、剪映草稿 | 不依赖 Tauri/GUI，不做网络请求；`transcode` 只产出建议，不执行转码 |
| `crates/dossier-inpaint` | 硬字幕与角标擦除管线（学习原型）：帧 IO、掩码、检测/修复后端编排、结果自动校验、桌面入口的命令拼装 | 不做 DRM 绕过，不处理他人作品的版权标识；桌面端只能经 `erase.rs` 拼参数，不另写一套规则 |
| `tools/model_worker` | 模型侧 worker：DBNet 检测、E2FGVI / LaMa / ProPainter 修复（`--serve` 常驻） | 不决定「改哪些像素」；不碰整帧回贴 |
| `src-tauri` | 桌面窗口、IPC 命令、文件夹选择、打包 | 不把业务规则写在命令层（`erase.rs` / `dossier.rs` 只做参数适配） |
| `src` | 桌面 WebView UI | 不绕过 Rust 命令访问文件系统 |
| `cloud/` | 云端制作系统（学习原型）：配方、任务队列、媒体处理模块、控制面 API 与 Web 客户端 | 不引入 GPU 依赖；不在配方里执行任意命令（模块走白名单）；桌面壳方案见 `cloud/docs/PACKAGING_GUIDE.md` |

`dossier-inpaint` 与报审材料链路相互独立，是命令行运行的学习原型：区域/时间窗配置 →
逐帧时序掩码 → inpaint 修复 → **输出帧自动校验**（残留 / 闪烁 / 接缝 / 一致性 / SSIM）→
处理日志 + 确认流退出码（0 通过 / 3 需抽查 / 2 校验失败 / 1 执行错误）。
接入方式与交换机协议见 [SUBTITLE_REMOVAL.md](SUBTITLE_REMOVAL.md)；
角标 / 水印场景的 ROI 取法、掩码规则与编码策略见
[WATERMARK_REMOVAL.md](WATERMARK_REMOVAL.md)。

擦除管线的三条工程约定：

- **区域分组**：多标识默认走外接 ROI；`--region-group-area-ratio`（默认 2.0）按面积代价
  把离得远的标识拆成多个裁剪作业各自送模型（阈值 `<= 1.0` 等价于始终合并）。
- **常驻 worker**：`--persistent-worker` + `worker.py --serve` 让整段视频只启动一次后端进程；
  协议是 stdio 行协议（`{"workDir":…}` → `{"ok":true}`），像素仍走目录交换，
  所以单请求内存与一次性模式一致。**worker 的 stdout 只能出现协议行**，
  库的打印必须走 stderr（`worker.py` 用 `os.dup2` 保证）。
- **只测量、不越权裁决**：`dossier-inpaint` 负责测量与产出报告；
  `cloud` 的配方用 `video.erase_verify` 按自己的阈值重新裁决，调阈值不需要重跑擦除。
- **退出码语义唯一**：`0/2/3` 分别代表通过 / 校验失败 / 需人工抽查，所以命令行参数解析
  失败必须走 `1`（执行错误）——clap 默认也会用 2，两个发布物一旦不同步就会被误判成
  「画面没擦干净」。`main.rs` 里有一个单测专门钉住这条约定。
- **按能力拼参数**：云端调外部工具前先读一次 `--help`（按「路径 + 大小 + mtime」缓存），
  工具不认识的可选开关就不传，并写进处理日志。发布物版本不同步时降级而不是崩掉。

## IPC 命令

| 命令 | 输入 | 说明 |
|---|---|---|
| `pick_folder` | 无 | 显示系统文件夹选择器 |
| `scan_library` | `root` | 扫描根目录下所有剧目 |
| `load_app_settings` | 无 | 读取本机设置文件，返回设置、告警与文件路径 |
| `save_app_settings` | `settings` | 校验检查规则并写入本机设置文件 |
| `analyze_drama` | `root`, `name`, `ffprobePath`, `rules` | 探测规格并返回检查报告 |
| `generate_package` | 剧目与包选项，含 `rules` | 重新探测后生成材料包 |
| `pick_video` | 无 | 选择待擦除的视频文件 |
| `erase_status` | `toolPath` | 查询擦除工具是否可用 |
| `plan_erase` | `request`, `toolPath` | 只拼装命令供预览，不执行 |
| `run_erase` | `request`, `toolPath`, `timeoutSeconds` | 执行擦除并回传确认流结论 |
| `transcode_advice` | `root`, `name`, `target`, `ffprobePath` | 转码建议 + 批处理脚本（不执行） |
| `diff_report_packages` | `leftDir`, `rightDir` | 两份报审包清单差异 |
| `platform_profiles` | 无 | 内置平台档案（学习示例） |
| `platform_review` | `root`, `name`, `profileId`, `ffprobePath` | 逐集对照平台档案 |
| `export_draft` / `import_draft` | 剧目或草稿目录 | 剪映草稿导出 / 读回 |

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
cargo clippy --workspace --all-targets --features drama-dossier/license-test-mode -- -D warnings
cargo test --workspace --features drama-dossier/license-test-mode
python3 -m json.tool src-tauri/tauri.conf.json
python3 -m json.tool src-tauri/capabilities/default.json
node --check src/main.js
```

新增领域规则必须有单元测试；处理不可信文件名或文件系统错误时优先返回可操作的中文错误信息。

工作区里**不要写 doctest**。`tauri-build` 在 MSVC 下会往 `src-tauri` 的 `OUT_DIR` 写一个空壳
`msvcrt.lib`（为了让 app 静态链接 VC 运行时）并把它加进原生库搜索路径，而配套的
`/NODEFAULTLIB:msvcrt.lib` 只作用于 `src-tauri` 自己的产物；于是别的链接会先搜到这个空壳，
Windows CI 上直接报 `LNK4003: invalid library format` 加一连串
`unresolved external symbol memcpy`。要守住公开接口（例如 `resolve_product_dir` 的
re-export）就放进 `crates/*/tests/`——集成测试走普通 `cargo test` 链接，不受影响。

`src-tauri` 需要 Tauri 的系统依赖（Linux 上要求 `glib >= 2.70`），在不满足的机器上
`cargo test --workspace` 会失败。只想验证核心与擦除管线时可以按 crate 跑：

```bash
cargo test --offline -p dossier-core -p dossier-inpaint -p dossier-media -p dossier-runtime
                                 # core=报审链路+进度/检查点/管线版本；inpaint=擦除；
                                 # media=渲染/切点/变速/分离/ASR/OCR；runtime=清单/能力/更新/凭据
cargo clippy --offline -p dossier-core -p dossier-inpaint -p dossier-media -p dossier-runtime \
    --all-targets -- -D warnings
cd tools/model_worker && python3 -m unittest discover -s tests -v
```

`--offline` 是必须的：本机构建环境访问不到 crates.io，所有依赖都已经在本地 vendor 里；
新增第三方 crate 目前**装不上**，能用的是工作区里已有的（clap / serde / thiserror / chrono / sha2 等）
与 crate 之间的 `path` 依赖。

擦除管线的一键体验脚本（造测试素材 → 跑链路）：`tools/prototype_demo.sh`。

## 云端制作系统（`cloud/`）

与桌面报审链路相互独立，Python 3.9 + FastAPI，运行目录 `cloud/var/`（可整体删除）。

```bash
python3.9 -m venv .venv && ./.venv/bin/pip install -e cloud[dev]

# 测试（215 个：单元 / 模块 / 集成 / 接口）
cd cloud && PYTHONPATH=src ../.venv/bin/python -m pytest tests/ -q

# 端到端演示：造素材 → 4 条配方 → 门禁 → 取产物
./cloud/demo.sh
./cloud/demo.sh --http     # 另外演示 API + Web 客户端
```

模块边界与约定：

- `plan.py` 只做「配方 → 可执行计划」，不碰磁盘；
- `modules/*` 只往 `ctx.plan` 里追加内容，**只有 `media.encode` 物化文件**；
- 状态迁移只能经 `tasks/state.py` 的 `transition()`，API 层不直接写 `tasks.state`；
- 配方模块名是白名单字面量，参数模型 `extra="forbid"`，不存在「配方里塞命令」的路径；
- 新增模块必须同时给出：注册项（`requires`/`produces`/`capability`）、参数模型与单元测试。

文档索引：`cloud/docs/ARCHITECTURE.md`、`RECIPE_SCHEMA.md`、`PROCESSING_PIPELINE.md`、
`API_SPEC.md`、`DATABASE_SCHEMA.md`、`PACKAGING_GUIDE.md`、`TEST_PLAN.md`。
