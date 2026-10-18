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

from drama_cloud import identity
from drama_cloud.auth import create_user
from drama_cloud.config import load_settings
from drama_cloud.db import connect, init_db
from drama_cloud.modules.ffmpeg import which
from drama_cloud.storage import LocalStorage
from drama_cloud.tasks import service

from .pg import temporary_database

RECIPES_DIR = Path(__file__).resolve().parents[1] / "recipes"

HAVE_FFMPEG = bool(which("ffmpeg") and which("ffprobe"))
requires_ffmpeg = pytest.mark.skipif(not HAVE_FFMPEG, reason="需要 ffmpeg/ffprobe")


@pytest.fixture()
def settings(tmp_path):
    """每个测试一套独立的 home 目录 + 一个独立的临时 PostgreSQL 库。

    M5b 起媒体控制面也走 PostgreSQL，所以测试需要 `COMMERCE_TEST_DSN`
    （指向一个有建库权限的账号）。每个测试建一个随机名的库、跑完删掉，
    互相之间完全不共享状态——和原来「每个测试一个 SQLite 文件」等价。
    """
    with temporary_database(prefix="media_test_") as dsn:
        settings = load_settings(
            home=tmp_path / "home",
            heartbeat_seconds=1.0,
            lease_seconds=5.0,
            dsn=dsn,
        )
        init_db(settings)
        yield settings


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
def fake_commerce(monkeypatch):
    """把「向 commerce 内省令牌」换成一张本地映射表。

    M5b 起媒体控制面的认证要问 commerce。单元测试不该真的起一个 commerce，
    所以这里把 `identity.introspect` 打桩：注册的令牌→身份，其余令牌一律无效。

    用法：
        user = fake_commerce.register("a@example.com")
        fake_commerce.token("a@example.com")   # 拿一个能用的令牌
    """

    class FakeCommerce:
        def __init__(self):
            self.by_token = {}
            self.unavailable = False

        def register(self, email: str, user_id: str | None = None):
            user_id = user_id or f"00000000-0000-4000-8000-{len(self.by_token):012d}"
            token = f"token-{email}-{len(self.by_token)}"
            self.by_token[token] = identity.Identity(user_id=user_id, email=email)
            return user_id, token

        def token(self, email: str) -> str:
            for token, ident in self.by_token.items():
                if ident.email == email:
                    return token
            raise KeyError(email)

        def user(self, conn, email: str, role: str):
            """注册一个身份并按角色准备好本地权限，返回令牌。

            角色只有两种落点：admin 进 `media_admins` 白名单；
            viewer/operator 的差别不在白名单，而在 `media_admins` 之外的
            `media_viewers`（只读）。这样测试里三种角色都能造出来。
            """
            user_id, token = self.register(email)
            if role == "admin":
                conn.execute(
                    "INSERT INTO media_admins (email, created_at) VALUES (%s,%s)"
                    " ON CONFLICT (email) DO NOTHING",
                    (email, "2026-01-01T00:00:00+00:00"),
                )
            elif role == "viewer":
                conn.execute(
                    "INSERT INTO media_viewers (email, created_at) VALUES (%s,%s)"
                    " ON CONFLICT (email) DO NOTHING",
                    (email, "2026-01-01T00:00:00+00:00"),
                )
            return token

    fake = FakeCommerce()

    def _introspect(base_url, internal_secret, access_token):
        if fake.unavailable:
            raise identity.IntrospectUnavailable("账号服务暂时不可用")
        return fake.by_token.get(access_token)

    monkeypatch.setattr(identity, "introspect", _introspect)
    # api.py 是 `from .identity import introspect`，也要一起打桩。
    import drama_cloud.api as api_module

    monkeypatch.setattr(api_module, "introspect", _introspect)
    return fake


@pytest.fixture()
def project(conn, fake_commerce):
    """一个有权限的用户 + 一个默认项目 + 已发布的内置配方。

    M5b 起身份来自 commerce：这里注册一个邮箱并在白名单里设为管理员，
    令牌由假 commerce 发放（认证链完整走一遍，只是不真的发 HTTP）。
    """
    email = "tester@example.com"
    user_id, token = fake_commerce.register(email)
    from drama_cloud.auth import add_admin, principal_for_email

    add_admin(conn, email, actor="test")
    # 建影子用户行：projects.owner_id 等地方有 REFERENCES users(id)，
    # 本地没有这一行就建不了项目。principal_for_email 会顺手建好它。
    principal_for_email(conn, user_id, email)
    project_id = service.ensure_default_project(conn, user_id)
    _seed_recipes(conn)
    return project_id, user_id, token


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
