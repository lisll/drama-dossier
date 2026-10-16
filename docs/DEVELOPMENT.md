# 开发说明

## 模块边界

| 模块 | 责任 | 禁止事项 |
|---|---|---|
| `crates/dossier-core` | 领域模型、目录扫描、ffprobe 解析、检查规则、报审包文档生成与组装（`render_documents` / `assemble_package`）、验签、转码建议、报审包差异、平台档案、剪映草稿 | 不依赖 Tauri/GUI，不做网络请求（与服务端通信在 `src-tauri/src/account/`）；`transcode` 只产出建议，不执行转码 |
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
| `generate_package` | 剧目与包选项，含 `rules` | 重新探测、逐集算哈希后调服务端生成文档，再验签组装报审包（需要联网与有效使用权） |
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
  → 逐集算 SHA-256（视频不上传）
  → POST /packages（服务端调 render_documents、渲染 PDF、盖章、签清单）
  → package::assemble_package（验签 + 重算哈希）
  → 报审包 + manifest.json + manifest.sig
```

生成报审包时会重新扫描和探测，避免使用过期的前端状态；没有成功探测规格的分集不会写入 CSV。
渲染文字内容只写在 Rust 的 `render_documents` 里一份，服务端只把它画成 PDF，不自己拼文字。

## 质量门槛

提交前运行：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-features
python3 -m json.tool src-tauri/tauri.conf.json
python3 -m json.tool src-tauri/capabilities/default.json
for file in src/main.js src/logic.js src/theme.js; do node --check "$file"; done   # 只给一个文件才会全检查
node --test 'tests/frontend/*.test.js'   # 界面纯函数（src/logic.js）的单元测试；不能只写目录
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

## 桌面端账号与授权（本地联调）

桌面端连 `cloud/` 里的账号与授权服务（`drama_cloud.commerce`，接口见 `cloud/docs/COMMERCE.md`）。
各部分的位置：

- `crates/dossier-license`：授权凭证验签、报审包清单与处理配方验签（共用 `envelope` 的用途前缀隔离）、
  防时钟倒退、使用权判定、机器码。纯逻辑，不联网；
- `src-tauri/src/account/`：联网（`api.rs`）、钥匙串（`store.rs`）、会话与自动刷新（`session.rs`）、
  功能闸门（`gate.rs`，按命令名放行或拒绝，没登记的命令按「本地功能」处理）、命令（`commands.rs`）；
- `src/logic.js`：界面用到的纯函数（卡密格式、倒计时、错误文案、引导、近 7 天统计），`node --test` 直接测。

### 1. 拉起服务端

按 `cloud/docs/COMMERCE.md` 第 1 节启动 PostgreSQL、Mailpit 和服务（`serve --port 8100`）。
`keygen` 打印的 `public_key` 下一步要用。

本地联调还要给服务端准备两样东西，缺了服务会拒绝启动或在生成时报错：

- **`dossier-artifacts` 二进制**：`POST /packages` 用它把「生成文档」的纯函数调用起来
  （`render-package` 子命令，stdin 读 JSON、stdout 写 JSON，不碰磁盘）。

  ```bash
  cargo build -p dossier-core --bin dossier-artifacts
  ```

  服务端按固定顺序找它：`DRAMA_COMMERCE_ARTIFACTS_BIN` 指定的路径 → `PATH` →
  仓库的 `target/release/dossier-artifacts` → `target/debug/dossier-artifacts`
  （实现见 `cloud/src/drama_cloud/toolchain.py` 的 `search_candidates` / `resolve_binary`）。
  所以刚 `cargo build` 完不用配就能找到；找不到时 `POST /packages` 返回
  503 `service_unavailable`「服务端缺少报审包生成工具」。
- **中文字体**：报审包的两份 PDF 要嵌入简体中文字体，Debian / Ubuntu 上装 `fonts-noto-cjk`：

  ```bash
  sudo apt install fonts-noto-cjk
  ```

  找不到字体时服务**拒绝启动**（不生成一份方块字的 PDF）。配置项是
  `DRAMA_COMMERCE_PDF_FONT`（.otf / .ttf / .ttc 文件路径，不设时按 `documents/pdf.py` 里的
  常见系统路径找 Noto Sans CJK SC）。

### 2. 编译时变量

| 变量 | 作用 |
| --- | --- |
| `DOSSIER_API_BASE` | 服务端地址，编译进程序。不设时是 `http://127.0.0.1:8100`。不是本机地址的必须用 `https://`。调试版运行时也能用同名环境变量覆盖，正式版不能 |
| `DOSSIER_LICENSE_PUBKEYS` | 受信的授权签名公钥（base64url，逗号分隔），追加在内置列表后面。现在内置列表是空的，**不设就验不过任何凭证**，登录后会停在「授权凭证无效」 |

```bash
export DOSSIER_LICENSE_PUBKEYS='<keygen 打印的 public_key>'
cargo tauri dev                       # 或 cargo build -p drama-dossier
```

- 换了公钥要重新编译（`option_env!` 在编译时读取）。
- 测试向量 `cloud/tests/commerce/license_vectors_v1.json` 里的两把公钥是公开种子算出来的，
  出现在受信列表里一律按「安装包配置有误」处理。

### 3. 联调要点

- 验证码在 Mailpit（<http://127.0.0.1:8025>）里看；卡密用 `drama-commerce cards generate` 生成。
- 登录状态存在系统钥匙串：Windows 凭据管理器、macOS 钥匙串；Linux 存在 `<应用数据目录>/credentials/`
  下权限 0600 的文件。服务名 `dev.dramadossier.app`，两条记录 `session`、`license`。退出登录、注销都会删掉这两条。
- 断网测试：停掉服务端即可。进入离线档后，本地功能照常可用，「生成报审包」会被拒绝。
- 「名额满了」要两台机器（机器码不同）登录同一个账号才能触发：同一台电脑重复绑定不占新名额。
- 机器码由系统编号加盐计算：克隆的虚拟机会被当成同一台电脑。

### 4. 端到端测试（真实 HTTP）

`src-tauri/src/account/e2e_tests.rs` 默认忽略，只在 Linux 上编译（Windows / macOS 上存储是系统钥匙串，测试不该往里写）。
两条测试：

- `register_redeem_kick_export_delete`：注册（验证码从 Mailpit 取）→ 兑换 → 第二台电脑名额满 → 踢掉第一台 →
  第一台刷新失败并退出 → 断网进离线档、往回调时间判倒退 → 导出 → 注销 → 再登录失败。
- `package_and_recipe_against_real_server`：注册 → 兑换 → 服务端生成报审包（只发校验值）→ 本机组装 →
  清单改一个字节、清单是别的电脑的、两集调换、提交后改了一集，组装都被拒且不留目录 →
  签发配方并本机验签 → 消费一次成功、第二次 409 → 注销。要先 `cargo build -p dossier-core --bin dossier-artifacts`。

每条测试兑换一张卡，所以要两张没用过的月卡：

```bash
# 按第 1 节拉起服务端（建议用一个临时库，跑完删掉），再生成两张月卡
export DOSSIER_E2E_BASE=http://127.0.0.1:8100
export DOSSIER_E2E_MAILPIT=http://127.0.0.1:8025
export DOSSIER_E2E_PUBKEY='<keygen 打印的 public_key>'
export DOSSIER_E2E_CARD='<cards.csv 里一张没用过的卡>'
export DOSSIER_E2E_PACKAGE_CARD='<cards.csv 里另一张没用过的卡>'
cargo test -p drama-dossier --lib e2e -- --ignored
```
```

「服务端停了」用指向没人监听的端口模拟，不用真的停服务；时钟倒退用注入的时钟模拟，不改系统时间。

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
