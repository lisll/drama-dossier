"""报审包文档：PDF 渲染（pdf）与签章（signer）。

文字内容由 Rust 的 `render_documents` 给出（经 `dossier-artifacts render-package`），
这里只负责把排版模型画成 PDF、在签章位上盖章，不自己拼文字。
"""
