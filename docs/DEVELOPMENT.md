# 开发说明

## 模块边界

| 模块 | 责任 | 禁止事项 |
|---|---|---|
| `crates/dossier-core` | 领域模型、目录扫描、ffprobe 解析、检查规则、报审包文档生成与组装（`render_documents` / `assemble_package`）、验签、转码建议、报审包差异、平台档案、剪映草稿；处理版本的模板与滤镜图（`version.rs`：`VersionTemplate` / `build_version_graph` / `build_cover_args`、画质统计解析、`版本说明.json` 与处理台账记录的结构） | 不依赖 Tauri/GUI，不做网络请求（与服务端通信在 `src-tauri/src/account/`）；`transcode` 只产出建议，不执行转码；`version.rs` 纯计算，不碰文件系统、不跑进程 |
| `crates/dossier-inpaint` | 硬字幕与角标擦除管线（学习原型）：帧 IO、掩码、检测/修复后端编排、结果自动校验、静止区域检测（`still.rs`）、桌面入口的命令拼装 | 不做 DRM 绕过，不处理他人作品的版权标识；桌面端只能经 `erase.rs` 拼参数，不另写一套规则 |
| `tools/model_worker` | 模型侧 worker：DBNet 检测、E2FGVI / LaMa / ProPainter 修复（`--serve` 常驻） | 不决定「改哪些像素」；不碰整帧回贴 |
| `src-tauri` | 桌面窗口、IPC 命令、文件夹选择、打包；处理版本的命令层与任务队列、编码执行层（`version.rs`：权属声明 / 模板 / 计划预览 / 开始生成与队列执行器；`jobs.rs`：持久化队列与状态机；`encode.rs`：统一编码参数、可中断子进程、硬件失败退软件）；区域擦除的桌面入口（`erase_batch.rs`：取帧 / 自动识别 / 对比预览 / 批量入队） | 不把业务规则写在命令层（`erase.rs` / `dossier.rs` / `erase_batch.rs` 只做参数适配）；队列不懂视频、执行器不懂队列状态机 |
| `src` | 桌面 WebView UI | 不绕过 Rust 命令访问文件系统 |
| `cloud/` | 云端制作系统（学习原型）：配方、任务队列、媒体处理模块、控制面 API 与 Web 客户端 | 不引入 GPU 依赖；不在配方里执行任意命令（模块走白名单）；桌面壳方案见 `cloud/docs/PACKAGING_GUIDE.md` |

`dossier-inpaint` 与报审材料链路相互独立，是命令行运行的学习原型：区域/时间窗配置 →
逐帧时序掩码 → inpaint 修复 → **输出帧自动校验**（残留 / 闪烁 / 接缝 / 一致性 / SSIM）→
处理日志 + 确认流退出码（0 通过 / 3 需抽查 / 2 校验失败 / 1 执行错误）。
区域怎么来有两条路：人工给 `--roi`，或者先跑 `--detect-only` 由 `still.rs` 找出「位置固定、
内容不变」的区域（四条判据 + 默认阈值见该文件头注释），结果当作 `--roi` 交给同一条管线，
擦除主流程一行都不用改。
接入方式与交换机协议见 [SUBTITLE_REMOVAL.md](SUBTITLE_REMOVAL.md)；
角标 / 水印场景的 ROI 取法、自动识别的判据与编码策略见
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
| `video_frame` | `path`, `seconds`, `maxWidth` | 取某一秒的画面，等比缩到 `maxWidth`（默认 720、宽高取偶数）后返回 JPEG 的 data URL |
| `detect_still_regions` | `path`, `samples`, `toolPath` | 调 `dossier-inpaint --detect-only` 找「位置固定、内容不变」的候选区域（`samples` 夹到 2–600），只做检测、不产视频 |
| `erase_preview` | `input`, `seconds`, `startSec`, `durationSec`, `regions`, `toolPath` | 只擦一小段（默认 `seconds - 2` 起、4 秒）并取同一时刻的帧，返回擦除后的 JPEG data URL 供左右对比 |
| `erase_batch_start` | `request` | 门槛（权利确认 / 至少一个区域 / 至少一集）→ 逐集算哈希 → 台账留痕 → 入队，返回 `JobView` |
| `pick_scan_file` | 无 | 选已盖章的扫描件（`png` / `jpg` / `jpeg` / `pdf`，单份 ≤ 5 MB） |
| `transcode_advice` | `root`, `name`, `target`, `ffprobePath` | 转码建议 + 批处理脚本（不执行） |
| `diff_report_packages` | `leftDir`, `rightDir` | 两份报审包清单差异 |
| `platform_profiles` | 无 | 内置平台档案（学习示例） |
| `platform_review` | `root`, `name`, `profileId`, `ffprobePath` | 逐集对照平台档案 |
| `export_draft` / `import_draft` | 剧目或草稿目录 | 剪映草稿导出 / 读回 |
| `rights_status` | `root`, `name` | 读权属声明并重算文件哈希：是否已声明、是否需要重新声明 |
| `rights_declare` | `root`, `name`, `file` | 声明权属材料（只记路径与 SHA-256，不核验真伪），写 `.dossier/rights.json` 并记处理台账 |
| `pick_rights_file` | 无 | 选权属材料文件（`pdf` / `jpg` / `jpeg` / `png`） |
| `pick_version_asset` | `kind` | 选片头片尾视频（`video`）、背景音乐（`audio`）或中文字体（`font`） |
| `version_template_load` | `root`, `name` | 读剧目目录的 `版本模板.json`；文件不存在时返回默认模板 |
| `version_template_save` | `root`, `name`, `template` | 校验并写入 `版本模板.json` |
| `version_plan` | `root`, `name`, `strength`, `outputRoot` | 门槛检查 + 逐集哈希，返回任务哈希、输出目录、每集规格、处理步骤与待解决问题；不联网、不扣次数 |
| `version_start` | `root`, `name`, `strength`, `outputRoot` | 申请配方并本机验签 → 服务端消费 → 入队；有问题时直接拒绝 |
| `job_list` | 无 | 列任务（新到旧） |
| `job_pause` | `id` | 暂停：排队中立刻停，运行中做完当前这一集再停 |
| `job_resume` | `id` | 继续：已暂停 / 已终止 / 失败的任务重新排队，核对已完成产物后跳过对得上的集 |
| `job_terminate` | `id` | 终止：杀掉正在跑的子进程并删掉这一集的半成品 |
| `client_config` | 无 | 拉公开的客户端配置（公告 / 功能开关 / 版本策略）。**不需要登录**；失败时返回 `{unavailable: true, reason}` 而不是抛错，因为前端有安全默认值（空公告 / 开关全开 / 不强制更新） |
| `report_client_error` | `code`, `location` | 上报一条诊断信息。**由前端决定要不要报**（设置里的开关默认关）；这里用 `sanitize_short` 只保留字母数字、下划线与短横线并截断到 64 字符，失败不打扰用户 |

Rust 命令使用 snake_case，Tauri 会把 JavaScript 的 camelCase 参数映射到对应 Rust 参数。

**擦除任务走任务队列，`kind = erase`**：`erase_batch_start` 只做门槛检查与入队，
真正逐集执行由 `erase_batch::EraseRunner` 注册到 `JobQueue` 的 `erase` 执行器完成，
所以暂停、终止、续跑与其他任务共用同一套状态机。

`rules` 为可选的 `ChecklistRules`；缺省时使用内置默认阈值。设置持久化由 `dossier-core` 的 `settings` 模块负责：文件缺失或损坏时回退默认值并带回告警，写入采用临时文件替换。

处理版本还会推两个事件（前端监听，不是命令）：

- `job-progress`：任务状态变化时推一份 `JobView`（状态、进度、每集状态与说明），前端按 id 合并进列表；
- `task-progress`：算任务哈希时的进度（`task: "version"`，带已完成 / 总数与当前文件名），
  大文件要算几十秒，靠它撑住进度条。

功能闸门（`account/gate.rs`）：`version_start` 和 `generate_package` 一样按「需要有效使用权」放行；
`job_list` / `job_pause` / `job_terminate` 归为不需授权——授权到期时也要能把正在跑的任务停下来；
其余处理版本命令按「本地功能」处理。

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

处理版本（`src-tauri/src/version.rs` 文件头注释的 5 步）：

```text
剧目目录（含分集）
  → 门槛：权属材料已声明且文件没变；这部剧还没有处理版本目录
  → 算任务哈希（分集文件 + 模板 + 权属材料 + 档位）
  → POST /version-recipes 申请配方 → 本机验签（签名 / 机器码 / 任务 / 有效期）
  → POST /version-recipes/{id}/consume 消费（服务端记账，只能一次），配方原文存进任务目录 recipe.token
  → 任务队列逐集处理：一次 ffmpeg 调用完成两层处理、编码、画质检测
  → 全部完成后写 版本说明.json、封面.jpg 和台账 processing.jsonl
```

续跑走的是另一条路：不再找服务端，用本机保存的配方原文重新验签名、机器码和任务哈希
（`dossier-license::version_task::verify_consumed`），**不再验有效期**——配方已经在服务端消费过，
再限时间没有意义；参数和种子固定在配方里，所以反复续跑产出的文件和第一次完全相同。
执行器每集开工前还会重算一次源文件哈希，避免「申请配方时用 A、处理时换成 B」。

区域擦除（`src-tauri/src/erase_batch.rs` + `crates/dossier-inpaint/src/still.rs`）：

```text
当前剧第 1 集的某一秒
  → video_frame：ffmpeg 取一帧，等比缩到 720 宽，返回 JPEG data URL（坐标仍按原始分辨率保存）
  → detect_still_regions：dossier-inpaint --detect-only --report-json
       取样 min(采样帧数, 总帧数) 帧，逐像素算帧间方差 / 极差 / 边缘梯度，
       聚成连通块后交给 stable_rois 合并，输出候选框（带 score）
  → 人在画布上确认：新建 / 移动 / 缩放，左右对比（erase_preview 只擦 4 秒再取同一时刻的帧）
  → erase_batch_start：勾选权利确认 → 逐集算 SHA-256 → 写入台账 processing.jsonl → 入队（kind = erase）
  → 队列执行器 EraseRunner 逐集调 dossier-inpaint（每集开工前重算源文件哈希）
  → 每集 <擦除输出>/<分集主名>.mp4 + results/<集名>.json
  → 收尾写 <擦除输出>/擦除报告.json，并把「擦除完成」再记一次台账
```

`--detect-only` 是**预定位**而不是检测器：它不写视频、不跑修复、不做校验，退出码恒为 0
（没找到候选也算成功，`candidates` 是空数组）；给了 `--output` 会直接报错，避免被误解成「顺便也擦一遍」。

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
处理版本的滤镜图（`dossier-core/version.rs`）、队列状态机（`jobs.rs`）、编码参数（`encode.rs`）
和界面侧的纯函数（`src/logic.js` 里的任务状态与按钮可用性）都各自有单元测试。

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
| `DOSSIER_API_BASE` | 账号服务（登录、授权、积分、配方）地址，编译进程序。不设时是 `http://127.0.0.1:8100`。不是本机地址的必须用 `https://`。调试版运行时也能用同名环境变量覆盖，正式版不能。CI 打包从仓库变量（Variables，不是 Secrets）`DOSSIER_API_BASE` 注入 |
| `DOSSIER_MEDIA_BASE` | 媒体控制面（分片上传、服务端任务、产物下载）地址，规则与 `DOSSIER_API_BASE` 一致。**不设时与它相同**：部署后两个服务经 Caddy 共用一个 https 入口，什么都不用配；本地开发两个服务各占一个端口（8100 / 8000）时设上它，桌面端的服务端处理模式才连得通。调试版运行时同样可用环境变量覆盖 |
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

与桌面报审链路相互独立，Python + FastAPI。**M5b 起媒体控制面用 PostgreSQL**（此前是 `cloud/var/` 下的 SQLite 文件）。

```bash
python3.9 -m venv .venv && ./.venv/bin/pip install -e cloud[dev]

# 需要真 PostgreSQL：媒体控制面与 commerce 各用一套库（两个服务各有自己的表）
export COMMERCE_TEST_DSN='postgresql://drama:drama-dev-only@127.0.0.1:5432/postgres'

# 测试（单元 / 模块 / 集成 / 接口 / 两个服务的内部接口）
cd cloud && PYTHONPATH=src ../.venv/bin/python -m pytest tests/ -q

# 端到端演示：造素材 → 4 条配方 → 门禁 → 取产物
./cloud/demo.sh
./cloud/demo.sh --http     # 另外演示 API + Web 客户端

# 两个端到端脚本（真进程 + 真 HTTP + 真 PostgreSQL，库名随机、跑完自删）
./.venv/bin/python cloud/tests/e2e_m5a.py           # 积分与支付
./.venv/bin/python cloud/tests/e2e_m5b.py           # 服务端处理（上传续传 / 扣费 / 隔离 / 到期删除）
./.venv/bin/python cloud/tests/e2e_m5b_identity.py  # 一套登录进两个服务
./.venv/bin/python cloud/tests/e2e_m7.py            # 管理后台 + 客户端配置 + 错误上报（M7）
```

`cloud/tests/e2e_m7.py` 需要本机已跑着 `cloud/compose.dev.yaml` 的 PostgreSQL
（默认 `postgresql://drama:drama-dev-only@127.0.0.1:5432/postgres`，脚本里写死了这个管理员 DSN）。
它建一个随机库、生成临时私钥、跑迁移、用 CLI 建管理员、起服务，然后逐步断言：
管理员登录 → 默认配置不拦人 → 改版本策略 → 发公告 → 关一个开关 → 用户端公开配置跟着变
→ 公开配置里没有用户数据 → 普通用户令牌进不了管理接口 → 错误上报（含敏感字段被拒、同一错误只累加计数）
→ 停用管理员后会话立刻失效。跑完删掉临时库，不动既有数据。

### 媒体控制面的环境变量（M5b）

| 变量 | 说明 |
| --- | --- |
| `DRAMA_CLOUD_DSN` | 媒体控制面自己的 PostgreSQL 连接串（不是 commerce 那个库） |
| `DRAMA_CLOUD_COMMERCE_URL` | commerce 的地址，例如 `http://127.0.0.1:8100`。**不配时任何需要登录的请求返回 503**，不会退回不鉴权 |
| `DRAMA_CLOUD_INTERNAL_SECRET` | 与 commerce 的 `DRAMA_COMMERCE_INTERNAL_SECRET` **必须相同**，用于调用内部接口 |
| `DRAMA_CLOUD_ASSET_RETENTION_DAYS` | 素材保留天数，默认 30；0 表示不自动清理 |
| `DRAMA_CLOUD_UPLOAD_RETENTION_DAYS` | 放弃的分片上传保留天数，默认 7 |
| `DRAMA_CLOUD_ARTIFACT_RETENTION_DAYS` | 产物保留天数，默认 0（不清理） |
| `DRAMA_CLOUD_REQUIRE_TLS` | 置 1 时拒绝以明文启动；取值写错（如 `ture`）会直接报错，不静默当成「没要求」 |

### 常用命令（M5b 新增）

```bash
drama-cloud initdb --admin you@example.com    # 把某个邮箱加入管理员白名单（不再是「创建用户并打印令牌」）
drama-cloud serve --tls-cert cert.pem --tls-key key.pem   # HTTPS；只给一个会启动失败
drama-cloud cleanup    # 过期产物 + 过期素材（真删文件、留元数据）+ 放弃的分片上传 + 回收过期租约
```

### M7 新增：管理后台与运营数据

#### 新增的环境变量（commerce 侧）

| 变量 | 说明 |
| --- | --- |
| `DRAMA_COMMERCE_LICENSE_KEY_FILE_SECONDARY` | 次签名私钥的文件路径（密钥轮换过渡期用）。留空表示不做轮换；配了之后启动日志会打一条 warning 把两把私钥的 `kid` 都写出来。流程见 [`KEY_ROTATION.md`](KEY_ROTATION.md) |
| `DRAMA_COMMERCE_INTERNAL_SECRET` | 已有变量（M5b），M7 期间与部署编排一起用；留空时 `/internal/*` 一律 404 |

**没有**新增 `DRAMA_COMMERCE_REQUIRE_TLS` 这类开关：commerce 的 `serve` 只接受
`--host` / `--port` / `--log-level`（`commerce/cli.py:43-49`），不支持 HTTPS，对外 TLS 由反向代理终结。
强制 TLS 的开关只有媒体控制面的 `DRAMA_CLOUD_REQUIRE_TLS`。

#### 新增的表（迁移 0011 / 0012）

| 迁移 | 表 |
| --- | --- |
| `0011_admins.sql` | `admins`、`admin_sessions`、`admin_tokens` |
| `0012_ops_content.sql` | `announcements`、`feature_flags`、`version_policy`、`client_errors` |

`feature_flags.key` 是 `CHECK` 白名单（`signup` / `recharge` / `server_processing` / `version_task`），
`version_policy` 是单行表（`id` 固定 1），种子数据把四个开关默认设成开、版本策略设成 `0.0.0`。
`migrate` 必须在 `serve` 之前跑：`serve` 启动时检查迁移，缺了直接拒绝启动。

#### 新增的测试

```bash
cd cloud
export COMMERCE_TEST_DSN='postgresql://drama:drama-dev-only@127.0.0.1:5432/postgres'
.venv/bin/pytest tests/commerce -q
```

| 文件 | 验什么 |
| --- | --- |
| `tests/commerce/test_admins.py` | 管理员会话、令牌前缀、两种令牌互不通用、停用即刻失效、审计不记邮箱、登录限流、`/admin` 页面 |
| `tests/commerce/test_admin_views.py` | 七个视图的鉴权、分页上限、设备只给前 12 位、卡密无明文、订单状态白名单、公告增改删、四个开关、版本策略校验、写操作进审计 |
| `tests/commerce/test_client_config.py` | 公开可读、默认不拦人、不含用户数据、生效窗口与排序、开关与版本策略反映到配置、限流 |
| `tests/commerce/test_client_errors.py` | 免登录上报、多传字段 422、表里没有敏感列、带斜杠的 `location` 清空、只累加计数、后台查看要管理员令牌 |
| `tests/commerce/test_key_rotation.py` | 双私钥语义：默认主钥、`with_secondary` 用次钥、两把都信任、退役后旧钥被拒、没配次钥却要求用它签会报错 |
| `tests/commerce/test_signing_purposes.py` | Rust 的 `Purpose::context()` 与 Python 的 `PURPOSE_CONTEXTS` 逐字节对拍 |

密钥轮换那组测试逐条验什么，见 [`KEY_ROTATION.md`](KEY_ROTATION.md) 第 7 节。

#### 部署与开发的 compose 区别

| | `cloud/compose.dev.yaml` | `cloud/compose.deploy.yaml` |
| --- | --- | --- |
| 起什么 | 只有 `postgres` + `mailpit` | `postgres` + `commerce` + `media` + `caddy` |
| 应用进程 | **不容器化**，本机 `drama-commerce serve` 跑，改代码立刻生效 | 容器化，靠 `command` 区分两个服务 |
| 端口 | 只绑 `127.0.0.1` | 只有 `caddy` 暴露 80 / 443 |
| 密码与密钥 | 固定的开发值，进仓库 | 从 `.env` 读，缺了拒绝启动；私钥目录只读挂载 |
| 数据 | 一次性，`down -v` 就清空 | 磁盘卷，备份脚本能直接打包 |
| TLS | 不需要 | Caddy + 自签证书 |

**为什么分开**：开发要「改一行立刻看到效果」，所以应用不进容器；部署要「别人能用、密钥不进仓库」，
所以密码外部注入、对外只有一个入口。一份 compose 同时满足两头只会两头不讨好。
完整部署步骤见 [`../cloud/docs/DEPLOYMENT.md`](../cloud/docs/DEPLOYMENT.md)。

#### 本地调试管理接口（curl）

先建一个管理员（密码从 stdin 读，至少 12 个字符）：

```bash
.venv/bin/drama-commerce admin create --email ops@example.com
```

拿 `adm_` 令牌并调接口：

```bash
TOKEN=$(curl -s -X POST http://127.0.0.1:8100/api/v1/admin/login \
  -H 'Content-Type: application/json' \
  -d '{"email":"ops@example.com","password":"你的密码"}' | python3 -c 'import json,sys;print(json.load(sys.stdin)["token"])')

curl -s http://127.0.0.1:8100/api/v1/admin/me -H "Authorization: Bearer $TOKEN"
curl -s 'http://127.0.0.1:8100/api/v1/admin/users?limit=5' -H "Authorization: Bearer $TOKEN"
curl -s http://127.0.0.1:8100/api/v1/client/config          # 公开接口，不用令牌
```

管理后台页面在 <http://127.0.0.1:8100/admin>（令牌存 `sessionStorage`，关标签页就失效）。
更多排查用的 curl 写法与各视图返回的字段见 [`../cloud/docs/ADMIN_GUIDE.md`](../cloud/docs/ADMIN_GUIDE.md)。

### 模块边界与约定

- `plan.py` 只做「配方 → 可执行计划」，不碰磁盘；
- `modules/*` 只往 `ctx.plan` 里追加内容，**只有 `media.encode` 物化文件**；
- 状态迁移只能经 `tasks/state.py` 的 `transition()`，API 层不直接写 `tasks.state`；
- 配方模块名是白名单字面量，参数模型 `extra="forbid"`，不存在「配方里塞命令」的路径；
- 新增模块必须同时给出：注册项（`requires`/`produces`/`capability`）、参数模型与单元测试；
- **表结构只能改迁移文件**（`cloud/src/drama_cloud/migrations/*.sql`），不要改已应用的迁移：
  已应用的记在 `schema_migrations` 里，改了不会重跑。加字段就新建一个编号更大的文件；
- **按用户隔离只有一处入口**：取任务 / 产物 / 复核记录前必须过 `ensure_task_access` 一类函数，
  不要在每个查询里各写一遍 JOIN——漏一处就是一条数据泄露（`tests/test_isolation.py` 有越权矩阵）。

文档索引：`cloud/docs/ARCHITECTURE.md`、`RECIPE_SCHEMA.md`、`PROCESSING_PIPELINE.md`、
`API_SPEC.md`、`DATABASE_SCHEMA.md`、`COMMERCE.md`、`PACKAGING_GUIDE.md`、`TEST_PLAN.md`、
`ADMIN_GUIDE.md`（管理后台操作）、`DEPLOYMENT.md`（单机部署与备份恢复）；
根目录的 `docs/KEY_ROTATION.md`（签名密钥轮换）。
