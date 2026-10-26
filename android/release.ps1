<#
.SYNOPSIS
构建并发布 Android APK 到 GitHub Release（自更新下载源）。

.DESCRIPTION
流程：读 Cargo.toml 版本 → 盖章 AndroidManifest（versionName / versionCode）
→ build.ps1 构建 arm64-v8a → 固定资产名上传 Release。

签名提醒：正式发布必须用 android/make-signing-key.ps1 生成的发布密钥
（环境变量 NSWT_ANDROID_KEYSTORE/_PASSWORD、NSWT_ANDROID_KEY_ALIAS/_PASSWORD）。
脚本会拒绝发布 debug 证书（CN=Android Debug）签名的包；签名变化后手机上
无法覆盖安装，卸载重装还会丢 identity.json / session.json。

.EXAMPLE
./android/release.ps1 -AndroidRev 4                        # 发到默认仓库（客户端更新源）
./android/release.ps1 -AndroidRev 4 -Repo owner/other -Notes "修复"   # 显式指定别的仓库
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][ValidateRange(1, 99)][int]$AndroidRev,
    # 默认必须是客户端更新源（src/update.rs 的 REPO_OWN）：发到别的仓库时
    # Release 照样成功，但所有已装用户永远收不到更新。
    [string]$Repo = 'czcdr/NekoSportsWorldTool-antiban',
    [string]$Notes = '',
    [switch]$Draft,
    [switch]$SkipBuild,
    # 明确允许发布 debug 证书签名的包（只用于本地演示；正式 Release 不要开）。
    [switch]$AllowDebugKey
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$projectRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$manifest = Join-Path $PSScriptRoot 'AndroidManifest.xml'

# ── 版本号（Cargo.toml 是唯一真值）─────────────────────────────
$cargoToml = Get-Content (Join-Path $projectRoot 'Cargo.toml') -Raw
if ($cargoToml -notmatch '(?m)^version\s*=\s*"([^"]+)"') { throw 'Cargo.toml 中找不到 version' }
$version = $Matches[1]
# Cargo 版本可以带预发布尾巴（`0.4.0-antiban`），AndroidManifest 的 versionCode
# 只能吃数字：先切掉 `-suffix`，再逐段取前导数字。直接 [int]'0-antiban' 会抛
# "Cannot convert value"，脚本会在盖章之前就中止（本地发布链路整条不可用）。
$base = ($version -split '-')[0]
$parts = @()
foreach ($segment in ($base -split '\.')) {
    if ($segment -notmatch '^(\d+)') { throw "版本号段无法解析：$version（段 '$segment'）" }
    $parts += [int]$Matches[1]
}
if ($parts.Count -lt 3) { throw "版本号格式异常：$version" }
# versionName 用去掉预发布尾巴的基线（`0.4.0-android.N`），与 CI（apk.yml）一致；
# 否则同一版本会同时存在 `0.4.0-antiban-android.4` 和 `0.4.0-android.1` 两种写法。
$versionName = "$base-android.$AndroidRev"
# 沿用现有编码 0.2.5-android.3 = 20503（major*1e6 + minor*1e4 + patch*1e2 + rev）
$versionCode = $parts[0] * 1000000 + $parts[1] * 10000 + $parts[2] * 100 + $AndroidRev
Write-Output "版本：$versionName（versionCode $versionCode，基线 $base）→ $Repo"

# ── 盖章 AndroidManifest ─────────────────────────────────────
$manifestText = Get-Content $manifest -Raw
# 双引号里的 `\"` 不是 PowerShell 的转义（那是 cmd/原生命令的写法），
# 原稿这两行会直接把字符串截断导致整个脚本解析失败。用 -f 拼，别用反斜杠转义。
$updated = $manifestText -replace 'android:versionCode="\d+"', ('android:versionCode="{0}"' -f $versionCode)
$updated = $updated -replace 'android:versionName="[^"]*"', ('android:versionName="{0}"' -f $versionName)
$stamped = 'android:versionName="{0}"' -f $versionName
if ($updated -eq $manifestText -and $manifestText -notmatch [regex]::Escape($stamped)) {
    throw 'AndroidManifest.xml 盖章失败（未匹配到 versionCode/versionName）'
}
[IO.File]::WriteAllText($manifest, $updated)
Write-Output "已盖章 AndroidManifest.xml"

# ── 构建 ────────────────────────────────────────────────────
if (-not $SkipBuild) {
    & (Join-Path $PSScriptRoot 'build.ps1') -Abi arm64-v8a -Profile release
    if ($LASTEXITCODE -ne 0) { throw 'build.ps1 失败' }
}
$builtApk = Join-Path $PSScriptRoot 'build\arm64-v8a-release\NekoSportsWorldTool-arm64-v8a.apk'
if (-not (Test-Path -LiteralPath $builtApk)) { throw "找不到构建产物：$builtApk" }

# ── 签名门禁：debug 证书签出来的包不能进 Release ─────────────
# CN=Android Debug + 默认口令的密钥一旦泄露，别人能签出"签名一致"的更新包，
# 系统安装器和 App 内的签名自检都会放行。正式发布必须用 make-signing-key.ps1
# 生成的密钥（CI 侧另有 ANDROID_CERT_SHA256 固定校验）。
$sdkPath = if ($env:ANDROID_HOME) { $env:ANDROID_HOME } else { Join-Path $env:LOCALAPPDATA 'Android\Sdk' }
$apksigner = Join-Path $sdkPath 'build-tools\36.0.0\apksigner.bat'
if (Test-Path -LiteralPath $apksigner) {
    # 5.1 下 `2>&1` 会把 stderr 包成 RemoteException（与 build.ps1 同样的坑），
    # 这里改成落临时文件再读，避免"证书检查自己把发布打断"。
    $certFile = Join-Path $env:TEMP ("nswt-certs-" + [guid]::NewGuid().ToString('N') + ".txt")
    & $apksigner verify --print-certs $builtApk *> $certFile
    $certExit = $LASTEXITCODE
    $certs = Get-Content -LiteralPath $certFile
    Remove-Item -LiteralPath $certFile -Force -ErrorAction SilentlyContinue
    if ($certExit -ne 0) { throw 'apksigner verify 失败，产物不可发布' }
    $certs | Write-Output
    if (($certs -join "`n") -match 'CN=Android Debug') {
        if (-not $AllowDebugKey) {
            throw '构建产物用的是 debug 证书（CN=Android Debug），拒绝发布。请用 android/make-signing-key.ps1 生成的密钥，或显式加 -AllowDebugKey 只做本地演示。'
        }
        Write-Warning '产物由 debug 证书签名（-AllowDebugKey）：任何人都能伪造同签名更新包，不要对外分发。'
    }
} else {
    Write-Warning "未找到 apksigner（$apksigner），跳过发布前证书检查。"
}

# 固定资产名（更新器按此匹配；也兼容任意 *.apk 资产）
$asset = Join-Path $PSScriptRoot 'build\NekoSportsWorldTool-android-arm64.apk'
Copy-Item -LiteralPath $builtApk -Destination $asset -Force

# ── 发布 ────────────────────────────────────────────────────
if (-not (Get-Command gh -ErrorAction SilentlyContinue)) {
    throw "未安装 gh CLI。手动发布：`ngh release create v$versionName --repo $Repo --latest `"$asset`""
}
if ([string]::IsNullOrWhiteSpace($Notes)) {
    # 不要在这里写"签名未变可直接覆盖安装"：签名是否变化只有部署密钥的人知道，
    # 猜错一次就是全体用户装不上。需要时用 -Notes 显式说明。
    $Notes = "NekoSportsWorldTool $versionName`n`n签名与校验步骤见 RELEASE_SIGNING.md。"
}
$tag = "v$versionName"
$arguments = @('release', 'create', $tag, '--repo', $Repo, '--latest',
    '--title', "NekoSportsWorldTool $versionName", '--notes', $Notes)
if ($Draft) { $arguments += '--draft' }
$arguments += $asset
Write-Output "gh $($arguments -join ' ')"
& gh @arguments
if ($LASTEXITCODE -ne 0) { throw 'gh release create 失败' }
Write-Output "√ 已发布 $tag → https://github.com/$Repo/releases/tag/$tag"
Write-Output '提醒：Android 端（关于页）会自动检查该 Release 并提示安装。'
