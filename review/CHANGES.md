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
| HTTP/TLS 指纹（ureq+rustls、HTTP/1.1、无 ALPN h2、无 `Accept-Encoding`/`Connection`） | 要对齐 OkHttp+Conscrypt 得换 BoringSSL 栈并启用 h2，属独立一轮的工程；半对齐（只加 `Accept-Encoding: gzip` 而不解压）反而会把响应解坏。**这是当前最大的单一特征，优先级最高。** |
| `complete: true` / `unCompleteReason: 0` | 服务端若做交叉核对会露馅，但改成 false 等于主动让记录不算数 —— 直接损害代跑目的 |
| `validDistance == totalDis` 恒等（100% 有效里程） | 扣一点里程更像真机，但一旦扣到 `minDistance` 以下记录直接不算数；在没有服务端口径证据前不动 |
| `faceCheck=1` 而 `runFaceCheck` 为空 | 同样是「像真机」与「能算数」的取舍：置 0 可能让需要人脸核验的学校判无效 |
| 提交窗口 `id = 60000+i` 与 OBS 的 `id` 口径不同 | 代码注释表明这是两种格式的既有差异（`submit_windows` 的注释），没有抓包证据前不擅自统一 |
| 机型库仅 13 款、`androidId` 由 `device_id` 派生 | 属数据工程（补机型长尾）与「别整份复制 identity.json」的使用约束，不是代码 bug |

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

**验证**：231 passed；clippy 退出码 0；`--all-targets` 与 `--target aarch64-linux-android` 均通过；
`cargo build`（桌面）通过；CLI 实跑 `update --check` 走通真实 TLS（GitHub API 支持 h2，
返回"没有可用的发布资产"属正常 —— 该仓库没有 Release）。




