# 设置页对齐 Grove Max + 账号 / 挂载 / 转码模块改造计划

## Context

用户重读竞品截图（/Users/sll/tmp/1014，13 张 Grove Max 界面）后提出：本项目与竞品相比缺了很多模块（如用户注册），且**设置页面要和竞品做的一样**。经确认的方向：

1. **账号体系先做本地演示版**，但代码结构保留真实云端账号的接入口（后续要做真实云端）
2. **转码独立成主导航模块**（从工具箱拆出）+ **挂载模块做本地壳**
3. 设置里的 **AI 子页先做本地模板编辑**，留云端拉取入口

当前应用：纯本地 Tauri 2 应用，四模块（剧目库/生成报审包/工具箱/设置），无任何账号代码；`sha2 = "0.10"` 已是 dossier-core 依赖（密码哈希无需新增依赖）。

## 竞品基准（截图转录结论）

- **入口**：首次打开 → 登录卡（邮箱+密码+忘记密码）；注册卡（邮箱+6 位验证码+密码，规则：≥8 位含字母数字）；底部注明「报审包只生成在本地目录，账号仅用于 AI 积分与账单」
- **主导航**：生成 / 挂载 / 转码 / 工具箱 / 设置；侧栏底部账号 chip（邮箱 + 「已登录」徽章）
- **设置 = 独立视图**：顶部「← 返回应用」，内部侧边栏三组
  - 个人：账号（登录方式卡：邮箱 + 密码/修改密码）、账单（还能生成 N 次 / 当前余额 / 积分说明 / 最近 30 天消耗 / 积分流水）
  - 配置：剪映（安装目录+自动检测+当前版本、草稿目录）、AI（提示词模板，云端拉取，失败显示重试）
  - 应用：本机环境（工作目录输入框+浏览、本地引擎行：状态徽章+「下载运行环境」+「重新检测」+红色错误行）、关于（软件版本+引擎版本，徽章「已是最新版」/「发现新版本」）
- **挂载**：四步步骤条（素材→文案→账号→执行）；计划保存时在剧目目录创建/复用「挂载文案.csv」；上传需登录视频号（第 3 步）
- **转码**：两张模式卡片（分集重组 / 智能微帧），运行环境未就绪时整页阻断
- **工具箱**：模式卡片切换（改名工具 / 码率修复）

## 改造方案

### 1. 后端（Rust）

| 文件 | 改动 |
|---|---|
| `crates/dossier-core/src/settings.rs` | `AppSettings` 增加 `ai_template`、`jianying_dir`、`jianying_draft_dir`（均 `Option` + `#[serde(default)]` 向后兼容） |
| `crates/dossier-core/src/account.rs`（新） | 本地账号纯逻辑：`register/login/logout/change_password/status`；存储结构 `{email, salt, password_hash: SHA256(salt+password), logged_in}`；校验邮箱格式、密码 ≥8 位含字母数字；**接口按命令签名设计，后续云端版只换实现不动调用方**；单测覆盖成功/重复注册/密码错/格式错 |
| `crates/dossier-core/src/ledger.rs`（新） | 本机生成流水 JSONL：`append_record / list_records`（时间倒序），记录 `{时间, 剧名, 集数, 包路径, 大小}`；单测 |
| `crates/dossier-core/src/tools.rs` | 增加挂载文案 CSV 校验纯逻辑 `validate_mount_csv`（必填列、集号与分集匹配）+ 单测 |
| `src-tauri/src/account.rs`（新） | Tauri 命令层：`account_register/account_login/account_logout/account_status/account_change_password`，账号文件存 `app_config_dir/account.json`；验证码为本地演示（前端生成并回显，不发邮件） |
| `src-tauri/src/jianying.rs`（新） | `jianying_detect`：探测 `/Applications/JianyingPro.app` + 从 Info.plist 提取版本（正则，std only）；返回 `{installed, version, draftDir 默认路径}` |
| `src-tauri/src/mount.rs`（新） | `mount_plan_save/mount_plan_load`（`app_data_dir/mount-plan.json`）、`mount_csv_template`（生成挂载文案.csv 模板到剧目目录） |
| `src-tauri/src/lib.rs` | `generate_package` 成功后追加 ledger 记录；invoke_handler 注册全部新命令 |

### 2. 前端（HTML/JS/CSS，无构建链）

**`src/index.html`**
- 新增 `#auth-view`（全屏）：登录卡 / 注册卡切换（品牌 logo + 居中卡片 + 底部「报审包只生成在本地目录…」说明）；主应用 `.app` 初始隐藏
- 侧边栏 6 项：剧目库 / 生成报审包 / **挂载** / **转码** / 工具箱 / 设置；底部账号 chip（邮箱+已登录徽章）
- 新 `#module-transcode`：模式卡片（分集重组/智能微调）+ 参数区 + 折叠的「转码建议」块（逻辑从工具箱整体迁移）
- 工具箱移除「视频转码」card（改名/码率修复/差异对比/平台要求/剪映草稿/擦除保留）
- 新 `#module-mount`：四步步骤条 + 每步面板（素材=选当前剧目；文案=生成/校验挂载文案.csv + AI 按钮 disabled 留入口；账号=占位卡「视频号登录待接入」；执行=保存计划 + 上传按钮 disabled）
- **设置改造为独立视图 `#settings-view`**（点击导航「设置」时替换 `.workspace`）：顶部「← 返回应用」+ 内部侧边栏（个人：账号/账单；配置：剪映/AI；应用：本机环境/检查规则/关于）+ 七个子页 section，布局对照竞品截图逐页复刻

**`src/main.js`**
- 启动时 `account_status` 判断：未登录 → 显示 auth 视图；登录后进入主应用并刷新环境横幅
- 注册流程：获取验证码（本地随机 6 位 toast 回显）→ 校验 → 注册即登录
- 设置子页切换、账号页（修改密码/退出登录）、账单页（`ledger_list` 渲染流水+统计）、剪映页（自动检测+目录选择+保存到设置）、AI 页（模板编辑存 `aiTemplate`）、本机环境页（工作目录+本地引擎行，复用现有 `install_runtime`/`runtime_status`）、关于页（软件版本+引擎版本徽章）
- 挂载四步流、转码模块迁移后的调用（`transcode_execute`/`transcode_advice` 参数不变）
- `loadSettings/saveSettings` 带回新增三个设置字段

**`src/styles.css`**：auth 卡片、设置视图侧边栏、步骤条、模式卡片（竞品样式：选中绿色描边）、状态徽章（就绪绿/未就绪红）

### 3. 收尾

- 版本号升 **0.2.0**（新增模块）
- `cargo test --workspace` 全量验证（新增 account/ledger/csv 单测）
- 浏览器（127.0.0.1:8642 静态预览）验证：注册→登录→主界面→设置七子页逐页对照竞品截图
- 提交推送（**含上一轮未推成功的 a941ff1**，当时网络被重置）→ `cargo tauri build --target universal-apple-darwin` 重出 dmg

## 复用清单

- 运行环境一键安装：`toolbox::install_runtime` / `runtime_status` / `resolve_ffmpeg` 直接复用，本机环境页只换展示布局
- 生成/探测链：`generate_package`、`analyze_drama` 不动，只加 ledger 追加
- 转码后端：`transcode_execute`/`transcode_advice` 不动，纯前端迁移
- 设置读写：`load_settings/save_settings`（原子替换 + serde default 兼容）直接扩展字段

## 验证

1. `cargo test --workspace`（新单测 + 存量 340+ 测试）
2. 浏览器 mock 布局验证（auth 卡、设置七子页、挂载步骤条、转码模式卡）
3. `cargo tauri dev` 冒烟：注册→登录→生成报审包（流水出现）→设置各子页保存
4. universal dmg 构建产物存在且双架构

## 进度（2026-10-14 收工，m1/m2 已完成，m3 起明天继续）

**已完成（后端全部，含验证）：**

- m1 dossier-core：
  - `settings.rs`：`AppSettings` 新增 `ai_template` / `jianying_dir` / `jianying_draft_dir`（round-trip 测试已更新）
  - `account.rs`（新）：本地演示版账号纯逻辑（register/login/logout/change_password/status，SHA256(salt+password)，`/dev/urandom` 盐注意不能用 `fs::read`——无限流，用 `read_exact`）；8 个单测
  - `ledger.rs`（新）：JSONL 生成流水 + `now_timestamp()`（chrono，供 src-tauri 复用免加依赖）；追加前检测半行补换行（断电隔离）；3 个单测
  - `tools.rs`：`validate_mount_csv`（必填列「集号」「文案」、越界/重复/非数字/空文案，支持引号字段）+ `split_csv_line`；5 个单测
  - `lib.rs`：注册并导出上述全部
- m2 src-tauri：
  - `account.rs`（新）：5 个命令，账号文件 `app_config_dir/account.json`（临时文件+替换原子写）
  - `jianying.rs`（新）：`jianying_detect`（Info.plist 版本手写解析 + 默认草稿目录 `~/Movies/JianyingPro/...`）；1 个单测
  - `mount.rs`（新）：`mount_plan_save/load`（`app_data_dir/mount-plan.json`）、`mount_csv_template/save/check`（剧目目录「挂载文案.csv」）、`ledger_list`（含本月次数）、`record_generation`（供 generate 追加）
  - `lib.rs`：`generate_package` 增加 `AppHandle` 参数并在成功后追加流水；invoke_handler 注册全部新命令；`toolbox::read_settings` 改 `pub(crate)`；`src-tauri/Cargo.toml` 补 `serde_json`
- 验证：`cargo check -p drama-dossier` 通过；`cargo test --workspace` **354 通过**；1 个存量环境失败与本次无关：`dossier-runtime` 的 `real_openssl_end_to_end_verification`（本机 openssl pkeyutl 参数不兼容，业务代码未触碰）

**明天从 m3 开始（前端三件套，按计划文件「改造方案 2」执行）：**

1. m3 `src/index.html`：`#auth-view`（登录/注册卡，注册=邮箱+6位验证码+密码≥8位含字母数字）+ 侧边栏 6 项（剧目库/生成报审包/挂载/转码/工具箱/设置 + 底部账号 chip）+ `#module-transcode`（两张模式卡片+参数区+转码建议折叠块，逻辑从工具箱迁移、命令不变）+ 工具箱删「视频转码」card + `#module-mount`（四步步骤条：素材→文案→账号→执行；文案步=生成/校验 CSV + AI 按钮 disabled；账号步=视频号登录占位）+ `#settings-view` 独立视图（顶部「← 返回应用」+ 内部侧栏：个人=账号/账单，配置=剪映/AI，应用=本机环境/检查规则/关于）
2. m4 `src/main.js`：启动 `account_status` 判断显隐 auth；注册（验证码本地生成 toast 回显，命令入参 `args: {email, password, code}`）；设置子页切换与各页逻辑（账号改密/登出、账单 `ledger_list`、剪映 `jianying_detect` + 保存设置、AI 模板存 `aiTemplate`、本机环境复用 `install_runtime`/`runtime_status`、关于版本）；挂载四步流；转码迁移（`transcode_execute`/`transcode_advice` 参数不变）；`loadSettings/saveSettings` 带回三个新字段
3. m5 `src/styles.css`：auth 卡片、设置视图侧栏、步骤条、模式卡片（选中绿色描边）、状态徽章
4. m6 验证：`cargo test --workspace` + 8642 静态预览逐页对照竞品截图
5. m7 收尾：版本升 **0.2.0**（tauri.conf.json 与 Cargo.toml）、提交推送（**含未推送的 a941ff1**，上次网络被重置）、`cargo tauri build --target universal-apple-darwin`

**遗留提醒：**

- `real_openssl_end_to_end_verification` 失败是本机环境问题，明天跑测试看到它不算回归
- 静态预览服务器 8642 可能已停，m6 时用 `serve src/` 重启即可

## 进度（2026-10-15，m3-m7 全部完成）

- m3/m4/m5 前端三件套完成（index.html 重写 / main.js 改造 / styles.css 新增），m6 验证通过（cargo test 354 + 8642 静态预览对照竞品截图逐页 PASS）
- m7 收尾遇到远程新提交（615bbe0 可用性修复 / 272413a 手册 / 50f0951 CI / ba7dd9b / e2e2190 / dad8e0a），`git pull --rebase` 冲突 7 文件全部解决：
  - 合并原则：远程可用性修复全保留（env-chip、env-banner、probe-progress 进度条、码率修复确认 settings-dirty 徽章、open_path、输出目录校验），本地新功能全保留
  - 关键合并点：`generate_package` 同时保留 `window: tauri::Window`（远程进度事件）与 `app: AppHandle`（本地流水）；loadSettings/saveSettings 取远程 `#settings-output` 口径 + 本地三个新字段；rememberRoot 补带回三新字段防抹掉；env-chip 点击改为打开设置视图并跳「本机环境」子页；工具箱旧转码卡删除（已迁移独立模块）；settings-dirty 徽章移植到规则子页头部
- 验证：`cargo test --workspace` 通过（唯一失败为已知本机 openssl 存量问题）；node 语法/括号配平、111 个函数定义齐全、HTML id 交叉校验无缺失
- 提交 645a250 已推送远程 master（rebase 后变基到 dad8e0a 之上）
- `cargo tauri build --target universal-apple-darwin` 重出 0.2.0 dmg（后台进行中）
