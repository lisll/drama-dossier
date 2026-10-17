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
    #: 授权签名私钥（Ed25519 PEM）的文件路径。私钥本身不进环境变量，也不进日志。
    license_key_file: str
    product_name: str = "剧备 DramaDossier"
    terms_version: str = "2026-10-draft"
    #: 前面有几层可信的反向代理。0 表示直接面向客户端，不读 X-Forwarded-For。
    trusted_proxies: int = 0
    pool_min_size: int = 1
    pool_max_size: int = 10
    #: 同时进行的密码哈希上限。默认参数下每次哈希占 64 MiB 内存，不设上限时并发登录能把内存撑爆。
    hash_concurrency: int = 4
    #: 每个账号同时绑定的电脑数。
    device_limit: int = 1
    #: 解绑（踢设备）次数上限：每 unbind_window_days 天最多 unbind_limit 次，防止一个账号轮流给多台电脑用。
    unbind_limit: int = 3
    unbind_window_days: int = 30
    #: 授权凭证有效期，也就是断网后还能用多久（离线宽限期）。
    license_ttl_hours: int = 72
    #: 联网时桌面端每隔多久刷新一次凭证，让断网时剩下的宽限期接近完整的 license_ttl_hours。
    license_refresh_hours: int = 1
    #: 报审包 PDF 用的中文字体（.otf / .ttf / .ttc）。不设时按常见系统路径找 Noto Sans CJK SC。
    pdf_font: Optional[str] = None
    #: `dossier-artifacts` 可执行文件。不设时按 PATH → target/release → target/debug 找。
    artifacts_bin: Optional[str] = None
    #: 本服务的对外地址（例如 `https://drama.example.com`），用来拼支付回调地址。
    #:
    #: 支付渠道是**主动把通知推到这个地址**的，所以它必须能从公网访问。
    #: 本地开发用 Mock 支付不需要它；接支付宝沙箱时要配（本机开发得用内网穿透）。
    #: 没配时，真实渠道下单会明确报「未配置对外地址」，而不是拼出一个假地址。
    public_base_url: Optional[str] = None
    #: 模拟支付的回调签名密钥（HMAC-SHA256）。只在 mock 渠道用，不进日志。
    #:
    #: 开发环境必须显式配置（哪怕是一个固定串）：留空时 Mock 渠道会报「未配置」，
    #: 而不是用空密钥签名——空密钥等于谁都能伪造回调。
    mock_pay_secret: str = field(default="", repr=False)
    #: 支付宝沙箱凭证（M5a 的 5a-7 才用；现在只有占位实现）。
    alipay_app_id: Optional[str] = None
    alipay_private_key: Optional[str] = field(default=None, repr=False)
    alipay_public_key: Optional[str] = None
    alipay_gateway: Optional[str] = None
    #: 微信支付凭证（个人申请不了商户号，本期只留接口）。
    wechat_mch_id: Optional[str] = None
    wechat_api_v3_key: Optional[str] = field(default=None, repr=False)
    wechat_private_key: Optional[str] = field(default=None, repr=False)
    wechat_serial_no: Optional[str] = None


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
    license_key_file = _get(env, "LICENSE_KEY_FILE")
    if not license_key_file:
        raise ConfigError(
            f"缺少 {ENV_PREFIX}LICENSE_KEY_FILE（授权签名私钥的文件路径，可以用 drama-commerce keygen 生成）"
        )
    ttl_hours = _int(env, "LICENSE_TTL_HOURS", 72, 1, 720)
    refresh_hours = _int(env, "LICENSE_REFRESH_HOURS", 1, 1, 720)
    if refresh_hours >= ttl_hours:
        raise ConfigError(f"{ENV_PREFIX}LICENSE_REFRESH_HOURS 必须小于 {ENV_PREFIX}LICENSE_TTL_HOURS")
    return CommerceSettings(
        dsn=dsn,
        code_secret=code_secret,
        smtp=smtp,
        license_key_file=license_key_file,
        product_name=_get(env, "PRODUCT_NAME") or CommerceSettings.product_name,
        terms_version=_get(env, "TERMS_VERSION") or CommerceSettings.terms_version,
        trusted_proxies=_int(env, "TRUSTED_PROXIES", 0, 0, 5),
        pool_min_size=_int(env, "POOL_MIN_SIZE", 1, 1, 50),
        pool_max_size=_int(env, "POOL_MAX_SIZE", 10, 1, 200),
        hash_concurrency=_int(env, "HASH_CONCURRENCY", 4, 1, 64),
        device_limit=_int(env, "DEVICE_LIMIT", 1, 1, 20),
        unbind_limit=_int(env, "UNBIND_LIMIT", 3, 1, 100),
        unbind_window_days=_int(env, "UNBIND_WINDOW_DAYS", 30, 1, 365),
        license_ttl_hours=ttl_hours,
        license_refresh_hours=refresh_hours,
        pdf_font=_get(env, "PDF_FONT") or None,
        artifacts_bin=_get(env, "ARTIFACTS_BIN") or None,
        public_base_url=_get(env, "PUBLIC_BASE_URL") or None,
        mock_pay_secret=_get(env, "MOCK_PAY_SECRET"),
        alipay_app_id=_get(env, "ALIPAY_APP_ID") or None,
        alipay_private_key=_get(env, "ALIPAY_PRIVATE_KEY") or None,
        alipay_public_key=_get(env, "ALIPAY_PUBLIC_KEY") or None,
        alipay_gateway=_get(env, "ALIPAY_GATEWAY") or None,
        wechat_mch_id=_get(env, "WECHAT_MCH_ID") or None,
        wechat_api_v3_key=_get(env, "WECHAT_API_V3_KEY") or None,
        wechat_private_key=_get(env, "WECHAT_PRIVATE_KEY") or None,
        wechat_serial_no=_get(env, "WECHAT_SERIAL_NO") or None,
    )
