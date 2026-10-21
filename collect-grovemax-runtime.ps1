<#
.SYNOPSIS
  打包 Grove Max 运行环境 runtime\current 的「可分析信息」v2（只读收集，不动原文件）

.DESCRIPTION
  v2 修正：v1 把 node_modules / Python Lib / chromium / LibreOffice 的第三方文本全收了
  （16000+ 文件），Compress-Archive 因文件数与 260 字符路径限制打包失败，且失败后误删了
  中间目录。v2 只收**第一方文件**（对还原运行逻辑有价值的），几十到几百个：
    - runtime-manifest.json 等顶层清单/配置（深度≤2）
    - sidecar\ 下全部脚本（pipeline_v5 / subtitle_removal / title_watermark / ai_label）
    - dist\ 下的 .js/.json（cost-report-cli、cloud-subtitle-analyze-cli 等入口）
    - node_modules\@grove-max\ 下（第一方包，排除其它第三方）
  打包失败时保留源文件夹，不再删除。

  用法（Windows 机器任意位置执行，无需管理员）：
  powershell -ExecutionPolicy Bypass -File collect-grovemax-runtime.ps1
#>

$ErrorActionPreference = 'Continue'
$src = "$env:LOCALAPPDATA\cn.grovemax.review\runtime\current"
$out = "$env:USERPROFILE\Desktop\runtime-current-信息包"

if (-not (Test-Path $src)) {
    Write-Host "未找到 $src —— 请确认 Grove Max 已安装且运行环境已下载" -ForegroundColor Yellow
    exit 1
}
if (Test-Path $out) { Remove-Item $out -Recurse -Force }
New-Item -ItemType Directory $out | Out-Null

Write-Host "1/3 生成文件清单.csv（全量，可能需要一两分钟）..." -ForegroundColor Cyan
Get-ChildItem $src -Recurse -File -Force |
    Select-Object @{n = '相对路径'; e = { $_.FullName.Substring($src.Length + 1) } },
                  @{n = '大小KB'; e = { [math]::Round($_.Length / 1KB, 1) } },
                  LastWriteTime |
    Sort-Object '相对路径' |
    Export-Csv "$out\文件清单.csv" -NoTypeInformation -Encoding UTF8

Write-Host "2/3 拷贝第一方文本文件（sidecar / dist / 清单 / @grove-max 包）..." -ForegroundColor Cyan
New-Item -ItemType Directory "$out\文本文件" -Force | Out-Null
$copied = 0
Get-ChildItem $src -Recurse -File -Force -Include *.json, *.py, *.js, *.txt, *.toml, *.cmd, *.bat, *.manifest, *.cfg, *.ini |
    ForEach-Object {
        $rel = $_.FullName.Substring($src.Length + 1)
        # 只收第一方。注意实际布局带组件前缀：engine\sidecar\...、engine\dist\...、
        # mount-runner\...、report-package-runner\...（v2 的 'sidecar\*' 前缀匹配漏掉了它们）
        $segs = $rel.Split('\')
        $keep =
            ($rel -like 'engine\sidecar\*') -or
            ($rel -like 'engine\dist\*') -or
            ($rel -like '*\node_modules\@grove-max\*') -or
            ($rel -like 'mount-runner\*') -or
            ($rel -like 'report-package-runner\*') -or
            ($segs.Count -le 2)
        if (-not $keep) { return }
        if ($rel.Length -gt 180) { return }   # 规避 Windows 260 字符路径限制，保证能压缩
        if ($_.Length -ge 200KB) { return }   # 大文件（打包产物等）不收
        $flat = $rel -replace '\\', '__'
        Copy-Item -LiteralPath $_.FullName -Destination "$out\文本文件\$flat" -Force
        $script:copied++
    }
Write-Host ("    收集了 $copied 个文本文件") -ForegroundColor Gray

Write-Host "3/3 压缩打包（失败也会保留文件夹）..." -ForegroundColor Cyan
# ZIP 时间戳合法范围是 1980-01-01 ~ 2107-12-31。Python/node 第三方分发包常带 1970 年
# 时间戳，会让 Compress-Archive 抛「指定的 DateTimeOffset 不能转换为 Zip 文件时间戳」，
# 打包前把越界时间戳钳制为固定值（原始时间已在 文件清单.csv 里保留，不丢分析信息）
Get-ChildItem $out -Recurse -Force | ForEach-Object {
    if ($_.LastWriteTime -lt [datetime]'1980-01-02' -or $_.LastWriteTime -gt [datetime]'2107-12-31') {
        $_.LastWriteTime = [datetime]'2026-01-01 00:00:00'
    }
}
$zip = "$env:USERPROFILE\Desktop\runtime-current-信息包.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
$zipped = $false
try {
    Compress-Archive -Path $out -DestinationPath $zip -Force
    $zipped = $true
} catch {
    Write-Host "压缩失败（不影响已收集内容）：$($_.Exception.Message)" -ForegroundColor Yellow
}

if ($zipped -and (Test-Path $zip)) {
    Remove-Item $out -Recurse -Force
    $size = '{0:N1}' -f ((Get-Item $zip).Length / 1MB)
    Write-Host ""
    Write-Host "完成：$zip （$size MB）" -ForegroundColor Green
    Write-Host "把这个 zip 发回即可。"
} else {
    Write-Host ""
    Write-Host "zip 未生成，但文件夹已保留：$out" -ForegroundColor Green
    Write-Host "直接把这个文件夹压缩后发回即可。"
}
