#!/usr/bin/env python3
"""生成应用自动升级用的 latest.json，并在发布前逐个验证安装包签名。

用法（CI 的 release 任务里调用，见 .github/workflows/desktop.yml）：

    # 发布前确认标签与两处版本号一致
    python3 scripts/make_latest_json.py check-version --tag app-v0.3.0

    # 验签并生成 latest.json
    python3 scripts/make_latest_json.py build \\
        --tag app-v0.3.0 --repo lisll/drama-dossier --out dist/latest.json \\
        --artifact windows-x86_64=dist/DramaDossier_0.3.0_x64-setup.exe \\
        --artifact darwin-aarch64=dist/DramaDossier_0.3.0_aarch64.app.tar.gz \\
        --artifact darwin-x86_64=dist/DramaDossier_0.3.0_x64.app.tar.gz

每个产物旁边必须有同名 `.sig`（Tauri 打包时生成）。验签用的是 `src-tauri/tauri.conf.json`
里的 `plugins.updater.pubkey`，也就是客户端内置的那把公钥：

- CI 里的签名私钥填错（不是这把公钥对应的私钥）时，打包照样成功，但发出去的更新所有用户都会
  验签失败、永远装不上。所以这里先用同一把公钥验一遍，不通过就让发布失败。
- 签名里记录的版本（trusted comment 的 `version:`）必须等于本次发布版本：客户端开启了
  `requireSignedVersion`，对不上同样装不上。

签名格式是 minisign：公钥 `Ed` + 8 字节 key id + 32 字节 Ed25519 公钥；签名 `ED`（先对文件做
BLAKE2b-512 再签）或 `Ed`（直接签文件）+ key id + 64 字节签名；另有一个「全局签名」覆盖
签名本体 + trusted comment，防止有人改 trusted comment 里的版本号。
"""

from __future__ import annotations

import argparse
import base64
import datetime as _dt
import hashlib
import json
import re
import sys
from pathlib import Path

from cryptography.exceptions import InvalidSignature
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey

ROOT = Path(__file__).resolve().parents[1]
TAURI_CONF = ROOT / "src-tauri" / "tauri.conf.json"
CARGO_TOML = ROOT / "src-tauri" / "Cargo.toml"
RELEASE_NOTES_DIR = ROOT / "docs" / "release-notes"
TAG_PREFIX = "app-v"
# 本项目发布的三个平台；任何一个缺失都不生成清单，避免某个平台的用户永远收不到更新。
REQUIRED_PLATFORMS = ("windows-x86_64", "darwin-aarch64", "darwin-x86_64")


class ReleaseError(Exception):
    """发布前检查不通过。"""


# ---------------------------------------------------------------- 版本


def version_from_tag(tag: str) -> str:
    if not tag.startswith(TAG_PREFIX):
        raise ReleaseError(f"标签 {tag!r} 不是 {TAG_PREFIX}X.Y.Z 格式")
    version = tag[len(TAG_PREFIX):]
    if not re.fullmatch(r"\d+\.\d+\.\d+", version):
        raise ReleaseError(f"标签 {tag!r} 的版本号必须是 X.Y.Z")
    return version


def config_version(conf_path: Path = TAURI_CONF) -> str:
    return json.loads(conf_path.read_text(encoding="utf-8"))["version"]


def cargo_version(cargo_path: Path = CARGO_TOML) -> str:
    # 只读 [package] 段里的 version，不引入 TOML 依赖（Python 3.9 也能跑）
    in_package = False
    for line in cargo_path.read_text(encoding="utf-8").splitlines():
        stripped = line.strip()
        if stripped.startswith("["):
            in_package = stripped == "[package]"
            continue
        match = re.fullmatch(r'version\s*=\s*"([^"]+)"', stripped)
        if in_package and match:
            return match.group(1)
    raise ReleaseError(f"{cargo_path} 的 [package] 段里没有 version")


def check_version(tag: str, conf_path: Path = TAURI_CONF, cargo_path: Path = CARGO_TOML) -> str:
    version = version_from_tag(tag)
    found = {"tauri.conf.json": config_version(conf_path), "Cargo.toml": cargo_version(cargo_path)}
    wrong = {name: value for name, value in found.items() if value != version}
    if wrong:
        detail = "，".join(f"{name} 是 {value}" for name, value in wrong.items())
        raise ReleaseError(f"标签版本 {version} 与源码版本不一致：{detail}。先改版本号再打标签。")
    return version


# ---------------------------------------------------------------- minisign


def _b64(text: str, what: str) -> bytes:
    try:
        return base64.b64decode(text.strip(), validate=True)
    except ValueError as error:
        raise ReleaseError(f"{what} 不是合法的 base64：{error}") from None


def parse_public_key(encoded: str) -> tuple[bytes, Ed25519PublicKey]:
    """解析 tauri.conf.json 里的 pubkey（整份 minisign 公钥文件再 base64 一次）。"""
    lines = _b64(encoded, "公钥").decode("utf-8").splitlines()
    if len(lines) < 2:
        raise ReleaseError("公钥格式不对：应为两行的 minisign 公钥")
    raw = _b64(lines[1], "公钥正文")
    if len(raw) != 42 or raw[:2] != b"Ed":
        raise ReleaseError("公钥格式不对：不是 minisign Ed25519 公钥")
    return raw[2:10], Ed25519PublicKey.from_public_bytes(raw[10:])


def _file_blake2b(path: Path) -> bytes:
    digest = hashlib.blake2b(digest_size=64)
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.digest()


def verify_artifact(path: Path, signature_text: str, encoded_pubkey: str, version: str) -> None:
    """验证一个产物的 .sig：文件签名、全局签名、签名里的版本号三项都要通过。"""
    key_id, public_key = parse_public_key(encoded_pubkey)
    lines = _b64(signature_text, f"{path.name}.sig").decode("utf-8").splitlines()
    if len(lines) < 4 or not lines[2].startswith("trusted comment: "):
        raise ReleaseError(f"{path.name}.sig 格式不对：应为四行的 minisign 签名")
    sig_raw = _b64(lines[1], f"{path.name}.sig 签名正文")
    trusted_comment = lines[2][len("trusted comment: "):]
    global_sig = _b64(lines[3], f"{path.name}.sig 全局签名")
    if len(sig_raw) != 74 or sig_raw[:2] not in (b"Ed", b"ED"):
        raise ReleaseError(f"{path.name}.sig 不是 minisign Ed25519 签名")
    if sig_raw[2:10] != key_id:
        raise ReleaseError(
            f"{path.name} 不是用 tauri.conf.json 里这把公钥对应的私钥签的（key id 不符）。"
            "检查 GitHub Secret TAURI_SIGNING_PRIVATE_KEY 是否填对。"
        )
    signature = sig_raw[10:]
    message = _file_blake2b(path) if sig_raw[:2] == b"ED" else path.read_bytes()
    try:
        public_key.verify(signature, message)
    except InvalidSignature:
        raise ReleaseError(f"{path.name} 的签名校验失败：文件被改过，或签名不属于这个文件") from None
    try:
        public_key.verify(global_sig, signature + trusted_comment.encode("utf-8"))
    except InvalidSignature:
        raise ReleaseError(f"{path.name}.sig 的 trusted comment 被篡改（全局签名不符）") from None
    fields = dict(part.split(":", 1) for part in trusted_comment.split("\t") if ":" in part)
    signed_version = fields.get("version")
    if signed_version != version:
        raise ReleaseError(
            f"{path.name} 的签名记录的版本是 {signed_version!r}，本次发布是 {version}；"
            "客户端开启了 requireSignedVersion，对不上会拒绝安装。"
        )


# ---------------------------------------------------------------- latest.json


def release_notes(version: str, notes_dir: Path = RELEASE_NOTES_DIR) -> str:
    path = notes_dir / f"v{version}.md"
    if path.is_file():
        text = path.read_text(encoding="utf-8").strip()
        if text:
            return text
    return f"剧备 v{version}"


def build_latest(
    *,
    tag: str,
    repo: str,
    artifacts: dict[str, Path],
    encoded_pubkey: str,
    notes: str,
    now: _dt.datetime | None = None,
) -> dict:
    version = version_from_tag(tag)
    missing = [platform for platform in REQUIRED_PLATFORMS if platform not in artifacts]
    if missing:
        raise ReleaseError(f"缺少平台产物：{', '.join(missing)}")
    unknown = sorted(set(artifacts) - set(REQUIRED_PLATFORMS))
    if unknown:
        raise ReleaseError(f"未知平台：{', '.join(unknown)}")
    platforms = {}
    for platform in REQUIRED_PLATFORMS:
        path = artifacts[platform]
        sig_path = path.with_name(path.name + ".sig")
        if not path.is_file():
            raise ReleaseError(f"{platform} 的产物不存在：{path}")
        if not sig_path.is_file():
            raise ReleaseError(f"{platform} 的签名不存在：{sig_path}")
        signature = sig_path.read_text(encoding="utf-8").strip()
        verify_artifact(path, signature, encoded_pubkey, version)
        platforms[platform] = {
            "signature": signature,
            "url": f"https://github.com/{repo}/releases/download/{tag}/{path.name}",
        }
    stamp = (now or _dt.datetime.now(_dt.timezone.utc)).replace(microsecond=0)
    return {
        "version": version,
        "notes": notes,
        "pub_date": stamp.isoformat().replace("+00:00", "Z"),
        "platforms": platforms,
    }


def _parse_artifacts(values: list[str]) -> dict[str, Path]:
    result: dict[str, Path] = {}
    for value in values:
        platform, sep, path = value.partition("=")
        if not sep or not path:
            raise ReleaseError(f"--artifact 应为 平台=路径，收到 {value!r}")
        if platform in result:
            raise ReleaseError(f"平台 {platform} 重复")
        result[platform] = Path(path)
    return result


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    check = sub.add_parser("check-version", help="确认标签与源码版本号一致")
    check.add_argument("--tag", required=True)
    build = sub.add_parser("build", help="验签并生成 latest.json")
    build.add_argument("--tag", required=True)
    build.add_argument("--repo", required=True, help="owner/name")
    build.add_argument("--out", required=True)
    build.add_argument("--artifact", action="append", default=[], help="平台=产物路径")
    args = parser.parse_args(argv)
    try:
        if args.command == "check-version":
            print(f"版本一致：{check_version(args.tag)}")
            return 0
        version = check_version(args.tag)
        pubkey = json.loads(TAURI_CONF.read_text(encoding="utf-8"))["plugins"]["updater"]["pubkey"]
        latest = build_latest(
            tag=args.tag,
            repo=args.repo,
            artifacts=_parse_artifacts(args.artifact),
            encoded_pubkey=pubkey,
            notes=release_notes(version),
        )
        out = Path(args.out)
        out.write_text(json.dumps(latest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        print(f"已验签 {len(latest['platforms'])} 个平台，写入 {out}")
        return 0
    except ReleaseError as error:
        print(f"::error::{error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
