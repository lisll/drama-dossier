"""测试夹具。

关键约定：每个测试都用自己的 `DRAMA_CLOUD_HOME`，所以数据库、存储与工作目录互不干扰；
媒体素材是**用 ffmpeg 现场合成**的（不含任何第三方内容），并且按会话缓存一次，
避免每个测试都重新编码。
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

import pytest

from drama_cloud.auth import create_user
from drama_cloud.config import load_settings
from drama_cloud.db import connect, init_db
from drama_cloud.modules.ffmpeg import which
from drama_cloud.storage import LocalStorage
from drama_cloud.tasks import service

RECIPES_DIR = Path(__file__).resolve().parents[1] / "recipes"

HAVE_FFMPEG = bool(which("ffmpeg") and which("ffprobe"))
requires_ffmpeg = pytest.mark.skipif(not HAVE_FFMPEG, reason="需要 ffmpeg/ffprobe")


@pytest.fixture()
def settings(tmp_path):
    home = tmp_path / "home"
    settings = load_settings(home=home, heartbeat_seconds=1.0, lease_seconds=5.0)
    init_db(settings)
    return settings


@pytest.fixture()
def conn(settings):
    connection = connect(settings)
    try:
        yield connection
    finally:
        connection.close()


@pytest.fixture()
def storage(settings):
    return LocalStorage(settings)


def _seed_recipes(conn) -> None:
    """把内置配方发布一次，让任务能有配方可用。"""
    for path in sorted(RECIPES_DIR.glob("*.json")):
        payload = json.loads(path.read_text(encoding="utf-8"))
        service.publish_recipe(
            conn, payload, actor="test", status=str(payload.get("status") or "published")
        )


@pytest.fixture()
def project(conn):
    """一个有权限的用户 + 一个默认项目 + 已发布的内置配方。"""
    token = create_user(conn, "tester", role="admin", actor="test")
    row = conn.execute("SELECT id FROM users ORDER BY created_at LIMIT 1").fetchone()
    project_id = service.ensure_default_project(conn, row["id"])
    _seed_recipes(conn)
    return project_id, row["id"], token


@pytest.fixture(scope="session")
def media_cache(tmp_path_factory) -> Path:
    """会话级素材缓存：2 秒 160x120 的合成视频 + 外挂字幕。"""
    directory = tmp_path_factory.mktemp("media")
    if not HAVE_FFMPEG:
        return directory
    source = directory / "tiny.mp4"
    subprocess.run(
        [
            "ffmpeg", "-hide_banner", "-loglevel", "error", "-nostdin", "-y",
            "-f", "lavfi", "-i", "testsrc2=size=160x120:rate=10:duration=2",
            "-f", "lavfi", "-i", "sine=frequency=440:duration=2",
            "-c:v", "libx264", "-preset", "ultrafast", "-crf", "34", "-pix_fmt", "yuv420p",
            "-c:a", "aac", "-b:a", "64k", "-shortest", str(source),
        ],
        check=True,
    )
    (directory / "tiny.srt").write_text(
        "1\n00:00:00,200 --> 00:00:01,200\n测试字幕\n", encoding="utf-8"
    )
    return directory


@pytest.fixture()
def tiny_video(media_cache) -> Path:
    path = media_cache / "tiny.mp4"
    if not path.exists():
        pytest.skip("无法合成测试素材")
    return path


@pytest.fixture()
def tiny_subtitle(media_cache) -> Path:
    path = media_cache / "tiny.srt"
    if not path.exists():
        pytest.skip("无法合成测试素材")
    return path


@pytest.fixture()
def seeded(settings, conn):
    """只发布内置配方（不需要用户/项目的用例用这个）。"""
    _seed_recipes(conn)
    return settings
