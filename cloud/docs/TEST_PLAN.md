# 测试计划

> 交付物之七。对应设计文档「阶段 5 验收」与 `Test_Plan.md`（媒体样本、任务状态、恢复、
> 并发、跨平台安装与质量测试）。
>
> **原则**：测试素材全部由 ffmpeg **现场合成**，不含任何第三方内容；测试只依赖本机
> ffmpeg/ffprobe，不需要 GPU、不需要外部服务。

## 1. 测试层次

| 层次 | 位置 | 依赖 | 特点 |
| --- | --- | --- | --- |
| 单元 | `tests/test_recipes.py` `test_plan.py` `test_state_machine.py` `test_queue.py` `test_storage.py` | 无 ffmpeg | 快；校验规则、状态机、队列语义、签名 |
| 模块 | `tests/test_modules.py` | 无 ffmpeg | 模块只写 `ctx.plan`，因此可直接断言「生成的滤镜链」 |
| 集成 | `tests/test_runner.py` | ffmpeg | 真跑 Worker：编码、门禁、检查点、取消、租约 |
| 接口 | `tests/test_api.py` | ffmpeg（仅全链路用例） | FastAPI TestClient：认证、授权、限流、幂等 |
| 端到端 | `cloud/demo.sh` → `drama_cloud.demo` | ffmpeg | 不起 HTTP，跑通 4 条配方并校验产物 |
| 手工冒烟 | §6 | 浏览器/curl | Web 客户端与真实 HTTP 链路 |

## 2. 媒体样本

| 样本 | 生成方式 | 规格 | 用途 |
| --- | --- | --- | --- |
| `tiny.mp4` | `conftest.media_cache`（会话级缓存一次） | 160×120 @10fps，2s，libx264 + AAC 440Hz | 所有集成/接口测试 |
| `tiny.srt` | 同上（手写文本） | 1 条字幕，200ms–1200ms | 字幕工作流 |
| `source.mp4` | `demo.make_source` | 1280×720 @25fps，6s | 端到端演示 |
| `sample.srt` | `demo.make_subtitle` | 2 条字幕 | 演示 C3 |
| `clip.mp4` | 手工（可选） | 640×360，4s | 开发期手工调试 |

约定：**没有 ffmpeg 时**用 `requires_ffmpeg` 标记跳过，纯逻辑测试仍然全绿
（`tests/conftest.py` 里的 `HAVE_FFMPEG`）。

每个测试用自己的 `DRAMA_CLOUD_HOME`（`tmp_path`），因此数据库、存储、工作目录互不干扰；
测试可以并行跑，也不会污染 `cloud/var/`。

## 3. 运行方式

```bash
# 全部测试
cd cloud && PYTHONPATH=src ../.venv/bin/python -m pytest tests/ -q

# 只跑不需要 ffmpeg 的部分
cd cloud && PYTHONPATH=src ../.venv/bin/python -m pytest tests/ -q -k "not runner and not full_http"

# 端到端演示（造素材 → 4 条配方 → 门禁 → 取产物）
./cloud/demo.sh

# 端到端 + 启动 API 与 Web 客户端
./cloud/demo.sh --http
```

## 4. 覆盖矩阵

### 4.1 配方与计划

| 需求 | 用例 |
| --- | --- |
| 6 份内置配方全部合法 | `test_builtin_recipes_all_validate` |
| 未注册模块被拒（白名单） | `test_unknown_module_is_rejected` |
| 未知参数字段被拒（`extra="forbid"`） | `test_unknown_param_field_is_rejected` |
| 参数范围与容器矩阵 | `test_param_range_and_container_matrix_are_enforced_at_runtime` |
| 依赖闭包必须闭合 | `test_dependency_closure_is_enforced` |
| 门禁必须是最后一步 | `test_last_step_must_be_quality_gate_when_required` |
| subtitle 模式必须含字幕步骤 | `test_subtitle_mode_requires_subtitle_step` |
| `target` 与编码参数一致 | `test_target_must_match_encode_params` |
| 声明分辨率必须有 scale | `test_declared_resolution_requires_scale_step` |
| 步骤 id 唯一 | `test_duplicate_step_ids_are_rejected` |
| 计划展开、能力检查、输入下标 | `test_plan.py`（9 例） |

### 4.2 配方版本策略（C2）

| 需求 | 用例 |
| --- | --- |
| 内容哈希不受 `status` 影响 | `test_content_hash_ignores_status` |
| 已发布版本不可修改 | `test_published_version_is_immutable` |
| 新版本与回滚 | `test_new_version_and_rollback` |
| 撤回后指针回退 | `test_revoke_falls_back_to_previous_published_version` |
| 已撤回版本不可创建任务 | `test_revoked_version_cannot_be_created` |
| 灰度比例控制创建 | `test_rollout_percent_gates_creation` |
| 草稿不可解析 | `test_draft_recipe_is_not_resolvable` |

### 4.3 处理模块（C4–C11）

| 需求 | 用例 |
| --- | --- |
| 滤镜链生成与分辨率记录 | `test_video_quality_builds_filter_chain_and_records_resolution` |
| 只给高度时宽度偶数推导 | `test_video_quality_scale_height_only_uses_even_auto_width` |
| C9 只追加滤镜 | `test_fps_and_deflicker_only_append_filters` |
| 时间线默认整片 / 越界拒绝 / 时长上限 | `test_timeline_*`（3 例） |
| 章节模式不受单集下限约束 | `test_chapters_mode_ignores_episode_floor` |
| 分集尾段并入上一集 | `test_episode_mode_merges_short_tail` |
| 过短分集被拒 | `test_episode_mode_rejects_too_short_segments` |
| 定位元数据清除 | `test_metadata_policy_strips_location_fields` |
| 外挂字幕必须给下标 | `test_subtitle_external_needs_input_index` |
| 无音轨时跳过并记明 | `test_audio_quality_skips_without_audio_track` |
| 容器矩阵与码率模式 | `test_container_matrix_*`、`test_bitrate_mode_requires_bitrate` |
| 未审核时间线不得编码 | `test_encode_refuses_unapproved_timeline` |

### 4.4 状态机与队列

| 需求 | 用例 |
| --- | --- |
| 正常路径可迁移 | `test_happy_path_is_allowed` |
| 非法迁移被拒 | `test_illegal_transitions_are_rejected` |
| 终态无出边 | `test_terminal_states_have_no_outgoing_edges` |
| 失败分类 → 目标状态 | `test_failure_target` |
| 迁移落库且被校验 | `test_transition_is_persisted_and_validated` |
| 乐观并发拒绝过期写者 | `test_optimistic_guard_rejects_stale_writer` |
| 抢占、优先级、能力过滤 | `test_claim_then_empty`、`test_priority_orders_claims`、`test_capability_filter_skips_incapable_worker` |
| 心跳必须持有租约 | `test_heartbeat_requires_ownership` |
| 尝试记录 | `test_attempt_row_is_recorded` |

### 4.5 可靠性与恢复

| 需求 | 用例 |
| --- | --- |
| 端到端成功 | `test_end_to_end_success` |
| 字幕烧录端到端 | `test_subtitle_burn_end_to_end` |
| 取消（未开始 / 执行中协作退出） | `test_cancel_before_start`、`test_cancel_is_cooperative_and_never_marks_half_done_as_success` |
| 可重试失败 → 复用检查点 → 成功 | `test_retryable_failure_then_reuse_and_success` |
| 不可重试失败直接 FAILED 并清理 | `test_non_retryable_failure_goes_to_failed_and_cleans_up` |
| 门禁失败是终态 | `test_quality_gate_failure_is_terminal` |
| 人工重试重新入队 | `test_manual_retry_after_failure_requeues` |
| 能力不足的 Worker 不接任务 | `test_worker_without_capability_does_not_take_task` |
| 租约易主后旧 Worker 不改状态 | `test_lease_loss_does_not_clobber_new_owner` |
| 租约过期回收重排 | `test_expired_lease_is_reclaimed_and_requeued` |
| 过期且超过重试上限 → FAILED | `test_expired_lease_after_max_attempts_fails` |
| 指数退避与封顶 | `test_retry_uses_exponential_backoff`、`test_backoff_is_capped`、`test_non_retryable_error_goes_straight_to_failed` |

### 4.6 幂等、快照与多产物

| 需求 | 用例 |
| --- | --- |
| 幂等键返回同一任务 | `test_idempotency_key_returns_same_task` |
| `user_params` 覆盖且快照冻结 | `test_user_params_override_and_snapshot_is_frozen` |
| 任务不跟随发布指针 | `test_task_snapshot_does_not_follow_published_pointer` |
| 分集产出多个产物 | `test_chapter_split_produces_multiple_artifacts` |

### 4.7 接口与安全

| 需求 | 用例 |
| --- | --- |
| 健康检查免认证 | `test_health_needs_no_auth` |
| 缺失/无效令牌 | `test_missing_and_bad_token` |
| 身份回显 | `test_whoami` |
| viewer 不能建任务 / 不能发布配方 | `test_viewer_cannot_create_task`、`test_viewer_cannot_publish_recipe` |
| operator 不能发布配方 | `test_operator_cannot_publish_recipe` |
| admin 发布 + viewer 读取 | `test_admin_publishes_recipe_and_viewer_reads_it` |
| Schema 端点与配方列表 | `test_recipe_schema_endpoint`、`test_seeded_recipes_are_listed` |
| 上传角色校验与令牌匹配 | `test_upload_session_rejects_bad_role`、`test_upload_requires_the_matching_token` |
| 完整 HTTP 链路（上传→建任务→Worker→下载） | `test_full_http_flow` |
| 幂等头只创建一个任务 | `test_idempotency_header_creates_one_task` |
| 缺素材 / 未知配方 | `test_task_creation_rejects_missing_asset`、`test_task_creation_rejects_unknown_recipe` |
| 取消与重试端点 | `test_cancel_and_retry_endpoints` |
| 审计仅 admin | `test_audit_is_admin_only` |
| 状态端点列出模块 | `test_system_status_lists_modules` |
| 限流返回 429 | `test_rate_limit_returns_429` |

### 4.8 存储

| 需求 | 用例 |
| --- | --- |
| 读写往返 | `test_put_and_read_roundtrip` |
| 键按 assets/artifacts 分区 | `test_asset_and_artifact_keys_are_namespaced` |
| 路径穿越被挡 | `test_path_traversal_is_blocked` |
| 签名令牌往返 / 用途 / 篡改 / 过期 | `test_signed_payload_*`（4 例） |
| 对象键签名校验 | `test_object_key_signature_verify` |

## 5. 质量门禁测试

门禁的每一项都可以通过「构造一个违反它的产物」来负向验证；原型里主要覆盖：

| 门禁项 | 覆盖方式 |
| --- | --- |
| `decodable` | `test_quality_gate_failure_is_terminal`（注入不可解码产物） |
| `target_*` / `duration` / `av_sync` / `audio_track` | `test_end_to_end_success` 与 `test_full_edit` 类配方的正向断言；负向靠 `duration_tolerance_seconds` 与 `max_av_sync_ms` 收紧参数 |
| `subtitle_track` | `test_subtitle_burn_end_to_end`（烧录）+ 软字幕配方 |
| `artifact_hash` | 所有端到端用例都会比对产物 sha256 |

手工负向验证（可选）：把 `quality_policy.min_duration_seconds` 提到远大于素材时长，
或把 `max_av_sync_ms` 设成 `1`，任务应以 `quality_gate_failed` 结束且**不产生** `output` 产物。

## 6. 手工冒烟清单

1. `./cloud/demo.sh`：看到 `全部通过：no_edit=OK, subtitle=OK, full_edit=OK, episodes=OK`。
2. `./cloud/demo.sh --http`：浏览器打开 `http://127.0.0.1:8000/`，粘贴管理员令牌，
   上传一份自有素材，选 `drama.no_edit_generic` 建任务，另开终端 `run-worker`，页面状态转为
   `SUCCEEDED` 后点击下载，比对页面上显示的 sha256 与本地文件。
3. 反向验证授权：用 viewer 令牌调用 `POST /api/v1/tasks` 应得 `403`；不带令牌应得 `401`。
4. 反向验证幂等：同一个 `Idempotency-Key` 连发两次，`GET /api/v1/tasks` 只多出一条任务。
5. 可靠性：任务 `RUNNING` 时 `kill -9` Worker，等待超过租约时长（默认 30s）后重新
   `run-worker`，任务应被回收重排并最终成功；`GET /tasks/{id}` 的 `attempts_detail` 能看到两次尝试。
6. 取消：任务 `RUNNING` 时调 `POST /tasks/{id}/cancel`，任务应以 `CANCELLED` 结束，
   且 `artifacts` 里**没有** `output`。

## 7. 并发测试

| 场景 | 做法 | 期望 |
| --- | --- | --- |
| 多 Worker 抢同一任务 | 同一 `DRAMA_CLOUD_HOME` 起 3 个 `run-worker`，一次提交 6 个任务 | 每个任务只被执行一次；`task_attempts` 无重复 `(task_id, attempt)` |
| 能力隔离 | 起一个 Worker 时用 `capabilities` 去掉 `h264_encode` | 需要该能力的任务不被它抢走，也不会被判失败 |
| 乐观并发 | 两个进程同时改同一任务状态 | 后到的写入被 `transition(allowed_from=…)` 拒绝（`409 resource_contention`） |
| 限流 | 同一令牌快速连发请求 | 超过桶容量后返回 `429` 并带 `Retry-After` |

自动化的并发覆盖目前只在单进程内验证语义（`test_queue.py`、`test_state_machine.py`）；
多进程真实并发属于手工步骤。

## 8. 安装与跨平台测试

阶段 5 的安装/签名/升级验收清单见 `PACKAGING_GUIDE.md` §9（Windows `.exe`、macOS `.dmg`、
TLS 反向代理、`kill -9` 恢复、升级不破坏旧库）。本机无法产出桌面安装包，
因此这一节目前是**待执行清单**而非回归测试。

## 9. 已知未覆盖项

1. **GPU / 模型类处理**（光流插帧、E2FGVI 一类修复）没有对应模块，因此没有测试——
   本机无 GPU，且这类能力不在本原型范围内（见 `ARCHITECTURE.md` §9）。
2. **ASR**：C11 的转写需要外部提供，没有自动语音识别测试。
3. **多进程并发**：只有语义级自动化用例，真实多进程属于手工步骤。
4. **桌面安装包**：见 §8。
5. **容量/时长压力**：没有长素材（>1 小时）与高并发压测数据；测试素材都是秒级的。
6. **TLS 与反向代理**：属于部署配置，没有自动化验证。
