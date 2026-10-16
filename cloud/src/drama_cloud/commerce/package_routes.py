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


@router.post("/packages")
def create_package(
    payload: PackageRequest,
    request: Request,
    ctx: AuthContext = Depends(require_auth),
    conn=Depends(get_conn),
):
    ratelimit.hit(conn, PACKAGE_PER_USER, ctx.user_id)
    return request.app.state.packages.create(conn, ctx, payload)


@router.post("/recipes")
def issue_recipe(
    payload: RecipeRequest,
    request: Request,
    ctx: AuthContext = Depends(require_auth),
    conn=Depends(get_conn),
):
    ratelimit.hit(conn, RECIPE_PER_USER, ctx.user_id)
    return request.app.state.recipes.issue(conn, ctx, payload)


@router.post("/recipes/{recipe_id}/consume")
def consume_recipe(
    recipe_id: uuid.UUID,
    payload: RecipeConsumeRequest,
    request: Request,
    ctx: AuthContext = Depends(require_auth),
    conn=Depends(get_conn),
):
    ratelimit.hit(conn, RECIPE_CONSUME_PER_USER, ctx.user_id)
    return request.app.state.recipes.consume(conn, ctx, str(recipe_id), payload.device_id)
