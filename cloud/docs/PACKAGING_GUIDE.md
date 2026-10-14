# 打包与发布指南

> 交付物之六，对应设计文档 §「跨平台客户端打包」与「阶段 5：跨平台发布」。
>
> 文档建议 Windows 交付 `.exe` 安装程序、macOS 交付 `.dmg`（内含 `.app`），
> 媒体处理默认在云端执行。**本机实际形态是 API 自带的 Web 客户端**，
> 原因与替代方案见 §2；桌面壳的具体做法见 §5、§6。

## 1. 交付形态总览

| 形态 | 适用 | 本机可做 | 说明 |
| --- | --- | --- | --- |
| Web 客户端（当前） | 学习、内网、单人使用 | ✅ 已实现 | 浏览器打开 `http://127.0.0.1:8000/`，与 API 同源 |
| 服务端进程集合 | 有服务器时 | ✅ | `serve` + `run-worker` + `cleanup`（systemd 或容器） |
| PyInstaller 单一可执行 | 想把 CLI/Worker 分发到别的机器 | ✅（需在目标平台构建） | 见 §4 |
| Tauri 2 桌面壳（Windows `.exe` / macOS `.dmg`） | 正式跨平台交付 | ❌ 本机依赖不足 | 见 §5、§6 |

## 2. 为什么本机用 Web 客户端

Tauri 2 要求 Linux 上 `glib >= 2.70`；本机是 openEuler 22.03，gio/glib 为 **2.68**，
缺依赖且无法在不升级系统库的前提下安装（升级 glib 会带动整条 GTK 栈，风险远大于收益）。

因此 `api.py` 直接挂载了 `src/drama_cloud/web/index.html` 作为客户端：

- 令牌登录 → 直传素材 → 选配方建任务 → 轮询状态 → 下载产物，覆盖文档要求的客户端闭环；
- 与 API **同源**，因此不需要 CORS 配置，也没有「客户端版本与 API 版本」错配问题；
- 浏览器原生支持 `?token=` 短时下载链接，不需要自定义下载器。

这不是「不打算做桌面端」：桌面壳要替换的只是「渲染层」，业务能力（认证、任务、配方、
产物）全部由 API 提供，壳里不需要重写任何业务逻辑。

## 3. 服务端部署（推荐起点）

### 3.1 目录与配置

| 环境变量 | 默认 | 说明 |
| --- | --- | --- |
| `DRAMA_CLOUD_HOME` | `cloud/var` | 运行目录（数据库、存储、工作目录） |
| `DRAMA_CLOUD_FFMPEG` | `ffmpeg` | ffmpeg 可执行文件路径 |
| `DRAMA_CLOUD_FFPROBE` | `ffprobe` | ffprobe 可执行文件路径 |

运行目录里 `secret.key`（0600）用于签名上传/下载令牌，**首次运行自动生成**——
多实例部署时这个文件必须一致（或换成集中式密钥管理），否则跨实例的令牌失效。

### 3.2 systemd 单元（示例）

```ini
# /etc/systemd/system/drama-cloud-api.service
[Unit]
Description=drama-cloud API
After=network.target

[Service]
User=drama
Environment=DRAMA_CLOUD_HOME=/srv/drama-cloud/var
ExecStart=/srv/drama-cloud/.venv/bin/drama-cloud serve --host 127.0.0.1 --port 8000
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

```ini
# /etc/systemd/system/drama-cloud-worker@.service
[Unit]
Description=drama-cloud worker %i

[Service]
User=drama
Environment=DRAMA_CLOUD_HOME=/srv/drama-cloud/var
ExecStart=/srv/drama-cloud/.venv/bin/drama-cloud run-worker
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

再按机器核数起 2–4 个实例：`systemctl enable --now drama-cloud-worker@{1,2,3,4}`。
任务的抢占语义保证同一任务只会被一个 Worker 执行，多起实例是安全的。

定期回收过期租约与过期产物：

```ini
# drama-cloud-cleanup.timer → OnCalendar=hourly
ExecStart=/srv/drama-cloud/.venv/bin/drama-cloud cleanup
```

### 3.3 反向代理与 TLS

设计文档 §10 要求「对外暴露必须启用 TLS」。API 本身只监听环回地址，TLS 交给反向代理：

```nginx
server {
  listen 443 ssl http2;
  server_name drama.example.internal;
  ssl_certificate     /etc/ssl/certs/drama.crt;
  ssl_certificate_key /etc/ssl/private/drama.key;

  client_max_body_size 8g;          # 直传素材，别让代理把上传挡在门外
  proxy_request_buffering off;      # 让 PUT /uploads 真正流式落盘

  location / {
    proxy_pass http://127.0.0.1:8000;
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-For $remote_addr;
  }
}
```

### 3.4 容器

本机有 `docker 28.5.2`，可用容器隔离 ffmpeg 与 Python 依赖：

- 镜像只装 `python3.9+`、`ffmpeg 4.x`（含 libx264/x265/vpx/opus）与 `cloud[dev]`；
- `DRAMA_CLOUD_HOME` 挂载到卷上，`secret.key` 与存储一起持久化；
- API 与 Worker 用**同一镜像不同命令**（`drama-cloud serve` / `drama-cloud run-worker`）；
- 不要把 ffmpeg 打进桌面客户端包（见 §7「FFmpeg 分发边界」）。

## 4. 可选：把 CLI / Worker 打成单一可执行

如果要把 Worker 分发到另一台机器（例如有 GPU 的机器），用 PyInstaller：

```bash
./.venv/bin/pip install pyinstaller
./.venv/bin/pyinstaller --name drama-cloud \
  --paths cloud/src --collect-all pydantic --collect-all fastapi --collect-all uvicorn \
  cloud/src/drama_cloud/__main__.py
```

注意事项：

- `--add-data "cloud/src/drama_cloud/web:web"` 让 Web 客户端静态页进包（否则 `/` 返回提示页）；
- PyInstaller **不能跨平台交叉编译**：`.exe` 必须在 Windows 上打，macOS 产物必须在 macOS 上打；
- ffmpeg/ffprobe 仍是**外部依赖**：目标机需要自己装，或用 `DRAMA_CLOUD_FFMPEG` 指向随包目录的二进制
  （此时必须同时审查其许可证，见 §7）。

## 5. Windows `.exe`

文档要求 Windows 交付常见形态为 NSIS 安装程序。两种做法：

### 5.1 Tauri 2 桌面壳（推荐，与仓库现有 `src-tauri` 一致）

```powershell
cargo install tauri-cli --locked
cargo tauri build          # 产出 NSIS .exe
```

- 需要 WebView2 运行时（Windows 10 1803+ 通常已预装；否则安装程序会引导安装）；
- 壳里的页面直接调用云端 API（同 §2 的 Web 客户端，只是换到 WebView 里）；
- `src-tauri/tauri.conf.json` 的 `bundle.targets` 加 `nsis`；
- 在 Windows runner 上构建，用 GitHub Actions 的 `windows-latest`。

### 5.2 PyInstaller + 托盘（无 Tauri 工具链时的退路）

把 API 客户端页面用系统默认浏览器打开，程序本体只做「启动本地服务 + 打开浏览器」。
优点是不需要 GTK/WebView 依赖；缺点是体验不如原生壳。学习原型用 §2 的浏览器方式即可，
所以不建议为了这一点引入托盘框架。

### 5.3 签名

未签名包在 Windows 上会触发 SmartScreen 提示（用户需选「仍要运行」）。
正式分发建议购买 OV/EV Code Signing 证书，并在 CI 里用 `signtool` 对 `.exe` **和**
安装程序内的可执行文件一起签名。

## 6. macOS `.dmg`

```bash
cargo install tauri-cli --locked
cargo tauri build          # 产出 .app 与 .dmg
```

- **架构策略**：分别产出 `aarch64-apple-darwin`（Apple Silicon）与 `x86_64-apple-darwin`（Intel），
  或构建 universal binary；两者都要在 macOS 上验证；
- **签名**：使用 Developer ID Application 证书对 `.app` 内所有可执行文件签名（含嵌套 helper，
  用 `codesign --deep --force --options runtime` 不够严谨，应逐个签名）；
- **公证（notarization）**：`xcrun notarytool submit` + `xcrun stapler staple`，
  否则 Gatekeeper 会拦截；需要 Apple Developer Program 账号；
- 未签名/未公证时，用户需要右键「打开」才能首次运行。

## 7. 密钥与凭据管理（硬要求）

- 证书、密码、`TAURI_SIGNING_PRIVATE_KEY`、公证凭据、上传/下载用的 `secret.key`
  **一律不入库**；只作为 CI Secrets 或受保护的本机环境变量；
- 本仓库的 CI 产出**未签名**包，签名步骤留作本地/发布流水线的可选环节；
- `cloud/var/` 里包含数据库与 `secret.key`，已在 `.gitignore` 中排除。

### FFmpeg 分发边界

`cloud/` 只调用系统中的 `ffmpeg`/`ffprobe`，**不重新分发** FFmpeg 二进制。
若将来需要便携版，必须单独审查所选 FFmpeg 构建的许可证（LGPL/GPL）与体积，
并把路径写进配置（`DRAMA_CLOUD_FFMPEG`），不要硬编码。

## 8. 版本与升级

| 版本 | 载体 | 规则 |
| --- | --- | --- |
| 应用版本 | `drama_cloud.__version__` | 语义化版本；`drama-cloud version` 与 `/api/v1/health` 都会回显 |
| Worker 版本 | `plan.WORKER_VERSION` | 配方用 `compatibility.min_worker_version` 声明最低要求 |
| 引擎版本 | `plan.ENGINE_VERSION`（`ffmpeg-4`） | 换 ffmpeg 大版本时递增；检查点会因此失效并重跑 |
| 配方 schema 版本 | `recipes.schema.SCHEMA_VERSION`（`1.0`） | 新版本号需要显式迁移/兼容处理 |

升级步骤（服务端）：

1. 备份 `DRAMA_CLOUD_HOME`（至少 `drama_cloud.sqlite3` 与 `secret.key`）；
2. 停 Worker → 停 API（或在多实例下逐个滚动重启）；
3. 拉新代码、`pip install -e cloud[dev]`；
4. `drama-cloud initdb`（`_ensure_columns` 会自动补列，不需要删库）；
5. `drama-cloud status` 确认能力集与队列正常；
6. 起 API → 起 Worker。

**回滚**：代码回退到上一个 tag 即可；数据库向后兼容由 §`MIGRATIONS` 只增列的方式保证
（新增列都有默认值）。**正在执行的任务**：回滚前先 `drama-cloud tasks --state RUNNING`
确认没有在跑的任务，否则旧 Worker 会接到新计划。

桌面端升级：`.exe` 用安装器覆盖安装（NSIS 支持原地升级）；`.dmg` 拖入替换。
原型未实现应用内自动更新通道（Roadmap 项）。

## 9. 验收清单（阶段 5）

- [ ] Windows：`.exe` 安装 → 启动 → 登录 → 提交任务 → 下载产物 → 升级覆盖安装 → 卸载无残留；
- [ ] Windows：安装路径含中文/空格时仍可运行；非管理员账号可用；
- [ ] macOS：`.dmg` 挂载 → 拖入 Applications → 首次启动通过 Gatekeeper（已签名+公证时无提示）；
- [ ] macOS：Intel 与 Apple Silicon 两个产物都可运行；
- [ ] API：TLS 下所有端点可用；`client_max_body_size` 与 `proxy_request_buffering off` 生效；
- [ ] Worker：`kill -9` 后任务在租约过期后自动重排队并最终成功；
- [ ] 升级：旧库在新版本下启动不报错（补列生效），已发布配方与历史任务可读。

## 10. 已知限制

1. 本机装不了 Tauri 2（glib 2.68 < 2.70），桌面壳方案只写到「怎么做」，未在本机产出安装包；
2. 仓库 CI 产出未签名包，签名与公证需要证书，属于本地/发布流水线工作；
3. 无应用内自动更新通道；
4. 单机存储：没有分片上传、对象副本与多机共享，桌面端「大文件续传」不在原型范围内。
