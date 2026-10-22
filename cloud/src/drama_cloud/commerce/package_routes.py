"""报审包与处理配方的接口。都要求登录、这台电脑绑定在当前账号上、使用权有效。"""

from __future__ import annotations

import uuid

from fastapi import APIRouter, Depends, Request

from . import ratelimit
from .deps import get_conn, require_auth
from .ratelimit import Rule
from .schemas import PackageRequest, RecipeConsumeRequest, RecipeRequest
from .sessions import AuthContext

PACKAGE_PER_USER = Rule("package:user", 30, 3600)
RECIPE_PER_USER = Rule("recipe:user", 60, 3600)
RECIPE_CONSUME_PER_USER = Rule("recipe_consume:user", 120, 3600)

router = APIRouter(prefix="/api/v1")

# 处理配方的路径用 `version-recipes`，不用 `recipes`：
# 媒体控制面早就有一套 `/api/v1/recipes`（处理流程的配方目录，含网页控制台），
# 两个服务在部署时共用一个入口地址（Caddy），同一路径只能归一个服务。
# 这套是「处理版本」的一次性签名配方，名字也更贴切。


@router.post("/packages")
def create_package(
    payload: PackageRequest,
    request: Request,
    ctx: AuthContext = Depends(require_auth),
    conn=Depends(get_conn),
):
    ratelimit.hit(conn, PACKAGE_PER_USER, ctx.user_id)
    return request.app.state.packages.create(conn, ctx, payload)


@router.post("/version-recipes")
def issue_recipe(
    payload: RecipeRequest,
    request: Request,
    ctx: AuthContext = Depends(require_auth),
    conn=Depends(get_conn),
):
    ratelimit.hit(conn, RECIPE_PER_USER, ctx.user_id)
    return request.app.state.recipes.issue(conn, ctx, payload)


@router.post("/version-recipes/{recipe_id}/consume")
def consume_recipe(
    recipe_id: uuid.UUID,
    payload: RecipeConsumeRequest,
    request: Request,
    ctx: AuthContext = Depends(require_auth),
    conn=Depends(get_conn),
):
    ratelimit.hit(conn, RECIPE_CONSUME_PER_USER, ctx.user_id)
    return request.app.state.recipes.consume(conn, ctx, str(recipe_id), payload.device_id)
