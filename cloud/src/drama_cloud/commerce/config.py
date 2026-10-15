"""服务配置：全部来自环境变量（前缀 `DRAMA_COMMERCE_`），缺了关键项就拒绝启动。

密钥类配置（验证码密钥、SMTP 密码）只从环境变量读，不进仓库，也不进日志。
"""

from __future__ import annotations

import ipaddress
import os
from dataclasses import dataclass, field
from typing import Mapping, Optional

ENV_PREFIX = "DRAMA_COMMERCE_"
MIN_SECRET_LENGTH = 32
SMTP_SECURITY_MODES = ("starttls", "ssl", "plain")
_DEFAULT_PORTS = {"starttls": 587, "ssl": 465, "plain": 25}


class ConfigError(ValueError):
    """配置缺失或取值非法。消息里只出现变量名，不出现变量值。"""


@dataclass(frozen=True)
class SmtpSettings:
    host: str
    port: int
    security: str
    sender: str
    username: Optional[str] = None
    password: Optional[str] = field(default=None, repr=False)
    timeout_seconds: float = 10.0


@dataclass(frozen=True)
class CommerceSettings:
    dsn: str = field(repr=False)
    code_secret: str = field(repr=False)
    smtp: SmtpSettings
    product_name: str = "剧备 DramaDossier"
    terms_version: str = "2026-10-draft"
    #: 前面有几层可信的反向代理。0 表示直接面向客户端，不读 X-Forwarded-For。
    trusted_proxies: int = 0
    pool_min_size: int = 1
    pool_max_size: int = 10
    #: 同时进行的密码哈希上限。默认参数下每次哈希占 64 MiB 内存，不设上限时并发登录能把内存撑爆。
    hash_concurrency: int = 4


def _get(env: Mapping[str, str], name: str) -> str:
    return env.get(ENV_PREFIX + name, "").strip()


def _int(env: Mapping[str, str], name: str, default: int, low: int, high: int) -> int:
    raw = _get(env, name)
    if not raw:
        return default
    try:
        value = int(raw)
    except ValueError as error:
        raise ConfigError(f"{ENV_PREFIX}{name} 必须是整数") from error
    if not low <= value <= high:
        raise ConfigError(f"{ENV_PREFIX}{name} 必须在 {low}..{high} 之间")
    return value


def _is_loopback(host: str) -> bool:
    if host.lower() == "localhost":
        return True
    try:
        return ipaddress.ip_address(host).is_loopback
    except ValueError:
        return False


def load_dsn(env: Optional[Mapping[str, str]] = None) -> str:
    """迁移命令只需要数据库地址，不要求其他配置齐全。"""
    env = os.environ if env is None else env
    dsn = _get(env, "DSN")
    if not dsn:
        raise ConfigError(f"缺少 {ENV_PREFIX}DSN（PostgreSQL 连接串）")
    return dsn


def load_settings(env: Optional[Mapping[str, str]] = None) -> CommerceSettings:
    env = os.environ if env is None else env
    dsn = load_dsn(env)
    code_secret = _get(env, "CODE_SECRET")
    if len(code_secret) < MIN_SECRET_LENGTH:
        raise ConfigError(f"{ENV_PREFIX}CODE_SECRET 至少 {MIN_SECRET_LENGTH} 个字符")

    security = _get(env, "SMTP_SECURITY").lower() or "starttls"
    if security not in SMTP_SECURITY_MODES:
        raise ConfigError(f"{ENV_PREFIX}SMTP_SECURITY 只能是 {'/'.join(SMTP_SECURITY_MODES)}")
    host = _get(env, "SMTP_HOST")
    if not host:
        raise ConfigError(f"缺少 {ENV_PREFIX}SMTP_HOST（发验证码要用）")
    if security == "plain" and not _is_loopback(host):
        # 明文 SMTP 只允许连本机（开发用的 Mailpit），验证码不能明文走公网。
        raise ConfigError(f"{ENV_PREFIX}SMTP_SECURITY=plain 只允许连接本机地址")
    smtp = SmtpSettings(
        host=host,
        port=_int(env, "SMTP_PORT", _DEFAULT_PORTS[security], 1, 65535),
        security=security,
        sender=_get(env, "SMTP_FROM") or f"no-reply@{host}",
        username=_get(env, "SMTP_USERNAME") or None,
        password=env.get(ENV_PREFIX + "SMTP_PASSWORD") or None,
    )
    return CommerceSettings(
        dsn=dsn,
        code_secret=code_secret,
        smtp=smtp,
        product_name=_get(env, "PRODUCT_NAME") or CommerceSettings.product_name,
        terms_version=_get(env, "TERMS_VERSION") or CommerceSettings.terms_version,
        trusted_proxies=_int(env, "TRUSTED_PROXIES", 0, 0, 5),
        pool_min_size=_int(env, "POOL_MIN_SIZE", 1, 1, 50),
        pool_max_size=_int(env, "POOL_MAX_SIZE", 10, 1, 200),
        hash_concurrency=_int(env, "HASH_CONCURRENCY", 4, 1, 64),
    )
