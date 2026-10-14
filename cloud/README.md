# 云端短剧视频制作与多平台适配系统（学习原型）

对应设计文档《云端短剧视频制作与多平台适配系统——场景说明、架构设计与技术实现方案》，
覆盖 C1–C12 功能映射与「阶段 1：基础闭环」「阶段 2：任务可靠性」「阶段 3：擦除确认流」。

**仅用于技术学习与个人自有素材**，不用于绕过任何平台的识别机制或版权保护。

## 与文档的差异（按本机实际情况调整）

| 文档建议 | 实际选型 | 原因 |
| --- | --- | --- |
| PostgreSQL | SQLite（WAL + `BEGIN IMMEDIATE`） | 本机无 PG 服务；仓储层隔离，换 PG 只改 `db.py` 与 SQL 方言 |
| Celery + Redis | SQLite 持久化队列 + 租约/心跳 | 无 Redis；文档本身要求「关键任务状态仍以数据库为准」 |
| S3 兼容对象存储 | 本地文件系统实现 | 无对象存储；保留上传会话与短时凭证接口以便替换 |
| Tauri / Electron 客户端 | API 自带的 Web 客户端 | 本机 gio 2.68 < Tauri 2 要求的 2.70，装不了；见 `docs/PACKAGING_GUIDE.md` |

完整对照与已知限制见 `docs/ARCHITECTURE.md` §7/§9。

## 快速开始

```bash
python3.9 -m venv .venv && ./.venv/bin/pip install -e cloud[dev]
./.venv/bin/drama-cloud initdb --admin demo   # 令牌打印在最后一行
./.venv/bin/drama-cloud seed                  # 发布内置配方
./.venv/bin/drama-cloud serve                 # http://127.0.0.1:8000
./.venv/bin/drama-cloud run-worker            # 另开一个终端
```

端到端演示（造素材 → 提交任务 → 质量门禁 → 取产物）：

```bash
./cloud/demo.sh            # 不需要起 HTTP 服务
./cloud/demo.sh --http     # 另外演示 API + Web 客户端
```

测试：

```bash
cd cloud && PYTHONPATH=src ../.venv/bin/python -m pytest tests/ -q
```

硬字幕/角标擦除（可选，需要本机可执行 `dossier-inpaint`）靠部署侧环境变量注入工具路径，
不在配方里硬编码：

```bash
export DRAMA_CLOUD_INPAINT_BIN=../target/release/dossier-inpaint   # CLI（缺省再查 PATH 与 target/{release,debug}）
export DRAMA_CLOUD_INPAINT_WORKER=python3                          # 逐帧掩码 worker 解释器（仅 detector/inpainter=worker 时需要）
# export DRAMA_CLOUD_INPAINT_WORKER_ARGS="-m my_worker"             # 可选，worker 启动参数
# 之后用 drama-logo-erase 配方，或复现 INPAINT_REVIEW_FLOW.md §5 的端到端实测
```

确认流的人工复核既可以用命令行，也可以在 Web 客户端的「7 · 人工复核队列」面板里点：

```bash
drama-cloud reviews --status pending        # 待复核队列
drama-cloud review-approve <rev_id>          # 放行（失败任务放行的是候选产物）
drama-cloud review-reject  <rev_id>          # 判定失败
drama-cloud review-rerun   <rev_id> --param '{"erase":{"dilate":8}}'   # 修正 mask 后重跑
```

## 目录

```text
cloud/
├── src/drama_cloud/
│   ├── config.py        # 运行配置与目录布局
│   ├── db.py            # SQLite DDL、事务、审计、旧库补列
│   ├── storage.py       # 对象存储适配（本地实现 + 短时 HMAC 令牌）
│   ├── auth.py          # 令牌认证与角色（viewer/operator/admin）
│   ├── errors.py        # 错误分类与退避公式
│   ├── base.py          # StrictModel（extra="forbid"）
│   ├── plan.py          # 配方 → ProcessingPlan（能力检查、输入角色、指纹）
│   ├── recipes/         # C1/C2：配方 Schema、版本策略、模块白名单注册表
│   ├── modules/         # C4–C12 + Tier 2：21 个处理模块（含 inpaint）、擦除校验与质量门禁
│   ├── tasks/           # state.py 状态机、queue.py 队列、service.py 业务、runner.py Worker、review.py 人工复核队列
│   ├── api.py           # FastAPI 控制面
│   ├── cli.py           # 命令行入口
│   ├── demo.py          # 端到端演示
│   └── web/             # Web 客户端（静态页）
├── recipes/             # 内置配方（输出预设 + 擦除，7 份）
├── docs/                # 交付物文档，见下表
├── tests/               # 215 个测试（单元 / 模块 / 集成 / 接口 / 复核）
├── demo.sh              # 一键端到端演示
└── var/                 # 运行目录（数据库/存储/工作目录，可整体删除）
```

## 文档索引

| 文档 | 内容 |
| --- | --- |
| `docs/ARCHITECTURE.md` | 逻辑/部署架构、模块职责、C1–C12 覆盖、与设计文档的差异、已知限制 |
| `docs/RECIPE_SCHEMA.md` | 配方字段、21 个模块的参数表、校验规则、示例 |
| `docs/recipe.schema.json` | 配方 JSON Schema 快照（运行时也可从 `/api/v1/recipes/schema.json` 取） |
| `docs/PROCESSING_PIPELINE.md` | 计划构造、模块契约、一次性编码、检查点复用、门禁、产物登记、质量类重试 |
| `docs/API_SPEC.md` | 端点总表与详情、错误码表、认证与幂等规则、调用示例 |
| `docs/DATABASE_SCHEMA.md` | 11 张表（含 `review_queue`）的字段/索引/约束、状态机约束、迁移策略与 PG 替换点 |
| `docs/PACKAGING_GUIDE.md` | 服务端部署、桌面 `.exe`/`.dmg` 方案、签名、升级与验收清单 |
| `docs/TEST_PLAN.md` | 测试层次、媒体样本、覆盖矩阵、恢复/并发/安装/质量测试 |
| `docs/INPAINT_REVIEW_FLOW.md` | 擦除确认流：两模块参数、四组指标、分支A 自动重试、分支B 人工复核、端到端实测、已知限制 |
