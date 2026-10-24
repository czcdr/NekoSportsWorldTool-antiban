> 说明：本文件是改造过程中的只读审计快照（当时改动尚未提交）。部分条目已在后续提交中修复，当前状态见 `REVIEW-GUIDE.md`。

# 上游 main 差异报告：快照 `e229abd` vs `origin/main`

- 仓库：`E:\rea\upstream`（Windows / PowerShell）
- remote：`origin = https://github.com/YanamiNeko/NekoSportsWorldTool.git`
- 分析时间：2026-10-24
- 角色约束：本报告全程只读（`git status/log/diff/show/rev-parse/ls-remote/fetch`），未修改 `src/` 下任何文件，未执行 `reset/checkout/merge/pull/rebase/stash`。

---

## 0. 一句话结论

**上游 `origin/main` 没有前移**：`git ls-remote` 与 `git fetch` 后 `origin/main` 仍为 `e229abdbab71e377b86632e588e607413af4b93c`，与本地 `HEAD` 完全一致；`git log HEAD..origin/main` 为空、`git diff --stat HEAD origin/main` 为空。

因此本报告不存在"上游新增改动可借鉴"这一项。下文改为记录：① 实际执行的命令与原始输出；② 快照自身的历史与 PR 清单；③ 本地未提交改动与上游 tip 的冲突面；④ **上游既有实现中仍然值得对齐/借鉴的具体点（file:line）**——这部分对本地那批改动依旧有直接价值。

---

## 1. 实际执行的命令与输出（可复核）

### 1.1 工作区状态

```
$ git status --short
 M Cargo.lock
 M Cargo.toml
 M android/AndroidManifest.xml
 M android/java/org/nekosportsworld/tool/MainActivity.java
 M src/api/client.rs
 M src/api/flow.rs
 M src/api/mod.rs
 M src/api/model.rs
 M src/api/points.rs
 M src/api/submit.rs
 M src/crypto/header.rs
 M src/lib.rs
 M src/track/wire.rs
 M src/ui/ai.rs
 M src/ui/device.rs
 M src/ui/jobs.rs
 M src/ui/route_refresh_tests.rs
?? proxies.example.json
?? src/api/egress.rs
?? src/api/fingerprint.rs
?? src/safety.rs
```

### 1.2 fetch 与远端 refs

```
$ git fetch origin --prune
(无输出，退出码 0)

$ git for-each-ref --format="%(refname) %(objectname:short) %(committerdate:iso)" refs/remotes/origin
refs/remotes/origin/HEAD e229abd 2026-10-20 12:19:37 +0800
refs/remotes/origin/main e229abd 2026-10-20 12:19:37 +0800
```

### 1.3 直接向远端确认（绕过本地 ref 缓存）

```
$ git ls-remote origin refs/heads/main
e229abdbab71e377b86632e588e607413af4b93c	refs/heads/main
(退出码 0)
```

### 1.4 差异三连：全空

```
$ git log --oneline HEAD..origin/main
(空)

$ git log --oneline origin/main..HEAD
(空)

$ git diff --stat HEAD origin/main
(空，退出码 0)
```

`git diff HEAD origin/main` 同理为空，无可粘贴片段。

### 1.5 一个必须交代的坑：这份快照是 **depth=1 浅克隆**

首次 `git log --oneline -20` 只返回 1 条提交，`git log --merges` 返回空，这不符合"合并提交"的常理。核查结果：

```
$ cat .git/shallow
e229abdbab71e377b86632e588e607413af4b93c

$ git rev-parse --is-shallow-repository
true

$ git rev-list --count HEAD
1

$ git rev-list --parents -n 1 HEAD
e229abdbab71e377b86632e588e607413af4b93c          # ← 没有父提交，被 shallow 边界裁掉了
```

即：快照只带工作树 + 1 个提交对象，父历史不在本地。**注意 `HEAD..origin/main` 的"空"结论不受影响**——两边 ref 指向同一个 SHA，这是 SHA 相等，不是历史遍历结果。

为了能说出 PR 清单，我执行了一次加深（仍属 `fetch` 范畴）：

```
$ git fetch --deepen=60 origin
From https://github.com/YanamiNeko/NekoSportsWorldTool
 * [new tag]         v0.2.1     -> v0.2.1
 * [new tag]         v0.2.2     -> v0.2.2
 * [new tag]         v0.2.3     -> v0.2.3
 * [new tag]         v0.2.4     -> v0.2.4
 * [new tag]         v0.2.5     -> v0.2.5
 * [new tag]         v0.3.0     -> v0.3.0
(退出码 0)

$ git rev-list --count HEAD
44
$ git rev-parse origin/main
e229abdbab71e377b86632e588e607413af4b93c          # ← 仍然没变
```

**副作用声明**（请知悉）：`--deepen` 只往本地 `.git` 对象库补了历史对象，并把 `v0.2.1…v0.3.0` 写成**本地 tag 引用**；未改工作区、未改分支、未改 HEAD，`origin/main` 的 SHA 不变。若不想留这些本地 tag，可 `git tag -d v0.2.1 v0.2.2 v0.2.3 v0.2.4 v0.2.5 v0.3.0`（本次未执行）。

---

## 2. 快照里有什么：历史与 PR 清单

```
$ git log --oneline -44            # 加深后可读
e229abd Merge pull request #88 from GAN1112233131/fix/confirmed-bugs-69856c91
36eba07 fix: clarify violation status, guard duplicate submissions and correct lap pace units
f416472 fix: refresh road routes without mandatory fences and repeat checkpoint loops
6325f14 fix: unify checkpoints, track statistics, run outcomes and compact UI
69856c9 Merge pull request #69 from Erfan817/fix/log-redaction
2fea70a fix(api): 日志脱敏 token 与响应敏感字段
6b677e1 fix(build): rfd 依赖限定非 Android 目标
57a1969 Merge pull request #7 from laotierocket/feature/route-planner
...
eb5515e Merge pull request #10 from Yunlk/fix/run-start-time-random
965dc06 Merge pull request #11 from hb-degithub/auto-update
155c5a2 Merge pull request #5 from hb-degithub/android-apk
0d24ad2 Merge pull request #3 from laotierocket/main
9fa7657 NekoSportsWorldTool v0.2.1
```

### 2.1 合并进来的 PR（快照内，已全部在本地 HEAD 中）

| PR | 合并提交 | 分支 | 干了什么（依据） |
|---|---|---|---|
| #88 | `e229abd` | `GAN1112233131/fix/confirmed-bugs-69856c91` | 大规模 bugfix：新增 `crates/route-planner/{boundary,check,gpx}.rs`、`src/api/{campus,school_roads,submission_guard}.rs`、`src/diagnostics.rs`、`src/road_status.rs`、`src/task_control.rs`、`src/track/{analysis,school_track,route_view}.rs`、`src/ui/{road_tools,preview_jobs}.rs` + 4 个测试文件；实测 `git diff --stat 69856c9 e229abd` = **70 文件 +19196/−1177** |
| #69 | `69856c9` | `Erfan817/fix/log-redaction` | 日志脱敏（token / 响应敏感字段）：新增 `src/textlog.rs`（97 行），改 `client.rs`/`login.rs`/`submit.rs` 等；实测 `git diff --stat 57a1969 6325f14` = **34 文件 +6501/−975** |
| #7 | `57a1969` | `laotierocket/feature/route-planner` | 真实道路路由：新增 `route-planner` crate + 围栏接口 + 必经点提取（`fence.rs`、`route.rs`、`smooth.rs`） |
| #25 | `b22d5f8` | `a176073240-cmd/fix/issue-23-device-location` | 设备位置/说明文案（`aa3150a fix: clarify device location and update instructions`） |
| #17 | `22b90fd` | `a176073240-cmd/fix/location-city-consistency` | 跑步地点与城市一致性（`f0082cd fix run location and city consistency`） |
| #10 | `eb5515e` | `Yunlk/fix/run-start-time-random` | 开始时间随机化（`f007d09`：随机模式支持按日期随机） |
| #11 | `965dc06` | `hb-degithub/auto-update` | 自动更新：桌面 GitHub Release 检查/下载/自替换 + Android 应用内安装 |
| #5 | `155c5a2` | `hb-degithub/android-apk` | Android APK host：原生键盘、移动布局、APK 构建 |
| #3 | `0d24ad2` | `laotierocket/main` | 设备信息页城市输入框等（`9b2c491`） |

> 注：#88 的分支基点 `6325f14` 与主线 `69856c9` 不同，所以 `e229abd` 是真正的三方合并（父：`69856c9` + `36eba07`），这解释了为什么它带了 1.9 万行增量。
>
> 现有 tag（加深后可见）：`v0.2.1 9fa7657` / `v0.2.2 0dba9d6` / `v0.2.3 00edacc` / `v0.2.4 6833e89` / `v0.2.5 12b3f33` / `v0.3.0 2dbbf8`；最新 tag 晚于 `v0.3.0` 的提交（PR #88 系列）尚无 tag 标注 → **v0.3.0 之后上游已经又改了一大轮**，但都已包含在本快照内。

---

## 3. 本地未提交改动概览

```
$ git diff --stat
17 files changed, 624 insertions(+), 56 deletions(-)
（另有 4 个未跟踪文件：src/safety.rs 18258B、src/api/egress.rs 11638B、src/api/fingerprint.rs 12177B、proxies.example.json 256B）
```

逐文件（标 ★ = 我逐行看过 diff；其余仅依据 stat 与新增行抽样）：

| 文件 | 增量 | 本地在做什么 |
|---|---|---|
| ★ `src/api/flow.rs` | +134 | 新增 `failure_reasons()` 解析 `reasonList` 中 `complete=false` 的原因；`sleep_secs` 换成 `sleep_jitter`（抖动到 `[base,2*base)`）；`server_skew_ms()` 校正时钟偏差；提交前跑 `safety::check_run` 与 `bind_egress_for_city`（出口与轨迹同城） |
| ★ `src/crypto/header.rs` | +78 | `HeaderIdentity` 增 `android_id`/`pixel`/`fingerprint_from_device`；`ensure_fingerprint()` 按机型库补齐；`fingerprint_seed()` 由 `device_id` 派生稳定 seed；`build_android_header` 改用真实 `pixel` 与真机 `androidId`，MAC 用 `02:00:00:00:00:00` 占位 |
| ★ `src/track/wire.rs` | +91 | `five_point_payload` 的 `position` 改为数组下标 `i`；`build_windows` 的 `id` 改为跨跑步单调递增（`id_base=start_ms/1000 % 100000` + 窗口序）、`queueNum` 改为 0 起下标、末窗按实际时长缩放并夹步频上限；新增 `window_and_position_tests` |
| ★ `android/.../MainActivity.java` | +14 | 授权弹窗文案扩到"分辨率 + ANDROID_ID"；`nativeDeviceInfo` 里实际读 `DisplayMetrics.widthPixels/heightPixels`、`densityDpi`、`Settings.Secure.ANDROID_ID` |
| ★ `src/api/submit.rs` | +33 | 提交固定走 Android 身份；原硬编码 `22081212C` 改为按 `device_id` 派生整套自洽指纹；`ensure_fingerprint()` 兜底 |
| ★ `src/api/points.rs` | +17 | 点位路径参数化：`POINTS_PATH_PREFIX = "/api/v560/get/1/distance/"` + `points_path(distance_m)`，取代写死 `/distance/1` |
| ★ `src/api/model.rs` | +43 | `Config` 增 `safety: SafetyPolicy`；加载时 `id.ensure_fingerprint()` 补旧配置；`DeviceInfo` 增 `pixel` / `density_dpi` |
| ★ `src/ui/device.rs` | +115 | 机型变更即重置派生指纹（清 `android_id`/`pixel`，`fingerprint_from_device=false`）；只补空字段不覆盖真机读数 |
| ★ `src/lib.rs` | +1 | `pub mod safety;` |
| `src/api/client.rs` | +100 | 出口/代理绑定相关（`bind_egress_for_city` 等），未逐行核对 |
| `src/ui/ai.rs` | +12 | 未逐行核对 |
| `src/ui/jobs.rs` | +8 | 未逐行核对 |
| `src/ui/route_refresh_tests.rs` | +16 | 未逐行核对 |
| `src/api/mod.rs` | +2 | 应为新模块声明（`egress` / `fingerprint`），未逐行核对 |
| `Cargo.toml` / `Cargo.lock` / `AndroidManifest.xml` | +2 / +12 / +2 | 依赖与清单微调，未逐行核对 |

新增文件（读过头注）：
- `src/safety.rs`：提交行为护栏（深夜时段、每日条数、批量补签计数），fail-closed，按"记录所属那一天"计次，北京时间口径；明确写了"上游原本已有什么"以避免重复造轮子。
- `src/api/egress.rs`：出口池，城市取自设备锚点、出口必须同城；`COOLDOWN_STEPS_SECS = [60, 180, 3600]` 失败退避；未配置即直连。
- `src/api/fingerprint.rs`：机型档案（`DeviceProfile`/`ModelProfile`：device_name、logical 分辨率、dpi）+ androidId/MAC 自洽生成。

---

## 4. 与上游 tip 的冲突面

前提：本地 `HEAD` 就是 `origin/main`，所以现在**没有冲突**；下表是"上游下一次前移时，这些文件被上游改到的概率"，用 PR #88/#69 的 diffstat 做证据。

| 本地改动文件 | 上游近期是否在动 | 冲突风险 |
|---|---|---|
| `src/api/flow.rs` | PR #88 改 383 行、PR #69 改 330 行 | **高**（上游最活跃文件之一） |
| `src/api/submit.rs` | PR #88 234 行、PR #69 148 行 | **高** |
| `src/api/points.rs` | PR #88 152 行、PR #69 139 行 | **高** |
| `src/track/wire.rs` | PR #88 334 行、PR #69 303 行 | **高** |
| `src/api/model.rs` | PR #88 83 行、PR #69 80 行 | 中高 |
| `src/lib.rs` | PR #88 +5 | 中（模块声明行，易手工合并） |
| `src/ui/ai.rs` | PR #88 仅 6 行 | 低 |
| `src/crypto/header.rs` | PR #88 / #69 diffstat 中**均未出现** | 低 |
| `src/ui/device.rs` | PR #88 / #69 diffstat 中**均未出现** | 低 |
| 新增 `src/safety.rs` / `src/api/egress.rs` / `src/api/fingerprint.rs` | 上游无同名文件 | 无（但 `model.rs`/`lib.rs`/`api/mod.rs` 的挂载点会被上游动到） |

结论：**风险集中在 `flow.rs` / `submit.rs` / `points.rs` / `wire.rs` 四个上游高频改动文件**；`header.rs`、`device.rs` 是本地改动最安全落点。真正麻烦的不是文本冲突，而是 `points.rs` 路径与 `wire.rs` 窗口语义若上游再次重构，本地口径可能被静默覆盖（需要测试兜住——本地已加 `window_and_position_tests`、`layout_tests` 等）。

---

## 5. 上游既有实现里仍值得对齐/借鉴的点（file:line）

> 全部基于 `git show HEAD:<file>` 的**已提交版本**行号（工作树有本地改动，行号会偏移，已注明）。

### 5.1 一个真实的矛盾：`appVersion` 请求头与信封内版本不一致

```
$ git show HEAD:src/api/client.rs | Select-String 'appVersion'
146: req = req.set("appVersion", "7.3.40");

$ git show HEAD:src/crypto/header.rs | Select-String 'ANDROID_APP_VERSION|IOS_APP_VERSION'
 10: pub const IOS_APP_VERSION: &str = "7.3.40";
 11: pub const ANDROID_APP_VERSION: &str = "7.3.70";
204: m.insert("appVersion".into(), ... ANDROID_APP_VERSION ...);   // build_android_header 内
```

即：Android 信封里声明 `7.3.70`，而 HTTP 头 `appVersion` 硬写 `7.3.40`（iOS 的值）。工作树（`src/api/client.rs:230`）**依旧是 7.3.40**，本地这批改动没有碰它 → 任何"机型-A 版本-B"的交叉校验都会命中这一条。是否被服务端校验 **未确认**（需要抓一次官方 Android 包对比请求头），但属于零成本对齐项。

### 5.2 Android 指纹：上游原样，本地已修（别回退）

```
$ git show HEAD:src/crypto/header.rs   # build_android_header，函数起始于 187
209: m.insert("IMEI".into(), "");                              // 留空符合官方行为
210: m.insert("logicPixel".into(), "1080x2400");
211: m.insert("physicPixel".into(), "1080x2400");
212: m.insert("androidId".into(), "");
213: m.insert("blMac".into(), "");
214: m.insert("wifiMac".into(), "");

$ git show HEAD:src/api/submit.rs
240: device_name: "22081212C".into(),                            // 写死小米 12T
```

`22081212C` 真机分辨率为 `1220x2712`，与写死的 `1080x2400` 组合即"机型-屏幕"矛盾指纹；`androidId` 恒空、MAC 恒空（Android 6+ 普通应用读 MAC 得到 `02:00:00:00:00:00`）。
→ 本地 `header.rs`(+78) / `submit.rs`(+33) / `fingerprint.rs` 已按"同一 `device_id` 派生一套自洽档案 + 真机读数优先"修好，**上游没有更优实现可借鉴，保持本地版本**。

### 5.3 协议版本硬校验点（改版本号前必看）

```
$ git show HEAD:src/crypto/decrypt.rs
155: let version = obj["v"]...
159: if version != 101 {
160:     return Err(format!("响应版本错误: {version} != 101"));
```

任何"升协议版本 / 对齐新客户端"的尝试，都必须同步这里，否则全部响应直接报错。这是"信封版本号"的唯一硬闸。

### 5.4 开始时间与频次控制的基线窗口

```
$ git show HEAD:src/ui/run.rs
 14: const RAND_HOUR_LO: u32 = 7;
 15: const RAND_HOUR_HI: u32 = 20;
 17: const JITTER_MARGIN_MS: i64 = 5_000;    // flow.rs 提交时给开始时间加 0-4s 抖动
 23: fn random_time_ago(days_ago: i64, latest_ms: i64) -> i64
155: crate::crypto::envelope::now_ms() - dur * 1000 - JITTER_MARGIN_MS
1039: p.start_ms + p.dur * 1000 + JITTER_MARGIN_MS <= now     // 随机模式断言"不越未来"
```

用途：本地 `safety.rs` 要拦"深夜提交"时，判定窗口必须与这里 **7:00–20:00 + 5s 余量** 一致；否则会把上游认为合法的随机抽样误判为越界。本地 `safety.rs` 头注已声明"不重造窗口"，方向正确。

### 5.5 10 秒窗与 `fivePointJson` 的上游原版语义（本地已升级）

```
$ git show HEAD:src/track/wire.rs
195: pub(crate) fn build_windows(track: &Track, rrid: i64) -> (Vec<Value>, Vec<Value>) {
204:     let id = (rrid % 100000) * 1000 + hi;    // id = rrid 派生 + 窗口结束秒
211:     "queueNum": 0,                            // 恒 0
222:     "queueNum": 0,                            // 恒 0
 63:     "queueNum": p.queueNum,                   // fivePointJson 透传
```

本地把 `id` 改成"以开始时刻为基的跨跑步单调序号"、`queueNum` 改成 0 起下标、末窗按 `window_s / a.time` 缩放并夹 `220 步/分` 上限（`src/track/wire.rs` 本地 +91 行）。上游此处**不提供**这些语义 → 无需借鉴，保留本地。

### 5.6 幂等/防重复提交：上游 `submission_guard.rs` 比本地 `safety.rs` 更硬

```
src/api/submission_guard.rs:1     //! Write-ahead receipts, scoped to an account and school. No tokens or coordinates.
src/api/submission_guard.rs:2     //! OS file locking covers GUI/CLI processes using the same data directory.
src/api/submission_guard.rs:14-27  enum Event { Pending{uuid,start_ms,stop_ms}, Accepted{uuid,rrid}, Rejected{uuid} }
src/api/submission_guard.rs:161   pub fn begin(&mut self, uuid, start_ms, stop_ms) -> Result<(), String>
src/api/submission_guard.rs:201   pub(crate) fn preflight(...)
```

上游用"写前回执 + OS 文件锁"实现跨进程幂等（Pending→Accepted/Rejected 三态，避免重复提交同一段跑步）；本地 `safety.rs` 只做"按自然日计次"。**两者互补**：建议本地护栏保留日计数，但把"同一段 `start_ms/stop_ms` 只能提交一次"的写前回执接进 `flow.rs`（当前 `submission_guard` 在本地未被改动，可直接复用）。

### 5.7 服务端策略口径（`minDistance` 决定 `selDistance`）

```
src/api/policy.rs:8    pub const POLICY_PATH: &str = "/api/v70103/runModePolicy";
src/api/policy.rs:2    //! 返回 data.runRuleModel.minDistance（提交时 selDistance 用它）与 data.policy。
src/api/policy.rs:12-17 pub struct PolicyInfo { timestamp, policy, min_distance, valid_time, must_points }
```

提交时 `selDistance` 必须取服务端 `runRuleModel.minDistance` 的口径，而不是客户端自算；本地 `flow.rs` 已用 `PolicyInfo`，改动时别破坏这个取值链。

### 5.8 轨迹生成的采样节奏（10s 窗的来源）

```
src/track/generator.rs:4  //! 采样间隔主 5s（80%）；速度曲线 = ramp × 疲劳 × 三正弦 × 余弦凹陷 × 微噪；
src/track/mod.rs:152      // 采样间隔：5s 占比 ≥ 60%
src/track/mod.rs:165      // 10s 窗非空、结构合法
src/track/stats.rs:58-62  track.speedPerTenSec.push(TenWindow{..}); track.stepsPerTenSec.push(TenWindow{..})
```

主采样 5s / 80% 的分布 + `stats.rs` 聚合成 10s 窗，是"10 秒窗口"口径的唯一来源；改 `wire.rs` 窗口内容时不要改采样节奏。

### 5.9 点位接口按目标距离取点（本地已修）

```
$ git show HEAD:src/api/points.rs
 1: //! 校园点位：POST /api/v560/get/1/distance/1（sportType=4）
13: pub const POINTS_PATH: &str = "/api/v560/get/1/distance/1";
```

上游写死 `/distance/1`；本地已改为 `/distance/<目标距离>`（本地 `points.rs` +17，注释称真机实测 `.../distance/2000`）。**"真机抓包实测"这点我这次没有独立证据**（本报告只读仓库，未抓包）→ 标记 **未确认**，但方向与 `points_path()` 的实现一致。

---

## 6. 未确认事项

1. 服务端是否校验 HTTP `appVersion` 头（5.1 的矛盾是否真的被利用）。
2. 点位路径末尾必须为目标距离 —— 依赖上游注释与本地注释里的"真机实测"，我未抓包验证。
3. `src/api/client.rs`(+100) / `src/ui/ai.rs` / `src/ui/jobs.rs` / `route_refresh_tests.rs` / `Cargo.*` / `AndroidManifest.xml` 的本地改动我只看了 stat 与少量新增行，未逐行核对。
4. 上游是否存在**非 main** 的新分支（例如 #88 之外的活跃 PR）——本次只 `ls-remote refs/heads/main`，未枚举全部 head。
5. `--deepen=60` 只覆盖到 44 个提交，`v0.2.1` 之前的历史仍不在本地；若需要更早历史可 `git fetch --unshallow`（本次未执行）。

---

## 结论

### 一、上游 main 相比这份快照改了什么

**没有任何改动。** 已执行 `git fetch origin --prune`（退出码 0）与 `git ls-remote origin refs/heads/main`，两条路径都指向 `e229abdbab71e377b86632e588e607413af4b93c`，与本地 `HEAD` 同一 SHA；`git log --oneline HEAD..origin/main` 空、`git diff --stat HEAD origin/main` 空。故"按文件分组的上游改动清单"为空表，不做任何编造。

（附带条件：本地是 `depth=1` 浅克隆，`git rev-list --count HEAD` 原始值为 1；上述"无差异"结论基于 SHA 相等，不依赖历史遍历，因此不受浅克隆影响。）

### 二、哪些提交是合并进来的 PR

本次 fetch **没有带进任何新 PR**。快照内部已合并的 PR 共 9 个（#3、#5、#7、#10、#11、#17、#25、#69、#88），全部已在本地 `HEAD` 中：`#88` 是一次三方合并的大规模 bugfix（70 文件 +19196/−1177，新增 route-planner 子 crate、`campus/school_roads/submission_guard/task_control/diagnostics/road_status` 等），`#69` 是日志脱敏（34 文件 +6501/−975，新增 `src/textlog.rs`），`#7` 是真实道路路由，其余为 Android 移植、自动更新、开始时间随机化、地点/城市一致性等。

### 三、与本地未提交改动的冲突面

现在无冲突（本地 HEAD == 上游 tip）。按上游近期改动密度看风险：**高** —— `src/api/flow.rs`、`src/api/submit.rs`、`src/api/points.rs`、`src/track/wire.rs`（PR #88 与 #69 对这四个文件的改动量都在 140–383 行级）；**中高** —— `src/api/model.rs`（83/80 行）与 `src/lib.rs`（+5，模块声明行）；**低** —— `src/ui/ai.rs`（6 行）；**低且安全** —— `src/crypto/header.rs`、`src/ui/device.rs`（两个 PR 的 diffstat 里都没出现这两个文件）。三个新增文件（`safety.rs`/`egress.rs`/`fingerprint.rs`）上游无同名，风险只落在 `lib.rs`、`api/mod.rs`、`model.rs` 的挂载点。

### 四、最值得直接借鉴的 5 条（上游既有实现，file:line）

1. **`appVersion` 头与信封版本对齐** —— `src/api/client.rs:146` 硬写 `7.3.40`（= `IOS_APP_VERSION`，`src/crypto/header.rs:10`），而 Android 信封用 `7.3.70`（`src/crypto/header.rs:11`，写入点 `:204`）。本地工作树仍是 7.3.40，零成本对齐项（服务端是否校验未确认）。
2. **写前回执式幂等** —— `src/api/submission_guard.rs:1-2`（Write-ahead receipts + OS 文件锁）、`:14-27`（Pending/Accepted/Rejected 三态）、`:161 begin()`、`:201 preflight()`。本地 `safety.rs` 只有日计数，缺"同一段 `start_ms/stop_ms` 只提交一次"这一层，直接复用即可。
3. **开始时间/频次基线窗口** —— `src/ui/run.rs:14-17`（`7:00–20:00` + `JITTER_MARGIN_MS=5000`）、`:23 random_time_ago()`、`:1039`（整段不越未来的断言）。本地深夜拦截的窗口必须与此一致，否则误拦合法随机抽样。
4. **协议版本硬闸** —— `src/crypto/decrypt.rs:155-160`（`v != 101` 直接报错）。任何版本号/信封调整必须同步；同一处还有 `src/api/policy.rs:8`（`/api/v70103/runModePolicy`）与 `:12-17`（`minDistance` 决定提交时 `selDistance`）的路径/口径约束。
5. **10 秒窗与采样节奏的既有口径** —— `src/track/wire.rs:195-222`（上游原版 `id=(rrid%100000)*1000+hi`、`queueNum` 恒 0）、`src/track/generator.rs:4`（5s 主采样 80%）、`src/track/mod.rs:152/165`（5s 占比与 10s 窗断言）、`src/track/stats.rs:58-62`（聚合出 `speedPerTenSec/stepsPerTenSec`）。本地已把 `wire.rs` 窗口升级（单调 id、`queueNum` 下标、末窗缩放 + 220 步/分夹取），此处应**保留本地**、只把 `generator/stats` 的采样节奏当作不改动的基线。

> 补充（负面结论，同样重要）：Android 设备指纹（`androidId` / 分辨率）上游是**坏的** —— `src/crypto/header.rs:210-214` 写死 `1080x2400` 且 `androidId`/MAC 全空，配 `src/api/submit.rs:240` 写死机型 `22081212C`（真机 `1220x2712`）。这一块上游无可借鉴，本地 `header.rs`(+78)/`submit.rs`(+33)/`fingerprint.rs` 已修好，不要回退。点位接口同理（上游 `src/api/points.rs:13` 写死 `/distance/1`，本地已参数化）。
