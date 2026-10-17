"""报审包：服务端生成文档并签名清单，桌面端组装（总方案 3.3 节）。

流程：
1. 检查登录、这台电脑绑定在当前账号上、使用权有效；
2. 调 `dossier-artifacts render-package`（Rust，文字内容只在那里写一份）生成文本文件和排版模型；
3. 把两份需要盖章的文档画成 PDF，交给 Signer 盖章（占位章或扫描件）；
4. 拼出清单 `drama-dossier/package@2`，用 Ed25519 签名（用途前缀 package-manifest，和授权凭证互不冒充）；
5. 记一行 packages、一条审计日志（只记剧名的哈希和集数），把文件发回去。

视频不上传：分集、封面、简介只传桌面端算好的哈希，清单里登记哈希，桌面端组装时重新算一遍对比。
"""

from __future__ import annotations

import base64
import binascii
import hashlib
import json
import logging
import subprocess
import uuid
from typing import Dict, List, Tuple

from .. import toolchain
from . import credits, entitlements
from .config import CommerceSettings
from .db import audit
from .documents.pdf import Font, LayoutError, render_sealed_document
from .documents.signer import MockSigner, ScanUploadSigner, SealSlot, SignerError, SignRequest, SignState
from .errors import CommerceError
from .license_token import Signer
from .schemas import PackageRequest
from .sessions import AuthContext

log = logging.getLogger(__name__)

MANIFEST_SCHEMA = "drama-dossier/package@2"
RENDER_TIMEOUT_SECONDS = 30
UNSIGNED_SUFFIX = "_未签署"


def encode_manifest(manifest: dict) -> bytes:
    """清单的字节：签名针对这些字节，桌面端原样写回包里，不会重新序列化。"""
    return json.dumps(manifest, ensure_ascii=False, indent=2).encode("utf-8")


def sign_manifest(signer: Signer, manifest_bytes: bytes) -> str:
    """`manifest.sig` 的内容：`{"kid", "sig"}`，签名用途前缀是 package-manifest。"""
    return json.dumps({"kid": signer.kid, "sig": signer.sign_detached("package-manifest", manifest_bytes)})


def require_access(conn, user_id: str, device_id: str) -> None:
    """付费步骤的共同前提：这台电脑绑定在当前账号上，并且有有效的使用权。"""
    bound = conn.execute(
        "SELECT 1 FROM devices WHERE user_id = %s::uuid AND device_id = %s AND unbound_at IS NULL",
        (user_id, device_id),
    ).fetchone()
    if bound is None:
        raise CommerceError(403, "device_not_bound", "这台电脑没有绑定到当前账号，或者已经被解绑，请重新绑定")
    if not entitlements.current(conn, user_id).active:
        raise CommerceError(403, "entitlement_required", "这个账号还没有使用权，或者使用权已到期，请先兑换卡密")


class PackageService:
    def __init__(self, settings: CommerceSettings, signer: Signer, font: Font):
        self.settings = settings
        self.signer = signer
        self.font = font

    def create(self, conn, ctx: AuthContext, payload: PackageRequest) -> dict:
        require_access(conn, ctx.user_id, payload.device_id)
        doc_signer = self._doc_signer(payload)
        # ---- 计费（M5a）：先算价、预扣，再渲染 ----
        #
        # 为什么预扣放在渲染**之前**：渲染要跑子进程、可能几秒。先渲染再查余额，
        # 等于让一个余额不足的请求白占 CPU 与 PDF 字体资源。
        #
        # 为什么不能把扣费与渲染放在同一个数据库事务里：渲染是耗时操作，
        # 长事务会一直占着连接与锁。所以分三段，见下面的 settle / except release。
        amount = credits.cost_for(conn, "package", len(payload.episodes))
        # ref 会进流水与审计日志，**不能带剧名**：审计日志一直刻意只存剧名的哈希，
        # 把明文写进 ref 等于绕过了那条规矩。这里用一次性的开销号做引用，足够对账。
        with conn.transaction():
            hold_key = credits.hold(
                conn, ctx.user_id, amount, ref=f"package:{uuid.uuid4()}",
                memo=f"报审包（{len(payload.episodes)} 集）",
            )
        try:
            result = self._create_charged(conn, ctx, payload, doc_signer)
        except BaseException:
            # 渲染、签章、写库任何一处出错都要把钱退回去。用 BaseException 而不是 Exception：
            # 进程被取消（KeyboardInterrupt 等）时同样不能把钱留着。
            credits.release(conn, hold_key, memo="报审包生成失败，已退还")
            raise
        credits.settle(conn, hold_key)
        result["credits"] = amount
        return result

    def _create_charged(self, conn, ctx: AuthContext, payload: PackageRequest, doc_signer) -> dict:
        """真正生成报审包（计费在外层完成）。"""
        rendered = self._render(payload)
        package_id = str(uuid.uuid4())
        generated_at = conn.execute("SELECT now() AS now").fetchone()["now"].isoformat()

        files: List[Tuple[str, bytes]] = [
            (item["path"], item["content"].encode("utf-8")) for item in rendered["textFiles"]
        ]
        signed_documents: List[str] = []
        placeholder = False
        # 逐份记录实际用的签章方式（`mock` / `scan`）：混合签章时整包不能只写一个结论，
        # 否则使用者会以为「有扫描件 = 整包都盖好了」。
        documents_signature: List[Dict[str, str]] = []
        footer = f"报审包 {package_id} · 生成于 {generated_at}"
        sealed_paths = {document["path"] for document in rendered["sealed"]}
        unknown = set(payload.scans or {}) - sealed_paths
        if unknown:
            raise CommerceError(422, "scan_invalid", f"不需要盖章的文档：{', '.join(sorted(unknown))}")
        for document in rendered["sealed"]:
            try:
                pdf = render_sealed_document(document, self.font, footer=footer)
                result = doc_signer.start(SignRequest(
                    document_id=document["path"],
                    pdf=pdf,
                    slots=[SealSlot.from_model(slot) for slot in document["slots"]],
                    signer_name=(payload.cost_sheet.company_name if payload.cost_sheet else ""),
                ))
            except LayoutError as error:
                raise CommerceError(422, "invalid_request", str(error)) from error
            except SignerError as error:
                raise CommerceError(422, error.code, str(error)) from error
            if result.state is not SignState.COMPLETED or result.signed_pdf is None:
                # 占位章和扫描件都是当场完成；走到这里说明接了还没实现的服务商。
                raise CommerceError(503, "service_unavailable", "签章服务暂时不可用，请稍后重试")
            path = document["path"]
            if result.is_placeholder:
                placeholder = True
                path = path.removesuffix(".pdf") + UNSIGNED_SUFFIX + ".pdf"
            files.append((path, result.signed_pdf))
            signed_documents.append(path)
            documents_signature.append(
                {"document": path, "signer": "mock" if result.is_placeholder else "scan"}
            )

        manifest = {
            "schema": MANIFEST_SCHEMA,
            "packageId": package_id,
            "generatedAt": generated_at,
            "userId": ctx.user_id,
            "deviceId": payload.device_id,
            "dramaName": payload.drama_name.strip(),
            "aiGenerated": payload.ai_generated,
            "copyEpisodes": payload.copy_episodes,
            "signature": {
                "signer": doc_signer.name,
                "placeholder": placeholder,
                "documents": signed_documents,
                # 每份文档的实际签章方式（混合签章时各不相同）。
                "perDocument": documents_signature,
            },
            "files": [
                {"path": path, "sizeBytes": len(content), "sha256": hashlib.sha256(content).hexdigest(),
                 "source": "server"}
                for path, content in files
            ] + [
                {"path": item["path"], "sizeBytes": item["sizeBytes"], "sha256": item["sha256"], "source": "local"}
                for item in rendered["localFiles"]
            ],
            "references": [
                {"path": item["path"], "sizeBytes": item["sizeBytes"], "sha256": item["sha256"]}
                for item in rendered["references"]
            ],
        }
        manifest_bytes = encode_manifest(manifest)
        signature = sign_manifest(self.signer, manifest_bytes)

        with conn.transaction():
            conn.execute(
                "INSERT INTO packages (id, user_id, device_id, signer, episode_count, manifest_sha256)"
                " VALUES (%s::uuid, %s::uuid, %s, %s, %s, %s)",
                (package_id, ctx.user_id, payload.device_id, doc_signer.name, len(payload.episodes),
                 hashlib.sha256(manifest_bytes).digest()),
            )
            # 不记剧名、公司名、联系方式原文：只记剧名的哈希，能对上是哪部剧就够了。
            audit(conn, ctx.user_id, "package.create", package_id, {
                "drama": hashlib.sha256(payload.drama_name.strip().encode("utf-8")).hexdigest()[:16],
                "episodes": len(payload.episodes),
                "signer": doc_signer.name,
            })
        return {
            "package_id": package_id,
            "manifest": manifest_bytes.decode("utf-8"),
            "signature": signature,
            "files": [{"path": path, "content": base64.b64encode(content).decode("ascii")} for path, content in files],
        }

    def _doc_signer(self, payload: PackageRequest):
        if payload.signer == "mock":
            if payload.scans:
                raise CommerceError(422, "scan_invalid", "用占位章时不需要上传扫描件")
            return MockSigner(self.font)
        scans: Dict[str, bytes] = {}
        for path, text in (payload.scans or {}).items():
            try:
                scans[path] = base64.b64decode(text, validate=True)
            except (binascii.Error, ValueError) as error:
                raise CommerceError(422, "scan_invalid", f"{path} 的扫描件不是合法的 base64") from error
        return ScanUploadSigner(self.font, scans)

    def _render(self, payload: PackageRequest) -> dict:
        tool = toolchain.resolve_binary(self.settings.artifacts_bin, "dossier-artifacts")
        if tool is None:
            searched = " → ".join(map(str, toolchain.search_candidates(self.settings.artifacts_bin, "dossier-artifacts")))
            log.error("找不到 dossier-artifacts（查找顺序：%s）", searched)
            raise CommerceError(503, "service_unavailable", "服务端缺少报审包生成工具，请联系管理员")
        request = json.dumps(_render_request(payload), ensure_ascii=False).encode("utf-8")
        try:
            done = subprocess.run([str(tool), "render-package"], input=request, capture_output=True,
                                  timeout=RENDER_TIMEOUT_SECONDS, check=False)
        except (OSError, subprocess.TimeoutExpired) as error:
            log.error("dossier-artifacts render-package 运行失败：%s", type(error).__name__)
            raise CommerceError(503, "service_unavailable", "报审包生成工具运行失败，请稍后重试") from error
        if done.returncode == 2:
            # 请求不合法：Rust 给的是中文说明，原样转给用户（去掉「错误：」前缀）。
            message = done.stderr.decode("utf-8", "replace").strip().removeprefix("错误：")
            raise CommerceError(422, "invalid_request", message or "报审包信息不正确")
        if done.returncode != 0:
            log.error("dossier-artifacts render-package 退出码 %s", done.returncode)
            raise CommerceError(503, "service_unavailable", "报审包生成工具运行失败，请稍后重试")
        try:
            return json.loads(done.stdout)
        except ValueError as error:
            log.error("dossier-artifacts render-package 输出不是 JSON")
            raise CommerceError(503, "service_unavailable", "报审包生成工具运行失败，请稍后重试") from error


def _asset(item) -> dict:
    return {"fileName": item.file_name, "sizeBytes": item.size_bytes, "sha256": item.sha256}


def _render_request(payload: PackageRequest) -> dict:
    """接口用 snake_case，Rust 用 camelCase：在这里显式转换，不让两种写法混在一起。"""
    cost = payload.cost_sheet
    return {
        "dramaName": payload.drama_name,
        "aiGenerated": payload.ai_generated,
        "copyEpisodes": payload.copy_episodes,
        "costSheet": None if cost is None else {
            "companyName": cost.company_name,
            "totalInvestmentWan": cost.total_investment_wan,
            "actorRatioPercent": cost.actor_ratio_percent,
            "contact": cost.contact,
        },
        "synopsis": None if payload.synopsis is None else _asset(payload.synopsis),
        "cover": None if payload.cover is None else _asset(payload.cover),
        "episodes": [
            {**_asset(episode), "episodeNumber": episode.episode_number, "spec": {
                "durationSec": episode.spec.duration_sec,
                "width": episode.spec.width,
                "height": episode.spec.height,
                "videoCodec": episode.spec.video_codec,
                "audioCodec": episode.spec.audio_codec,
                "fps": episode.spec.fps,
                "bitrateBps": episode.spec.bitrate_bps,
            }}
            for episode in payload.episodes
        ],
    }
