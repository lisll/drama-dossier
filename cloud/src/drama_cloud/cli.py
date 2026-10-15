"""命令行入口：初始化、内置配方、起服务、跑 Worker、提交与观察任务。

所有命令都只是对 `tasks/service.py` 与 `api.py` 的薄封装，逻辑没有第二份实现。
"""

from __future__ import annotations

import argparse
import json
import sys
import time
from pathlib import Path
from typing import List, Optional

from . import __version__
from .auth import create_user
from .config import Settings, load_settings
from .db import connect, init_db
from .errors import WorkError
from .recipes.schema import validate_recipe
from .storage import LocalStorage
from .toolchain import resolve_binary
from .runtime import RuntimeManager, RuntimeManifest, local_target
from .tasks import review as review_service
from .tasks import service
from .tasks.queue import TaskQueue

#: 内置配方目录：cloud/recipes/
def recipes_dir() -> Path:
    return Path(__file__).resolve().parents[2] / "recipes"


BUILTIN_RECIPES = "recipes"


# ---------- 工具 ----------


def _settings(args) -> Settings:
    overrides = {}
    if getattr(args, "home", None):
        overrides["home"] = Path(args.home).expanduser()
    return load_settings(**overrides)


def _print(value) -> None:
    if isinstance(value, (dict, list)):
        print(json.dumps(value, ensure_ascii=False, indent=2))
    else:
        print(value)


def _human_size(size: int) -> str:
    value = float(size)
    for unit in ("B", "KB", "MB", "GB"):
        if value < 1024 or unit == "GB":
            return f"{value:.1f}{unit}"
        value /= 1024
    return f"{value:.1f}GB"


# ---------- 命令 ----------


def cmd_initdb(args) -> int:
    settings = _settings(args)
    path = init_db(settings)
    settings.ensure_dirs()
    print(f"数据库：{path}")
    print(f"存储根目录：{settings.storage_dir}")
    conn = connect(settings)
    try:
        token = None
        if args.admin:
            token = create_user(conn, args.admin, role="admin", actor="cli")
        users = conn.execute("SELECT COUNT(*) AS n FROM users").fetchone()["n"]
        print(f"当前用户数：{users}")
        if token:
            # 令牌固定打印在最后一行，方便脚本用 `| tail -1` 取。
            print(f"管理员 {args.admin} 已创建。令牌（只显示一次，请立即保存）：")
            print(token)
    finally:
        conn.close()
    return 0


def cmd_seed(args) -> int:
    """发布内置配方（幂等：内容一致时跳过）。"""
    settings = _settings(args)
    init_db(settings)
    root = Path(args.dir) if args.dir else recipes_dir()
    if not root.is_dir():
        print(f"找不到配方目录：{root}", file=sys.stderr)
        return 2
    conn = connect(settings)
    published, skipped, failed = 0, 0, 0
    try:
        for path in sorted(root.glob("*.json")):
            payload = json.loads(path.read_text(encoding="utf-8"))
            try:
                recipe = validate_recipe(payload)
            except WorkError as error:
                print(f"  ✗ {path.name}: {error.message}", file=sys.stderr)
                failed += 1
                continue
            existing = conn.execute(
                "SELECT content_sha256 FROM recipe_versions WHERE recipe_id = ? AND version = ?",
                (recipe.recipe_id, recipe.version),
            ).fetchone()
            if existing is not None:
                if existing["content_sha256"] == recipe.content_sha256():
                    skipped += 1
                    continue
                print(
                    f"  ✗ {path.name}: {recipe.recipe_id}@{recipe.version} 已存在且内容不同；"
                    "发布新版本请修改 version",
                    file=sys.stderr,
                )
                failed += 1
                continue
            info = service.publish_recipe(
                conn,
                payload,
                actor="cli:seed",
                status=str(payload.get("status") or "published"),
                reviewed_by="cli",
            )
            print(f"  ✓ {recipe.recipe_id}@{info['version']} → {info['status']}")
            published += 1
    finally:
        conn.close()
    print(f"完成：新发布 {published}，已存在 {skipped}，失败 {failed}")
    return 0 if failed == 0 else 1


def cmd_publish(args) -> int:
    settings = _settings(args)
    init_db(settings)
    payload = json.loads(Path(args.file).read_text(encoding="utf-8"))
    conn = connect(settings)
    try:
        info = service.publish_recipe(
            conn, payload, actor="cli",
            status=args.status, rollout_percent=args.rollout,
            reviewed_by=args.reviewed_by,
        )
    finally:
        conn.close()
    _print(info)
    return 0


def cmd_rollback(args) -> int:
    settings = _settings(args)
    conn = connect(settings)
    try:
        _print(service.rollback_to(conn, args.recipe_id, args.version, actor="cli"))
    finally:
        conn.close()
    return 0


def cmd_recipes(args) -> int:
    settings = _settings(args)
    conn = connect(settings)
    try:
        rows = service.list_recipe_versions(conn, args.recipe_id)
    finally:
        conn.close()
    if args.json:
        _print(rows)
        return 0
    print(f"{'配方':<32}{'版本':<6}{'状态':<12}{'灰度':<6}{'内容哈希':<18}发布指针")
    for row in rows:
        pointer = "★" if row["published_version"] == row["version"] else ""
        print(
            f"{row['recipe_id']:<32}{row['version']:<6}{row['status']:<12}"
            f"{row['rollout_percent']:<6}{row['content_sha256'][:16]:<18}{pointer}"
        )
    return 0


def cmd_user_create(args) -> int:
    settings = _settings(args)
    conn = connect(settings)
    try:
        token = create_user(conn, args.name, role=args.role, actor="cli")
    finally:
        conn.close()
    print(f"用户 {args.name}（{args.role}）已创建。令牌（只显示一次）：")
    print(token)
    return 0


def cmd_serve(args) -> int:
    import uvicorn

    settings = _settings(args)
    init_db(settings)
    from .api import create_app

    application = create_app(settings)
    uvicorn.run(application, host=args.host, port=args.port, log_level=args.log_level)
    return 0


def cmd_run_worker(args) -> int:
    from .tasks.runner import Worker

    settings = _settings(args)
    init_db(settings)
    worker = Worker(settings, worker_id=args.worker_id)
    print(f"Worker {worker.worker_id} 启动；能力：{', '.join(worker.capabilities)}")
    if args.once:
        handled = worker.run_once()
        print("已处理 1 个任务" if handled else "队列为空")
        return 0
    try:
        worker.run_forever(poll_interval=args.poll, max_tasks=args.max_tasks)
    except KeyboardInterrupt:
        print("\n收到中断，Worker 退出")
    return 0


def cmd_submit(args) -> int:
    settings = _settings(args)
    init_db(settings)
    storage = LocalStorage(settings)
    conn = connect(settings)
    try:
        token = _resolve_cli_token(conn, args)
        principal = _principal_from_token(conn, token)
        project_id = service.ensure_default_project(conn, principal["id"], args.project or "默认项目")
        assets = [
            service.register_local_asset(conn, settings, storage, project_id, path, role=role)
            for path, role in zip(args.input, _roles_for(args))
        ]
        params = json.loads(args.param) if args.param else None
        task = service.create_task(
            conn,
            settings,
            project_id=project_id,
            asset_ids=[a["id"] for a in assets],
            recipe_id=args.recipe,
            recipe_version=args.version,
            user_params=params,
            priority=args.priority,
            idempotency_key=args.idempotency_key,
            actor=principal["id"],
        )
    finally:
        conn.close()
    print(f"任务已创建：{task['id']}（{task['state']}）")
    for asset in assets:
        probe = asset.get("probe") or {}
        print(
            f"  输入 {asset['id']} {asset['name']} [{asset['role']}] "
            f"{probe.get('width', '?')}x{probe.get('height', '?')} "
            f"{probe.get('duration', 0):.1f}s {_human_size(asset['size_bytes'])}"
        )
    if args.wait:
        return _wait_for_task(settings, task["id"], args.timeout)
    print(f"查看进度：drama-cloud task {task['id']}   （或先运行 drama-cloud run-worker）")
    return 0


def _roles_for(args) -> List[Optional[str]]:
    roles = list(args.role or [])
    while len(roles) < len(args.input):
        roles.append(None)
    return roles[: len(args.input)]


def _resolve_cli_token(conn, args) -> str:
    if args.token:
        return args.token
    row = conn.execute(
        "SELECT id FROM users WHERE role IN ('operator', 'admin') ORDER BY created_at LIMIT 1"
    ).fetchone()
    if row is None:
        raise SystemExit(
            "没有可用用户。请先运行：drama-cloud initdb --admin 管理员"
        )
    # CLI 本地使用：用库里已有的第一个 operator/admin 作为操作主体。
    stored = conn.execute(
        "SELECT token_hash FROM users WHERE id = ?", (row["id"],)
    ).fetchone()
    return _TOKEN_SHORTCUT + stored["token_hash"]


#: CLI 在本地以「首个有权限用户」的身份操作，不需要明文令牌：
#: 用一个只有 CLI 认识的哨兵前缀 + token_hash，绕开明文令牌在终端里的暴露面。
_TOKEN_SHORTCUT = "__local__:"


def _principal_from_token(conn, token: str) -> dict:
    if token.startswith(_TOKEN_SHORTCUT):
        row = conn.execute(
            "SELECT id, name, role FROM users WHERE token_hash = ?",
            (token[len(_TOKEN_SHORTCUT):],),
        ).fetchone()
    else:
        from .auth import hash_token

        row = conn.execute(
            "SELECT id, name, role FROM users WHERE token_hash = ?", (hash_token(token),)
        ).fetchone()
    if row is None:
        raise SystemExit("令牌无效")
    return {"id": row["id"], "name": row["name"], "role": row["role"]}


def _progress_text(progress: dict) -> str:
    """任务进度摘要：带上了逐帧进度就显示「done/total（百分比）」，否则只有阶段名。

    单位名不在这里猜：`progress.progress` 是工具报上来的原始事件，不同模块的单元不同
    （擦除是帧、编码可能是秒），所以只显示两个计数与比例，不写「帧」这种具体单位。
    """
    stage = str((progress or {}).get("stage", "-"))
    event = (progress or {}).get("progress")
    if not isinstance(event, dict):
        return stage
    done = event.get("unitDone")
    total = event.get("unitTotal")
    if not isinstance(done, int) or not isinstance(total, int) or total <= 0:
        return stage
    percent = min(100, round(done * 100 / total))
    return f"{stage} {done}/{total}（{percent}%）"


def _wait_for_task(settings: Settings, task_id: str, timeout: float) -> int:
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        conn = connect(settings)
        try:
            task = service.get_task(conn, task_id)
        finally:
            conn.close()
        if task["state"] != last:
            print(f"  [{task['state']}] {_progress_text(task['progress'] or {})}")
            last = task["state"]
        if task["state"] in ("SUCCEEDED", "FAILED", "CANCELLED"):
            for artifact in task["artifacts"]:
                print(f"  产物 {artifact['kind']:<10} {_human_size(artifact['size_bytes']):>9} "
                      f"sha256={artifact['sha256'][:16]}…")
            if task["error"]:
                print(f"  错误 {task['error']['class']}: {task['error']['message']}")
            return 0 if task["state"] == "SUCCEEDED" else 1
        time.sleep(1.0)
    print("等待超时；任务仍在进行。请运行 drama-cloud run-worker 推进队列。", file=sys.stderr)
    return 1


def cmd_task(args) -> int:
    settings = _settings(args)
    conn = connect(settings)
    try:
        task = service.get_task(conn, args.task_id, include_plan=args.plan)
        attempts = service.attempts_for(conn, args.task_id)
        checkpoints = service.get_checkpoints(conn, args.task_id)
    finally:
        conn.close()
    if args.json:
        task["attempts_detail"] = attempts
        task["checkpoints"] = checkpoints
        _print(task)
        return 0
    print(f"任务 {task['id']}")
    print(f"  状态      {task['state']}（尝试 {task['attempts']} 次）")
    print(f"  配方      {task['recipe_id']}@{task['recipe_version']} sha256={task['recipe_sha256'][:16]}…")
    print(f"  目标      {task['target']}")
    print(f"  进度      {_progress_text(task['progress'] or {})}")
    if task["error"]:
        print(f"  错误      {task['error']['class']}: {task['error']['message']}")
    for attempt in attempts:
        print(
            f"  尝试#{attempt['attempt']} {attempt['started_at']} "
            f"{attempt['error_class'] or 'ok'}"
        )
    for checkpoint in checkpoints:
        print(f"  检查点    {checkpoint['stage']:<22} completed={checkpoint['completed']}")
    for artifact in task["artifacts"]:
        print(f"  产物      {artifact['kind']:<10} {_human_size(artifact['size_bytes']):>9} {artifact['id']}")
    return 0 if task["state"] == "SUCCEEDED" else 0


def cmd_tasks(args) -> int:
    settings = _settings(args)
    conn = connect(settings)
    try:
        rows = service.list_tasks(conn, state=args.state, limit=args.limit)
    finally:
        conn.close()
    if args.json:
        _print(rows)
        return 0
    for row in rows:
        print(f"{row['state']:<16}{row['id']:<26}{row['recipe_id']}@{row['recipe_version']}"
              f"  attempt={row['attempts']}  {row['created_at']}")
    return 0


# ---------- 人工复核队列（确认流分支B） ----------


def cmd_reviews(args) -> int:
    settings = _settings(args)
    conn = connect(settings)
    try:
        rows = review_service.list_reviews(conn, status=args.status, task_id=args.task,
                                          limit=args.limit)
    finally:
        conn.close()
    if args.json:
        _print(rows)
        return 0
    if not rows:
        print("没有复核记录" + (f"（status={args.status}）" if args.status else ""))
        return 0
    for row in rows:
        detail = row.get("detail") or {}
        summary = (detail.get("inpaint") or {}).get("summary") or {}
        extra = ""
        if summary:
            extra = (f"  残留峰值 {(summary.get('residual_max_ratio') or 0.0) * 100:.2f}%"
                     f"  闪烁 {summary.get('flicker_max_index') or 0.0:.2f}"
                     f"  接缝 {summary.get('seam_max_score') or 0.0:.2f}")
        print(f"{row['status']:<11}{row['severity']:<6}{row['id']:<26}{row['reason']:<20}"
              f"task={row['task_id']}")
        if extra:
            print(f"           {extra.strip()}")
    return 0


def cmd_review(args) -> int:
    settings = _settings(args)
    conn = connect(settings)
    try:
        item = review_service.get_review(conn, args.review_id)
    finally:
        conn.close()
    _print(item)
    return 0


def cmd_review_decide(args, decision: str) -> int:
    settings = _settings(args)
    conn = connect(settings)
    try:
        item = review_service.decide(
            conn, args.review_id, decision, note=args.note or "", actor=args.actor
        )
    finally:
        conn.close()
    print(f"复核 {item['id']} → {item['status']}（{item['reason']}）")
    return 0


def cmd_review_rerun(args) -> int:
    settings = _settings(args)
    conn = connect(settings)
    try:
        params = json.loads(args.param) if args.param else None
        result = review_service.rerun(
            conn,
            settings,
            args.review_id,
            user_params=params,
            note=args.note or "",
            actor=args.actor,
            priority=args.priority,
        )
    finally:
        conn.close()
    task = result["task"]
    print(f"已按修正后的参数重跑：新任务 {task['id']}（{task['state']}），"
          f"原复核记录 → {result['review']['status']}")
    print(f"  查看参数：drama-cloud task {task['id']} --plan")
    if args.wait:
        return _wait_for_task(settings, task["id"], timeout=args.timeout)
    return 0


def cmd_status(args) -> int:
    from .modules.ffmpeg import ffmpeg_version

    settings = _settings(args)
    conn = connect(settings)
    try:
        queue = TaskQueue(settings)
        print(f"数据库      {settings.db_path}")
        print(f"存储        {settings.storage_dir}")
        print(f"队列深度    {queue.depth(conn)}")
        print(f"任务状态    {queue.stats(conn)}")
        users = conn.execute("SELECT COUNT(*) AS n FROM users").fetchone()["n"]
        assets = conn.execute("SELECT COUNT(*) AS n FROM assets").fetchone()["n"]
        artifacts = conn.execute("SELECT COUNT(*) AS n FROM artifacts").fetchone()["n"]
        print(f"用户/素材/产物  {users}/{assets}/{artifacts}")
    finally:
        conn.close()
    print(f"ffmpeg      {ffmpeg_version(settings)}")
    print(f"能力        {', '.join(service.cluster_capabilities(settings))}")
    runtime_root = settings.runtime_root or (settings.home / "runtime")
    print(f"运行时      {json.dumps(RuntimeManager(runtime_root).status(), ensure_ascii=False)}")
    return 0


def _runtime_manifest(settings: Settings) -> RuntimeManifest:
    if settings.runtime_manifest is None:
        raise ValueError("未配置 DRAMA_CLOUD_RUNTIME_MANIFEST")
    return RuntimeManifest.load(Path(settings.runtime_manifest))


def cmd_runtime_status(args) -> int:
    settings = _settings(args)
    manifest = _runtime_manifest(settings)
    platform, arch = local_target()
    result = {
        "target": {"platform": platform, "arch": arch},
        "available_roles": [r.name for r in manifest.for_target(platform, arch)],
        **RuntimeManager(settings.runtime_root or (settings.home / "runtime")).status(),
    }
    _print(result)
    return 0


def cmd_runtime_install(args) -> int:
    settings = _settings(args)
    manifest = _runtime_manifest(settings)
    platform, arch = local_target()
    role = next((r for r in manifest.for_target(platform, arch) if r.name == args.role), None)
    if role is None:
        raise ValueError(f"manifest 中没有适配本机的角色：{args.role}")
    result = RuntimeManager(settings.runtime_root or (settings.home / "runtime")).install(role)
    _print(result)
    return 0


def cmd_runtime_rollback(args) -> int:
    settings = _settings(args)
    result = RuntimeManager(settings.runtime_root or (settings.home / "runtime")).rollback(args.role)
    _print(result)
    return 0


def cmd_cleanup(args) -> int:
    settings = _settings(args)
    storage = LocalStorage(settings)
    conn = connect(settings)
    try:
        removed = service.cleanup_expired_artifacts(conn, storage)
        queue = TaskQueue(settings)
        recovered = queue.reclaim_expired(conn)
    finally:
        conn.close()
    print(f"清理过期产物 {removed} 个；回收过期租约任务 {recovered} 个")
    return 0


def cmd_artifacts_check(args) -> int:
    """跨产物一致性校验（Tier 3 的消费端）。

    把若干任务的**产物戳记**取出来交给 Rust 侧 `dossier-artifacts check`：它会回答
    「这批产物是不是同一套管线、同一个工具版本、同一组参数、同一批输入出的」。
    典型用法是审一批分集——一次跑批里混进了旧工具产出的分集，只看单个任务是看不出来的。

    这里刻意**不重新实现**一致性规则（那是 `dossier_core::pipeline_version` 的职责），
    只负责把文件凑齐、把结论如实带回：工具的退出码就是命令的退出码。
    """
    from .modules.ffmpeg import run

    settings = _settings(args)
    storage = LocalStorage(settings)
    conn = connect(settings)
    staging = settings.work_dir / "artifact-stamps"
    staging.mkdir(parents=True, exist_ok=True)
    collected: List[Path] = []
    try:
        for task_id in args.task_ids:
            task = service.get_task(conn, task_id)
            stamps = [a for a in service.list_artifacts(conn, task_id) if a["kind"] == "artifact_stamp"]
            if not stamps:
                print(f"任务 {task['id']} 没有产物戳记（擦除工具较旧，或该任务不含擦除步骤）")
                continue
            for artifact in stamps:
                name = Path(str(artifact["storage_key"])).name
                target = staging / f"{task_id}_{artifact['id']}_{name}"
                target.write_bytes(storage.read_bytes(artifact["storage_key"]))
                collected.append(target)
    finally:
        conn.close()

    if not collected:
        print("没有任何可校验的产物戳记", file=sys.stderr)
        return 1

    tool = resolve_binary(getattr(settings, "artifacts_bin", None), "dossier-artifacts")
    if tool is None:
        print(
            "错误[capability_missing] 找不到 dossier-artifacts；请设置 DRAMA_CLOUD_ARTIFACTS_BIN，"
            "或在仓库根目录执行 cargo build --release -p dossier-core --bin dossier-artifacts",
            file=sys.stderr,
        )
        return 1

    command = [str(tool), "check", *[str(path) for path in collected]]
    if args.json:
        command.append("--json")
    result = run(command, timeout=300.0)
    stdout = result.stdout.decode("utf-8", "replace")
    stderr = result.stderr.decode("utf-8", "replace")
    if stdout.strip():
        print(stdout.rstrip())
    if result.returncode != 0 and stderr.strip():
        print(stderr.rstrip(), file=sys.stderr)
    return int(result.returncode)


def cmd_demo(args) -> int:
    """造素材 → 提交 → 跑 Worker → 校验产物（不需要起 HTTP 服务）。"""
    from .demo import run_demo

    return run_demo(_settings(args), keep=args.keep)


def cmd_version(args) -> int:
    print(f"drama-cloud {__version__}")
    return 0


# ---------- 参数解析 ----------


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="drama-cloud",
        description="云端短剧视频制作与多平台适配系统（学习原型）",
    )
    parser.add_argument("--home", help="运行目录（默认 cloud/var）")
    parser.add_argument("--version", action="store_true", help="显示版本并退出")
    sub = parser.add_subparsers(dest="command")

    p = sub.add_parser("initdb", help="初始化数据库并可选创建管理员")
    p.add_argument("--admin", help="创建该名字的管理员并打印一次性令牌")
    p.set_defaults(func=cmd_initdb)

    p = sub.add_parser("seed", help="发布内置配方")
    p.add_argument("--dir", help="配方目录（默认 cloud/recipes）")
    p.set_defaults(func=cmd_seed)

    p = sub.add_parser("publish", help="发布配方文件")
    p.add_argument("file")
    p.add_argument("--status", default="published")
    p.add_argument("--rollout", type=int, default=100)
    p.add_argument("--reviewed-by", default=None)
    p.set_defaults(func=cmd_publish)

    p = sub.add_parser("rollback", help="回滚配方发布指针")
    p.add_argument("recipe_id")
    p.add_argument("--version", type=int, default=None)
    p.set_defaults(func=cmd_rollback)

    p = sub.add_parser("recipes", help="列出配方版本")
    p.add_argument("recipe_id", nargs="?")
    p.add_argument("--json", action="store_true")
    p.set_defaults(func=cmd_recipes)

    p = sub.add_parser("user-create", help="创建用户并打印一次性令牌")
    p.add_argument("name")
    p.add_argument("--role", default="operator", choices=["admin", "operator", "viewer"])
    p.set_defaults(func=cmd_user_create)

    p = sub.add_parser("serve", help="启动 API 服务与 Web 客户端")
    p.add_argument("--host", default="127.0.0.1")
    p.add_argument("--port", type=int, default=8000)
    p.add_argument("--log-level", default="info")
    p.set_defaults(func=cmd_serve)

    p = sub.add_parser("run-worker", help="启动媒体 Worker")
    p.add_argument("--worker-id", default=None)
    p.add_argument("--poll", type=float, default=2.0, help="空闲轮询间隔（秒）")
    p.add_argument("--once", action="store_true", help="只处理一个任务后退出")
    p.add_argument("--max-tasks", type=int, default=None)
    p.set_defaults(func=cmd_run_worker)

    p = sub.add_parser("submit", help="登记素材并创建任务")
    p.add_argument("--input", action="append", required=True, help="输入文件，可重复")
    p.add_argument("--role", action="append", default=None,
                   help="与 --input 一一对应的角色（video/audio/subtitle/metadata）")
    p.add_argument("--recipe", required=True)
    p.add_argument("--version", type=int, default=None)
    p.add_argument("--project", default=None)
    p.add_argument("--param", default=None, help="参数覆盖 JSON，形如 {\"encode\":{\"crf\":23}}")
    p.add_argument("--priority", type=int, default=100)
    p.add_argument("--idempotency-key", default=None)
    p.add_argument("--token", default=None, help="以指定令牌的身份提交")
    p.add_argument("--wait", action="store_true", help="等待任务结束并打印产物")
    p.add_argument("--timeout", type=float, default=3600.0)
    p.set_defaults(func=cmd_submit)

    p = sub.add_parser("task", help="查看单个任务")
    p.add_argument("task_id")
    p.add_argument("--json", action="store_true")
    p.add_argument("--plan", action="store_true", help="同时打印配方快照与执行计划")
    p.set_defaults(func=cmd_task)

    p = sub.add_parser("tasks", help="列出任务")
    p.add_argument("--state", default=None)
    p.add_argument("--limit", type=int, default=50)
    p.add_argument("--json", action="store_true")
    p.set_defaults(func=cmd_tasks)

    p = sub.add_parser("status", help="环境与队列概览")
    p.set_defaults(func=cmd_status)

    p = sub.add_parser("runtime-status", help="查看 RuntimeRoles 清单与本地激活状态")
    p.set_defaults(func=cmd_runtime_status)

    p = sub.add_parser("runtime-install", help="按清单安装并激活一个 RuntimeRole")
    p.add_argument("role")
    p.set_defaults(func=cmd_runtime_install)

    p = sub.add_parser("runtime-rollback", help="回滚一个 RuntimeRole 到上一个版本")
    p.add_argument("role")
    p.set_defaults(func=cmd_runtime_rollback)

    p = sub.add_parser("reviews", help="列出人工复核队列")
    p.add_argument("--status", default="pending",
                   choices=["pending", "approved", "rejected", "superseded"])
    p.add_argument("--task", default=None, help="只看某个任务的复核记录")
    p.add_argument("--limit", type=int, default=100)
    p.add_argument("--json", action="store_true")
    p.set_defaults(func=cmd_reviews)

    p = sub.add_parser("review", help="查看单条复核记录")
    p.add_argument("review_id")
    p.set_defaults(func=cmd_review)

    p = sub.add_parser("review-approve", help="人工复核：标记通过（放行产物）")
    p.add_argument("review_id")
    p.add_argument("--note", default="")
    p.add_argument("--actor", default="cli")
    p.set_defaults(func=lambda args: cmd_review_decide(args, "approve"))

    p = sub.add_parser("review-reject", help="人工复核：标记失败")
    p.add_argument("review_id")
    p.add_argument("--note", default="")
    p.add_argument("--actor", default="cli")
    p.set_defaults(func=lambda args: cmd_review_decide(args, "reject"))

    p = sub.add_parser("review-rerun", help="人工复核：修正 mask/参数后重跑（新建任务）")
    p.add_argument("review_id")
    p.add_argument("--param", default=None,
                   help='参数覆盖 JSON，形如 {"logo_erase":{"dilate":6}}')
    p.add_argument("--note", default="")
    p.add_argument("--actor", default="cli")
    p.add_argument("--priority", type=int, default=100)
    p.add_argument("--wait", action="store_true", help="等待重跑任务结束")
    p.add_argument("--timeout", type=float, default=3600.0)
    p.set_defaults(func=cmd_review_rerun)

    p = sub.add_parser("artifacts", help="产物戳记与跨产物一致性检查")
    artifacts_sub = p.add_subparsers(dest="artifacts_command")
    q = artifacts_sub.add_parser("check", help="对若干任务的产物戳记做一致性校验")
    q.add_argument("task_ids", nargs="+", help="要一起校验的任务 id（同批分集放在一次比对里）")
    q.add_argument("--json", action="store_true", help="输出 JSON（便于脚本消费）")
    q.set_defaults(func=cmd_artifacts_check)

    p = sub.add_parser("cleanup", help="清理过期产物并回收过期租约")
    p.set_defaults(func=cmd_cleanup)

    p = sub.add_parser("demo", help="跑一遍端到端演示")
    p.add_argument("--keep", action="store_true", help="保留演示产生的临时文件")
    p.set_defaults(func=cmd_demo)

    p = sub.add_parser("version", help="显示版本")
    p.set_defaults(func=cmd_version)
    return parser


def main(argv: Optional[List[str]] = None) -> int:
    from .recipes.registry import ensure_modules_loaded

    # 配方校验、能力检查都要依赖模块注册表，先确保它被填满。
    ensure_modules_loaded()
    parser = build_parser()
    args = parser.parse_args(argv)
    if getattr(args, "version", False) and not getattr(args, "command", None):
        return cmd_version(args)
    if not getattr(args, "command", None):
        parser.print_help()
        return 0
    if not hasattr(args, "func"):
        # 分组命令（如 `artifacts`）没给子命令时给出帮助，而不是抛 AttributeError。
        parser.print_help()
        return 0
    try:
        return args.func(args)
    except WorkError as error:
        print(f"错误[{error.error_class}] {error.message}", file=sys.stderr)
        return 1
    except (OSError, ValueError) as error:
        print(f"错误：{error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
