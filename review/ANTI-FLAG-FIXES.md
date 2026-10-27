# 防异常自洽性修复（t13）—— 逐条记录

- 依据：`review/ANTI-FLAG-AUDIT.md`（t11 只读审计，14 条）。**只做审计点名的必修 8 条**（A1/A3/A5/A7/A4/A6/A8/A13），
  审计「建议不动」的 A2/A9/A10/A11/A12 与 `mac`/`imei` 常量**一律没碰**。
- 基线：`antiban-review`，每条一个独立提交（便于单条回滚），未 push / 未打 tag / 未 bump 版本。
- 验证：`cargo check --lib --offline`、`cargo test --release --lib --offline`、`cargo fmt --check`、`git diff --check` 全绿。

| # | 提交 | 改了什么 | 新测试 | 对「能否算数」的影响 | 回滚 |
|---|---|---|---|---|---|
| **A1**（high） | `953f2f6` | `api/submit.rs::submit_record` 最前面加平台准入：`identity.platform != "android"` 直接 `Err`（文案「当前设备身份为 iOS，官方客户端只有 Android；请到设备页切换为 Android 身份后再提交」）。**在写回执、发请求之前**返回，不留 pending。`ui/device.rs` 的 iOS 单选加风险说明 + 选中 iOS 时一行黄字警告。测试夹具改显式 Android 身份。 | `api::submit::submission_tests::ios_identity_is_rejected_before_any_request_or_receipt`；`api::flow::fake_run_tests::ios_identity_is_refused_before_the_submit_request`（原「两条身份」用例改写成「提交请求一条都不发、整链无 Android 报文」） | **不会**让已通过的记录变成不计入。代价：显式选了 iOS 身份的用户**无法提交**（切回 Android 即可）——这是当前设计下「要么整链跟随 iOS、要么拒绝」的必然；不修则是同一条 token 上登录 iOS / 提交 Android 的硬矛盾（更容易被判异常）。 | `git revert 953f2f6` |
| **A3**（medium） | `a4ef64d` | `track/wire.rs::build_windows` 的 `stepsPerTenSec.*Diff` 不再写死 `0.0/0.0/1000.0`，改由该窗内相邻采样点的**步数差分**算均值/最大/最小（新增 `window_step_diffs` + `step_diff_stats`）；空窗/单点用该窗自己的步数与会话推一个自洽量级，保证 `minDiff ≤ avgDiff ≤ maxDiff` 且跨窗不恒定。提交与 OBS 共用同一个构造器，两侧自动同源。 | `track::wire::window_and_position_tests::step_diff_fields_are_derived_from_each_window`（不同窗 `avgDiff` 不相同、不再是 `(0,0,1000)`、`min ≤ avg ≤ max`；空 `locations` 也不回落） | **不影响**：只动窗内附加字段，`stepsNum`/距离/步数/达标判定都没变。 | `git revert a4ef64d` |
| **A5**（medium） | `5e40ba6` | `track/generate_road.rs`：Road 模式海拔初值与 OU 回归目标从写死 `82.0` 改成 `base_alt = rng.uniform(20.0, 260.0)`（与 `generator.rs` 的 Legacy 同款），整条轨迹围绕它做 OU 回归（连续、不逐点跳）。 | `track::generate_road::preview_tests::road_altitude_is_sampled_per_run_not_pinned_to_82m`（两个 seed 顶点海拔不同、都在 20–260、相邻点不跳） | **不影响**：只改 `bdA`/`totalAscent`（爬升展示），距离/时长/步数/判定都不变。 | `git revert 5e40ba6` |
| **A7**（medium） | `5d820d9` | `api/flow.rs`：新增 `second_precision_start`，把「整分」起点补上真随机的秒与毫秒（< 60s，仍落在用户选的那一分钟内，不跨日、不改时长、不动 10s 窗口径），替换原来 `+ (rand % 5) * 1000` 的半吊子偏移；值在 `reserve_run` 之前落定，`flag`/`beginTime`/`gainTime` 全部由同一个 `start_ms` 派生。随机模式的时刻本来就是均匀抽样（带毫秒），原样保留。 | `api::flow::verification_tests::specified_start_gets_real_seconds_and_millis`（同一分钟内、秒与毫秒不再恒 0、不同抽样不同时刻、非整分原样返回） | **不影响**：只动亚秒位；记录所属日、`totalTime`、窗口数、达标判定都不变。 | `git revert 5d820d9` |
| **A4**（medium） | `c0e561f` | `api/flow.rs`：轨迹生成后加**只读**围栏校验 `count_points_outside_fences`（围栏为 BD09，与 `gLat/gLng` 同系），出界就 `fence_warning` 出「本路线有 N 个采样点在围栏外，可能被判无效」，写进日志 + `RunOutcome.warnings`；**不改轨迹、不拦提交**。Legacy/Manual 退化路径都会检查。 | `api::flow::verification_tests::fence_check_counts_outside_samples_and_warns_without_blocking`（无围栏 0 且不告警；远围栏 = 全部点；包住 = 0；文案带数量/问题/后果；校验只读） | **不影响**：纯新增可见性，不改变轨迹形状与提交。最坏情况只是多一句警告。 | `git revert c0e561f` |
| **A6**（medium） | `4064331` | `track/generate_road.rs`：有围栏时不再整条丢弃 AR(1) GPS 抖动。候选点必须自身在围栏内、**且**它到上一个已发点的线段也在围栏内（凹多边形两点在内连线仍可能出界）；不满足就把偏移折半重试（1.0/0.5/0.25），仍不行回退中心线点 —— 宁可贴中心线也不出界。 | `track::generate_road::preview_tests::fenced_road_keeps_lateral_gps_jitter`（笔直走廊：有围栏时纬度标准差 > 0.3m 且 < 15m，所有点仍在围栏内 —— 旧的「贴中心线」实现会被这条判据抓住） | **不影响**：距离由 `d_step` 累加，与坐标无关；总距离/步数/判定不变。`long_distance_repeats_inside_concave_campus_and_keeps_statistics` 仍通过（每段仍在围栏内）。 | `git revert 4064331` |
| **A8**（medium） | `c0e561f`（与 A4 同一提交，共享 `RunOutcome.warnings` 通道） | `api/flow.rs`：`policy.face_required == Some(true)` 时把「已提交，但校方要求人脸核验而未提供核验数据，可能不计入有效次数」写进 `RunOutcome.warnings`；`RunOutcome::summary()` 把 warnings 追加在结论后面 —— UI 的结果弹窗（`结论：…`）与 CLI 的 `结论：…` 都自动带出。**不拒绝、不改 faceCheck**。 | `api::flow::verification_tests::face_required_warning_is_visible_in_the_shared_summary`（无警告时不含「人脸核验」；push 后结论含提示与后果、原结论不被替换；退出码/机器码不变） | **不影响**：提交行为完全不变，只是结论里多一句如实说明（原本只在日志里）。 | `git revert c0e561f` |
| **A13**（low） | `728ef35` | `safety.rs`：`SafetyPolicy` 新增 `min_submit_interval_secs`（默认 180，`0` = 关闭这道闸）；`State` 记 `last_submit_ms`；`reserve_run_at` 在每日配额检查**之后**判间隔（配额是更具体的原因），超频拒绝并说清「还差多少秒 / 怎么调」。**失败不启动间隔**：基准在占位时写入，`Drop`（未 commit）还原成上一次成功的时刻 —— 网络失败后的立即重试不受影响。只动这一个闸，深夜/每日/AI/时区/回补逻辑未改。 | `safety::tests::submissions_are_throttled_by_a_minimum_interval`（60s 后被拒且含「还差 120s」与配置键；失败的一次不启动间隔；180s 后放行；配 0 关闭） | **不影响**已提交记录的有效性；**会减少可提交次数**（默认 3 分钟内不能连提两条）——这正是审计要的「把批量从形态上消掉」。需要连提的用户把 `safety.min_submit_interval_secs` 调小或设 `safety.enabled=false`。 | `git revert 728ef35` |

## 顺带修的测试夹具（不是行为改动）

- `api/submission_tests.rs::client()` 与 `api/fake_run_tests.rs` 的 iOS 用例：显式使用 Android 身份 / 改为断言「拒绝」。
- `safety.rs` 里 4 条「同一分钟连续占位」的用例（`backfill_counts_per_record_day_but_caps_one_sitting`、
  `same_record_day_still_capped_by_daily_runs`、`broken_state_recovers_from_backup`、`state_keeps_only_recent_days`）
  显式 `min_submit_interval_secs: 0`，让它们继续只测自己的计数/恢复语义；间隔闸由
  `submissions_are_throttled_by_a_minimum_interval` 单独覆盖。

## 明确没做（审计「建议不动」，避免把伪造做得更明显或影响成绩判定）

A2（`validDis == totalDis`）、A9（10s 窗严格对齐）、A10（满圈 `distance == 1000.0`）、
A11（`dtr/lat/lng/stepDistance` 常量）、A12（窗 `id` 口径）、`mac_address`/`imei` 的「常量」
（那是 Android 6+/10+ 的官方固定值，改了反而露馅）。

## 回滚整批

```powershell
cd E:\rea\upstream
git log --oneline -8        # 找 A1 之前那个提交（t9 的 ab81d28）
git reset --hard <A1 之前的提交>
```

单条回滚用上表的 `git revert <hash>`（每条都独立提交，互不依赖；A4+A8 共享一个提交）。
