这个目录由 CI（.github/workflows/desktop.yml 的「内置擦除工具」步骤）在打包时
放入当前目标平台的 dossier-inpaint 可执行文件，让安装版的「字幕擦除」无需另装工具。
只内置 Rust 管线本体：安装版支持 fixed + column 基线；worker（DBNet / 时序模型）
需要另行配置 Python 与模型权重。
本地开发无需此目录内容，运行时会退回 PATH 与仓库 target/ 里的构建产物。
