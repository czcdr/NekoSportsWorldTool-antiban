<#
.SYNOPSIS
生成 Android 正式发布签名密钥（一次），并输出配置 GitHub Secrets / 仓库变量的命令。

.DESCRIPTION
历史 APK 用 `CN=Android Debug` 的调试证书签名，口令是公开的 `android`：
拿到 keystore 文件的人可以签出"签名一致"的更新包，系统安装器和 App 内
签名自检都会放行。本脚本生成专用发布密钥（随机强口令、独立 DN），
keystore 与口令写到仓库之外的目录，不进入 Git。

用法（在仓库根执行）：
  ./android/make-signing-key.ps1                 # 生成到 ~/.nswt-signing/
  ./android/make-signing-key.ps1 -PrintSecrets    # 额外打印 gh secret set 命令

生成后需要：
  1) 把 ANDROID_KEYSTORE_BASE64 / ANDROID_KEYSTORE_PASSWORD /
     ANDROID_KEY_ALIAS / ANDROID_KEY_PASSWORD 四个 Secret 写进发布仓库；
  2) 把仓库变量 ANDROID_CERT_SHA256 设为输出里的证书指纹（CI 用它保证
     "这次发布的密钥 == 上次发布的密钥"，避免手滑换钥）；
  3) 本地发布时设置同名环境变量 NSWT_ANDROID_KEYSTORE(_PASSWORD) /
     NSWT_ANDROID_KEY_ALIAS / NSWT_ANDROID_KEY_PASSWORD。

注意：换钥后旧版本无法覆盖安装（签名不同），已装用户需要卸载重装一次，
identity.json / session.json 会一起清掉——这正是不应该随手换钥的原因。
#>
[CmdletBinding()]
param(
    [string]$OutputDirectory = (Join-Path $HOME '.nswt-signing'),
    [string]$Alias = 'nekosports',
    [string]$DistinguishedName = 'CN=NekoSportsWorldTool, O=NekoSportsWorldTool, C=CN',
    [int]$ValidityDays = 10000,
    [switch]$Force,
    [switch]$PrintSecrets
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$keytool = (Get-Command keytool -ErrorAction Stop).Source

New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
$keyStore = Join-Path $OutputDirectory 'nekosports-release.keystore'
$passwordFile = Join-Path $OutputDirectory 'keystore-password.txt'
if ((Test-Path -LiteralPath $keyStore) -and -not $Force) {
    throw "已存在 $keyStore。换钥会让已装用户无法覆盖安装；确实要换请加 -Force（并先备份旧钥）。"
}

# 32 字符随机口令（大小写+数字），仅写入仓库外的口令文件。
$bytes = New-Object 'byte[]' 24
$rng = [Security.Cryptography.RandomNumberGenerator]::Create()
try { $rng.GetBytes($bytes) } finally { $rng.Dispose() }
$password = [Convert]::ToBase64String($bytes).Replace('+', 'A').Replace('/', 'B').TrimEnd('=').Substring(0, 32)

if (Test-Path -LiteralPath $keyStore) { Remove-Item -LiteralPath $keyStore -Force }
# keytool 把进度写到 stderr；Windows PowerShell 5.1 在 ErrorActionPreference=Stop
# 下会把原生命令的 stderr 包成终止性 RemoteException（build.ps1 里同样的坑）。
# 这里降级为 Continue，只按退出码判定，输出落到临时文件便于排错。
function Invoke-Keytool([string[]]$Arguments, [string]$LogPath) {
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { & $keytool @Arguments *> $LogPath } finally { $ErrorActionPreference = $previous }
    # 只回传退出码：函数里再 Write-Output 会把输出混进返回值，
    # `$code -ne 0` 就成了数组比较，成功也会被判成失败。
    return $LASTEXITCODE
}
$keytoolLog = Join-Path $env:TEMP ("nswt-keygen-" + [guid]::NewGuid().ToString('N') + ".txt")
$code = Invoke-Keytool @('-genkeypair','-keystore',$keyStore,'-storetype','PKCS12','-storepass',$password,'-keypass',$password,
    '-alias',$Alias,'-keyalg','RSA','-keysize','4096','-validity',"$ValidityDays",'-dname',$DistinguishedName) $keytoolLog
if ($code -ne 0) {
    Get-Content -LiteralPath $keytoolLog -ErrorAction SilentlyContinue | Write-Output
    Remove-Item -LiteralPath $keytoolLog -Force -ErrorAction SilentlyContinue
    throw 'keytool 生成密钥失败'
}
Remove-Item -LiteralPath $keytoolLog -Force -ErrorAction SilentlyContinue
[IO.File]::WriteAllText($passwordFile, $password)
# 口令文件只给当前用户读。
$acl = Get-Acl -LiteralPath $passwordFile
$acl.SetAccessRuleProtection($true, $false)
$rule = New-Object Security.AccessControl.FileSystemAccessRule(
    [Security.Principal.WindowsIdentity]::GetCurrent().Name, 'FullControl', 'Allow')
$acl.AddAccessRule($rule)
Set-Acl -LiteralPath $passwordFile -AclObject $acl

$sha256 = [Security.Cryptography.SHA256]::Create()
try { $digest = $sha256.ComputeHash([IO.File]::ReadAllBytes($keyStore)) } finally { $sha256.Dispose() }
$base64 = [Convert]::ToBase64String($digest)
Write-Output "keystore : $keyStore"
Write-Output "password : $passwordFile（仅当前用户可读）"
Write-Output "alias    : $Alias"
Write-Output "keystore SHA-256: $($digest | ForEach-Object { $_.ToString('x2') })"

# 自签名证书指纹（CI 用 ANDROID_CERT_SHA256 固定它，格式与 apksigner 一致：
# 小写十六进制、无冒号）。
$certSha = $null
$tmp = Join-Path $env:TEMP ("nswt-keytest-" + [guid]::NewGuid().ToString('N') + ".txt")
$previous = $ErrorActionPreference
$ErrorActionPreference = 'Continue'
try { & $keytool -list -v -keystore $keyStore -storepass $password -alias $Alias *> $tmp } finally { $ErrorActionPreference = $previous }
if ($LASTEXITCODE -eq 0) {
    # 不认 "SHA256:" 这个标签（keytool 输出跟着 JVM 语言走），直接匹配指纹格式：
    # 32 组十六进制、冒号分隔。SHA-1 是 20 组，不会误命中。
    $raw = Get-Content -LiteralPath $tmp -Raw
    $match = [regex]::Match($raw, '(?:(?:[0-9A-Fa-f]{2}:){31}[0-9A-Fa-f]{2})(?![0-9A-Fa-f:])')
    if ($match.Success) {
        $certSha = $match.Value.Replace(':', '').ToLowerInvariant()
        Write-Output "certificate SHA-256: $certSha"
    }
}
Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue

Write-Output ''
Write-Output '── 配置 GitHub Secrets（发布仓库）────────────────────────────'
Write-Output "gh secret set ANDROID_KEYSTORE_BASE64  --repo czcdr/NekoSportsWorldTool-antiban --body `"$([Convert]::ToBase64String([IO.File]::ReadAllBytes($keyStore)))`""
Write-Output "gh secret set ANDROID_KEYSTORE_PASSWORD --repo czcdr/NekoSportsWorldTool-antiban --body `"$password`""
Write-Output "gh secret set ANDROID_KEY_PASSWORD      --repo czcdr/NekoSportsWorldTool-antiban --body `"$password`""
Write-Output "gh secret set ANDROID_KEY_ALIAS         --repo czcdr/NekoSportsWorldTool-antiban --body `"$Alias`""
if ($certSha) {
    Write-Output "gh variable set ANDROID_CERT_SHA256     --repo czcdr/NekoSportsWorldTool-antiban --body `"$certSha`""
}
Write-Output ''
Write-Output '本地发布（避开把密钥写进命令行历史）：'
Write-Output "  `$env:NSWT_ANDROID_KEYSTORE = '$keyStore'"
Write-Output "  `$env:NSWT_ANDROID_KEYSTORE_PASSWORD = (Get-Content '$passwordFile' -Raw).Trim()"
Write-Output "  `$env:NSWT_ANDROID_KEY_ALIAS = '$Alias'"
Write-Output "  `$env:NSWT_ANDROID_KEY_PASSWORD = `$env:NSWT_ANDROID_KEYSTORE_PASSWORD"
Write-Output ''
Write-Output '备份提醒：keystore + 口令文件丢失后无法再发布可覆盖安装的更新（只能再换钥）。'
