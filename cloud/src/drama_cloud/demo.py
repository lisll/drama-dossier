"""端到端演示：造素材 → 登记 → 创建任务 → 跑 Worker → 校验产物。

不需要起 HTTP 服务，直接在同一进程里跑一遍「阶段 1 基础闭环」，用来确认
环境（ffmpeg、存储、配方、队列、门禁）全都通了。
"""

from __future__ import annotations

import json
import shutil
from pathlib import Path
from .auth import create_user
from .config import Settings
from .db import connect, init_db
from .modules.ffmpeg import ffmpeg_version, run_checked
from .storage import LocalStorage
from .tasks import service
from .tasks.runner import Worker

#: 演示素材规格：小而完整（有视频、有音频、有时长），编码很快。
DEMO_SECONDS = 6.0
DEMO_SIZE = "1280x720"


def make_source(directory: Path, name: str = "source.mp4", seconds: float = DEMO_SECONDS) -> Path:
    """用 ffmpeg 合成一段测试素材（自己生成的，不含任何第三方内容）。"""
    target = directory / name
    if target.exists():
        return target
    run_checked(
        [
            "ffmpeg", "-hide_banner", "-nostdin", "-y",
            "-f", "lavfi", "-i", f"testsrc2=size={DEMO_SIZE}:rate=25:duration={seconds:g}",
            "-f", "lavfi", "-i", f"sine=frequency=440:duration={seconds:g}",
            "-c:v", "libx264", "-preset", "ultrafast", "-crf", "30", "-pix_fmt", "yuv420p",
            "-c:a", "aac", "-b:a", "96k", "-shortest", str(target),
        ],
        timeout=600.0,
    )
    return target


def make_subtitle(directory: Path, seconds: float = DEMO_SECONDS) -> Path:
    """生成一份外挂字幕（演示 C3 字幕工作流）。"""
    target = directory / "sample.srt"
    target.write_text(
        "1\n00:00:00,500 --> 00:00:02,500\n演示字幕一 · 学习原型\n\n"
        f"2\n00:00:03,000 --> 00:00:{seconds:04.1f}".replace(".", ",") + "\n演示字幕二 · 仅用于自有素材\n",
        encoding="utf-8",
    )
    return target


def _ensure_identity(conn) -> dict:
    row = conn.execute(
        "SELECT id, name, role FROM users WHERE role IN ('operator','admin') ORDER BY created_at LIMIT 1"
    ).fetchone()
    if row is not None:
        return {"id": row["id"], "name": row["name"], "role": row["role"]}
    token = create_user(conn, "demo", role="operator", actor="demo")
    row = conn.execute("SELECT id, name, role FROM users ORDER BY created_at LIMIT 1").fetchone()
    print(f"已创建演示用户 demo，令牌（只显示一次）：{token}")
    return {"id": row["id"], "name": row["name"], "role": row["role"]}


def _seed_recipes(conn) -> int:
    from .cli import recipes_dir

    published = 0
    root = recipes_dir()
    if not root.is_dir():
        return 0
    for path in sorted(root.glob("*.json")):
        payload = json.loads(path.read_text(encoding="utf-8"))
        recipe = service.validate_recipe_safe(payload)
        if recipe is None:
            continue
        existing = conn.execute(
            "SELECT 1 FROM recipe_versions WHERE recipe_id = %s AND version = %s",
            (recipe.recipe_id, recipe.version),
        ).fetchone()
        if existing:
            continue
        service.publish_recipe(conn, payload, actor="demo", reviewed_by="demo")
        published += 1
    return published


def _run_task(settings: Settings, task_id: str, label: str) -> bool:
    worker = Worker(settings, worker_id="wrk_demo")
    handled = worker.run_once()
    conn = connect(settings)
    try:
        task = service.get_task(conn, task_id)
    finally:
        conn.close()
    ok = task["state"] == "SUCCEEDED"
    print(f"[{label}] {task_id} → {task['state']}（本次 Worker 处理了任务：{handled}）")
    for artifact in task["artifacts"]:
        media = artifact["media"] or {}
        size_mb = artifact["size_bytes"] / 1048576
        detail = ""
        if media.get("width"):
            detail = f" {media['width']}x{media['height']}@{media.get('fps')} {media.get('duration', 0):.2f}s"
        print(f"    产物 {artifact['kind']:<10} {size_mb:6.2f}MB{detail} "
              f"sha256={artifact['sha256'][:12]}…")
    checks = (task["artifacts"][0]["quality"] if task["artifacts"] else {}).get("checks")
    if checks:
        passed = sum(1 for c in checks if c["ok"])
        print(f"    质量门禁 {passed}/{len(checks)} 项通过")
    if task["error"]:
        print(f"    错误 {task['error']['class']}: {task['error']['message'][:200]}")
    return ok


def run_demo(settings: Settings, keep: bool = False) -> int:
    print("=" * 72)
    print("云端短剧视频制作与多平台适配系统 · 端到端演示（仅用于技术学习）")
    print("=" * 72)
    init_db(settings)
    demo_dir = settings.home / "demo"
    demo_dir.mkdir(parents=True, exist_ok=True)
    storage = LocalStorage(settings)
    conn = connect(settings)
    results = {}
    try:
        principal = _ensure_identity(conn)
        print(f"ffmpeg      {ffmpeg_version(settings)}")
        print(f"身份        {principal['name']}（{principal['role']}）")

        published = _seed_recipes(conn)
        print(f"内置配方    新发布 {published} 个")
        conn.commit()

        source = make_source(demo_dir)
        subtitle = make_subtitle(demo_dir)
        print(f"演示素材    {source.name} / {subtitle.name}")

        project_id = service.ensure_default_project(conn, principal["id"], "演示项目")
        video_asset = service.register_local_asset(
            conn, settings, storage, project_id, source, name="演示正片", actor="demo"
        )
        subtitle_asset = service.register_local_asset(
            conn, settings, storage, project_id, subtitle, name="演示字幕", actor="demo"
        )
        probe = video_asset.get("probe") or {}
        print(
            f"素材登记    {video_asset['id']} {probe.get('width')}x{probe.get('height')} "
            f"@ {probe.get('fps')}fps {probe.get('duration', 0):.2f}s "
            f"音轨={probe.get('has_audio')}"
        )
        conn.commit()

        recipes = service.list_recipe_versions(conn)
        by_id = {}
        for row in recipes:
            if row["status"] == "published" and row["recipe_id"] not in by_id:
                by_id[row["recipe_id"]] = row

        # ---- 任务 1：三档模式里的 no_edit（直转码） ----
        if "drama.no_edit.generic" in by_id:
            task = service.create_task(
                conn, settings, project_id, [video_asset["id"]],
                recipe_id="drama.no_edit.generic", actor="demo",
                idempotency_key="demo-no-edit",
            )
            conn.commit()
            results["no_edit"] = _run_task(settings, task["id"], "no_edit")

        # ---- 任务 2：subtitle 模式（外挂字幕烧录） ----
        if "drama.subtitle.burn" in by_id:
            task = service.create_task(
                conn, settings, project_id,
                [video_asset["id"], subtitle_asset["id"]],
                recipe_id="drama.subtitle.burn", actor="demo",
                idempotency_key="demo-subtitle",
            )
            conn.commit()
            results["subtitle"] = _run_task(settings, task["id"], "subtitle")

        # ---- 任务 3：full_edit（时间线剪辑 + 音频规范化 + 章节） ----
        if "drama.full_edit" in by_id:
            task = service.create_task(
                conn, settings, project_id, [video_asset["id"]],
                recipe_id="drama.full_edit", actor="demo",
                user_params={"encode": {"crf": 28}},
                idempotency_key="demo-full-edit",
            )
            conn.commit()
            results["full_edit"] = _run_task(settings, task["id"], "full_edit")

        # ---- 任务 4：C10 分集重组（真的切出多个分集文件） ----
        if "drama.episodes.split" in by_id:
            task = service.create_task(
                conn, settings, project_id, [video_asset["id"]],
                recipe_id="drama.episodes.split", actor="demo",
                idempotency_key="demo-episodes",
            )
            conn.commit()
            results["episodes"] = _run_task(settings, task["id"], "episodes")

        # ---- 幂等键验证 ----
        if results:
            again = service.create_task(
                conn, settings, project_id, [video_asset["id"]],
                recipe_id="drama.no_edit.generic", actor="demo",
                idempotency_key="demo-no-edit",
            )
            same = again["id"] if "no_edit" in results else "-"
            print(f"幂等键验证  重复提交得到同一个任务：{same}")
    finally:
        conn.close()

    print("-" * 72)
    if results and all(results.values()):
        print(f"演示结果    全部通过：{', '.join(f'{k}=OK' for k in results)}")
        code = 0
    elif results:
        print(f"演示结果    部分失败：{results}")
        code = 1
    else:
        print("演示结果    没有可运行的配方，请先运行 drama-cloud seed")
        code = 1

    if not keep:
        shutil.rmtree(demo_dir, ignore_errors=True)
    else:
        print(f"演示临时文件保留在 {demo_dir}")
    return code
