"""部署入口（Caddy）的路由一致性。

部署时两个服务共用一个 https 入口：Caddyfile 按路径把请求分给 commerce 或媒体控制面。
漏配一条，桌面端的那类请求就会落到另一个服务——之前报审包与处理配方就是这样：
报审包 404，`POST /api/v1/recipes` 撞上媒体控制面同名的配方目录接口。端到端脚本都直连
服务端口、不经过 Caddy，所以测不出来。

这里读**真实的** Caddyfile 和两个服务**真实的**路由表，按 Caddy 的匹配规则逐条判断
每条路由会落到哪个服务。以后加接口忘了配 Caddy，这条测试会直接失败。

Caddy 的规则（官方文档 caddyfile/directives 的 Sorting algorithm）：
同名指令（这里全是 `handle`）若都只带一个路径匹配器，按路径长度从长到短排，
长度不计末尾的 `*`；其余相同时不带 `*` 的更具体；同长按字母序；没有匹配器的排最后。
`handle` 之间互斥：第一个匹配上的生效。
"""

from __future__ import annotations

import re
from pathlib import Path
from typing import List, Optional, Tuple

import pytest
from fastapi.routing import APIRoute

from drama_cloud.api import create_app as create_media_app
from drama_cloud.commerce.api import create_app as create_commerce_app

from .conftest import FAST_HASHER

CADDYFILE = Path(__file__).resolve().parents[2] / "Caddyfile"

#: FastAPI 自带的文档路由：两个服务都有，经 Caddy 只能到媒体控制面，桌面端不用它们。
FRAMEWORK_PATHS = {"/openapi.json", "/docs", "/docs/oauth2-redirect", "/redoc"}

#: 两个服务都有、经 Caddy 只能到其中一个的路径，以及这样是可以接受的原因。
#: 新加条目必须写清楚理由——这张表不是「测试过不了就往里塞」的地方。
SHARED_PATHS = {
    # 健康检查两边都有。经 Caddy 访问到的是媒体控制面的；commerce 的健康检查
    # 由编排在容器网络里直接访问，不经过 Caddy（DEPLOYMENT.md 第 9 节）。
    "/api/v1/health": "media",
}

Handle = Tuple[Optional[str], str]  # (路径匹配器；None 表示没有匹配器, 目标服务)


def _https_site_block(text: str) -> List[str]:
    """取出第一个站点块（https 入口）里的行，去掉注释与空行。

    顶层块有两种：只有一个 `{` 的是全局选项块（跳过），`<地址> {` 的是站点块。
    第一个站点块就是 https 入口；后面的明文端口只做跳转。
    """
    depth = 0
    collecting = False
    block: List[str] = []
    for raw in text.splitlines():
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        if depth == 0:
            if line.endswith("{"):
                depth = 1
                # 只有 `{` 的是全局选项块；带地址的才是站点块。
                collecting = line != "{"
                block = []
            continue
        if line.endswith("{"):
            depth += 1
        elif line == "}":
            depth -= 1
            if depth == 0:
                if collecting:
                    return block
                continue
        if collecting:
            block.append(line)
    raise AssertionError("Caddyfile 里没有找到站点块")


def parse_handles(text: str) -> List[Handle]:
    """把站点块里的 `handle` 解析成 (匹配器, 目标)。目标：commerce / media / blocked。"""
    lines = _https_site_block(text)
    handles: List[Handle] = []
    index = 0
    while index < len(lines):
        line = lines[index]
        match = re.fullmatch(r"handle(?:\s+(\S+))?\s*\{", line)
        if not match:
            index += 1
            continue
        matcher = match.group(1)
        target = None
        index += 1
        while index < len(lines) and lines[index] != "}":
            body = lines[index]
            if body.startswith("reverse_proxy commerce:"):
                target = "commerce"
            elif body.startswith("reverse_proxy media:"):
                target = "media"
            elif body.startswith("respond "):
                target = "blocked"
            index += 1
        assert target is not None, f"handle {matcher} 里没有认得出的目标"
        if matcher is not None:
            # 这里只实现了「精确路径」和「末尾一个 *」两种写法。用了别的写法
            # （命名匹配器、中间带 *、多个路径），排序规则就不一样了，直接让测试失败，
            # 逼着改测试的人去对照官方文档，而不是让这里默默算错。
            assert matcher.startswith("/"), f"不支持的匹配器写法：{matcher}"
            assert "*" not in matcher[:-1], f"不支持的匹配器写法：{matcher}"
        handles.append((matcher, target))
        index += 1
    assert handles, "站点块里没有 handle"
    return handles


def _sort_key(handle: Handle):
    matcher, _ = handle
    if matcher is None:
        return (1, 0, 0, "")
    bare = matcher[:-1] if matcher.endswith("*") else matcher
    has_wildcard = matcher.endswith("*")
    # 长的在前；同长时不带 * 的在前；再按字母序。
    return (0, -len(bare), 1 if has_wildcard else 0, bare)


def _matches(matcher: Optional[str], path: str) -> bool:
    if matcher is None:
        return True
    path = path.lower()
    if matcher.endswith("*"):
        return path.startswith(matcher[:-1].lower())
    return path == matcher.lower()


def resolve(handles: List[Handle], path: str) -> str:
    """按 Caddy 的规则算出这个路径会被哪个 handle 接住。"""
    for matcher, target in sorted(handles, key=_sort_key):
        if _matches(matcher, path):
            return target
    return "unmatched"


def concrete(template: str) -> str:
    """把 `/tasks/{task_id}` 这类模板换成一个具体路径。"""
    return re.sub(r"\{[^}]+\}", "x0", template)


def route_paths(app) -> List[str]:
    paths = set()
    for route in app.routes:
        if isinstance(route, APIRoute) and route.path not in FRAMEWORK_PATHS:
            paths.add(route.path)
    return sorted(paths)


@pytest.fixture()
def handles() -> List[Handle]:
    return parse_handles(CADDYFILE.read_text(encoding="utf-8"))


@pytest.fixture()
def commerce_routes(commerce_settings, pool, mailer) -> List[str]:
    # 不进 TestClient：只要路由表，不需要跑生命周期。
    app = create_commerce_app(commerce_settings, pool=pool, mailer=mailer, hasher=FAST_HASHER)
    return route_paths(app)


@pytest.fixture()
def media_routes(settings) -> List[str]:
    return route_paths(create_media_app(settings))


def test_the_matcher_follows_caddys_documented_order():
    """先钉住匹配规则本身：用官方文档里的例子。"""
    handles = [("/foo*", "a"), ("/foo", "b"), ("/foobar", "c"), ("/foo/*", "d"), (None, "z")]
    assert resolve(handles, "/foobar") == "c"
    assert resolve(handles, "/foo") == "b", "/foo 比 /foo* 更具体"
    assert resolve(handles, "/foo/x") == "d", "/foo/* 比 /foo* 更具体"
    assert resolve(handles, "/food") == "a"
    assert resolve(handles, "/other") == "z", "没有匹配器的排最后"


def test_every_commerce_route_reaches_commerce(handles, commerce_routes):
    wrong = []
    for path in commerce_routes:
        expected = "blocked" if path.startswith("/internal/") else SHARED_PATHS.get(path, "commerce")
        actual = resolve(handles, concrete(path))
        if actual != expected:
            wrong.append(f"{path} → {actual}（应为 {expected}）")
    assert not wrong, "经 Caddy 落错服务的 commerce 路由：\n" + "\n".join(wrong)


def test_every_media_route_reaches_media(handles, media_routes):
    wrong = []
    for path in media_routes:
        expected = SHARED_PATHS.get(path, "media")
        actual = resolve(handles, concrete(path))
        if actual != expected:
            wrong.append(f"{path} → {actual}（应为 {expected}）")
    assert not wrong, "经 Caddy 落错服务的媒体控制面路由：\n" + "\n".join(wrong)


def test_the_two_services_do_not_share_paths(commerce_routes, media_routes):
    """同一路径两边都有，经 Caddy 必有一边到不了。除非在 SHARED_PATHS 里写明理由。"""
    shared = (set(commerce_routes) & set(media_routes)) - set(SHARED_PATHS)
    assert not shared, f"两个服务撞了路径：{sorted(shared)}"


def test_the_internal_api_is_never_exposed(handles):
    assert resolve(handles, "/internal/credits/hold") == "blocked"


def test_the_desktop_paths_land_where_the_desktop_expects(handles):
    """桌面端实际调用的几类路径（session.rs），逐类钉住。"""
    assert resolve(handles, "/api/v1/packages") == "commerce"
    assert resolve(handles, "/api/v1/version-recipes") == "commerce"
    assert resolve(handles, "/api/v1/version-recipes/x0/consume") == "commerce"
    assert resolve(handles, "/api/v1/assets/uploads") == "media"
    assert resolve(handles, "/api/v1/tasks/x0") == "media"
    assert resolve(handles, "/api/v1/artifacts/x0") == "media"
    # 媒体控制面网页控制台的「当前身份」。
    assert resolve(handles, "/api/v1/auth/whoami") == "media"
    assert resolve(handles, "/api/v1/auth/login") == "commerce"
