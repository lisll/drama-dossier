"""RuntimeRoles 管理器。

运行时组件以声明式 manifest 管理，不把 Python/模型/FFmpeg 依赖硬编码进任务配方。
支持本地 artifact 安装、HTTPS 下载、大小与 SHA-256 校验、原子激活和回滚。

这里不执行安装脚本，也不解压或运行不受信任的包；manifest 只描述单文件 artifact，
复杂安装步骤应由部署方完成后再声明能力。
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import tempfile
import urllib.parse
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Dict, Iterable, Optional

SCHEMA = "drama-dossier/runtime-manifest@1"
_NAME_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$")


def _safe_name(value: str, label: str = "名称") -> str:
    if not _NAME_RE.fullmatch(str(value or "")):
        raise ValueError(f"{label}包含非法字符：{value!r}")
    return str(value)


def local_target() -> tuple[str, str]:
    """返回 manifest 使用的本机平台/架构标签。"""
    import platform
    import sys

    system = {"win32": "windows", "darwin": "macos"}.get(sys.platform, sys.platform)
    machine = platform.machine().lower()
    arch = {"amd64": "x86_64", "aarch64": "arm64"}.get(machine, machine)
    return _safe_name(system, "平台"), _safe_name(arch, "架构")


class _HttpsOnlyRedirectHandler(urllib.request.HTTPRedirectHandler):
    """禁止 HTTPS manifest URL 通过重定向降级到明文 HTTP。"""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        parsed = urllib.parse.urlparse(newurl)
        if parsed.scheme != "https" or not parsed.hostname or parsed.username or parsed.password:
            raise ValueError("运行时下载重定向只允许到无凭据的 HTTPS URL")
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


@dataclass(frozen=True)
class RuntimeRole:
    name: str
    version: str
    platform: str
    arch: str
    sha256: str
    size_bytes: int
    artifact_name: str
    source: Optional[str] = None
    executable: bool = False

    @classmethod
    def from_dict(cls, payload: dict) -> "RuntimeRole":
        name = _safe_name(payload.get("name"), "RuntimeRole 名称")
        version = _safe_name(payload.get("version"), "RuntimeRole 版本")
        platform = _safe_name(payload.get("platform"), "平台")
        arch = _safe_name(payload.get("arch"), "架构")
        artifact_name = str(payload.get("artifact_name") or name)
        if (artifact_name in ("", ".", "..") or Path(artifact_name).name != artifact_name
                or "/" in artifact_name or "\\" in artifact_name):
            raise ValueError("artifact_name 只能是单个文件名")
        digest = str(payload.get("sha256") or "").lower()
        if not re.fullmatch(r"[0-9a-f]{64}", digest):
            raise ValueError(f"RuntimeRole {name} 的 sha256 必须是 64 位十六进制")
        size = payload.get("size_bytes", -1)
        if isinstance(size, bool) or not isinstance(size, int) or size < 0:
            raise ValueError(f"RuntimeRole {name} 的 size_bytes 不能为负数")
        source = payload.get("source")
        if source is not None:
            source = str(source)
            parsed = urllib.parse.urlparse(source)
            if (parsed.scheme != "https" or not parsed.hostname or parsed.username
                    or parsed.password or parsed.fragment):
                raise ValueError("运行时远程 source 必须是无凭据、无片段的绝对 HTTPS URL")
        if not isinstance(payload.get("executable", False), bool):
            raise ValueError("executable 必须是布尔值")
        return cls(name, version, platform, arch, digest, size, artifact_name, source, bool(payload.get("executable", False)))


@dataclass(frozen=True)
class RuntimeManifest:
    product: str
    roles: tuple[RuntimeRole, ...]
    schema: str = SCHEMA

    @classmethod
    def load(cls, path: Path) -> "RuntimeManifest":
        payload = json.loads(path.read_text(encoding="utf-8"))
        if payload.get("schema") != SCHEMA:
            raise ValueError(f"不支持的 runtime manifest schema：{payload.get('schema')}")
        product = _safe_name(payload.get("product"), "product")
        raw_roles = payload.get("roles")
        if not isinstance(raw_roles, list):
            raise ValueError("runtime manifest.roles 必须是数组")
        if not all(isinstance(item, dict) for item in raw_roles):
            raise ValueError("runtime manifest.roles 每项必须是对象")
        roles = tuple(RuntimeRole.from_dict(item) for item in raw_roles)
        keys = [(role.name, role.platform, role.arch) for role in roles]
        if len(set(keys)) != len(keys):
            raise ValueError("runtime manifest 中同一平台/架构的 RuntimeRole 重复")
        return cls(product=product, roles=roles)

    def for_target(self, platform: str, arch: str) -> tuple[RuntimeRole, ...]:
        return tuple(role for role in self.roles if role.platform == platform and role.arch == arch)


class RuntimeManager:
    """管理已校验组件的安装、激活和回滚。"""

    def __init__(self, root: Path):
        self.root = Path(root).expanduser().resolve()
        self.roles_dir = self.root / "roles"
        self.current_dir = self.root / "current"
        self.rollback_dir = self.root / "rollback"
        self.staging_dir = self.root / ".staging"
        for path in (self.roles_dir, self.current_dir, self.rollback_dir, self.staging_dir):
            path.mkdir(parents=True, exist_ok=True)
            try:
                path.resolve().relative_to(self.root)
            except ValueError as error:
                raise ValueError(f"RuntimeRoles 目录越出 runtime root：{path}") from error

    def install(self, role: RuntimeRole, source: Optional[Path] = None, timeout: float = 300.0) -> dict:
        """安装并激活一个已声明角色；source 为空时按 manifest 的 HTTPS source 下载。"""
        role = RuntimeRole.from_dict(role.__dict__)
        if timeout <= 0:
            raise ValueError("timeout 必须大于 0")
        final_dir = self.roles_dir / role.name / role.version
        if final_dir.exists():
            artifact = final_dir / role.artifact_name
            self._verify(artifact, role)
            return self.activate(role)

        stage = Path(tempfile.mkdtemp(prefix=f"{role.name}-", dir=self.staging_dir))
        staged_artifact = stage / role.artifact_name
        try:
            if source is not None:
                source = Path(source)
                if not source.is_file():
                    raise FileNotFoundError(source)
                shutil.copyfile(source, staged_artifact)
            else:
                if not role.source:
                    raise ValueError(f"RuntimeRole {role.name} 没有 source，且未提供本地 artifact")
                parsed = urllib.parse.urlparse(role.source)
                if parsed.scheme != "https" or not parsed.hostname or parsed.username or parsed.password:
                    raise ValueError("运行时远程下载只允许 HTTPS")
                request = urllib.request.Request(role.source, headers={"User-Agent": "drama-dossier-runtime/1"})
                opener = urllib.request.build_opener(_HttpsOnlyRedirectHandler())
                with opener.open(request, timeout=timeout) as response, staged_artifact.open("wb") as output:
                    final_url = urllib.parse.urlparse(response.geturl())
                    if final_url.scheme != "https":
                        raise ValueError("运行时下载最终地址必须是 HTTPS")
                    total = 0
                    while True:
                        block = response.read(1024 * 1024)
                        if not block:
                            break
                        total += len(block)
                        if total > role.size_bytes:
                            raise ValueError(f"RuntimeRole {role.name} 下载超过 manifest 声明大小")
                        output.write(block)
            self._verify(staged_artifact, role)
            final_dir.parent.mkdir(parents=True, exist_ok=True)
            os.replace(stage, final_dir)
            return self.activate(role)
        except Exception:
            shutil.rmtree(stage, ignore_errors=True)
            raise

    def activate(self, role: RuntimeRole) -> dict:
        role = RuntimeRole.from_dict(role.__dict__)
        artifact = self.roles_dir / role.name / role.version / role.artifact_name
        self._verify(artifact, role)
        current_path = self.current_dir / f"{role.name}.json"
        previous = None
        if current_path.exists():
            previous = self._read_pointer(current_path, role.name)
            self._atomic_json(self.rollback_dir / f"{role.name}.json", previous)
        pointer = {
            "name": role.name,
            "version": role.version,
            "artifact": str(artifact),
            "sha256": role.sha256,
            "size_bytes": role.size_bytes,
            "executable": role.executable,
        }
        self._atomic_json(current_path, pointer)
        return {"status": "activated", "role": role.name, "version": role.version, "previous": previous}

    def rollback(self, name: str) -> dict:
        name = _safe_name(name, "RuntimeRole 名称")
        marker = self.rollback_dir / f"{name}.json"
        if not marker.exists():
            raise FileNotFoundError(f"没有可回滚版本：{name}")
        previous = self._read_pointer(marker, name)
        current = self.current_dir / f"{name}.json"
        current_previous = self._read_pointer(current, name) if current.exists() else None
        if current_previous is not None:
            self._atomic_json(marker, current_previous)
        self._atomic_json(current, previous)
        return {"status": "rolled_back", "role": name, "version": previous.get("version")}

    def resolve(self, name: str) -> Optional[Path]:
        name = _safe_name(name, "RuntimeRole 名称")
        path = self.current_dir / f"{name}.json"
        if not path.exists():
            return None
        payload = self._read_pointer(path, name)
        return self._artifact_path(payload)

    def status(self) -> dict:
        result: Dict[str, dict] = {}
        for pointer in sorted(self.current_dir.glob("*.json")):
            name = pointer.stem
            try:
                payload = self._read_pointer(pointer, name)
                artifact = self._artifact_path(payload)
                result[name] = {
                    "version": payload.get("version"),
                    "available": artifact.is_file() and _sha256(artifact) == payload.get("sha256"),
                    "artifact": str(artifact),
                }
            except (OSError, ValueError, KeyError, TypeError, json.JSONDecodeError) as error:
                result[name] = {"available": False, "error": str(error)}
        return {"root": str(self.root), "roles": result}

    def _artifact_path(self, pointer: dict) -> Path:
        raw = Path(pointer["artifact"])
        artifact = (self.root / raw).resolve() if not raw.is_absolute() else raw.resolve()
        try:
            artifact.relative_to(self.root)
        except ValueError as error:
            raise ValueError("RuntimeRole pointer 的 artifact 路径越出 runtime root") from error
        return artifact

    def _read_pointer(self, path: Path, expected_name: str) -> dict:
        payload = json.loads(path.read_text(encoding="utf-8"))
        if not isinstance(payload, dict) or payload.get("name") != expected_name:
            raise ValueError(f"RuntimeRole pointer 名称不匹配：{expected_name}")
        version = _safe_name(payload.get("version"), "RuntimeRole 版本")
        _safe_name(payload.get("name"), "RuntimeRole 名称")
        digest = str(payload.get("sha256") or "").lower()
        if not re.fullmatch(r"[0-9a-f]{64}", digest):
            raise ValueError("RuntimeRole pointer SHA-256 无效")
        size = payload.get("size_bytes")
        if isinstance(size, bool) or not isinstance(size, int) or size < 0:
            raise ValueError("RuntimeRole pointer size_bytes 无效")
        artifact = self._artifact_path(payload)
        if not artifact.is_file() or artifact.stat().st_size != size or _sha256(artifact) != digest:
            raise ValueError(f"RuntimeRole {expected_name}@{version} 不存在或完整性校验失败")
        return {**payload, "version": version, "sha256": digest, "size_bytes": size,
                "artifact": str(artifact)}

    @staticmethod
    def _atomic_json(path: Path, payload: dict) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_name(path.name + ".tmp")
        tmp.write_text(json.dumps(payload, ensure_ascii=False, sort_keys=True), encoding="utf-8")
        os.replace(tmp, path)

    @staticmethod
    def _verify(path: Path, role: RuntimeRole) -> None:
        if not path.is_file():
            raise FileNotFoundError(path)
        actual_size = path.stat().st_size
        if actual_size != role.size_bytes:
            raise ValueError(f"RuntimeRole {role.name} 大小不匹配：{actual_size} != {role.size_bytes}")
        actual_hash = _sha256(path)
        if actual_hash != role.sha256:
            raise ValueError(f"RuntimeRole {role.name} SHA-256 不匹配")


def runtime_status(root: Path) -> dict:
    return RuntimeManager(root).status()


def load_runtime_manifest(path: Path) -> RuntimeManifest:
    return RuntimeManifest.load(Path(path))
