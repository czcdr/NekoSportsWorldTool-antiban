# 发布签名与产物校验

本文说明两类产物的签名现状、如何校验，以及换钥/没有证书时的边界。
最后更新：0.5.0（Android 发布密钥已从调试证书换成专用发布密钥）。

## 一、Android APK

### 现状

- 0.4.1 及更早的 APK 由 **Android 调试证书**签名（`CN=Android Debug`，口令
  `android`）。任何拿到该 keystore 文件的人都能签出"签名一致"的更新包，
  系统安装器与 App 内自检都会放行。
- 0.5.0 起改用 `android/make-signing-key.ps1` 生成的专用密钥（RSA 4096、
  随机强口令、独立 DN），keystore 与口令只保存在仓库之外。
- **签名变更意味着旧版本无法覆盖安装**：0.4.1 及更早的用户升级到 0.5.0
  需要先卸载（会清掉 `identity.json` / `session.json`），之后恢复正常覆盖安装。

### CI 门禁（`.github/workflows/apk.yml`）

tag 构建必须同时具备：

| 名称 | 类型 | 内容 |
|---|---|---|
| `ANDROID_KEYSTORE_BASE64` | Secret | keystore 文件的 base64 |
| `ANDROID_KEYSTORE_PASSWORD` | Secret | keystore 口令（不再回落到 `android`） |
| `ANDROID_KEY_ALIAS` | Secret | 密钥别名 |
| `ANDROID_KEY_PASSWORD` | Secret | 密钥口令（缺省同 keystore 口令） |
| `ANDROID_CERT_SHA256` | Variable | 证书 SHA-256 指纹（小写、无冒号） |

构建结束前会：

1. `apksigner verify --print-certs` 断言证书不是 `CN=Android Debug`；
2. 断言证书指纹与 `ANDROID_CERT_SHA256` 一致——**手滑换钥会直接失败**，
   而不是发一个所有用户都装不上的包；
3. 产出 `SHA256SUMS.txt` 并生成构建来源证明。

### 本地发布

```powershell
$env:NSWT_ANDROID_KEYSTORE = "$HOME\.nswt-signing\nekosports-release.keystore"
$env:NSWT_ANDROID_KEYSTORE_PASSWORD = (Get-Content "$HOME\.nswt-signing\keystore-password.txt" -Raw).Trim()
$env:NSWT_ANDROID_KEY_ALIAS = 'nekosports'
$env:NSWT_ANDROID_KEY_PASSWORD = $env:NSWT_ANDROID_KEYSTORE_PASSWORD
./android/release.ps1 -AndroidRev 1
```

`release.ps1` 会在打包后检查证书：仍是 `CN=Android Debug` 时直接拒绝发布，
除非显式加 `-AllowDebugKey`（仅供本地演示）。

### 校验下载的 APK

```bash
sha256sum -c SHA256SUMS.txt
gh attestation verify NekoSportsWorldTool-android-arm64.apk \
  --repo czcdr/NekoSportsWorldTool-antiban
apksigner verify --print-certs NekoSportsWorldTool-android-arm64.apk
```

## 二、桌面产物（Windows / macOS / Linux）

桌面二进制**没有** Authenticode 或 Apple 代码签名（需要购买代码签名证书），
所以：

- Windows 会提示"未知发布者"，SmartScreen 可能拦截首次运行；
- macOS 未公证，首次打开会提示"无法验证开发者"（需右键打开或在系统设置放行）；
- Linux 无签名概念，官方产物为 CLI-only。

现有的完整性保障是：

1. **SHA256SUMS.txt**：随 Release 发布，`sha256sum -c` 可逐项核对；
2. **GitHub 构建来源证明**：`gh attestation verify <文件> --repo <仓库>`，
   证明该文件确由本仓库对应 workflow 在 GitHub Actions 上构建；
3. **App 内更新**：只认固定仓库 + Release API 的 `sha256:<hex>` 摘要，并对
   同 tag/资产名做过 TOFU pin（摘要变化即拒绝）。

这三条都挡不住"发布账号被完全接管后，从新 tag 首次发一个恶意包"：
那需要发布侧签名（Authenticode / Apple notarization / minisign 私钥），
在拿到证书或自管签名密钥之前属于已知残余风险。

## 三、换钥流程（确实需要时）

1. 备份旧 keystore 与口令（回滚要用）；
2. `./android/make-signing-key.ps1 -Force`（不要把 keystore 放进仓库）；
3. 按脚本输出更新 4 个 Secret 与 `ANDROID_CERT_SHA256` 变量；
4. Release 说明里明确写"签名已更换，需卸载重装"；
5. 旧版本仍留在 Releases 里，必要时可让用户回退（同样需要卸载）。
