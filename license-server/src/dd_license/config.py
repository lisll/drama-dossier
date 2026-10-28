"""运行配置：全部来自环境变量，缺省值只适合本机开发。"""

from __future__ import annotations

import ipaddress
import os
from dataclasses import dataclass, field
from pathlib import Path


class ConfigError(RuntimeError):
    pass


def _int_env(name: str, default: int, lo: int, hi: int) -> int:
    raw = os.environ.get(name)
    if raw is None or raw == "":
        return default
    try:
        value = int(raw)
    except ValueError:
        raise ConfigError(f"{name} 必须是整数，当前为 {raw!r}") from None
    if not lo <= value <= hi:
        raise ConfigError(f"{name} 必须在 {lo}–{hi} 之间，当前为 {value}")
    return value


def _flag(name: str) -> bool:
    return os.environ.get(name, "").strip() in {"1", "true", "yes"}


def parse_networks(raw: str) -> list[ipaddress._BaseNetwork]:
    nets = []
    for part in raw.split(","):
        part = part.strip()
        if not part:
            continue
        try:
            nets.append(ipaddress.ip_network(part, strict=False))
        except ValueError:
            raise ConfigError(f"DD_LICENSE_ADMIN_ALLOW_IPS 中有无效地址：{part!r}") from None
    return nets


@dataclass
class Settings:
    db_path: Path
    key_file: Path
    # 离线凭证有效期（天）：也是「停用后一直不联网还能用多久」的上限。客户端每 24 小时续签一次，3 天留出 2 天余量。
    token_days: int = 3
    admin_allow_ips: list = field(default_factory=list)
    admin_allow_any_ip: bool = False
    trust_proxy: bool = False
    # 只在 HTTPS 背后运行时才应关闭；测试和本机 http://127.0.0.1 仍可用（浏览器把它视作安全来源）。
    cookie_secure: bool = True
    # 密钥轮换期间仍认可的旧公钥（base64url，逗号分隔）：只用于接受旧凭证来续签，新凭证一律用当前私钥签。
    extra_pubkeys: str = ""

    @property
    def admin_enabled(self) -> bool:
        return bool(self.admin_allow_ips) or self.admin_allow_any_ip


def load_settings() -> Settings:
    base = Path(os.environ.get("DD_LICENSE_HOME", "var"))
    return Settings(
        db_path=Path(os.environ.get("DD_LICENSE_DB", str(base / "license.sqlite3"))),
        key_file=Path(os.environ.get("DD_LICENSE_KEY_FILE", str(base / "key.pem"))),
        token_days=_int_env("DD_LICENSE_TOKEN_DAYS", 3, 1, 30),
        admin_allow_ips=parse_networks(os.environ.get("DD_LICENSE_ADMIN_ALLOW_IPS", "")),
        admin_allow_any_ip=_flag("DD_LICENSE_ADMIN_ALLOW_ANY_IP"),
        trust_proxy=_flag("DD_LICENSE_TRUST_PROXY"),
        cookie_secure=not _flag("DD_LICENSE_INSECURE_COOKIE"),
        extra_pubkeys=os.environ.get("DD_LICENSE_EXTRA_PUBKEYS", ""),
    )
