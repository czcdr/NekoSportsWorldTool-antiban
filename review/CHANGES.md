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


