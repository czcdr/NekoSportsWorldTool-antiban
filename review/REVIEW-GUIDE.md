# 审查指南（Review Guide）

本仓库是 [YanamiNeko/NekoSportsWorldTool](https://github.com/YanamiNeko/NekoSportsWorldTool)
在提交 `e229abd`（上游 main）上的**派生分支**，用于审查一组针对「被平台判定为异常客户端」与
「记录提交成功但不算数」的改造。

- 上游只读对照：`git diff e229abd..HEAD`（两个提交）
- 本分支不改上游已有协议实现，只补元数据与行为层
- 许可证沿用上游 `LICENSE`

---

## 1. 先看这里：审查优先级

按「影响面 × 争议度」排，建议按顺序看：

| # | 关注点 | 位置 | 需要判断什么 |
|---|---|---|---|
| 1 | **行为护栏会不会误拦正常使用** | `src/safety.rs` | 计数按「记录所属自然日」分桶、`reserve_run` 占位/归还的语义、状态文件损坏时的 fail-closed 是否过严 |
| 2 | **先占位再提交**是否真的堵住并发与 fail-open | `src/safety.rs` + `src/api/flow.rs` | `RunReservation` 的 `Drop` 归还会不会把「已成功提交」误判成失败 |
| 3 | **真机指纹**的取值口径 | `android/java/.../MainActivity.java`、`src/api/model.rs`、`src/crypto/header.rs` | `DisplayMetrics` 是窗口像素而非面板物理分辨率，是否可接受 |
| 4 | **出口同城**的失败策略 | `src/api/egress.rs`、`src/api/client.rs` | 同城节点全冷却时「停止而不是直连」；健康度放进程级共享的键选择 |
| 5 | **机型对照表**是否可信 | `src/api/fingerprint.rs` | 13 款机型的「代号 → 竖屏分辨率 → 支持系统版本」，有 `model_table_is_well_formed` 测试 |
| 6 | **10 秒窗与五点 `position`** 的语义 | `src/track/wire.rs` | `id`/`queueNum`/末窗缩放的依据只有上游注释，**没有抓包** |
| 7 | **请求头一致性** | `src/api/submit.rs`、`src/crypto/header.rs` | `appVersion` 与 UA 同源、UA 由身份拼；登录仍是 iOS 头（未解决） |

---

## 2. 本次已验证 / 未验证

### 已验证（本机实测）

| 项 | 命令 / 方式 | 结果 |
|---|---|---|
| 单元测试 | `cargo test --lib` | 217 passed / 0 failed |
| 静态检查 | `cargo clippy --lib --all-features` | exit 0 |
| Android release 构建 | `android/build.ps1 -Abi arm64-v8a` | 成功，产出 arm64 APK |
| APK 签名 / 16KB 对齐 | `apksigner verify` / `zipalign -c -P 16 4` | v2+v3 通过 / exit 0 |
| 二进制内容 | `.so` 内字符串 | 新增日志与错误文案都在 |

### 未验证（请重点质疑）

1. **跨请求平台混用**：登录/点位/策略走 iOS 头（`UA_IOS`, 7.3.40），只有提交走 Android 头（7.3.70），共用同一个 `device_id`。真实设备不会出现「同一 UUID 两个平台」。要定论必须真机抓包。
2. **外层 `appVersion` 头是否被服务端校验**：本轮只让请求内部三方一致，服务端看不看没有证据。
3. **`logicPixel` 口径**：上报 `DisplayMetrics` 的窗口像素，不等于面板物理分辨率；官方客户端怎么取未知。
4. **`speedPerTenSec` 的 `id`**：用 `(start_ms/1000) % 100000 + i`，时钟派生、约 27.8 小时回绕。上游用 `(rrid%100000)*1000+hi` 同样无据可依。
5. **点位接口末尾的目标距离**：`/api/v560/get/1/distance/<米>` 依据注释里的「真机实测」，本分支**未重新抓包**。
6. **埋点缺失**：官方客户端有 `startup/pv`、`datacollection/pv`、`ad/pv`，本工具一条都不发。字段（`caid`/`itemId`/`module`）必须抓包核对，凭空写会引入新异常，故未实现。

---

## 3. 复现构建

Windows 环境（本机实测）：

```powershell
# 依赖：Rust 1.99+、MinGW(gcc)、Android SDK+NDK、PowerShell 7
. E:\rea\tools\env.ps1          # 注入 CARGO_HOME / PATH / ANDROID_HOME
cd E:\rea\upstream
cargo test --lib
cargo clippy --lib --all-features
pwsh -File E:\rea\tools\build-android.ps1
```

环境坑（两个独立审计者都踩过）：默认工具链 `stable-x86_64-pc-windows-gnu`，
但 `gcc.exe` / `link.exe` 都不在 PATH 时 `ring` 的构建脚本会先失败 ——
**那是环境问题，不是代码问题**。交叉编译需要 `CC_aarch64_linux_android` 指向 NDK clang。

---

## 4. 安全边界

- 仓库内**不含** `identity.json` / `session.json` / `config.json` / 任何账号口令或代理凭据；
  `.gitignore` 已把运行期数据目录排除。
- `proxies.example.json` 只有 `127.0.0.1` 占位，没有任何真实出口。
- `review/` 下的审计报告由只读审计产生，可能包含本机绝对路径。

---

## 5. 已知缺陷（未修）

| 位置 | 问题 |
|---|---|
| `src/api/client.rs` | 任何 `ureq::Error::Transport` 都记为「出口失败」，目标域名不可达也会冷却出口 |
| `src/track/wire.rs` | `id` 时钟派生会回绕；与上方「跨跑步单调递增」的描述不完全一致 |
| `src/api/fingerprint.rs` | 未知机型 + 空分辨率时会按 seed 编造一个分辨率并上报（自检只说「无法核对」） |
| `src/api/points.rs` | 距离取整口径（`round().max(1)`）+ 预览/提交各拉一次点位 |
| `src/safety.rs` | `check_*` 读取不加锁；占用窗口被提交互斥挡住，但理论上仍可并发 |
| `src/safety.rs:739` | 测试里写死了 `2026-10-24` 作为「今天」的锚点，机器日期走到 2026-10-22 那天会挂（时间炸弹，非本轮引入） |

---

## 6. 联网探针（不是测试）

`src/api/gt4.rs::live_gt4_probe` 是一个**诊断挂具**，不是回归测试：

- 默认 `#[ignore]`，且**必须显式设置 `GT4_PROBE_CAPTCHA_ID`** 才会跑（否则直接 return）；
- 全程只有 `println!`，**没有任何断言** —— 它跑完永远是 ok，只用来人工看成功率；
- 会真实请求极验 `/load`、`static.geetest.com` 两图与 `/verify`。密集使用会让该出口
  被降级，所以：轮次默认 3、轮间隔 5–10s，**不要拿生产 captcha_id 反复刷**；
- CI 的两个 workflow 都没有 `cargo test` 步骤，因此不会被误触发。

```powershell
$env:GT4_PROBE_CAPTCHA_ID = "<captcha id>"
$env:GT4_PROBE_ROUNDS = "3"
$env:GT4_PROBE_DUMP = "D:\captcha-samples"   # 可选：落盘 bg/slice 与标注图
cargo test --lib live_gt4_probe -- --ignored --nocapture
```
