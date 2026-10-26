> 说明：本文件是改造过程中的只读审计快照（当时改动尚未提交）。部分条目已在后续提交中修复，当前状态见 `REVIEW-GUIDE.md`。

# 本地改动只读审计（safety / fingerprint / egress / wire / points / header）

审计对象：`E:\rea\upstream` 工作区（HEAD = `e229abd`）+ 未提交改动（17 个文件、3 个新文件）。
审计方式：只读阅读 + `cargo check` + `cargo test --lib`。**未修改任何源码文件**（仅写本文件）。
审计时 `src/api/model.rs`、`src/ui/device.rs`、`android/java/.../MainActivity.java` 可能处于并行编辑中间态，结论按「文件 + 函数/片段」定位。

---

## 0. 结论速览

| 项 | 结果 |
|---|---|
| `cargo check --lib --features gui` | **通过**（0 error / 0 warning） |
| `cargo check --lib --features android --target aarch64-linux-android` | **本机无法判定**（缺 NDK clang，`ring` 构建脚本先失败，非代码问题） |
| `cargo test --lib` | **211 passed / 0 failed**（4.18s） |

### 环境说明（可复现）

本机默认工具链是 `stable-x86_64-pc-windows-gnu`，但**既没有 gcc.exe 也没有 MSVC 的 link.exe**，`cargo check` 会先死在 `ring v0.17.14` 的 build script：

```
error occurred in cc-rs: failed to find tool "gcc.exe": program not found
```

定位到 WinLibs 的 MinGW（`...\BrechtSanders.WinLibs.POSIX.UCRT_...\mingw64\bin`, gcc 16.2.0）后，注入
`CC / CC_x86_64_pc_windows_gnu / AR` + PATH 即可干净编译：

```powershell
$mingw="C:\Users\lenovo\AppData\Local\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\mingw64\bin"
$env:Path="$mingw;C:\Users\lenovo\.cargo\bin;$env:Path"
$env:CC="$mingw\gcc.exe"; $env:CC_x86_64_pc_windows_gnu="$mingw\gcc.exe"; $env:AR="$mingw\ar.exe"
cargo check --lib --features gui      # exit 0, Finished dev profile, 0 warning
cargo test  --lib                     # 211 passed; 0 failed
```

Android 目标：`aarch64-linux-android` 目标已安装，但靶机侧 C 编译器缺失：

```
warning: ring@0.17.14: Compiler family detection failed ... failed to find tool "aarch64-linux-android-clang"
error occurred in cc-rs: failed to find tool "clang.exe": program not found
```

因此 **`#[cfg(target_os = "android")]` 的代码（`src/android.rs`、`ui/device.rs::poll_device_info`、`model::identity_with_device_info` 的 android 分支）在本次审计中没有被类型检查**，只能静态阅读。宿主机编译已覆盖除 android cfg 之外的全部改动。

---

## 1. 逐问回答

### 1.1 计数是否按「记录所属自然日」分桶？——是，符合预期

`safety.rs::check_run_at` 用 `cst_parts(start_ms)` 取**记录开始时刻所属日期**作键，`record_run_at` 用同一口径写盘，`backfill_across_different_dates_is_not_blocked` 用例也锁住了「3 天前 / 2 天前 / 昨天 / 今天各一条全部放行」。按提交日计数会把补跑堵死的那个陷阱**不存在**。

需要注意的不是分桶口径，而是两处外围问题：调用点传的 `start_ms` 与校验时不是同一个值（§2 问题 6）、UI 用系统时区而护栏用固定 UTC+8（§2 问题 7）。

### 1.2 `record_run` 与 `check_run` 的 `start_ms` 是否一致？——**不一致**

- 校验：`flow.rs::run_full_flow_cancellable` → `crate::safety::check_run(&safety, params.start_ms)`（**未抖动**）。
- 记账：同函数尾部 → `crate::safety::record_run(&safety, result.start_ms)`，而 `SubmitResult.start_ms` 就是 `track.startTime`，即 `flow.rs` 里 `let start_ms = params.start_ms + (rand::random::<u32>() % 5) as i64 * 1000;`（**已抖动 0–4s**）。

差 0–4 秒在默认策略下不会跨自然日（跨零点的那 4 秒本来就被 23:00–06:00 深夜档拦住）。但两点让它仍是真 bug：

1. `in_night` 在 `night_from == night_to` 时恒为 `false`（用户把两个值填成一样就等于关掉深夜档），此时 23:59:5x 的开始时间 + 抖动 → 校验记在 D 日、记账落在 D+1 日，D 日的额度被腾出来，可以再提一条；反向也能出现"刚记完就把下一次堵死"。
2. `safety.rs` 里那句注释「记账键用服务端确认的开始时刻：与 check_run 的判定日一致（含 0-4s 抖动）」是**错的**——check 用的是抖动前的值，注释描述的是意图而不是实现。

改法：把抖动后的 `start_ms` 提前到 `check_run` 之前生成（或把抖动移进 `RunParams` 构造处），让校验、轨迹、提交、记账共用同一个值；顺便修正注释。

### 1.3 状态文件的并发写 / 跨天 / 时区 / 损坏

- **并发写：有真实风险。** `record_at` 是 `load_state_at` → 改内存 → `save_state_at` 的读改写，没有任何锁；`save_state_at` 用的是 `std::fs::write`（truncate + 写）。UI 的跑步作业（`ui/run.rs` → `spawn_job`）和 AI 作业（`ui/ai.rs` → `spawn_job`）跑在**不同线程**，同时成功就会后写覆盖前写（丢计数）；进程在这中间被杀则留下半截 JSON，而 `load_state_at` 是 fail-closed → 之后**所有**跑步/AI 提交都被拒，提示"删掉该文件会重置计数"，用户只能手工修。
- **跨天边界**：按 `start_ms` 的自然日，行为正确；`KEEP_DAYS = 10` 的裁剪按 `BTreeMap` 字符串序删最旧键，`YYYY-MM-DD` 字典序＝时间序，正确（保留的是"最近 10 个有记录的日期"，够覆盖 3 天回溯）。
- **时区**：`safety.rs` 固定 `UTC+8`，与协议层一致；但 UI 侧（`ui/run.rs::random_time_ago / specified_time`、`ui/ai.rs::random_time_days_ago`）用的是 `chrono::Local`。非 UTC+8 的机器上两者会打架（见 §2 问题 7）。
- **损坏处理**：fail-closed 是刻意设计（可接受），但它和"非原子写"叠加就成了高代价陷阱；建议写临时文件 + `rename` 原子替换，并把损坏文件备份成 `safety_state.json.bad` 后**允许重建**（或在错误里给出"重建"开关），而不是只能手工删。

### 1.4 `wire.rs::build_windows`

- **末窗缩放：正确。** `hi = (lo + a.time).min(total_time)`、`window_s = (hi - lo).max(1)`、`frac = (window_s / a.time.max(1)).clamp(0.01, 1.0)`，距离按 `frac` 缩放、步数按 `frac` 缩放再用 `220 spm` 夹上限，末窗 7s 也能落在合理步频（`last_partial_window_keeps_cadence_sane` 锁住了）。`a.time == 0`（合并窗口）被 `max(1)` 兜住，不会除零。
- **`id` 生成：与注释相反，且会回绕/重叠。** `id_base = (start_ms / 1000).rem_euclid(100_000)`、`id = id_base + i` 是**时钟派生**，函数正上方注释却写着"`id` 是跨跑步单调递增的序号（两次跑步分别 2..96、97..102），与 rrid 无关"。实际行为：每 100000 秒（27.78 小时）回绕；两次开始时刻相差小于窗数（几十秒内）时 id 段直接重叠；`id_base + i` 还会越过 99999。提交侧无所谓（`submit_windows` 统一改写成 `60000 + i`），但 OBS 的 `speed_json` / `step_freq_json` 用的是它——服务端若按 id 去重/排序，跨记录的窗口会串（典型触发：护栏关掉后连发两条，或重试）。
- **`queueNum` 语义：正确。** `queueNum = i`（本次跑步内 0 起下标），提交侧再压一次 `i`，与真机样本一致。

### 1.5 `points.rs` 的 `distance` 与缓存键

缓存键是**能区分的**：`PointsCacheScope.distance_m` 参与 `self.scope == *scope` 的全等比较，`cache_is_scoped_to_anchor` 也断言了 `distance_m: 3000` 不复用 `2000` 的缓存。残留两个小口径问题（§2 问题 12）：

- `points_path` 用 `distance_m.round().max(1.0)`，scope 用 `distance_m.round() as i64`——距离 < 1m / 负值时路径恒为 `/distance/1`，键却是 0/负数，同一份响应被换 key 重拉一次（点位接口有"5 分钟 3 次"限流，多拉一次就是 10603 的诱因）。
- 兼容入口 `fetch_points()` 写死 `2000.0`，与 `flow` 传的 `params.dist` 不同键 → 预览与提交各拉一次。

### 1.6 `fingerprint::audit()` 与 `from_parts()` 是否互相打架

- **不会误报真机**：`ui/device.rs` 调的是 `audit_ex(&profile, !identity.fingerprint_from_device)`；`identity_with_device_info` 在真机读到 pixel + androidId 时置 `fingerprint_from_device = true`，于是 `check_model=false`，机型不在库内也不告警。这条设计是对的。
- **会打架的是"未知机型 + 空分辨率"**：`from_parts()` 在 `pixel` 为空时 `unwrap_or(base.pixel)`，`base` 是 `mint(seed)` 随机选中的**另一台机型**的分辨率（例如 user 手填 `SM-G9910`（不在库内）→ 可能拿到 `1440x3168`）。随后 `audit_ex(check_model=true)` 只会说"机型不在已知库内，分辨率无法核对"——也就是说**工具自己编了一个假分辨率，然后又声明无法核对**，设备页把它当"⚠ 提示"，提交时这个假值照样进请求头。
- 另有一处展示/落盘不一致：`ui/device.rs` 展示用 `DeviceProfile::from_parts(..., 0)`（seed 写死 0），而保存走 `ensure_fingerprint()` → `fingerprint_seed(device_id)`。`pixel`/`android_id` 为空时，"看到的自检结果"和"实际存下/上报的"不是同一套值。

改法：未知机型且 `pixel` 为空时**不要**用 seed 兜一个分辨率——留空并在提交前报"请填写本机分辨率"，或干脆要求走「读取本机信息」；展示与生成统一用 `fingerprint_seed(device_id)`。

---

## 2. 问题清单（按严重度）

### 1) [高] `safety` 字段缺一个，整份 `config.json` 被静默重置，护栏反而回到开启

- **位置**：`src/api/model.rs`（`Config` 的 `#[serde(default)] pub safety: SafetyPolicy`、`load_config()`）+ `src/safety.rs`（`SafetyPolicy` 无字段级 `#[serde(default)]`）+ `docs/antiban.md` §4.6 的 JSON 片段。
- **现象**：文档给的片段是 `enabled / night_from / night_to / daily_runs / daily_ai / backfill_days`——**少了 `batch_per_day`**。用户照着改 `config.json`，`SafetyPolicy` 反序列化失败 → `read_json::<Config>()` 返回 `None` → `load_config()` 落到 `Config::default()`。
- **为什么坏**：损失是全量的——username/password/school_campus/dist/pace/route_mode/osm_path 全部清空；用户想要的 `enabled:false` **没有生效**（回到默认 `true`），于是深夜提交继续被拒、错误信息还在教他"改 safety.enabled"，形成闭环。用户视角是"我按提示改了，越改越坏"。
- **建议**：`SafetyPolicy` 每个字段加 `#[serde(default = "...")]`（或容器级 `#[serde(default)]` + `impl Default`）；`load_config` 区分"文件不存在"与"解析失败"，后者应保留错误并提示（必要时把坏文件改名备份），不要静默 default；同步修正文档片段。

### 2) [高] `safety_state.json` 读改写无锁、写入非原子：并发丢计数，崩溃半截文件把提交永久堵死

- **位置**：`src/safety.rs::record_at` / `save_state_at` / `load_state_at`。
- **现象**：`load → mutate → std::fs::write` 三步没有互斥；跑步作业与 AI 作业是不同线程（`ui/run.rs`、`ui/ai.rs` 各自的 `spawn_job`），并发成功时后写覆盖前写；`fs::write` 先 truncate 再写，中途被杀留下非法 JSON。
- **为什么坏**：丢计数 → 每日上限失效（护栏形同虚设）；坏文件 → `load_state_at` fail-closed → 之后每一条提交都被拒，只能人工删文件（文档也这么写），是"看起来彻底不能用"级别的故障。
- **建议**：写临时文件 + `fs::rename` 原子替换；进程内用 `Mutex`（或 `OnceLock<Mutex<State>>`）串行化读改写；读失败时把坏文件备份后按空状态继续（至少给出"重建"入口），别让 UI 只能靠用户手删。

### 3) [高] 出口池的冷却/轮转只活在单次作业里，跨作业完全失效

- **位置**：`src/api/egress.rs`（`health`/`cursor` 都在内存）+ `src/api/client.rs::load_egress_from_disk` + `src/ui/mod.rs::App::client`（每个 job `ApiClient::new`）。
- **现象**：每个作业新建 `ApiClient` → `EgressPool::load` 重新读盘 → `health`、`cursor` 归零。`acquire_at` 里 `alive[self.cursor % alive.len()]`，cursor 从 0 起 → **每次作业都固定挑同一条**；`report(Failed)` 的冷却在下一次作业里不存在。
- **为什么坏**：死代理永远被选中、永远不受冷却约束，`COOLDOWN_STEPS_SECS` 和 `PoolError::Exhausted` 在真实路径上几乎不可达（只有单作业内二次 acquire 才可能看到，而流程只 bind 一次）——这套机制目前是装饰性的，测试也只覆盖了内存池。
- **建议**：健康度与游标落盘（同样用原子写），或在进程内做全局单例池（`OnceLock<Mutex<EgressPool>>`）；真正的轮转要跨作业生效。

### 4) [中高] 出口绑定失败直接中止整条流程；一次网络抖动 → 60s 内无法再提交且看不出原因

- **位置**：`src/api/flow.rs`（`client.bind_egress_for_city(&city)?`）、`client.rs::bind_egress_for_city`（`PoolError::Exhausted` → `Err`）、`client.rs::envelope_request` / `submit.rs::send_record`（`let transport = matches!(&e, ureq::Error::Transport(_)); self.report_egress(!transport);`）。
- **现象**：任何 `ureq::Error::Transport`（包含目标域名解析失败、TLS 握手超时，而不只是代理坏了）都被记成"出口失败"，进入 60s/180s/3600s 冷却；同城只有一条出口时，紧接着的下一次提交在 `Exhausted` 分支被拒，提示"同城出口全部处于冷却中，本次不发请求"。
- **为什么坏**：把"目标不可达"错判成"代理坏了"；用户没有任何界面能看到冷却截止时间，只能反复重试并持续失败；对只有单条出口的常见配置，这是"用一次就锁死一小时"。
- **建议**：只对**代理侧**失败（连接代理本身失败）计入冷却；区分 `NoCandidate`/`Exhausted` 的界面提示并显示冷却剩余时间；单出口场景允许"降级直连 + 显式告警"由用户选。

### 5) [中] `daily_ai` 没覆盖批量补签：默认"每天 2 条"能被一次批量打败

- **位置**：`src/ui/ai.rs::run_ai_batch`（只 `check_night` + `check_backfill`，循环里直接 `api::ai::upload`，**从不** `record_ai`）；对照 `flow.rs::run_ai_submit`（有 `check_ai(1)` / `record_ai(1)`）。
- **现象**：默认 `daily_ai = 2`、`batch_per_day = 2`，批量里 `day == 0`（今天）一轮就能提交 `per_day × 选中项目数` 条今天的记录（项目可多选，默认列表十几项）——今天最多 2 条这条规则被绕开。
- **为什么坏**：护栏文档与界面都宣称"每日 AI ≤ 2 次"，实际只约束单发；批量是风控画像里更可疑的形态，却完全没被这条规则约束，规则与实现不一致。
- **建议**：批量里按自然日累计并 `record_ai(count)`（`ai::upload` 已支持 `at`，`scoreDate` 就是记录所属日）；或在 `check_backfill` 里加上"今天将提交的条数 ≤ daily_ai"。

### 6) [中] `check_run` 与 `record_run` 用的不是同一个 `start_ms`（抖动导致）

- **位置**：`src/api/flow.rs::run_full_flow_cancellable`（`check_run(&safety, params.start_ms)`；稍后 `let start_ms = params.start_ms + (rand::random::<u32>() % 5) as i64 * 1000;`；末尾 `record_run(&safety, result.start_ms)`）。
- **现象/为什么坏**：默认策略下影响被深夜档掩盖；一旦 `night_from == night_to`（等价关闭深夜档）或用户自定义窗口，跨零点抖动会让"校验日"和"记账日"错位，上限可被绕过或误拦。注释声称两者一致，与实现不符，后续维护会被误导。
- **建议**：抖动提前生成一次，校验/轨迹/提交/记账共用；或把抖动放进 `RunParams` 构造里由 UI 完成。

### 7) [中] UI 用系统时区造时间，护栏/协议用固定 UTC+8

- **位置**：`src/ui/run.rs::random_time_ago` / `specified_time` / `latest_start_ms`（`chrono::Local`）、`src/ui/ai.rs::random_time_days_ago`（`Local`）vs `src/safety.rs::cst_parts`（`FixedOffset::east(8*3600)`）。
- **现象**：非 UTC+8 的机器上（桌面端很常见：系统时区不是北京），"7:00–20:00 随机窗口"和"今天/昨天"都按系统时区算，护栏却按北京时间判。系统时区 = UTC+0 且本地时间 15:00–23:00 时，抽到的时刻会被换算到北京 23:00–07:00 → 提交被深夜档拒，提示"开始时间是 X 点"，而用户选的是白天。同理"今天"的补跑可能被记到相邻自然日。
- **建议**：UI 侧统一用 `FixedOffset::east(8*3600)` 计算日期窗口与 `days_ago`，和 `safety`、协议层口径对齐（或至少在设备页显示"按北京时间"）。

### 8) [中] OBS 窗口 `id` 是时钟派生：每 27.8h 回绕，短间隔提交会重叠

- **位置**：`src/track/wire.rs::build_windows`（`id_base = (start_ms / 1000).rem_euclid(100_000)`、`id = id_base + i`；上方注释却写"跨跑步单调递增的序号"）。
- **现象/为什么坏**：注释与实现相反；`rem_euclid(100_000)` 每 100000 秒复位一次；两次跑步开始时刻相差小于窗口数（几十秒）时 id 段直接重叠；`id_base + i` 会越过 99999。提交侧会把 id 改写成 `60000 + i` 所以不受影响，**OBS 的 `speed_json`/`step_freq_json` 受影响**（服务端若按 id 排序/去重，跨记录窗口会串）。
- **建议**：改成持久递增计数器（或用 `rrid` 参与派生），显式处理回绕；同步修正注释与 `window_ids_are_sequential` 的语义说明（该用例只验证了单次跑步内递增）。

### 9) [中] UA 仍写死 `22081212C` + `Android 14`，与"机型↔分辨率自洽"目标冲突

- **位置**：`src/crypto/header.rs::UA_ANDROID`（`Mozilla/5.0 (Linux; Android 14; 22081212C Build/UKQ1.231003.002) ...`）、`src/api/submit.rs::send_record`（显式 `.set("User-Agent", UA_ANDROID)`）；其余请求走 `client.rs::envelope_request(..., UA_IOS, ...)`。
- **现象/为什么坏**：身份被随机成 `23127PN0CC / Android 14` 或 `2211133C / Android 13` 时，UA 里固定是 `Android 14; 22081212C`——同一请求内两处设备描述互相矛盾，正是本轮改造想消除的那类"廉价判据"。另外同一会话里"点位/策略走 iOS UA、提交走 Android UA"，与 `HeaderIdentity.platform` 无关（`docs/antiban.md` §5 已把 iOS/Android 混用列为已知风险）。
- **建议**：UA 按 `identity.device_name` / `identity.os_version` 拼装（`SWCampus/{app_version}` 段同步）；同一会话内统一 UA，别在 iOS UA 与 Android UA 之间切换。

### 10) [中] `from_parts()` 给未知机型**编造**分辨率，`audit()` 只能"无法核对"

- **位置**：`src/api/fingerprint.rs::from_parts`（`unwrap_or(base.pixel)`）、`audit_ex` 的 `!is_known_model` 分支；`src/ui/device.rs`（展示用 `from_parts(..., 0)`）。
- **现象**：手填真实但未收录的机型（如 SM-G9910）且分辨率为空 → 得到**随机另一台机型**的分辨率并写进请求头；自检只说"机型不在已知库内，分辨率无法核对"，于是"编造的值"既没被拦也没被提示为错。展示用 seed=0、保存用 `fingerprint_seed(device_id)`，空字段时"看到的"和"上报的"不是同一套。
- **建议**：未知机型 + 空分辨率时留空并在提交前要求填写（或强制走「读取本机信息」）；展示与生成统一 seed；`audit_ex` 对"由兜底生成"的值单独提示，而不是笼统说"无法核对"。

### 11) [中低] Android 真机分辨率取的是**应用窗口**尺寸，且 `fingerprint_from_device` 会永久关掉自检

- **位置**：`android/java/org/nekosportsworld/tool/MainActivity.java`（新增 `getResources().getDisplayMetrics()` → `pixel`）、`src/api/model.rs::identity_with_device_info`（`fingerprint_from_device = true` 当 pixel + androidId 都非空）、`ui/device.rs`（`audit_ex(profile, !from_device)`）。
- **现象**：`DisplayMetrics` 拿到的是应用窗口（逻辑）尺寸，分屏/手势导航/挖孔机上常见 `1220x2652` 之类，而机型规格是 `1220x2712`；同时 `fingerprint_from_device=true` 让 `audit_ex(check_model=false)` 从此不再核对分辨率。
- **为什么坏**：偏差永久不可见，还作为"真机权威值"上报——比不做更糟（`fingerprint.rs` 文件头的立论就是"机型与分辨率不匹配"是最廉价判据）。
- **建议**：改用 `WindowManager.getCurrentWindowMetrics()` / `Display.getRealMetrics()` 取真实屏幕分辨率；设备页把"真机读数 vs 机型库期望"并列展示（只提示、不阻断），别用一个布尔把自检整个关掉。

### 12) [低] 点位缓存键与请求路径的取整口径不一致，多一次请求就多吃一次 10603

- **位置**：`src/api/points.rs::points_path`（`distance_m.round().max(1.0) as i64`）vs `PointsCacheScope { distance_m: distance_m.round() as i64 }`；兼容入口 `fetch_points()` 写死 `2000.0`。
- **现象/为什么坏**：距离 < 1m 或负值时路径恒为 `/distance/1`、键却是 0/负数 → 同一份响应换 key 重拉；`fetch_points()` 与 `flow` 的键不同 → 预览与提交各拉一次。点位接口是"5 分钟 3 次"的限流，多拉一次就可能 10603。
- **建议**：由一个函数同时算出"路径里的距离"和"缓存键距离"，两者永远同源；去掉 `fetch_points()` 的写死 2000。

---

## 3. 默认值下会被拦的操作（与原功能的冲突清单）

默认策略：`enabled=true, night 23–6, daily_runs=1, daily_ai=2, backfill_days=3, batch_per_day=2`。

| 操作 | 原能力 | 默认值下 |
|---|---|---|
| 跑步页「今天」提交第 2 条 | 可以连跑多条 | **拦**（"同一天多条记录"） |
| 一天内补跑「昨天」+「1-3 天前」各一条 | 可以 | **放行**（各自然日各 1 条，最多 4 天 4 条） |
| 23:00–06:00（北京时间）提交任何跑步 | 可以 | **拦**（校验"开始时刻"和"当前时刻"两个字段） |
| 指定时刻填 00:00–05:59 / 23:00–23:59 的补跑 | 可以 | **拦**（开始时刻落在深夜档） |
| 深夜（23–6 点）做批量补签（补的是历史日期） | 可以 | **拦**（`check_night` 只看提交时刻） |
| 批量补签 4–60 天（UI 可拖到 60） | 可以 | **拦**（`backfill_days=3`）→ UI 上 4–60 天是死选项 |
| 批量补签每天 3–10 次（UI 可拖到 10） | 可以 | **拦**（`batch_per_day=2`）→ UI 上 3–10 次是死选项 |
| 同一项目每天补 3 条以上 | 可以 | **拦**（同上，"同项目同日重复"） |
| AI 单发第 3 条（`daily_ai=2`） | 可以连发 | **拦**（但见问题 5：批量路径不受此限） |
| 批量里"今天"那一轮连发（`per_day × 项目数`） | — | **放行**（漏洞，见问题 5） |
| `safety_state.json` 损坏 | — | **全部拦**（fail-closed） |

结论：**跑步页 3 天回溯本身没被堵死**（每天各 1 条放行），被堵的是"同日多条 / 深夜 / 大跨度批量 / 大批量重复"；但批量补签 UI 的 4–60 天、3–10 次两个档位在默认值下完全不可用，建议 UI 直接把上限按策略 clamp 并显示原因，而不是让用户点了才报错。

---

## 4. panic / 崩溃风险核查

本次未发现**可达**的 panic（宿主机全量编译 + 211 个用例通过，改动路径上的 `unwrap/expect` 都落在测试或不可失败的分支）。值得记一笔的几处：

1. `src/track/wire.rs::build_windows`：`let b = &track.stepsPerTenSec[i];` 直接下标。当前安全性依赖 `Track::validate_consistency()`（`track/model.rs` 里 `speedPerTenSec.len() != stepsPerTenSec.len()` → `Err`）先被调用；`flow`/`submit_record` 都调了，但 `build_obs_object` / `build_windows` 是 `pub`，`submit_windows` 自身不校验。建议改成 `get(i)` + `unwrap_or_default()`，把不变量从"调用者约定"变成"函数内保证"。
2. `src/api/submit.rs::send_record`：`let stop_ms = start_ms + total_time * 1000;` 与 `submit_record` 里的 `checked_mul/checked_add` 两套写法不一致；`total_time` 极大时 debug 构建会 panic（release 回绕）。建议统一用 checked。
3. `src/api/submit.rs::pace_speed_value` / `avg_step_freq`：`meters == 0` 或 `total_time == 0` 时是 f64 除零（→ inf），随后 `as i64` 饱和，不 panic 但会把巨值发给服务端；上游 `flow` 已校验 `dist>0 && dur>0`，属于"守卫在调用方"的隐式依赖。
4. `src/safety.rs::cst_parts`：`FixedOffset::east_opt(8*3600).expect(...)`、`timestamp_millis_opt(0).expect(...)` 都是编译期常量级不可失败，可接受。
5. `src/api/egress.rs::report_at`：`health.consecutive_failures as usize - 1` 依赖 `saturating_add(1)` 先执行（当前成立），若将来有人在 `Failed` 分支前改动就会下溢 panic；建议写成 `saturating_sub(1)`。
6. **Android 专属路径（本机未编译）**：`src/android.rs` 里 `app.internal_data_path().expect(...)`、`create_dir_all(...).expect(...)`、`DATA_DIR.get().expect("Android storage must be initialized before use")`。`safety.rs` 在**作业线程**里就会调 `crate::platform::data_dir()`，若 Android 侧初始化失败/顺序变化，panic 会发生在后台线程（UI 只表现为任务无响应）。建议把这几处 `expect` 换成可上报的错误。
7. `ui/ai.rs::random_time_days_ago` / `ui/run.rs` 里 `Local::now() - Duration::days(n)`：在有夏令时的时区上 `DateTime<Local> - Duration` 可能 panic（中国无 DST，属理论风险）；同时它也是问题 7 的成因之一。

---

## 5. 未覆盖 / 待并行编辑收敛后复查

- `android` cfg 下的编译（缺 NDK clang），以及 `MainActivity.java` 里新增的 display/androidId 取值最终形态。
- `src/api/model.rs`、`src/ui/device.rs` 的并行编辑收敛后，`fingerprint_from_device` 的置位/清除路径需要再走一遍（本次读到的是中间态：保存时按"机型是否变化"清 android_id/pixel，读取时按"两字段是否都非空"置 true，两者组合可能出现"手动改机型 → 清空 → ensure_fingerprint 用 seed 兜底 → from_device=false"的中间状态，行为正确但提示文案会短暂误导）。
- `submission_guard`（本次未在范围内）与 `safety` 的**双重**拦截叠加：`guard.has_pending()` 与 `safety` 都可能在"上次提交结果未知"时拒绝，两者的错误文案容易让用户以为被风控了。

---

## 6. 0.3.9-antiban 复核记录（issue #1–#4）

### 6.1 已修

| 项 | 位置 | 说明 |
|---|---|---|
| 偶发失败：`future_specified_time_is_clamped_and_detectable` | `src/ui/run.rs:1347` | `ensure_plan()` 与测试各读一次时钟，只在同一毫秒内相等；实测 500 次跑 5 次失败（≈1%）。改为区间断言（容差 2ms），与 `specified_time_is_clamped_to_now` 同写法 |
| 共享落盘目录 | `src/ui/violation_tests.rs` | 两处把 `route_fixture` 指向全系统 `%TEMP%`，并行测试会互相看到对方文件；改为每测试一份 uuid 子目录（`FixtureDir` + Drop 清理） |
| GT4 判错后仍重试 | `src/api/login.rs`、`src/api/gt4.rs` | 原来 10003 会重破滑块 3 轮，最坏 6 次 `/load` + 6 次 `/verify`；改为按失败原因分域重试，判错立即停手 |
| 验证码连点 | 新增 `src/api/captcha_guard.rs` | 连续判错触发持久冷却（10 / 30 / 60 分钟），冷却期一个验证码请求都不发 |
| `passtime` 常量 | `src/api/gt4.rs` | 原来固定 1887，改为真实耗时 |
| 延迟位置错误 | `src/api/gt4.rs` | 延迟应在取图之后（看图）而不是之前 |
| `lot_number` 切片 panic | `src/api/gt4.rs` | `lot.get(26..30)` 长度不足会 panic，改为安全取值 |
| `checkGeeUse` 三态压成一种 | `src/api/login.rs` | 问不出来就不进 GT4；`uuid` 三请求复用；兼容 `"true"` / `1` |
| geevalidate 解密失败 fail-open | `src/api/login.rs` | 拿不到业务码时重发一次，不再当成通过（否则会滑到带密码的失败登录） |
| 路网文件无体积上限 | `src/track/generate_road.rs` | 手机可从文件选择器挑任意大文件，加 64 MiB 上限 |
| 构建脚本 PS 5.1 断点 | `android/build.ps1` | `SHA256.HashData` / `Convert.ToHexString` / `ZipArchive` / `Invoke-Checked` 四处（见 CHANGES K.4） |

### 6.2 明确不改（有理由）

- `gee_guard.roe` 八键全 `"3"`：等于「全部未检出异常」，随机化成 1/2 反而是主动声称检测到异常。
- `device_id: ""`、`ep`、`biht`、`em` 全 0：抓包字面量 / 诚实留空，没有证据前不动。
- `complete: true`、`validDistance == totalDis`、`faceCheck = 1`：改了会直接影响记录是否算数。
- 提交与 OBS 的窗口 `id` 口径：无抓包证据。

### 6.3 仍未解决（需要真机或干净出口）

1. 极验在**干净出口**上的真实通过率：本机出口已被降级，且不应用生产 captcha_id 继续刷。
2. Android「选择文件…」的真机交互（本机只验证到编译 + 出包）。
3. TLS 指纹（JA3/密码套件顺序）仍非 Conscrypt/BoringSSL。
