# 剧备工作台化改造计划（对齐 Grove-Max 页面形态）

## Context

[docs/COMPETITOR_UI_ANALYSIS.md](file:///Users/sll/develop/drama-dossier/docs/COMPETITOR_UI_ANALYSIS.md) 梳理了竞品 Grove-Max 的 12 项页面差距。本轮按用户确认的范围执行：

- **信息架构**：单页堆叠 → 本地版四模块工作台（剧目库 / 生成报审包 / 工具箱 / 设置）
- **后端新增**：批量改名工具、码率修复工具、运行环境检测横幅、关于页版本、转码可执行（分集重组 + 智能微调帧）
- **不做**：账号 / 账单 / 云端 AI 模板 / 挂载上传（需云端服务，不在本地能力范围）

现有资产（探索确认，可直接复用）：
- 前端：`src/index.html`（单页，header L13-28，workspace L30-39，生成表单 L60-119，工具 details L121-179，擦除 L181-216，规则设置 L218-238）；`src/main.js`（state L11-16，scan/analyze L219-250，generate/transcodeAdvice L253-341，diff/platform/draft L343-466，擦除 L468-513）
- 后端命令：`src-tauri/src/lib.rs` L47-203（pick_folder/scan_library/load_app_settings/save_app_settings/analyze_drama/generate_package + erase 组 + dossier 组）；`src-tauri/src/dossier.rs` L50-125（transcode_advice/diff_report_packages/platform_profiles/platform_review/export_draft 等）
- crate：`dossier-core` settings.rs L14-84（AppSettings 持久化，临时文件原子写）；`dossier-runtime`（数据目录 paths.rs L15-41 + 通用工具探测 probe）；`dossier-media`（shot.rs 场景切点检测 detect_shots L194、tools.rs probe_media_tools L99 能力标签、timestretch/render 等）
- `tauri.conf.json` 已开 `withGlobalTauri: true` → 关于页版本可用 `window.__TAURI__.app.getVersion()`

## 一、前端改造（src/）

### 1.1 index.html — 导航骨架
- `header` 只留品牌 + 当前剧目名；「素材根目录 + 扫描剧目」移入剧目库页顶部
- `main.workspace`（现 295px 剧目列表 + detail，styles.css L135-141）→ `nav.sidenav + section.module` 结构：
  - `.sidenav`：剧目库 / 生成报审包 / 工具箱 / 设置 四个入口（图标+文字）
  - 4 个 `.module` 容器，按 `data-module` 切换显示，默认剧目库
- 内容迁移（不重写逻辑，只挪位置）：
  - **剧目库**：根目录选择 + 扫描 + 剧目列表 + 检查结果清单 + 分集规格表（现 detail 主体）
  - **生成报审包**：现生成表单整体迁移（L60-119），顶部加环境检测横幅（新）
  - **工具箱**：改卡片网格布局，每工具一张 `.tool-card`：批量改名(新)、码率修复(新)、视频转码(升级)、报审包差异对比、平台档案对照、剪映草稿导出/读回、字幕角标擦除；擦除的技术参数（dilate/feather/ROI/worker）收进卡片内「高级参数」折叠
  - **设置**：三个子区（规则与路径 = 现 L218-238 表单；本机环境 = 新探测展示；关于 = 版本信息）
- 选中剧目作为全局上下文：工具箱/生成页顶部显示当前剧目条（未选中时给引导空态）

### 1.2 main.js — 路由与新增调用
- `state` 增加 `module` 字段；新增 `switchModule(name)`（切 display + nav 高亮 + 懒初始化）
- 现有函数按域归组不重写：scan/analyze→剧目库，generate→生成，diff/platform/draft/erase→工具箱
- 新增 invoke 调用与渲染：`list_rename_plan`/`apply_rename`、`probe_bitrate`/`fix_bitrate`、`runtime_status`、`transcode_execute`（轮询进度，复用 erase_status 的轮询模式而非事件推送，保持简单）

### 1.3 styles.css
- 新增 `.sidenav`、`.module`、`.tool-card` 网格、`.env-banner`（ok/warn/error 三态）
- 复用现有主题变量与 `.grid`（L226-230）

## 二、后端改造

### 2.1 新命令（加在 src-tauri/src/dossier.rs，注册进 lib.rs invoke_handler L181-203）
1. `list_rename_plan(dir, drama_name)` — 按集数序扫描 MP4，生成「剧名 - 第X集」目标名清单；`apply_rename(plan)` — 逐条重命名，目标名冲突即报错不执行
2. `probe_bitrate(dir)` — 逐文件 ffprobe 码率，返回 `{file, bitrate_kbps, ok(≥4000)}`；`fix_bitrate(dir, min_kbps)` — 仅对不达标文件重编码（libx264，目标 `-b:v` 对齐阈值、封顶 maxrate，输出 tmp 文件后原子替换）
3. `runtime_status()` — 薄封装 `dossier_runtime` probe + `dossier_media::tools::probe_media_tools`，返回 `{ffmpeg, ffprobe, python, …}` 布尔/版本；前端横幅消费
4. `transcode_execute(dir, mode, output_dir)`，mode ∈ `recut | nudge`：
   - **recut（分集重组）**：复用 `dossier_media::shot::detect_shots` 取场景切点 + analyze_drama 的分集时长，按目标集数在切点处重分段；优先 `-c copy` 分段，切点非关键帧时回退重编码
   - **nudge（智能微调）**：每集随机裁去 1~3 帧（`-ss` 微量 + 重编码，保原码率），输出到新目录，绝不覆盖源文件
   - 长任务参照 erase.rs L45-101 的后台任务 + 状态轮询模式

### 2.2 设置结构
`AppSettings`（settings.rs L14-24）现有字段够用；如转码需要默认输出目录，加一个可选字段 `workspace_dir`，向后兼容（旧配置读取回退默认）

## 三、实施顺序（每步可编译可手测）

1. 前端骨架：导航 + 四模块空壳 + 现有功能迁移（纯前端，`cargo tauri dev` 手测切换与旧功能）
2. 设置模块：runtime_status 命令 + 本机环境展示 + 关于页（`window.__TAURI__.app.getVersion()`）
3. 工具箱新工具：批量改名（预览→确认）→ 码率修复（探测→修复）
4. 转码可执行：先 nudge（简单）后 recut（依赖 shot 切点）
5. 收尾：进度轮询 UI、空态文案、`cargo tauri build --target universal-apple-darwin` 重出 dmg

## 四、验证

- 每步 `cargo build && cargo test`（workspace 全量）
- `cargo tauri dev` 手测：四模块切换；扫描→检查→生成主链路不受影响；用临时测试目录验证改名预览/应用（含中文名）、码率修复只处理 <4Mbps 文件、nudge 输出与源帧数差 1~3 帧（ffprobe 核对）、recut 分段时长与切点一致
- 最终 `cargo tauri build --target universal-apple-darwin` 产出双架构 dmg

## 五、风险与对策

- **recut 关键帧对齐**：shot 切点非关键帧时 stream copy 会漂移 → 自动回退重编码并在结果中标注
- **长任务卡 UI**：沿用 erase 的后台任务+轮询，避免阻塞主线程
- **改名不可逆**：只改预览确认后执行，冲突即整体拒绝；不跨目录移动
- **保持「能跑」优先**：不引入前端框架/构建链，纯 HTML/JS/CSS 继续；nudge/recut 编码参数取保守默认（libx264 veryfast）
