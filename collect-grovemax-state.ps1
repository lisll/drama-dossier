<#
.SYNOPSIS
  抓取 Grove Max 任务状态 JSON（.grove\workflows）v1 —— 还原 V5 运行逻辑的决定性证据

.DESCRIPTION
  grove-tree 实测已证实 workflow.json / boundary-request.json / 各阶段 execution.json 等状态文件
  全部保留在任务目录里。本脚本只拷贝这些小体积文本（含全部扫描单元的 command/metadata），
  按任务 ID 分目录，最后压缩到桌面；压缩失败则保留文件夹。
  模型/视频/图片一概不收。

  用法（Windows 任意位置执行）：
  powershell -ExecutionPolicy Bypass -File collect-grovemax-state.ps1
  # 工作目录不是 D:\下载 时：powershell -ExecutionPolicy Bypass -File collect-grovemax-state.ps1 -WorkRoot "E:\别的目录"
#>
param(
    # 转码工作目录（.grove 的上级），默认 D:\下载
    [string]$WorkRoot = 'D:\下载'
)

$ErrorActionPreference = 'Continue'
$grove = Join-Path $WorkRoot '.grove'
$out = "$env:USERPROFILE\Desktop\grove-状态包"

if (-not (Test-Path $grove)) {
    Write-Host "未找到 $grove —— 请确认 WorkRoot 指向转码工作目录" -ForegroundColor Yellow
    exit 1
}
if (Test-Path $out) { Remove-Item $out -Recurse -Force }
New-Item -ItemType Directory $out | Out-Null

# workflow 根要收的文件
$rootFiles = @('workflow.json', 'source-snapshot.json', 'jianying-draft-version.txt',
    'watermark-result.log', 'watermark-error.log', 'ai-labels.json')
# pipeline-v5 根要收的文件（编排与两轮编码的执行记录）
$p5Files = @('boundary-request.json', 'canonical-request.stdout', 'canonical-request.stderr',
    'analysis-snapshot.json', 'native-snapshot.json',
    'analyze-execution.json', 'analyze.stdout', 'analyze.stderr',
    'first-encode-execution.json', 'first-encode.stdout', 'first-encode.stderr',
    'final-encode-execution.json', 'final-encode.stdout', 'final-encode.stderr',
    'overlay-title.stdout', 'overlay-title.stderr', 'overlay-ai.stdout', 'overlay-ai.stderr')
# 子目录里要收的（analysis 收全部 command/metadata，那是实际 ffmpeg/分析命令）
$p5Child = @('analysis\analysis.json', 'analysis\native-analysis.json',
    'mothers\manifest.json', 'mothers\run.json',
    'final\manifest.json', 'final\run.json')

$copied = 0
function Copy-One($from, $to) {
    if (Test-Path $from) {
        Copy-Item -LiteralPath $from -Destination $to -Force
        $script:copied++
    }
}

# 1) projects（素材库项目清单）
foreach ($p in Get-ChildItem (Join-Path $grove 'projects') -Directory) {
    Copy-One (Join-Path $p.FullName 'source-materials.json') "$out\project-$($p.Name)-source-materials.json"
}

# 2) 每个 workflow 一套
foreach ($w in Get-ChildItem (Join-Path $grove 'workflows') -Directory) {
    $id = $w.Name
    $dst = "$out\workflow-$id"
    New-Item -ItemType Directory "$dst\analysis" -Force | Out-Null
    foreach ($f in $rootFiles)   { Copy-One (Join-Path $w.FullName $f) "$dst\$f" }
    $p5 = Join-Path $w.FullName 'pipeline-v5'
    foreach ($f in $p5Files)     { Copy-One (Join-Path $p5 $f) "$dst\$f" }
    foreach ($f in $p5Child)     { Copy-One (Join-Path $p5 $f) ("$dst\" + ($f -replace '\\', '-')) }
    # 全部扫描单元的实际命令与元数据（errors.txt 通常为空，跳过）
    $an = Join-Path $p5 'analysis'
    if (Test-Path $an) {
        Get-ChildItem $an -File | Where-Object { $_.Name -match '^\d+-(command|metadata)' } |
            ForEach-Object { Copy-One $_.FullName "$dst\analysis\$($_.Name)" }
    }
}

Write-Host "共拷贝 $copied 个文件" -ForegroundColor Gray
$zip = "$env:USERPROFILE\Desktop\grove-状态包.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
try {
    Compress-Archive -Path $out -DestinationPath $zip -Force
    Remove-Item $out -Recurse -Force
    $size = '{0:N1}' -f ((Get-Item $zip).Length / 1MB)
    Write-Host "完成：$zip （$size MB）—— 发回这个 zip 即可" -ForegroundColor Green
} catch {
    Write-Host "压缩失败（$($_.Exception.Message)），文件夹已保留：$out" -ForegroundColor Yellow
    Write-Host "手动压缩该文件夹发回即可。" -ForegroundColor Yellow
}
