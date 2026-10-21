<#
.SYNOPSIS
  Grove Max 运行日志探查脚本（Windows / PowerShell 5.1+）

.DESCRIPTION
  基于对 grove-max-review.exe（Tauri 2.11.5，标识符 cn.grovemax.review）的静态逆向结论：
  - 应用本身无常驻日志文件，日志以「任务级 sidecar 子进程日志」形式散落在任务工作目录；
  - 已知日志名：audio-shift-N / audio-extract-N / render-draft-N 的 .stdout.log 与 .stderr.log、
    watermark-result.log、watermark-error.log、probe.error.log、encode-stderr.log、result.json；
  - 应用数据与 WebView2 数据在 %LOCALAPPDATA%\cn.grovemax.review；
  - 临时目录使用 .grove-max-* 前缀。

  本脚本扫描上述全部落点，输出：日志清单（路径/大小/时间）+ 每个日志的尾部内容 + 按功能阶段的归组，
  用于通过日志推测软件的运行逻辑。报告同时存到 Documents 下。

.USAGE
  powershell -ExecutionPolicy Bypass -File probe-grovemax-logs.ps1
  powershell -ExecutionPolicy Bypass -File probe-grovemax-logs.ps1 -Roots "D:\素材","E:\成片"
  （-Roots 额外指定你放素材/输出的大目录，日志常跟着任务目录走）
#>

param(
    [string[]]$Roots = @()   # 额外扫描根目录（素材盘 / 输出目录）
)

$ErrorActionPreference = 'SilentlyContinue'
$Stamp   = Get-Date -Format 'yyyyMMdd-HHmmss'
$LogFile = Join-Path ([Environment]::GetFolderPath('MyDocuments')) "GroveMax日志探查报告_$Stamp.txt"

# ---------------------------------------------------------------- 输出基建
$script:Log = New-Object System.Collections.Generic.List[string]
function Out ([string]$text = '') {
    Write-Host $text
    $script:Log.Add($text) | Out-Null
}
function Banner ([string]$title) {
    Out ''
    Out ('=' * 78)
    Out ("  $title")
    Out ('=' * 78)
}
# 文件名 -> 功能阶段推断表（来自逆向的常量字符串证据）
$StageMap = [ordered]@{
    'audio-extract-'  = '阶段1：音频轨提取（ffmpeg，按分集）'
    'audio-shift-'    = '阶段2：音频变调（rubberband/ffmpeg atempo，按分集）'
    'render-draft-'   = '阶段3：剪映草稿渲染成片（Node sidecar，按分集）'
    'watermark-'      = '阶段4：标题水印渲染（Python sidecar title_watermark.py）'
    'probe.'          = '云端字幕探测（CLOUD_SUBTITLE_PROBE）'
    'encode-stderr'   = 'V5 编码阶段（AI 标签/编码，encode-stderr.log）'
    'result.json'     = '任务结果清单（sidecar 产物）'
    'ai_label'        = 'AI 标签渲染（Python sidecar ai_label.py）'
}
function Get-Stage ([string]$name) {
    foreach ($k in $StageMap.Keys) { if ($name -like "*$k*") { return $StageMap[$k] } }
    return '未知阶段'
}
# 打印一个日志文件的元信息 + 尾部内容（推断运行逻辑靠内容）
function Show-LogTail ([System.IO.FileInfo]$f, [int]$lines = 40, [int]$maxChars = 300) {
    Out ''
    Out ("  文件: " + $f.FullName)
    Out ("  阶段: " + (Get-Stage $f.Name))
    Out ("  大小: {0:N0} 字节    修改时间: {1}" -f $f.Length, $f.LastWriteTime)
    if ($f.Length -gt 8MB) { Out '  （超过 8MB，只读尾部）'; }
    $tail = Get-Content -LiteralPath $f.FullName -Tail $lines -Encoding UTF8
    if (-not $tail) { $tail = Get-Content -LiteralPath $f.FullName -Tail $lines }
    Out ('  ---- 尾部 ' + $lines + ' 行 ----')
    foreach ($line in $tail) {
        $t = $line
        if ($t.Length -gt $maxChars) { $t = $t.Substring(0, $maxChars) + ' ...(截断)' }
        Out ('  | ' + $t)
    }
    Out ('  ---- 尾部结束 ----')
}
function Show-DirTree ([string]$path, [int]$depth = 2) {
    if (-not (Test-Path $path)) { return }
    Out ("  " + $path)
    Get-ChildItem -LiteralPath $path -Recurse -Depth ($depth - 1) |
        ForEach-Object {
            $indent = '    ' + ('  ' * ($_.FullName.Substring($path.Length).Split('\').Count - 1))
            $size = if ($_.PSIsContainer) { '[目录]' } else { '{0:N0} 字节' -f $_.Length }
            Out ("$indent$($_.Name)  ($($_.LastWriteTime.ToString('yyyy-MM-dd HH:mm')), $size)")
        }
}

# ---------------------------------------------------------------- 0. 总览
Banner ('Grove Max 运行日志探查报告  生成于 ' + (Get-Date))
Out ("脚本说明：本报告扫描 Grove Max（cn.grovemax.review）在本机所有已知/推断的日志落点。")
Out ("日志与功能阶段对照（来自逆向二进制常量，用于推断运行逻辑）：")
foreach ($k in $StageMap.Keys) { Out ("    $k*  ->  " + $StageMap[$k]) }

# ---------------------------------------------------------------- 1. 安装位置与版本
Banner '1. 安装位置与版本'
$installDirs = @(
    "$env:ProgramFiles\Grove-Max",
    "${env:ProgramFiles(x86)}\Grove-Max",
    "$env:LOCALAPPDATA\Programs\Grove-Max"
)
# 注册表卸载项是最可靠的安装位置来源
$uninstallKeys = @(
    'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*',
    'HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*',
    'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*'
)
$regHits = Get-ItemProperty $uninstallKeys |
    Where-Object { $_.DisplayName -like '*Grove*Max*' }
foreach ($r in $regHits) {
    Out ("  注册表：DisplayName=$($r.DisplayName)  Version=$($r.DisplayVersion)")
    Out ("          InstallLocation=$($r.InstallLocation)  Uninstall=$($r.UninstallString)")
    if ($r.InstallLocation) { $installDirs = @($r.InstallLocation.TrimEnd('\')) + $installDirs }
}
$mainExe = $null
foreach ($d in ($installDirs | Select-Object -Unique)) {
    if (Test-Path (Join-Path $d 'grove-max-review.exe')) {
        $mainExe = Get-Item (Join-Path $d 'grove-max-review.exe')
        Out ("  主程序: $($mainExe.FullName)  版本: $($mainExe.VersionInfo.ProductVersion)")
    }
    $cfg = Join-Path $d 'client\client-config.json'
    if (Test-Path $cfg) {
        Out "  客户端配置 $cfg ："
        Get-Content -LiteralPath $cfg | ForEach-Object { Out ("    " + $_) }
    }
}
if (-not $mainExe -and -not $regHits) { Out '  未找到安装位置（可能装在自定义目录，可手动确认）。' }

# ---------------------------------------------------------------- 2. 应用数据目录（WebView2/状态）
Banner '2. 应用数据目录（%LOCALAPPDATA% / %APPDATA%）'
foreach ($p in @("$env:LOCALAPPDATA\cn.grovemax.review", "$env:APPDATA\cn.grovemax.review")) {
    if (Test-Path $p) {
        Out ("  存在: $p")
        Show-DirTree $p 2
        # WebView2 崩溃转储单独点名
        $crash = Join-Path $p 'EBWebView\Crashpad\reports'
        if (Test-Path $crash) {
            $dmps = Get-ChildItem $crash -Filter *.dmp | Sort-Object LastWriteTime -Descending
            Out ("  WebView2 崩溃转储 $(($dmps | Measure-Object).Count) 个：")
            $dmps | Select-Object -First 10 | ForEach-Object {
                Out ("    $($_.Name)  $('{0:N0}' -f $_.Length) 字节  $($_.LastWriteTime)")
            }
        }
        # 运行环境原子交换状态（v1.2.7 逆向已证实：staged-* 下载 → verification 校验 → pending-swap → 重启换入 current）
        foreach ($j in @('runtime\runtime-pending-swap.json', 'runtime\selected-components.json')) {
            $jp = Join-Path $p $j
            if (Test-Path $jp) {
                Out ("  ---- $j 内容 ----")
                Get-Content -LiteralPath $jp | ForEach-Object { Out ('    ' + $_) }
            }
        }
        # 剪映草稿任务历史（Roaming 侧已证实存在 tracks-draft\runs）
        $runs = Join-Path $p 'tracks-draft\runs'
        if (Test-Path $runs) {
            Out '  ---- tracks-draft\runs 内容（剪映草稿任务历史） ----'
            Get-ChildItem -LiteralPath $runs -Recurse -Depth 3 | Select-Object -First 80 | ForEach-Object {
                $sz = if ($_.PSIsContainer) { '[目录]' } else { '{0:N0} 字节' -f $_.Length }
                Out ('    ' + $_.FullName.Substring($runs.Length) + "  ($sz, $($_.LastWriteTime))")
            }
        }
    } else {
        Out ("  不存在: $p")
    }
}

# ---------------------------------------------------------------- 3. 临时目录
Banner '3. 临时目录（%TEMP%\.grove-max-*）'
$tempHits = Get-ChildItem $env:TEMP -Filter '.grove-max-*' -Force |
    Sort-Object LastWriteTime -Descending
if ($tempHits) {
    foreach ($d in ($tempHits | Select-Object -First 20)) {
        $sz = (Get-ChildItem $d.FullName -Recurse -File | Measure-Object Length -Sum).Sum
        Out ("  $($d.FullName)  （$('{0:N0}' -f $sz) 字节，$($d.LastWriteTime)）")
        Get-ChildItem $d.FullName -Recurse -File | Select-Object -First 30 | ForEach-Object {
            Out ("      $($_.FullName.Substring($d.FullName.Length))  ($('{0:N0}' -f $_.Length) 字节)")
        }
    }
} else {
    Out '  未发现 .grove-max-* 临时目录（可能已自动清理，或任务尚未运行）。'
}

# ---------------------------------------------------------------- 4. 已知日志名全盘（用户目录）搜索
Banner '4. 任务日志搜索（已知文件名模式）'
$patterns = @(
    'audio-shift-*.log', 'audio-extract-*.log', 'render-draft-*.log', 'render-*.log',
    'watermark-result.log', 'watermark-error.log', 'watermark-*.log',
    'probe.error.log', 'encode-stderr.log', 'ai_label*', 'result.json',
    'checkpoint.json', 'mothers.progress', 'scan-progress.log', 'encode-progress.log',
    'failure.json', 'cost-report.png', 'cost-report-input.json'
)
$scanRoots = @(
    [Environment]::GetFolderPath('MyDocuments'),
    [Environment]::GetFolderPath('Desktop'),
    [Environment]::GetFolderPath('UserProfile') + '\Downloads',
    [Environment]::GetFolderPath('MyVideos'),
    [Environment]::GetFolderPath('MyPictures')
) + $Roots
$found = New-Object System.Collections.Generic.List[object]
$groveDirs = New-Object System.Collections.Generic.List[object]
foreach ($root in ($scanRoots | Select-Object -Unique)) {
    if (-not (Test-Path $root)) { continue }
    Out ("  扫描根: $root ...")
    foreach ($pat in $patterns) {
        Get-ChildItem -LiteralPath $root -Recurse -Depth 8 -Filter $pat -File -Force |
            Where-Object { $_.FullName -notmatch '\\(node_modules|\.git|AppData)\\' } |
            ForEach-Object { $found.Add($_) }
    }
    # 任务状态目录：逆向证实为工作目录下隐藏的 .grove（tasks/计划），mothers 为母版分集目录
    Get-ChildItem -LiteralPath $root -Recurse -Depth 8 -Directory -Force |
        Where-Object { $_.Name -in @('.grove', 'mothers', 'final') -and $_.FullName -notmatch '\\(node_modules|\.git|AppData)\\' } |
        ForEach-Object { $groveDirs.Add($_) }
}
# 任务状态目录单独列出（能直接证明工作目录位置）
if ($groveDirs) {
    Out ''
    Out ('  发现任务状态/产物目录 ' + ($groveDirs | Sort-Object FullName -Unique | Measure-Object).Count + ' 个：')
    $groveDirs | Sort-Object FullName -Unique | ForEach-Object {
        Out ("    [$($_.LastWriteTime.ToString('MM-dd HH:mm'))]  $($_.FullName)")
    }
}
# 去重后按修改时间排序
$allLogs = $found | Sort-Object FullName -Unique | Sort-Object LastWriteTime -Descending
Out ''
if ($allLogs) {
    Out ("  共发现 $(($allLogs | Measure-Object).Count) 个日志/结果文件，按修改时间排列：")
    foreach ($f in $allLogs) {
        Out ("    [$($f.LastWriteTime.ToString('MM-dd HH:mm'))]  $($f.FullName)  ($('{0:N0}' -f $f.Length) 字节)")
    }
} else {
    Out '  未发现已知命名的日志文件。若你确信跑过任务，请用 -Roots 指定素材所在目录再试。'
}

# ---------------------------------------------------------------- 5. 日志内容（推断运行逻辑的核心证据）
Banner '5. 日志内容尾部（按时间从新到旧，最多 12 个文件）'
if ($allLogs) {
    foreach ($f in ($allLogs | Select-Object -First 12)) {
        if ($f.Name -eq 'result.json' -and $f.Length -gt 1MB) { continue }
        Show-LogTail $f 40 300
    }
} else {
    Out '  （无）'
}

# ---------------------------------------------------------------- 6. Windows 事件日志中的崩溃记录
Banner '6. Windows 事件日志（近 14 天，Grove Max 相关）'
try {
    $evts = Get-WinEvent -FilterHashtable @{ LogName='Application'; StartTime=(Get-Date).AddDays(-14) } |
        Where-Object { $_.ProviderName -like '*grove*' -or $_.Message -like '*grove-max*' } |
        Select-Object -First 20
    if ($evts) {
        foreach ($e in $evts) {
            Out ("  [$($e.TimeCreated)] $($e.LevelDisplayName)  来源=$($e.ProviderName)  ID=$($e.Id)")
            $msg = $e.Message; if ($msg.Length -gt 400) { $msg = $msg.Substring(0,400) + ' ...(截断)' }
            Out ('    ' + ($msg -replace "`r`n", ' | '))
        }
    } else {
        Out '  近 14 天没有 Grove Max 相关的应用事件（说明没有崩溃上报）。'
    }
} catch {
    Out "  事件日志读取失败：$($_.Exception.Message)"
}

# ---------------------------------------------------------------- 7. 收尾
Banner '7. 运行逻辑推断指引'
Out @'
  如何用这些日志推断运行流程（结合逆向结论）：
  1) 找 render-draft-N.stdout.log：那是 Node sidecar 的输出，直接反映「生成」主流程的每一步；
  2) 对照 audio-extract-N -> audio-shift-N -> render-draft-N 的修改时间顺序，
     可以还原「音轨提取 -> 变调 -> 草稿渲染」的流水线节奏与并行度（N 即并行 worker 编号）；
  3) watermark-*.log / ai_label 相关日志出现与否，说明该任务是否启用了水印 / AI 标签阶段；
  4) probe.error.log / encode-stderr.log 报错 = 云端字幕或 V5 编码阶段失败；
  5) %LOCALAPPDATA%\cn.grovemax.review\EBWebView\Crashpad\reports 有 .dmp = 界面进程崩溃；
  6) result.json 是 sidecar 的结构化产物清单，JSON 键名直接对应内部状态机字段。
'@
Out ''
Out ("报告已保存到: $LogFile")

$Log | Set-Content -LiteralPath $LogFile -Encoding UTF8
Write-Host ''
Write-Host "完成。报告文件：$LogFile"
