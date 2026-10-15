"""请求体模型。字段校验失败返回 422，提示是中文；多出来的字段一律拒绝。"""

from __future__ import annotations

import re
import unicodedata
from typing import Annotated, Literal, Optional

from pydantic import AfterValidator, StrictBool, field_validator

from ..base import StrictModel
from . import cards

_EMAIL = re.compile(r"^[^@\s]+@[^@\s]+\.[^@\s]+$")
_CODE = re.compile(r"^[0-9]{6}$")
PASSWORD_MIN = 8
PASSWORD_MAX = 128


def _email(value: str) -> str:
    value = value.strip().lower()
    if len(value) > 254 or not value.isascii() or not _EMAIL.match(value):
        raise ValueError("邮箱格式不正确")
    return value


def _code(value: str) -> str:
    value = value.strip()
    if not _CODE.match(value):
        raise ValueError("验证码是 6 位数字")
    return value


def _new_password(value: str) -> str:
    if not PASSWORD_MIN <= len(value) <= PASSWORD_MAX:
        raise ValueError(f"密码长度要在 {PASSWORD_MIN} 到 {PASSWORD_MAX} 个字符之间")
    return value


def _any_password(value: str) -> str:
    # 登录时只挡住空值和超长输入，不检查长度下限：密码规则以后调整了，老密码也要能登录。
    if not 1 <= len(value) <= PASSWORD_MAX:
        raise ValueError("请输入密码")
    return value


def _refresh_token(value: str) -> str:
    if not 1 <= len(value) <= 256:
        raise ValueError("刷新令牌格式不正确")
    return value


Email = Annotated[str, AfterValidator(_email)]
Code = Annotated[str, AfterValidator(_code)]
NewPassword = Annotated[str, AfterValidator(_new_password)]
AnyPassword = Annotated[str, AfterValidator(_any_password)]


class SendCodeRequest(StrictModel):
    email: Email
    purpose: Literal["register", "reset_password"]


class RegisterRequest(StrictModel):
    email: Email
    code: Code
    password: NewPassword
    #: 只接受 JSON 的 true，不接受 "true"、1 之类的值：同意条款必须是明确的动作。
    accept_terms: StrictBool

    @field_validator("accept_terms")
    @classmethod
    def _must_accept(cls, value: bool) -> bool:
        if value is not True:
            raise ValueError("请先阅读并同意用户协议和隐私政策")
        return value


class LoginRequest(StrictModel):
    email: Email
    password: AnyPassword


class RefreshRequest(StrictModel):
    refresh_token: Annotated[str, AfterValidator(_refresh_token)]


class ResetPasswordRequest(StrictModel):
    email: Email
    code: Code
    new_password: NewPassword


class ChangePasswordRequest(StrictModel):
    current_password: AnyPassword
    new_password: NewPassword


class DeleteAccountRequest(StrictModel):
    #: 注销前必须重新输入密码：被偷的访问令牌不能直接拿来注销账号。
    password: AnyPassword


# ---------- 卡密与授权 ----------

#: 机器码：桌面端算出的 SHA-256 摘要，64 位小写十六进制。只认这一种写法，免得同一台电脑被当成两台。
_DEVICE_ID = re.compile(r"[0-9a-f]{64}")
_APP_VERSION = re.compile(r"[0-9][0-9A-Za-z.+-]{0,31}")


def _card_code(value: str) -> str:
    code = cards.normalize(value) if len(value) <= 64 else None
    if code is None:
        raise ValueError(cards.FORMAT_MESSAGE)
    return code


def _device_id(value: str) -> str:
    if not _DEVICE_ID.fullmatch(value):
        raise ValueError("机器码格式不正确")
    return value


def _device_name(value: str) -> str:
    value = value.strip()
    # 设备名会显示在设备列表里，不接受控制字符和零宽字符之类看不见的字符。
    # 全角空格、不换行空格这类看得见的空白（Unicode Zs 类）要放行：macOS 的电脑名可以含这些字符，
    # 而 str.isprintable() 只放行半角空格。
    visible = all(char.isprintable() or unicodedata.category(char) == "Zs" for char in value)
    if not 1 <= len(value) <= 64 or not visible:
        raise ValueError("设备名称要在 1 到 64 个字符之间，且不能含不可见字符")
    return value


def _app_version(value: str) -> str:
    if not _APP_VERSION.fullmatch(value):
        raise ValueError("版本号格式不正确")
    return value


DeviceId = Annotated[str, AfterValidator(_device_id)]
AppVersion = Annotated[str, AfterValidator(_app_version)]


class RedeemRequest(StrictModel):
    code: Annotated[str, AfterValidator(_card_code)]


class ActivateRequest(StrictModel):
    device_id: DeviceId
    name: Annotated[str, AfterValidator(_device_name)]
    platform: Literal["windows", "macos", "linux"]
    app_version: AppVersion


class LicenseRefreshRequest(StrictModel):
    device_id: DeviceId
    app_version: Optional[AppVersion] = None
