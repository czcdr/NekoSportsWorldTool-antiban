# 改动清单（Changes）

基线：上游 `e229abd`。下面是本分支相对基线的全部改动，按关注点分组；
同一个文件可能出现在多组（例如 `model.rs` 同时承载指纹与配置字段）。

---

## A. 「记录提交成功但不算数」的修复（`fix(record)`）

| 文件 | 改动 | 依据 |
|---|---|---|
| `src/track/wire.rs` | `fivePointJson[].position` 从写死 `999` 改为数组下标；`build_windows` 的 `queueNum` 改为窗内下标、末窗按实际时长缩放距离与步数并夹到 220 步/分 | 上游 PR #106（**至今未合并**）：服务端按下标对齐点位；详情页用 `步数/时长` 反算，末窗按满 10 秒会算出 300+ |
| `src/api/points.rs` | 点位接口 `/api/v560/get/1/distance/<米>`：末尾是**本次目标距离**，原来写死 `1` | 同上 |
| `src/api/model.rs` | `PointsCacheScope` 增加 `distance_m`，不同距离不复用缓存 | 同上 |
| `src/ui/jobs.rs` | 预览取点位时把当前方案距离传下去 | 同上 |
| `src/api/flow.rs` | 提交后读详情，把 `reasonList` 里 `complete=false` 的文案打进日志（`⚠ [verify] 未计数原因：…`） | 让"为什么不算数"可见 |

---

## B. 设备指纹自洽（`feat(fingerprint)`）

| 文件 | 改动 | 依据 |
|---|---|---|
| `src/api/fingerprint.rs`（新增） | 13 款机型的「代号 → 竖屏分辨率 → dpi → 支持系统版本」；`androidId` 16 位小写 hex；MAC 固定 `02:00:00:00:00:00`；`audit_ex()` 自检 | Android 6+ 普通应用读 MAC 固定为全零；`ANDROID_ID` 一定能取到，留空是强特征 |
| `src/crypto/header.rs` | `logicPixel`/`physicPixel` 不再写死 `1080x2400`；`androidId`/`blMac`/`wifiMac` 改由身份提供；新增 `ensure_fingerprint()`、`ua_android()`；空 `device_id` 不派生指纹、兜底 ID 进程内固定 | 原实现：`androidId` 与 MAC 全空、分辨率与机型不符 |
| `android/java/.../MainActivity.java` | 读 `DisplayMetrics` 宽高/dpi 与 `Settings.Secure.ANDROID_ID`，同意后才读 | 真机读数「由构造即自洽」，比机型表可靠 |
| `src/api/model.rs` | `DeviceInfo` 增加 `pixel`/`density_dpi`/`android_id`；`identity_with_device_info` 优先采用真机读数并标记来源 | 同上 |
| `src/ui/device.rs` | 「随机生成」改为整套自洽生成；保存时按需重算派生字段；设备页显示 `√ 指纹自洽（本机读数）` 或标黄 | 伪机型名（`Redmi K60` 等）不是 `Build.MODEL` |

---

## C. 出口 IP 与轨迹同城（`feat(egress)`）

| 文件 | 改动 | 依据 |
|---|---|---|
| `src/api/egress.rs`（新增） | 按城市选出口、绝不跨城；失败按 60/180/3600 秒升档冷却；健康度与游标放**进程级共享**（键 = 配置内容指纹） | 原实现每作业重建 `ApiClient`，状态挂在实例上 → 冷却失效、永远选同一条 |
| `src/api/client.rs` | `make_agent_with_proxy`、`bind_egress_for_city`（先校验代理串再绑定）、`report_egress`、请求间隔随机化 | 代理串写错时宁可报错，不能"以为走代理、其实直连" |
| `src/api/flow.rs` | 整条链路（policy/点位/提交/OBS/详情）共用同一出口；无同城节点时告警「本次直连」 | 轨迹城市与来源 IP 不一致是最廉价的风控手段 |

---

## D. 行为护栏（`feat(safety)`）

| 文件 | 改动 | 依据 |
|---|---|---|
| `src/safety.rs`（新增） | 深夜 23:00-06:00 拦截；按**记录所属自然日**限制跑步/AI 条数；批量补签限制天数、同日重复、单批总量；`reserve_run()` 锁内「校验+占位」，`Drop` 归还；状态文件原子写 + `.bak` 回退 + 抢锁 2s 超时 | 原版**已有**时段设计（随机 7:00-20:00 / 补签 7:00-21:00），缺的是频次约束；且"检查→提交→记账"存在并发窗口与 fail-open |
| `src/api/flow.rs` | `reserve_run` → 提交 → `commit()`；设备时区非 UTC+8 时告警 | 同上 |
| `src/ui/ai.rs` | 批量补签的规模检查 + **逐条按记录日期入账** | 原来完全不记账，`day==0` 一轮可给今天发多条 |
| `src/api/model.rs` | `Config.safety` 字段（容器级 `#[serde(default)]`，缺字段回落默认值） | 少写字段会让整份 `config.json` 反序列化失败被静默重置 |
| `src/ui/device.rs` | 设备页底部护栏开关 | — |

---

## E. 请求头一致性（`fix(headers)`）

| 文件 | 改动 | 依据 |
|---|---|---|
| `src/api/submit.rs` | 外层 `appVersion` 跟随平台（Android → 7.3.70）；UA 由身份拼（`ua_android`） | 原来 UA 与信封都是 7.3.70、外层头却是 iOS 的 7.3.40；UA 还写死机型 |
| `src/api/client.rs` | `appVersion` 按 UA 平台取值 | 同上 |
| `src/api/login.rs` | 登录 body 的 `mac_address` 改用占位值 | 原来发随机 MAC，与提交头的占位 MAC 矛盾 |

---

## F. 构建与元数据

| 文件 | 改动 |
|---|---|
| `Cargo.toml` / `Cargo.lock` | `ureq` 增加 `socks-proxy` feature |
| `android/AndroidManifest.xml` | `versionCode 30007` / `versionName 0.3.5-antiban`（高于原版 30001，可覆盖安装） |
| `proxies.example.json`（新增） | 出口池样例（`127.0.0.1` 占位，无真实出口） |
| `src/api/mod.rs`、`src/lib.rs` | 挂载 `egress` / `fingerprint` / `safety` |
| `review/`（新增） | 本审查材料 |

---

## G. 已知未修

见 `review/REVIEW-GUIDE.md` 第 5 节。

---

## H. 0.3.6：外部审计十条的核实与修复（`fix(audit)`）

来源：一份只读外部审计（10 条）。逐条回到代码核实后再修，**核实为「设计取舍」的没有硬改**。

| # | 条目 | 核实 | 改动 | 文件 |
|---|---|---|---|---|
| 1 | 明文凭据且无文件权限 | 属实 | `write_json` 改为 tmp→rename 原子写 + Unix 0600；解析失败的文件改名 `.corrupt` 并告警 | `src/api/model.rs` |
| 2 | 更新无完整性校验 | 属实 | 取 Release API `assets[].digest` 做 SHA-256 + 大小校验；桌面无摘要拒绝替换，APK 交给系统签名校验 | `src/update.rs`、`src/ui/jobs.rs`、`src/cli/cmds.rs` |
| 3 | 日志泄露响应与代理凭据 | 属实 | 响应摘要 200 字符封顶（`NSWT_LOG_VERBOSE=1` 才全量）；`redact_url`/`redact_text` 打码 `user:pass@`；错误体按键脱敏 | `src/textlog.rs`、`src/api/client.rs`、`src/api/egress.rs` |
| 4 | `--pass` 进历史/进程列表 | 属实 | `--pass -` 读标准输入；`NSWT_PASS` 兜底；usage 同步 | `src/cli/cmds.rs`、`src/cli/mod.rs` |
| 5 | 桌面版本号停在 0.3.0 | 属实 | `0.3.6-antiban`，Cargo/lock/manifest 三处同步 | `Cargo.toml`、`Cargo.lock`、`android/AndroidManifest.xml` |
| 6 | 下载/解压无上限 | 属实（且 zip 按声明 size 预分配） | 512 MB 下载、256 MB 单文件、1 GB 累计；预分配不再信任声明值 | `src/update.rs` |
| 7 | Android 更新源指向第三方账号 | 属实 | 默认源改为本项目仓库；`NSWT_UPDATE_REPO` 可覆盖；关于页同源 | `src/update.rs`、`src/ui/about.rs` |
| 8 | CI 权限、签名密钥、Action 未固定 SHA | 属实 | 构建 job `contents: read`、发布 job 单独 `contents: write`；非 tag 强制一次性密钥；全部 action 固定 SHA | `.github/workflows/apk.yml`、`release.yml` |
| 9 | 写文件失败仍报「已保存」 | 属实（3 处） | 传播错误并返回 1 | `src/cli/cmds.rs` |
| 10 | 按字节截断 UTF-8 panic | 属实（3 处） | 改 `textlog::truncate`（按字符）+ 回归测试 | `src/cli/cmds.rs` |

**未采用审计建议的部分**（理由见 `dist/README.md` 第三节）：系统钥匙串存密码、发布侧签名清单、GUI 保存失败提示、`save_session` 失败改返回契约。

**新增测试**：`textlog` 的 URL 脱敏、`model` 的原子写/权限/坏文件留档、`update` 的摘要校验 / 大小校验 / 解包上限 / 下载上限（含本地 HTTP 服务器触发两条 limiter 分支）、`cli` 的多字节预览截断。

---

## I. 0.3.7-antiban：第二轮审计（出口覆盖、UA 一致、补签节奏）

第二轮审计只看一件事：**服务端看到的「同一条记录、同一个账号」是否前后自洽**。七项都是这条主线上的缺口。

| # | 问题 | 改法 | 文件 |
|---|---|---|---|
| 1 | 出口只在 `flow::run_full_flow` 绑定，登录 / AI 提交 / 读记录 / 排行榜 / 违规自查全部直连真实 IP —— 同一账号几分钟内「家宽→代理→家宽」来回跳，比一直直连更像异常 | 绑定提到 `ApiClient::new`（`bootstrap_egress`）；`bind_egress_for_city` 改幂等；新增 `egress_guard()`，绑定失败时**请求前拦下**而不是降级直连；提交路径在写 pending 回执**之前**调用 | `src/api/client.rs`、`src/api/submit.rs` |
| 2 | `call_host` 的 UA 写死 iOS，而信封按 `identity.platform` 生成：android 身份会发出「osType=0 + androidId 有值 + iPhone UA + 外层 appVersion 7.3.40 / 信封 7.3.70」 | 新增 `ua_for(identity)`，UA 与信封、外层 `appVersion` 同源 | `src/crypto/header.rs`、`src/api/client.rs` |
| 3 | iOS 侧完全没做指纹自洽：`cpuModel: x86_64`（模拟器签名）、`logicPixel 360x640` / `physicPixel 1080x1920` 写死 | 新增 `IosProfile` / `IOS_MODELS`（5 台真机，逻辑×2或×3=物理）；`cpuModel` 改 `arm64`；逻辑分辨率缺失时按 @3x 反推，保证整数倍自洽 | `src/api/fingerprint.rs`、`src/crypto/header.rs` |
| 4 | `avgStepFreq` 用原始值，而 `stepsPerTenSec` 已按 220 步/分夹过 —— 短时长会出现「窗口 220、汇总 250」 | 抽出 `wire::MAX_STEP_FREQ_PER_MIN`，两处共用 | `src/track/wire.rs`、`src/api/submit.rs` |
| 5 | 护栏只按「记录所属日」计数，跑步页支持「今天/昨天/1-3 天前」，一分钟内可连提交 3 天、日期各不相同（补签特征） | 新增 `daily_submits`（默认 2，按**提交日**计），与 `daily_runs` 两层并行；`RunReservation` 同时归还两个计数 | `src/safety.rs` |
| 6 | `acquire("")` 时空城市让任意城市出口中选（fail-open），与「绝不跨城」冲突；测试还把这条钉成预期行为 | 空城市 → `NoCandidate`，测试与注释同步改为 fail-closed | `src/api/egress.rs` |
| 7 | 提交响应解不开时记 `Unknown` → pending 回执，此后每次提交被挡；核对不到就永久卡死 | `Guard::pending_count/clear_pending`（只追加 `Rejected`，不删记录）+ CLI `unlock [--yes]` | `src/api/submission_guard.rs`、`src/cli/cmds.rs`、`src/cli/mod.rs` |

**未改动（需要真机抓包才能定论）**：提交 body 的 `complete: true` / `unCompleteReason: 0` 是否由客户端自报、`selDistance` 应填校方最低要求还是本次目标距离。这两处改了是猜，猜错比不改更糟。

**验证**：`cargo test --lib` 227 passed / 0 failed；`cargo clippy --lib --all-features` 退出码 0（本次新增代码零告警）；Android release 交叉编译 + `apksigner verify`（v2+v3）+ `zipalign -c -P 16 4` 通过。

---

## J. 0.3.7-antiban 续：一次运行只能是一台设备（假跑驱动）

这一节的问题不是读代码读出来的，是**假跑**跑出来的：把整条链（策略 → 围栏 → 点位 →
轨迹 → 提交 → OBS → 详情）跑在 loopback mock 上，再逐条看客户端真正发出的报文。
结果是同一次运行出现了**四套 User-Agent**。

| # | 问题 | 改法 | 文件 |
|---|---|---|---|
| 1 | OBS 直传没设 UA，发出去的是 `ureq/2.12.1` —— 唯一携带轨迹数据的那一步在自报「我是 Rust 脚本」 | `put_object` 显式收 `ua` 并设置；`upload_both_keys` / `fetch_object` 传 `ua_for(&client.identity)` | `src/api/obs.rs` |
| 2 | 登录与点位写死 iOS UA（`SWCampus/7.3.40 (iPhone; …)`），其余链路是 Android UA | 全部改走 `ua_for(&client.identity)`；`ua_for` 成为全链唯一 UA 来源 | `src/api/login.rs`、`src/api/points.rs` |
| 3 | Android UA 曾由 `ua_android()` 拼出，把 iOS 的 `osVersion`/`deviceName` 塞进 Android 模板，得到 `Android 26.5.2; iPhone` | 删除 `ua_android()`；新增 `HeaderIdentity::android_model_os()` —— 机型与系统版本**成对**从同一台设备取，占位值按 `device_id` 从机型库派生 | `src/crypto/header.rs` |
| 4 | 提交与非提交链路各自派生 Android 档案 → 同一账号报两种机型（`ALN-AL80/12` 与 `22081212C/14`）；空 `device_id` 时提交随机生成、其余链路另算 | 删除 `submit.rs` 的私有派生，统一调 `android_model_os()`；空 `device_id` 共用 `stable_fallback_device_id()` | `src/api/submit.rs`、`src/crypto/header.rs` |
| 5 | Android UA 用的是自造标识 `SWCampus/7.x`。官方 7.4.0 **Android** 包里 `SWCampus` **0 命中**，dex 只有 `okhttp/3.14.9`、`okhttp/4.12.0` 两条默认值 —— 官方 Android 客户端没有设自定义 UA | Android UA 改为 `okhttp/4.12.0`。注意结论边界：这只证明 Android 客户端没设 UA，**不能**据此否定 iOS 侧的 `SWCampus`（`UA_IOS` 保持原样、仍待验证） | `src/crypto/header.rs` |
| 6 | 外层 `appVersion` 靠 `ua.contains("Android")` 判断平台 —— UA 一改成 `okhttp/4.12.0` 就会退回 iOS 的 7.3.40 | 改为按 `identity.platform` 取版本 | `src/api/client.rs` |
| 7 | 桌面端默认身份是 iOS（`cfg!(target_os="android")` 才给 Android），于是「提交走 Android、其余链路走 iOS」，一次运行两套身份 | `load_identity()` 统一按 Android 加载；新增 `platform_migrated` 标记做一次性迁移：**只迁移从没被动过的旧默认身份**（`iPhone` + `26.5.2`），用户自己选过的 iOS 原样保留 | `src/api/model.rs`、`src/crypto/header.rs` |
| 8 | **OBS 直传不走出口代理**：`put_object` 自建 `make_agent()`，配了同城出口时这一步从家宽直连 —— 唯一带轨迹数据的那一步换了 IP | `put_object` 改为接收调用方的 `agent`（`client.agent.clone()`，带同城代理）；极验 `gt4::solve_once` 同样改用它 | `src/api/obs.rs`、`src/api/gt4.rs` |
| 9 | 提交体 `body.timeStamp` 恒等于 `header.timeStamp + 1`，服务端两层解密做一次减法就得到常量 1 | 改为独立读一次时钟（允许相等），去掉恒定偏移 | `src/api/client.rs`、`src/api/submit.rs` |
| 10 | 同一条 `geevalidate` 请求里 body `osType=1`（iOS 写死）而 `headerSign` 是 Android | `osType` 按 `identity.platform` 派生 | `src/api/login.rs` |
| 11 | iOS 身份切到 Android 提交时没清 `pixel`，`ensure_fingerprint` 只补空字段 → 报「Android 机型 + iPhone 分辨率」 | 跨平台时先清 `pixel`/`logic_pixel`/`idfa` 再派生 | `src/api/submit.rs` |

**回归防护**：`src/api/fake_run_tests.rs` 在 loopback mock 上跑完整条链，断言
（1）请求集合完整（含 2 次 OBS 签名 + 2 次 PUT，少一条即红）、
（2）`ua_for(identity)` 的**具体值**逐条匹配（不是"去重后只有一种"）、
（3）`appVersion` 与信封同源、OBS 直传不带 `appVersion`、
（4）`obs_ok == 2` 且详情验证 `complete == Some(true)`（这两处 `flow` 只写日志、不改返回值，
`outcome.is_ok()` 盖不住）、
（5）iOS 身份的「提交 Android / 其余 iOS」双身份被显式断言，改动即提醒同步结论。
mock 对未知路径返回业务错误，避免"什么都说成功"掩盖意外请求。

**仍然未定（需要真机/抓包）**：
- iOS 侧的 `UA_IOS` 没有任何证据支撑 —— 手上的官方包只有 Android 版。
- 提交固定走 Android 信封（`osType=0` + `androidId` 结构），显式选 iOS 的用户提交那条仍是 Android。

### J.2 第二轮：按「被识别成本」再收一遍

| # | 问题 | 改法 | 文件 |
|---|---|---|---|
| 12 | `nonce`/`timeStamp`/`tokenSign` 三个防重放头只在提交请求上出现，其余六七个请求都没有 —— 同 token 做一次头集合 diff 就能挑出提交那条 | 已登录请求统一带上（未登录的登录请求不发，避免空 token 签名） | `src/api/client.rs` |
| 13 | `keyDataFour` 恒以 `.000000` 结尾：整数毫秒当 double 格式化，服务端一个正则分桶 | 改用带小数的毫秒（`timestamp_micros/1000`），与 native `CFAbsoluteTime*1000` 的形态一致 | `src/crypto/envelope.rs` |
| 14 | `appVersion` 报 7.3.70，而现网是 7.4.0 —— 全部账号共用一个旧版本号，容易成簇、也可能被强更拦 | Android 上报版本改为 `7.4.0`（本次逆向的正是这个包，端点与 7.3.70 完全一致） | `src/crypto/header.rs` |
| 15 | 点位拉取与提交落在同一秒：真机这两件事隔着整段跑步时间 | 提交前加 2.5~5s 抖动，至少把两者拆开 | `src/api/flow.rs` |
| 16 | 所有记录的 `bdA` 都围绕恒定 82m 回归，海拔均值成了聚类特征 | 基准海拔每次跑随机抽 20~260m，围绕它做回归 | `src/track/generator.rs` |

**明确没改的（改了可能对代跑不利，或需要证据）**：

| 项 | 为什么不动 |
|---|---|
| HTTP/TLS 指纹（ureq+rustls、HTTP/1.1、无 ALPN h2、无 `Accept-Encoding`/`Connection`） | 要对齐 OkHttp+Conscrypt 得换 BoringSSL 栈并启用 h2，属独立一轮的工程；半对齐（只加 `Accept-Encoding: gzip` 而不解压）反而会把响应解坏。**这是当前最大的单一特征，优先级最高。** → 已在 **J.3**（h2/gzip）与 **L.1**（cipher 顺序/曲线/ALPN）处理；仍差扩展集合 |
| `complete: true` / `unCompleteReason: 0` | 服务端若做交叉核对会露馅，但改成 false 等于主动让记录不算数 —— 直接损害代跑目的 |
| `validDistance == totalDis` 恒等（100% 有效里程） | 扣一点里程更像真机，但一旦扣到 `minDistance` 以下记录直接不算数；在没有服务端口径证据前不动 |
| `faceCheck=1` 而 `runFaceCheck` 为空 | 同样是「像真机」与「能算数」的取舍：置 0 可能让需要人脸核验的学校判无效 |
| 提交窗口 `id = 60000+i` 与 OBS 的 `id` 口径不同 | 代码注释表明这是两种格式的既有差异（`submit_windows` 的注释），没有抓包证据前不擅自统一 → **已在 L.2 统一**（以内部一致优先） |
| 机型库仅 13 款、`androidId` 由 `device_id` 派生 | 属数据工程（补机型长尾）与「别整份复制 identity.json」的使用约束，不是代码 bug → **已在 L.3 扩到 24 款并解耦 `androidId`** |

**验证**：`cargo test --lib` 231 passed / 0 failed（含假跑双身份回归）；`cargo clippy --lib --all-features` 退出码 0；`cargo check --all-targets --all-features` 与 `--target aarch64-linux-android --features android` 均通过。假跑抓包已核对：`policy` 请求头含 `nonce/timeStamp/tokenSign`，`appVersion: 7.4.0`，全链 UA 统一。

### J.3 HTTP 栈整体更换：ureq(HTTP/1.1) → reqwest(ALPN h2 + gzip)

这是「被识别成本最低」的那一条：官方客户端是 OkHttp + Conscrypt，TLS 握手里 ALPN 带 `h2`、
之后走 h2 帧；而原来的 `ureq` 只能 HTTP/1.1，ALPN 里也只报 `http/1.1`。网关取一次
JA3/JA4 + ALPN 就能把两类客户端分开，零误伤、零成本。

**做法：在调用面之下换栈**，而不是改 35 个调用点。新增 `src/http.rs` 作为门面，对外保持
`Agent` / `Request::set/call/send_string/send_bytes/send_form` /
`Response::status/header/into_string/into_reader` / `Error::Status|Transport`，内部是 reqwest。
这样 8 个文件的改动只是把 `ureq::` 换成 `crate::http::`。

| 项 | 变化 |
|---|---|
| ALPN | 协商 `h2` / `http/1.1`（`http2` feature 打开后 rustls 自动带上） |
| 协议 | 服务端支持时真正走 h2 帧 |
| 压缩 | `Accept-Encoding: gzip` 并自动解压（OkHttp 默认就发这个头） |
| 代理 | 保留 `socks5://` / `http://`（出口池依赖），非法代理串仍然报错而不是静默直连 |
| 响应体 | **保持惰性**。一开始写成一次读全，`update` 的「先看 Content-Length 再决定要不要下」就失效了（测试直接抓到）；改成 `Body::Live` 流式后才与 ureq 语义一致 |
| 桌面链接 | reqwest 的符号量让 MinGW 报 `export ordinal too large: 69390`。新增 `build.rs`，只在 `windows + gnu` 下加 `-Wl,--exclude-all-symbols`（只保留 .def 显式导出）；ELF/Android 不受影响 |

**仍然不同（无法靠换 crate 解决）**：密码套件顺序仍是 rustls 而非 Conscrypt 的 BoringSSL，
JA3 的 cipher 列表依旧对不上。要完全一致得换 BoringSSL 绑定，属另一量级工程。
→ **后半句已在 L.1 推翻**：cipher 集合/顺序、曲线顺序、ALPN 都能在不换 crate 的前提下对齐，
真正做不到的是**扩展集合**（那确实要 BoringSSL）。

**验证**：231 passed；clippy 退出码 0；`--all-targets` 与 `--target aarch64-linux-android` 均通过；
`cargo build`（桌面）通过；CLI 实跑 `update --check` 走通真实 TLS（GitHub API 支持 h2，
返回"没有可用的发布资产"属正常 —— 该仓库没有 Release）。

## K. 0.3.9-antiban：issue #1–#4（滑块与手机路网导入）

对应 GitHub 上的四个 issue：#1「滑块验证过不了」、#2「v0.3.8 滑块验证失败」、
#3「滑块」、#4「手机不能手动导入路网 建议改成文件形式」。前三个是同一件事。

### K.1 滑块：实测出的两个真问题

先做了一次不带账号的联网探针（`live_gt4_probe`，默认 `#[ignore]`，用 `--ignored` 手动跑），
直连极验 `/load` → 下载两图 → 识别 → `/verify`，连续多轮统计：

| 轮次 | 成功 | 说明 |
|---|---|---|
| 第 1 批 12 轮 | 11/12 | 唯一失败那次 `distance=75`，缺口实际在 ~200 —— 配到了背景里同纹理的别处 |
| 后续各批 | 2/16、4/16、0/8… | 一路下滑，换 socks5 出口也没恢复 |

结论两条：

1. **识别是概率性的，偶发错配确实存在**（约 8%），这就是用户撞到的失败；
2. **同一出口连续失败会把极验对该会话/IP 的容忍度打掉**：答错一次就消耗一次，
   越试越不通过。所以「失败就立刻重试」这个既有行为本身在加剧问题。

**试过又撤回的方案**：用 alpha 掩码裁掉切片透明边距 + 灰度 NCC、再换成梯度方向一致性
匹配（`orient_match`）。在抓下来的样本图上**人工核对**过几轮，新算法反而不如旧管线
（缺口左缘 x≈190 时它给 121，配到背景的蓝色装饰图案上），且验证码被降级后 `/verify`
恒 fail、无法再用返回结果评估，于是不再赌新算法，**回退到原 Canny 管线**。

**最终做法**（`src/api/gt4image.rs`、`src/api/gt4.rs`、`src/api/login.rs`、
`src/api/captcha_guard.rs`）：

- `/load` 的 `ypos` 被实测确认是「切片原图顶部」在背景中的 y（与整张切片模板的最佳 y
  逐轮吻合 ±2px），拿它做**一致性校验**：识别出的 y 偏离 `ypos` 超过 8px 就判定这一张
  不可靠，直接换图重试 —— **宁可不答，也不交错答案**；
- **按失败原因分域重试**（`Gt4Error::{Unreliable, Rejected, Fatal}`）：认不准/网络抖动
  才换图（最多 2 张，间隔 3–6s）；**极验判错立即停手**，不再重试 —— 原来 `login` 会在
  10003 上重破滑块 3 轮，最坏一次登录要打 6 次 `/load` + 6 次 `/verify`，而这类失败
  恰恰不该靠重试解决；
- **验证码冷却**（新模块 `captcha_guard`）：连续 2 次判错冷却 10 分钟、3 次 30 分钟、
  4 次以上 1 小时，状态落 `data_dir/captcha_state.json`，冷却期内 `login` 一个验证码
  请求都不发。用户看到「过不了」就会连点，而连点正好在加深降级；
- **`passtime` 改成真实耗时**（原来是常量 1887，所有账号所有次数完全一致）；
- **延迟挪到正确的顺序**：取图 → 看图 0.9–1.9s → 拖动 0.12–0.38s → 交卷，
  再在验证码通过后停 1.2–3.5s 才发登录（原来延迟加在取图之前，
  可观测形态是「图到手 200ms 就交卷」）；
- `checkGeeUse` 三态化：**问不出来（网络/解密失败）就不进 GT4**，否则会在坏出口上
  继续烧验证码，把「网络不通」变成「滑块答错」；`uuid` 三条请求复用同一个
  （原来一次登录三个 uuid）；`data` 兼容 `bool` / `"true"` / `1`；
- geevalidate 拿不到业务码时**重发一次**而不是当成通过（原来 `other => validated = true`
  会让「没验证成功」滑到带密码的登录请求，被服务端记成一次失败登录）；
- 探针**必须显式给 `GT4_PROBE_CAPTCHA_ID`** 才跑（原来默认用生产 captcha_id 连跑，
  本身就是一次密集打点，会污染之后的实测数据），轮次默认 3、间隔 5–10s；
- 顺手修：`get_w` 里对 `lot_number` 直接切片，长度不足会 panic，改为安全取值。

**没动**（没有证据、改了可能破坏通过路径）：`gee_guard.roe` 八键全 `"3"`（等于「全部
未检出异常」，随机化等于主动承认异常）、`device_id: ""`、`ep` / `biht` 等抓包字面量、
`em` 全 0。

### K.2 手机路网导入（issue #4）

原来「浏览…」按钮被 `#[cfg(not(target_os = "android"))]` 挡掉，Android 上只能手敲路径，
而手机上没有可敲的路径 —— 等于功能在手机上不存在。

改成走**系统文件选择器（SAF）**，不申请任何存储权限：

| 层 | 改动 |
|---|---|
| `MainActivity.java` | 新增 `pickOsmFile()`（`ACTION_OPEN_DOCUMENT`）与 `onActivityResult`；选中后把文件**复制进私有目录**再回传路径 —— `content://` URI 只在授权瞬间可读，交给后台解析会失效 |
| `src/android.rs` | JNI 调用 + `nativePickedFile` 回调 + `take_picked_file()` |
| `src/platform.rs` | `pick_file()` / `take_picked_file()` 平台抽象（桌面为 no-op） |
| `src/ui/osm.rs` | Android 上显示「选择文件…」，每帧轮询结果，选好即自动加载 |

桌面路径不变（仍用 rfd）。

### K.3 顺带修掉的偶发失败（不是本轮引入的）

`cargo test --lib` 曾经偶发失败一次（`assertion left == right`，随后连跑多次全绿）。
交给子代理专门去复现，定位到：

- 用例 `ui::run::tests::future_specified_time_is_clamped_and_detectable`（`ui/run.rs:1347`）；
- 原因：`ensure_plan()` 内部读一次时钟写 `start_ms`，测试随后又读一次 `now_ms()`，
  两者只在同一毫秒内才相等 → 实测 500 次跑 5 次失败（约 1%）；
- 修法：改成与相邻 `specified_time_is_clamped_to_now` 一致的**区间断言**（容差 2ms）；
- 另一处同型隐患：`ui/violation_tests.rs` 两处把 `route_fixture` 指向共享的
  `std::env::temp_dir()`，并行测试会互相看到对方落盘的文件 → 改成每个测试一份
  uuid 子目录（`FixtureDir`，Drop 时清理）。

### K.4 构建脚本在 Windows PowerShell 5.1 下的三个断点

本机就是 5.1（`$PSVersionTable.PSVersion` = 5.1.26100）。`android/build.ps1` 原来有三处
只在 PowerShell 7 / .NET 6+ 成立，导致 `build.ps1` 直接起不来：

| 断点 | 原因 | 改动 |
|---|---|---|
| `[Security.Cryptography.SHA256]::HashData` | .NET Framework 没有该方法 | 改用 `SHA256.Create()` + `ComputeHash` |
| `[Convert]::ToHexString` | 同上，.NET Framework 没有 | 改用 `-join ($bytes \| % { $_.ToString('X2') })` |
| `System.IO.Compression.ZipArchive` | 本机 `Add-Type` 报成功但类型解析不到 | 改用 JDK 自带的 `jar --update` 更新 APK |
| `Invoke-Checked` | 显式 `2>&1` 收进来的 stderr 记录，在 `$ErrorActionPreference='Stop'` 下会被包成 `RemoteException` 直接中止（裸调用不触发，只有 `2>&1` 才触发） | 这一段临时降级 `$ErrorActionPreference`，只按 `$LASTEXITCODE` 判定 |

### K.5 验证

```
cargo test --lib                                234 passed / 0 failed（新增 3 个用例）
cargo clippy --lib --all-features               exit 0（改动文件零新增告警）
cargo check --all-targets --all-features        exit 0
cargo check --target aarch64-linux-android --features android   Finished
android/build.ps1  release arm64-v8a            APK 出包通过，v2+v3 签名验证通过
                                                versionCode 30011 / 0.3.9-antiban
                                                SHA-256 EF9D3A34…E6810
```

新增用例：
- `api::gt4image::tests::ypos_gate_follows_the_measured_offset`：先用 `match_at` 求合成图上的
  真实定位（x≈135 / y≈45）做基准，再断言「一致放行 / 8px 内放行 / 9px 拒绝」。
  **不用写死像素的恒真式断言**（第一版 `assert!(x <= 300)` 就是恒真的，被审查抓出来）。
- `api::captcha_guard::tests::second_rejection_starts_a_cooldown_and_success_clears_it`
- `api::captcha_guard::tests::repeated_rejections_escalate_the_cooldown`

**没有验证到的**（本机做不到，不冒充）：真机上「选择文件…」的实际交互、极验在干净出口
上的真实通过率（本机出口已被降级，且不应用生产 captcha_id 去刷）。

---

## L. 0.3.9-antiban 续：密码套件 / 窗口 id 口径 / 机型库长尾（**未发版**）

这一轮只动三处「不需要服务端行为证据也能判断对错」的地方：握手参数、两份报文
自相矛盾的编号、机型与 androidId 的生成方式。J.2 里那三个「自报结论」
（`complete`、`validDistance`、`faceCheck`）**一个都没动** —— 理由和风险在 L.4 单独列清。

### L.1 密码套件：把 ClientHello 的 cipher / 曲线顺序压到 Conscrypt 的形状

J.3 换到 reqwest 只解决了 ALPN 与 h2，**密码套件顺序还是 rustls 自己的**。
网关取一次 JA3 的 cipher 段（第 2 段）就能把官方客户端和本工具分开，成本近乎为零。
新增 `src/tls.rs`，在 `Agent::new` 里用 `use_preconfigured_tls` 接上：

| 项 | 做法 |
|---|---|
| 密码套件 | `rustls::crypto::ring` 的 provider 按 Conscrypt 的 9 套**重排**：TLS1.3 三套在前，之后 ECDHE_ECDSA / ECDHE_RSA 交替，每种密钥交换按 GCM-128 → GCM-256 → CHACHA20。rustls 恰好只支持这 9 套，所以是**逐项相等**，不是「Conscrypt 列表 + rustls 尾巴」 |
| 曲线组 | `X25519` → `secp256r1` → `secp384r1`（BoringSSL 默认顺序），决定 JA3 的 EC 段，也决定 TLS1.3 `key_share` 首选 |
| ALPN | `h2` / `http/1.1`，h2 在前 |
| 版本 | TLS 1.3 + 1.2（Android 10+ 关掉 1.0/1.1） |
| 失败处理 | 配置构造失败**必须打日志再退回默认 rustls**，不允许静默 —— 静默退回就是「以为对齐了、其实没对齐」 |

对齐是**可断言的**：
- `tls::tests::cipher_suites_match_conscrypt_order` 逐项比对 9 个套件名；
- `tls::tests::kx_groups_match_conscrypt_order` 比对三条曲线；
- `tls::tests::client_config_sets_h2_first_and_both_versions` 比对 ALPN 与 0-RTT 关闭；
- `http::tests::reqwest_accepts_our_preconfigured_rustls_config` 卡住「rustls 版本漂移
  导致 reqwest 的 `Any` downcast 失败、静默回到默认顺序」这个最难查的坑。

**怎么确认配置真的被用了（而不是写了一个没人读的常量）**：读了 reqwest 0.12.28 的源码链——
`ClientBuilder::use_preconfigured_tls` → `TlsBackend::BuiltRustls` →
`connect::new_rustls_tls(…, tls, …)` 里 `Arc::new(tls)` 原样使用，**不覆盖 `alpn_protocols`**
（覆盖只发生在内置 `TlsBackend::Rustls` 分支）。再实跑 `update --check` 走通真实 TLS：
GitHub API 返回「没有可用的发布资产」（与仓库无 Release 一致），说明握手、证书校验、
JSON 解析全通。这条是代码级 + 实跑级证据，不是推断。

**仍然不同的（本轮不做，也不宣称做到）**：
- **扩展集合与顺序**：BoringSSL 会发 `ec_point_formats`、`session_ticket` 等，rustls 的扩展表不同
  → **完整 JA3 字符串仍然不等**。等的是 cipher 段与 EC 段，不是整串。
- `signature_algorithms` 的列表/顺序由 rustls 的 `WebPkiSupportedAlgorithms` 决定，未改写。
- 要逐字节一致只能换 BoringSSL 绑定（`boring` / `tokio-boring`）：cmake 构建 + Android
  交叉编译 + 与 reqwest 连接器对接，是独立一轮的工程，不是改常量能收口的。
- **没有抓包证明服务端看到的 JA3**。本机出口不适合拿生产端点做对比实验；
  结论止于「客户端侧的握手参数按 Conscrypt 配好了，且有单测锁住」。

### L.2 提交与 OBS 的 10s 窗口 id 统一

同一次跑步里，提交体 `speedPerTenSec[i].id` 是 `60000 + i`，OBS `speed_json[i].id` 是
`起点秒 % 100000 + i` —— 两份报文对同一个窗口报了不同编号。服务端把
`speedPerTenSec` 与 OBS 的 `speed_json` 对一遍就是硬矛盾，而且是自己造成的。

| 改法 | 细节 |
|---|---|
| 唯一出口 | `track::wire::build_windows` 成为全链唯一窗口构造函数 |
| 删除二次改写 | `submit.rs` 的 `submit_windows` 不再覆盖 `id`/`queueNum`，直接转发 |
| 去掉死参数 | `build_windows` 删了没人用的 `rrid`，并修掉一条与代码不符的注释（曾写 `id=(rrid%100000)*1000+…`，实际用的是 `startTime`） |
| 回归锁 | `wire::window_and_position_tests::submit_and_obs_share_window_ids` 把 OBS 对象 gzip+base64 解回来，逐项比对两侧 `id`/`queueNum`，并断言 id 基数 = 起点秒 % 100000 |

**风险（要明说）**：没有抓包证据证明服务端要哪一种。这一改动的取舍是
「**内部一致**」优先于「**沿用某个会话内编号**」：两份报文自相矛盾是确定会被交叉核对
抓到的；而「提交必须从 60000 起」只是从旧代码继承的猜测，没有任何样本支撑。
如果将来抓到真机包证明提交侧另有基数，只需改 `build_windows` 一处（它仍是唯一出口）。

### L.3 机型库长尾 + `androidId` 与 `device_id` 解耦

| 项 | 变化 |
|---|---|
| 机型数 | 13 → **24 款**（新增 11）：Redmi K60 至尊版 / K70 / K40 / Note 12 Turbo、realme GT Neo5、vivo X100 Pro、Mate 40 Pro、P30、Galaxy S24 / S21 / A54 |
| 分布 | 新增 `ModelProfile.weight`，按**走量档 > 旗舰档**近似真实人群（走量权重合计必须大于旗舰/长尾合计，有断言）。均匀抽样本身就是个统计特征：一批账号挤在同样几台 Ultra/Pro 上不像真人 |
| androidId | 不再从 `MD5(device_id)` 派生，改为 `random_android_id()`：16 位小写 hex、独立随机、首次生成后落盘。真机上 ANDROID_ID 与 App 自定义 DeviceId 没有任何派生关系 |
| 黑名单 | `BAD_ANDROID_ID`：全零 / 全 f / `9774d56d682e549c`（Android 2.3 时代大量设备共用的著名值）。生成时排除，自检时拦下 |

新增/加固的测试：机型代号唯一（重复项会挤占别人的抽样概率）、`weight > 0`、长尾 ≥ 20 款、
品牌 ≥ 5 个、权重抽样真的偏（4000 次抽样比较最重/最轻机型的命中数）、`random_android_id`
的格式/唯一性/黑名单、坏 androidId 被 `audit` 拦下。

**兼容性**：老 `identity.json` 里已有 `android_id` 的**完全不受影响**（`ensure_fingerprint`
只补空值）；只有新建身份或点「随机」时才走新逻辑。

**风险**：机型热度是按公开销量近似，不是本产品的真实用户分布；若服务端有机型白名单/黑名单，
新补的机型可能落在名单外。比「只有 13 款、一批账号挤在同样几台机」要好，但这是估计，不是证据。

### L.4 明确没改的：人脸与三个「自报结论」（附风险）

按要求保留原样，**这些字段的取值一行代码都没动**。风险逐条写清，供决定要不要动：

| 项 | 现状 | 风险 |
|---|---|---|
| `complete: true` / `unCompleteReason: 0` | 提交体恒报「本次记录完整」 | 服务端若把 `complete` 与 `faceCheck` / 点位 / 有效里程交叉核对，会判自相矛盾；但改成 `false` 等于主动让记录不算数 —— 直接损害代跑目的，所以不动 |
| `validDistance == totalDis`（100% 有效里程） | 有效里程恒等于总里程 | 真机通常有 5–15% 无效里程（漂移、暂停、信号差），100% 是稳定特征；但扣减一旦跌破 `minDistance`，记录直接不算数，且没有服务端阈值证据 |
| `faceCheck = 1` + `runFaceCheck = ""`（OBS 里是空串再 gzip） | 声明「本次需要人脸核验」，却不带任何核验数据 | **这条矛盾最硬**：真机 `faceCheck=1` 通常伴随核验结果。置 0 可能让需要核验的学校判无效；保持 1 又没有数据。两条路都缺服务端行为证据，当前选择「保持能算数的一侧」 |
| `useMobilityTools=0` / `isUpload=false` / `more=false` | 维持真机默认值 | 无明显问题 |
| 指纹自洽（机型↔分辨率↔os↔androidId） | 已收紧（见 L.3） | 本轮唯一「改了反而更安全」的一项 |

> 一句话：**人脸这条不动，是因为动了会直接损害「记录是否算数」，不是因为没有改法。**

### L.5 验证

```
cargo test --lib --offline                                    244 passed / 0 failed / 1 ignored（较 HEAD 的 234 新增 10 个用例）
cargo clippy --lib --all-features --offline                    exit 0（14 条告警全部是既有的；新增文件零告警）
cargo check --all-targets --all-features --offline             exit 0
cargo check --target aarch64-linux-android --features android  Finished（需按 android/build.ps1 设 CC_*/AR_*/LINKER，缺 NDK 时报 ring 编译失败）
cargo run -- update --check                                    走通真实 TLS（返回「没有可用的发布资产」，与仓库无 Release 一致）
```

新增用例（10）：
- `tls::tests` 四条：cipher 顺序 / 曲线顺序 / ALPN 与 TLS 版本 / `describe()` 文案；
- `http::tests::reqwest_accepts_our_preconfigured_rustls_config`：类型契约；
- `track::wire::window_and_position_tests::submit_and_obs_share_window_ids`：两侧 id 同源；
- `api::fingerprint::tests` 四条：长尾 ≥20 款、加权分布真的偏、`random_android_id`
  格式与独立性、黑名单值被 `audit` 拦下。

既有用例的加固（不算新增）：`model_table_is_well_formed` 增加「机型代号唯一」与
`weight > 0`；`mint_spreads_across_models` 抽样次数 200 → 2000，门槛 10 → 20 款。

**没有验证到的**（本机做不到，不冒充）：
- 服务端实际收到的 JA3/JA4 —— 无抓包；
- 提交侧真机的窗口 id 基数 —— 无新抓包样本，只有旧代码里的猜测；
- 补进来的机型与真实用户分布的吻合度 —— 权重是公开销量的近似。

---

## M. 0.3.9-antiban 续：把「能伪装的」收口（未发版）

依据是本地官方包 `msw/official.apk`（`com.zjwh.android_wh_physicalfitness`，
**versionName 7.3.70 / versionCode 401**，Flutter + 133 个 so）的 `classes.dex`
字符串核查（77 MB，ASCII 直读）。只动**有官方证据、且不影响记录是否算数**的项。

### M.1 磁盘上的随机 MAC 归一为 Android 占位值

`identity.json` 里一直存着一个随机 MAC（`random_mac()`），但**请求从不读它**——
登录体的 `mac_address`、信封的 `blMac`/`wifiMac` 都直接用
`ANDROID_PLACEHOLDER_MAC`。这个字段是"落盘但不使用"的残留，两个坏处：

1. 导出/复制 `identity.json` 时它看上去像一个「设备指纹」，但它不是真机值，
   也不是任何请求里出现过的值；
2. 一旦将来有人把它接进某条链路，就是凭空造出一个 Android 6+ 根本读不到的值
   （官方 `getMacAddress()` 固定返回 `02:00:00:00:00:00`）。

改法：`load_identity_for_platform` 把任意非占位值**迁移**为占位值（`dirty` 落盘），
删掉 `random_mac()`，`HeaderIdentity::default()` 直接给占位值。
回归测试 `legacy_random_mac_is_normalized_to_the_android_placeholder`：
写入随机 MAC → 加载后为占位值 → 再加载仍是同值（幂等）。

### M.2 服务端的人脸核验要求：读出来、说清楚，但不伪造

官方 dex 里有 `faceEnable` / `faceVerifyType` / `faceCheck` / `serverCheckStatus` 与
DTO `entity.RunFaceCheckConfigDTO`。我们不知道它放在 policy 响应的哪一层，
也不知道 `faceCheckData` 的格式，所以**不做任何伪造**，只做可见化：

| 改动 | 行为 |
|---|---|
| `PolicyInfo.face_required: Option<bool>` | 在 `runRuleModel` / 顶层 / `runFaceCheck` 对象三处找 `faceEnable`→`faceCheck`→`serverCheckStatus`；认不出是 `None`（= 不知道），**不是** `Some(false)` |
| `PolicyInfo.face_verify_type` | 独立取 `faceVerifyType`（可能只下发类型、不下发开关） |
| `flow.rs` 日志 | `Some(true)` → 「服务端要求本次人脸核验，本工具不执行，记录可能被判无效」；`Some(false)` → 未要求；`None` → 未下发（不代表不要求） |

约束写死在注释里：**这两字段不参与提交体构造**，`faceCheck` 仍取用户配置。
测试 `face_rule_is_read_only_and_lenient` 覆盖五种落点 + 缺失字段 + 只下发类型。

> 这条不解决人脸缺口，只是把「这次可能白跑」提前到跑步之前说出来。

### M.3 核对结论：这些字段已经与官方同构，无需再改

`classes.dex` 命中，与我们 Android 信封一一对应：
`androidId`、`logicPixel`、`physicPixel`、`blMac`、`wifiMac`、`cpuModel`、
`CustomDeviceId`、`tokenSign`、`headerSign`。

UA 的归属也核了一遍：
- `libapp.so`（Dart 层）里 **`okhttp` 0 命中**，只有 `dart:io` 系列——Dart 若自己发请求，
  走的是 Dart HttpClient；
- `classes.dex`（原生层）里 `okhttp/4.12.0` 与 `okhttp/3.14.9` 都在。

所以「跑步提交由原生 OkHttp 发出、UA 取 OkHttp 默认值」这个判断有据，`okhttp/4.12.0`
的方向正确。**仍未确定**的是提交具体走 4.12.0 还是 3.14.9（3.14.9 可能是某个第三方
SDK 携带的），没有抓包不做二选一。

### M.4 明确不做的（附理由，别再重复问）

| 项 | 为什么不做 |
|---|---|
| 百度人脸 detect 的调用链（`aip.baidubce.com/oauth/2.0/token` + `face/v3/detect`） | 缺三样：`access_token` 的来源（客户端内置 AK/SK 换？服务端下发？）、`faceCheckData` 的编码、`RunFaceCheckConfigDTO` 的下发时机。任何一项猜错都是**新增**异常，比现状更差 |
| `datapoint.gxapp.iydsj.com` 埋点（`/api/v76/startup/pv`、`/api/v70240/run/pv` 等） | 端点已确认存在，但请求体字段一个都没抓过。凭空发空 body 会在服务端多出一条"异常埋点"，属于自伤 |
| HTTP/2 SETTINGS / 帧参数 | OkHttp 与 hyper 的 `INITIAL_WINDOW_SIZE`、`MAX_FRAME_SIZE` 默认值不同，但没有抓包就改调优参数，只能靠猜 |
| `appVersion` 回退到 7.3.70 | **证据冲突**：手上的官方包是 7.3.70，而 J.2 记录"现网 7.4.0"。改回旧版本可能触发强更，保持 7.4.0。这一条列入待核实 |
| `complete` / `validDistance` / `faceCheck` 取值 | 见 L.4：改了直接损害记录是否算数 |

### M.5 验证

```
cargo test --lib --offline        246 passed / 0 failed / 1 ignored（较 L 节净增 2）
```
新增：`model::points_cache_tests::legacy_random_mac_is_normalized_to_the_android_placeholder`、
`policy::tests::face_rule_is_read_only_and_lenient`。

**没有验证到的**：官方提交到底走 OkHttp 4.12.0 还是 3.14.9；policy 是否真的下发
`faceEnable`（三处落点都只是"能认"，不是"见过"）；`faceVerifyType` 的取值含义。

---

## N. 0.4.0-antiban：禁跑诊断 / 路网补全 / 自定义路由

本轮三件事：把「被禁跑还剩多久」变成可读数字、补掉路网与轨迹真实性的四处缺口、加第三种路线模式。

### N.1 禁跑倒计时 + `recover` 命令

**发现**：`POST /api/v70103/runModePolicy` 的响应里 `data.freezeRunTime` 是**剩余冻结秒数**。
这个接口我们一直在调，但只取了 `policy/minDistance/validTime/mustPoints`，把该字段丢了。
官方文案「您已被查出跑步违规.无法进行跑步.将在…」的「将在」就是它换算出来的解禁时刻。

- `src/api/policy.rs`：`PolicyInfo` 增 `freeze_run_time`（负值按 0）与 `freeze_message`，
  加 `frozen()` / `freeze_text()`（`还剩 2 天 00:00:00`）。
- `src/api/recover.rs`（新增）：`diagnose` = 冻结倒计时 + 违规榜 + 记录列表 + 逐条 `reasonList`；
  `fix_one` 用**原记录 `startTime`** 覆盖提交（`save/record` 对同日同 `sportType` 无去重锁，
  商品截图里 `1.72km→1.68km` 证明是重算覆盖而非改状态位）。
- `src/cli/cmds.rs`：`recover [--yes] [--verify] [--only rrid] [--limit n]`。默认 dry-run；
  **冻结期直接拒绝提交**（服务端独立校验资格，强提交只是白挨一次风控计数）；修完 `--verify` 复查。

**边界（写进代码注释，不吹）**：修复动的是**记录**，不是台账。`freezeRunTime` 归零是唯一能区分
「派生冻结 vs 独立台账」的证据，`--verify` 就是为这个实验留的。

### N.2 路网与轨迹真实性的四处缺口

| 缺口 | 改动 | 影响 |
|---|---|---|
| OSM 只按 node id 建图，**同坐标不同 id 的路口不连通** | `load.rs` 改按量化坐标焊接（1e-7°≈1.1 cm），3 个回归测试 | 「看着连、图上断」的假断点；参考实现同样没有 |
| GPS 漂移硬编码 `(0.72,0.75,0.3)`，稳态 std 仅 1.08 m 且每 seed 相同 | `generate_road.rs` 每 seed 随机 α∈[0.86,0.95]、稳态 std∈[2.8,4.2] m、测量噪声∈[0.25,0.65] | 轨迹不再贴线；固定参数＝所有人共用一枚指纹 |
| 手动导入上限 64 MiB、自动下载 24 MiB，自相矛盾 | 统一 `24 MiB`（`generate_road.rs` 与 `school_roads::MAX_BYTES` 同额） | 消除「自动下载能跑、手动导入闪退」 |
| `align_network` 无幂等门，重复调用二次偏移数百米且不报错 | `RoadGraph.align_applied` + 门 + 2 个测试 | 将来新增调用方不会静默毁掉整个路网 |

顺带：`accuracy` 从均匀 1.4–2.4 m 改成对数正态（中位 7 m；异常点 6 m，保留 `≤10 m` 原契约）。

### N.3 GPX 补全（`crates/route-planner/src/gpx.rs`）

原来 89 行只取经纬度。现在：

- **海拔** `<ele>`（非法值按缺失，不让整条轨迹失败）+ `ascent_descent()`（3 m 阈值滤 GPS 抖动）
- **RFC3339 → 毫秒**（`parse_time_ms`），`analysis.rs` 不再逐点重复解析
- **`parse_point_text`**：每行 `lat,lon[,ele]`，跳表头/注释，分隔符兼容逗号/空白/分号
- **`parse_point_json`**：`[{lat,lon}]` / `[[lat,lon]]` / `{points:…}`，键名兼容 `latitude/lng/ele/alt`
- `analysis::parse_any` 按**内容**识别格式（`<`→GPX、`[`/`{`→JSON、其余→文本），扩展名不可靠
- 分析结果输出真实累计爬升/下降；没有 `<ele>` 时显示「未提供」而不是 0

### N.4 第三种路线模式 `Manual`

`RouteMode` 增 `Manual`（`from_str` 兼容 `manual`/`custom`）：

| | 强制集合 | 无路网时 |
|---|---|---|
| `Road` | 实时点位 **+ 策略必经点** | **失败**（这是设计） |
| `Manual` | **只有实时点位**，`must_bd` 传空 | **退回按点位连线**（经典 `gen_track`） |

配套：`as_str()` 做配置持久化（`run.rs` 的三元改成它）、`requires_network()`（只有 Road 必须路网）、
`needs_school_route()`（Road/Manual 都要拉学校数据，`jobs.rs` 原来 `!= Road` 会漏掉 Manual）。
UI 加第三项并带 hover 说明；CLI `--route manual`。

### N.5 验证

```
cargo test --release --lib --offline    257 passed / 0 failed / 1 ignored（较 M 节净增 11）
cargo test --release -p route-planner     16 passed（lib）+ 7 passed（tests/basic.rs）
cargo clippy --release --lib --all-features   exit 0
cargo check --all-targets --all-features      exit 0
cargo check -p route-planner --target aarch64-linux-android   Finished
```

本轮新增用例：`recover` 5 个（含冻结优先于修复建议）、`policy` 2 个（冻结解析/格式化）、
`load` 3 个（焊接/不误并/单连通分量）、`align` 2 个（幂等/空图不标记）、
`RouteMode` 1 个（字符串往返 + 能力位）、`build_road` Manual 1 个、
`gpx` 5 个（海拔时间/爬升阈值/文本/JSON/DTD）。

**Android 交叉编译的真实边界**：`route-planner` 全 crate 在 `aarch64-linux-android` 上通过
（纯 Rust 依赖）。**主 crate 未通过**——本机 Android SDK/NDK 不存在
（`C:\Users\lenovo\AppData\Local\Android\Sdk` 无此目录），`ring` 找不到
`aarch64-linux-android-clang`。这是**环境缺失，不是本轮代码问题**；要真验证需装 NDK 后按
`android/build.ps1` 设 `CC_*`/`AR_*`/`LINKER` 再跑。本轮新增代码无平台相关 API：
`road_tools.rs` 的 `rfd::FileDialog` 仍在 `#[cfg(not(target_os = "android"))]` 内，
其余为纯逻辑或 `std`。

**没有验证到的**：`freezeRunTime` 归零是否真等于解冻（需真实被冻结账号跑一次 `recover --verify`）；
`save/record` 的覆盖键到底是日期还是 `startTime`（同理，需实测一条）；焊接容差 1 cm 在真实
杂乱 OSM 上的收益幅度（只有构造用例，没有真实脏数据样本）。








---

## O. 0.4.1-antiban：第三方审计（v0.4.0 全仓）的逐条核实与修复

依据：外部审计报告 `NekoSportsWorldTool-v0.4.0-audit`（基线提交 `ab10001`）。审计列了 3 高 / 8 中 / 6 低，
本轮逐条核实，**全部按报告建议处理**；另外用 WSL2 里的 Ubuntu 22.04 独立构建/测试了一遍 Linux 侧，
又发现两个审计没覆盖的 Linux 专属缺陷（O.10）。审计中"未执行"的项（Rust 测试、Android 构建、脚本运行）
本轮给出了可复核的结果或明确的"仍未验证"。

### O.1 H1 网络与 TLS 依赖的已知公告（`chore(deps)`）

| 包 | 旧 | 新 | 对应公告 |
|---|---|---|---|
| `rustls` | 0.23.44 | **0.23.46** | RUSTSEC-2026-0285/0348/0349/0346 |
| `rustls-webpki` | 0.103.15 | **0.103.16** | RUSTSEC-2026-0382 / CVE-2026-111943 |
| `webpki-roots` | 1.0.9 | **1.0.10** | RUSTSEC-2026-0381（eM Sign 中间证书） |

只改了 `Cargo.lock`（`Cargo.toml` 用的是 `0.23` / `1` 宽范围），`cargo tree` 里
`reqwest → hyper-rustls → rustls` 与 `src/tls.rs` 的 preconfigured TLS 仍是同一份 0.23.x，
`use_preconfigured_tls` 的类型契约没变（有既有测试 `http::tests::reqwest_accepts_our_preconfigured_rustls_config` 守着）。

### O.2 H2 `login --pass -` 根本不可达（`fix(cli)`）

`parse_flags` 原来对所有 `-` 开头的 token 一律当开关，于是 `--pass -` 被解析成
`("pass","1")` + 一个空名开关，`read_password` 里 `value == "-"` 的分支永远进不去 ——
"避免密码进 shell 历史"的安全设计等于没有。

- `src/cli/mod.rs`：单独一个 `-` 现在按**值**取走；空名开关不再产出。
- 回归测试 4 条：`dash_is_consumed_as_a_value`、`ordinary_flags_and_values_still_parse`、
  `get_returns_first_match`、`short_stamp_survives_invalid_timestamp`。

### O.3 H3 本地 Android 发布脚本（`fix(android)`）

- `$version -split '\.'` 直接 `[int]` 在 `0.4.0-antiban` 上会抛异常，且**抛在盖章之前**，整条本地发布链不可用。
  现在先切掉 `-suffix`，逐段取前导数字，并给 `versionName` 用去掉尾巴的基线（与 CI 的
  `0.4.0-android.N` 一致，不再出现 `0.4.0-antiban-android.4` 这种写法）。
- 默认 `$Repo` 从 `hb-degithub/NekoSportsWorldTool` 改为 **`czcdr/NekoSportsWorldTool-antiban`**
  （`src/update.rs::REPO_OWN`）。原来即使修好解析，产物也会发到客户端不看的地方。

### O.4 M1/M8 自更新：fail-closed + 本机 pin + 安装前校验

| 项 | 旧行为 | 现在 |
|---|---|---|
| 资产无 `digest` | 桌面包拒绝、**APK 放行**（"系统安装器会验签名"） | **两条路都拒绝**：安装器只保证"签名是本 App 的"，不保证"内容是本 App 该有的" |
| APK 资产匹配 | 精确 → 任意含 arm64 → **任意 .apk** | 精确 → `*-arm64.apk` 且含 `android`；匹配不到就当"本平台无产物"（不更新、不报错） |
| 同一 tag 的资产被替换 | 无感（摘要与资产来自同一个 API） | `update_pins.json` 记录首次摘要，**同 tag/资产名摘要变化即拒绝**并提示核对 Release |
| Release 不可变 | 未识别 | 读取 `immutable`；置位时摘要由服务端保证，跳过 pin |
| Android 安装前 | 直接交系统安装器 | `MainActivity.verifyUpdatePackage`：**包名 + 签名指纹(SHA-256) + versionCode 不得低于已装**，不符即删包提示 |

残余风险照旧写清：pin 是 TOFU（首次仍信任首次），发布账号被盗且**首次**就换包这条路只有发布侧签名能挡；
本轮没有引入签名密钥流程（那需要把私钥放进 CI Secret 并改发布脚本，属于独立工程）。

### O.5 M2 凭据不再明文落盘（`feat(crypto)`）

新增 `src/crypto/local_seal.rs`：`AES-256-CBC + PKCS7`，密钥 = `SHA-256(用户 + 主机标识 + 数据目录 + 每安装随机 salt)`，
salt 首次生成即 0600。`save_config`/`save_session` 落盘前把 `password`/`token` 换成
`password_sealed`/`token_sealed` 并清空明文；读取时还原进明文字段（**结构体不动，迁移面最小**）。

- 旧明文文件原样可读（迁移发生在"下次保存"），测试 `legacy_plaintext_credentials_still_load` 守着；
- 换机器/改用户名/删 salt 解不开时，走"提示重新登录"而不是把配置当损坏清空；
- 边界写进模块注释：salt 与密文同机同存储，**同账户进程仍可解**。这挡的是"文件被拷走/同步/截图"，不是本机被入侵。

### O.6 M3/M4 日志脱敏（`fix(textlog)`）

- `redact_text` 的 scheme 回溯用 `rfind` 拿**字节**下标再 `+1`，URL 紧跟中文时切在多字节字符内部 →
  `byte index 4 is not a char boundary` panic。改用 `char_indices().rev()` + `len_utf8()`，
  回归测试 `test_redact_text_multibyte_prefix_does_not_panic`（5 种前缀 + 纯多字节文本）。
- 解密失败的 `raw_head` 原来直接 `truncate(&raw,160)` 拼进错误串（含手机号/令牌/网关页）。
  新增 `redact_snippet`：按字符截断 → 能解析 JSON 就 `redact_json` → 统一遮 URL 凭据、
  11 位手机号、≥40 位令牌串。`client.rs` 的 `RequestOutcome.raw_head` 与 `call_host` 的错误串都走它。

### O.7 M5 路网上限统一到 24 MiB

`generate_road::load_network_path` 的常量改为引用 `school_roads::MAX_BYTES`（唯一真值），
错误文案同步；`MainActivity.MAX_OSM_BYTES` 64 MiB → **24 MiB**，注释也改成"与 Rust 侧一致"。
现在不会出现"先复制 24–64 MiB 再被 Rust 拒绝"。

### O.8 M6 `recover` 在冻结状态未知时停手（`fix(recover)`）

- `Diagnosis::freeze_unknown()` 新增；`verdict()` 在 `freeze_seconds == None` 时**先说"未知"**，
  不再落到"最可修"分支。
- `cmd_recover`：`--yes` 时若策略接口没取到，**默认拒绝提交**并给出下一步；`--force` 才跳过（并打印警告）。
  usage 同步。

### O.9 M7/L2 原子写与冷却状态（`fix(captcha)`）

- `captcha_guard` 原来"读-改-写"裸奔、写失败被 `let _ =` 吞掉。现在：进程内 `Mutex` + 跨进程
  `create_new` 锁文件（最多等 500 ms 后放行，不阻塞登录）+ 原子写；写失败打 stderr。
- `write_secret_file` 的临时文件名从 `名字.PID.tmp` 改成 `名字.PID.线程.计数器.tmp`：
  同进程两线程保存同一目标时不会再互相截断（L2）。

### O.10 Linux 专属缺陷（WSL2 实跑发现，审计未覆盖）

| 缺陷 | 证据 | 修复 |
|---|---|---|
| `data_dir()` = **exe 所在目录** | `src/platform.rs` 原实现；WSL 里跑一次 GUI 就在 `target/release/` 生成 `identity.json`；装到 `/usr/local/bin`、`/opt`、AppImage 只读位置时登录态/身份/防重放全部静默失效，且违反 FHS | Windows 保持便携（exe 同目录）；Linux/macOS 走 `$XDG_DATA_HOME`/`~/Library/Application Support` 下的 `nekosportsworldtool/`；`NSWT_DATA_DIR` 可覆盖；切换时**一次性搬运**旧文件（尤其 `identity.json`，丢了 `device_id` 会触发 10121） |
| `GT4_PROBE_DUMP` 用 `format!("{dir}\\…")` | `src/api/gt4.rs` | 改 `Path::join`，Linux 下不再生成"名字里带反斜杠"的文件 |
| GUI 运行期依赖未声明（缺 `libxkbcommon-x11.so` 直接 panic） | `WINIT_UNIX_BACKEND=x11` → `Library libxkbcommon-x11.so could not be loaded` | 不改代码：官方 Linux 产物是 `mode: cli`（`--no-default-features`），GUI 不在 Linux 发布面；在此记录，避免以后再当"编译通过就等于能跑" |

### O.11 L1/L4/L5/L6

- **L1**：GUI 里 9 处 `let _ = save_config(...)` 统一换成 `ui::save_config(&config) -> Result`，
  失败原因写进各自状态栏（`jobs`/`run`/`osm`/`road_tools`/`about`）。
- **L4**：`print_rows` 的 `fmt_hms(..)[5..]` 在时间戳非法（`fmt_hms` 返回空串）时会 panic，
  改用 `short_stamp`（`get(5..)` + 兜底），测试覆盖 `i64::MAX`。
- **L5**：`android/test.ps1` 的 `SHA256.HashData`/`Convert.ToHexString`（PowerShell 7 only）
  改为 `SHA256::Create()` + `ComputeHash`，5.1 与 7 都能跑。
- **L6**：`cargo fmt`（54 文件），单独一个 `style:` 提交，与功能改动分开审。

### O.12 验证（实测）

```
cargo test --release --lib                                  269 passed / 0 failed / 1 ignored（0.4.0 为 257，净增 12）
cargo test -p route-planner                                  16 passed（lib）+ 7 passed（tests/basic.rs）
cargo fmt --check                                            无差异
（clippy / check --all-targets 结果见提交信息）
```

**Linux 独立验证**（WSL2 Ubuntu 22.04，非本机 Windows 侧）：

```
cargo build --release --no-default-features         Finished（纯 CLI）
cargo build --release（默认 features = GUI）        Finished，21.7 MB ELF（GUI 在 Linux 也能编）
cargo test --release --lib --no-default-features    202 passed / 0 failed
cargo test --release --lib（含 GUI 测试）           263 passed / 0 failed
cargo test -p route-planner --release                16 + 7 passed
cargo clippy --release --lib --no-default-features    0 error
./nekosportsworldtool --help / records               帮助正常；records 报"未登录"（预期）
```

**仍未验证的**（不冒充）：
- Android 真机构建/覆盖安装/签名连续性 —— 本机无 SDK/NDK/设备；由 tag 触发的 `apk.yml` 在 CI 上编译 Java 并出包；
- `MainActivity` 新增的签名/versionCode 校验只做了 javac 语法检查（缺 `android.jar`，符号级靠 CI）；
- 服务端行为（禁跑、覆盖提交、`freezeRunTime` 归零是否等于解冻）—— 未登录、未发请求；
- GUI 在 Linux 桌面的实际启动 —— 只有编译与"缺库会 panic"的证据。

### O.13 0.4.0 遗留的旧 API 风险（未动，仅记录）

`Config.password` 与 `Session.token` 在**内存里**仍是 `String`（登录/请求必须用它），
进程内存转储不在本轮威胁模型内；如需覆盖，应改用 `secrecy`/`zeroize` 一类包装，属于独立改动。
