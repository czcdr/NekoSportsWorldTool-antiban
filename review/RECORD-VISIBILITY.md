# 记录页异常可见性（0.9.1）独立验收

- 结论：**needs_revision**（1 条 high + 1 条 low；三条主判据里「三态语义」「零副作用」「回归」通过，**「限速」在失败路径上不成立**）
- 验收人：gate-review（只读）
- 被验树：`E:\rea\upstream` @ `769ad25`（`wip(0.9.1): 记录页三态/列表徽标(t1,t2) + 核查 R1-R4`）
- 基线：`v0.9.0` = `7bd9e2c`（含 lock 修复）
- 时间：2026-10-30 12:43（Asia/Hong_Kong）
- 验收启动时工作树：`git status` 干净（t1/t2 改动已随 `769ad25` 落库）

> 判定口径按 captain 2026-10-29 的更新：must-be-empty = `src/crypto`、`src/track/`、`src/api/submission_guard.rs`；
> `src/api/submit.rs` 仅允许 R3 文案、`flow.rs` 仅 R1+R4 注释、`submission_tests.rs` 仅 R2 新增测试。
> （任务书原文把 `submit.rs` 也列为「必须为空」，已被该口径覆盖。）

---

## 1. 命令（本机实跑，非引用）

| # | 命令 | 结果 |
|---|---|---|
| 1 | `cargo fmt --all -- --check` | **EXIT=0**（全仓无 diff；captain 预期的 `src/ui/mod.rs` 模块顺序问题在当前树上已不存在 —— `record_badges`/`record_status` 按字典序插在 `records` 前，rustfmt 接受） |
| 2 | `cargo check --all-targets` | **EXIT=0**（仅存量 dead_code 警告：lib 9 warnings / lib test 16 warnings） |
| 3 | `cargo test --release --lib` | **EXIT=0**：`test result: ok. 502 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 17.49s` |
| 4 | `cargo test -p route-planner` | **EXIT=0**：`22 passed` / `7 passed` / `0 passed`（三个 target，均 0 failed） |
| 5 | `cargo check --lib --no-default-features` | **EXIT=0**（103 warnings，均存量） |
| 6 | `cargo test --release --lib run_primary_action_is_visible_without_scrolling_on_a_phone` | **EXIT=0**：`1 passed; 0 failed`（502 filtered out） |

完整日志：`E:\rea\gate-pristine\run2.log`（含每条命令的 EXIT 标记与 stderr）。

## 2. 三态语义（独立构造，不复用作者夹具）

在只读副本 `E:\rea\tmp\gate-t3-rev2`（`git archive 769ad25` + 仅追加探针模块，产品代码逐字节未改）里，用**原始 JSON** 走 `classify_detail`（连带验证 `ReasonItem::from_list` 解析），四组数据：

```
running 4 tests
test ui::record_status_probe::probe::probe_passed_case ... ok
test ui::record_status_probe::probe::probe_failed_case_never_red ... ok
test ui::record_status_probe::probe::probe_passed_but_anomalous_case_is_red ... ok
test ui::record_status_probe::probe::probe_missing_reason_list_case ... ok
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 505 filtered out
```

断言要点：

| 数据 | 期望 | 实测 |
|---|---|---|
| 6 条规则全过、无额外项 | `Passed`，`is_anomalous()==false` | 一致 |
| `complete=false`、规则项有 false | `Failed`，label=「未达标」，**`is_anomalous()==false`（不标红）** | 一致 |
| 规则全过 + 「运动数据有问题」 | `PassedButAnomalous`，label=「通过但异常」，**`is_anomalous()==true`（标红）** | 一致 |
| 只有 `statusInfo` 的空壳 | `None`（不猜） | 一致 |
| 只有 `complete=true` | `Passed` | 一致 |
| 非空 `reasonList`、缺 `complete` | `Failed`（服务端没说达标） | 一致 |
| `{"reasonList":[]}`、缺 `complete` | `None`（无判定信息；与「缺 complete」区分开，属有意设计） | 一致 |

界面层「红/不红」另由作者测试断言到像素颜色，抽样核对通过：

- `ui::layout_tests::violation_tests::passed_but_anomalous_detail_is_red_and_visible_on_a_phone`：360×640 与 412×600 下「通过但异常」与「运动数据有问题」均在 `available_rect()` 内且颜色 == `theme::err()`。
- `failed_detail_stays_neutral_colored`：`未达标` 颜色 == `theme::text()` 且 `!= theme::err()`。
- `badge_backfill_marks_anomalous_row_red_without_touching_the_list`：列表徽标「异常」== `theme::err()`，`未达标` == `theme::text_dim()`。
- 三态判定为纯函数（`classify` 无 IO/egui/全局状态），详情页与徽标共用同一入口。

## 3. 限速与缓存

常量与结构（`src/ui/record_badges.rs`，注释均写明理由）：`BADGE_LIMIT=5`、`BADGE_INTERVAL_MS=1000`、`BADGE_TTL_MS=300_000`；`BadgePacer` 只管节奏，`run_backfill` 注入时钟/等待/取数，`BadgeState` 管缓存/在飞/取消/generation 换代。

作者测试（`502 passed` 内）已覆盖：只取最近 5 条、假时钟断言 ≥1 s（**不真 sleep**）、TTL 边界（`TTL-1` 命中 / `TTL` 过期重拉）、单条失败不影响其它条、取消后不再发下一条、旧代回包丢弃、失败只记原因不清缓存。全量 `--lib` 17.49 s，**没有被测试拖慢**（真实 `sleep` 只出现在 `jobs.rs` 的生产闭包里，不在测试路径）。

**但失败路径的连接层不满足「间隔 ≥1 s」——见 F1。**

独立探针（`E:\rea\tmp\gate-t3-rev2\src\ui\record_badges_probe.rs`，仅副本）：

```
test ui::record_badges_probe::probe::failed_round_leaves_every_target_plannable_again ... ok
test ui::record_badges_probe::probe::one_failing_target_is_re_requested_without_any_spacing ... ok
```

两个测试是**对缺陷的正向证明**（断言的是现状）：

1. 5 条目标全部失败 → 逐条 apply 失败回包 + `done` 收尾后，`plan()` 在 0 ms 就把同样 5 条重新吐出来（失败不写缓存、无负缓存/退避）。
2. 只有 1 条且持续失败：模拟 `ensure_record_badges` 三轮「新建 `BadgePacer`（`last_started_ms=None`）→ 立即发 → 失败 → done」，三次请求发生在**同一毫秒**，轮间**没有任何间隔**。

接线（`src/ui/jobs.rs:730` `ensure_record_badges` + `src/ui/records.rs:188` 在 `draw_run_view` 每帧调用）：
守卫只有 `rows.is_empty() / session.is_none() / records_busy / badges.in_flight()`，**不看 `badges.error()`，也没有任何冷却**；`ui/mod.rs:186` 每 200 ms 轮询重绘。`done` 消息清掉 `in_flight` 后，下一帧即重新规划 → 失败的目标被无限重拉。

## 4. 零副作用

```
$ git diff v0.9.0 -- src/crypto src/track/ src/api/submission_guard.rs
（无输出）
$ git diff v0.9.0 --stat -- src/api/submit.rs src/api/flow.rs src/api/submission_tests.rs
 src/api/flow.rs             | 15 ++++++++++++---
 src/api/submission_tests.rs | 41 +++++++++++++++++++++++++++++++++++++++++
 src/api/submit.rs           |  4 +++-
```

逐 hunk 核对，**未超出授权**：

- `submit.rs`：只改 Unknown 分支文案 `还没到自动对账时间（约 {} 秒后自动核对记录列表）` → `还没到对账时间（约 {} 秒后再试一次会自动核对记录列表）` + 3 行注释；无逻辑/无请求变化。（比 captain 描述多删了「自动」二字，语义更准确，仍属 R3 范围。）
- `flow.rs`：`reject_below_min_distance(...)?` → `if let Err(e) = ... { return Err(拼 pending_floor_warning) }`（R1）；`THIN_SHAPE_AXIS_RATIO` 测试注释 0.40→0.45（R4）。其余分支未动。
- `submission_tests.rs`：仅新增 `receipt_file_on_disk_records_the_run_distance_not_the_threshold`，无删改既有断言。
- `src/api/records.rs`（t1 允许的解析面）：只把 `reasonList` 解析抽成 `ReasonItem::from_json/from_list` 并保留 `ruleId`/`kindHint`；`fetch_detail` 的请求构造、`submit`/`recover` 链路未动（diff 中新增行不含任何请求/URL/body 相关代码）。
- 新增请求仅 `fetch_detail`（B 档设计所需），提交/生成链路无新增调用点。

**R2 不是假锁（在 `769ad25` 上重做反向验证）**：副本中把 `submit.rs:166` 改成 `receipt_target_distance(p.min_distance as f64)` →

```
test api::submit::submission_tests::receipt_file_on_disk_records_the_run_distance_not_the_threshold ... FAILED
thread ... panicked at src\api\submission_tests.rs:251:5:
回执应记实跑距离 1000：{"state":"pending",...,"target_dist":2000}
test result: FAILED. 0 passed; 1 failed; ...
```

它断言落盘回执内容（`target_dist`），不是辅助函数；改回即绿。唯一小偏差：先炸的是 `:251`「回执应记实跑距离」，与 captain 记的「不得记门槛 2000」是同一测试的相邻两条断言，不影响力。

## 5. 回归

`ui::layout_tests::run_primary_action_is_visible_without_scrolling_on_a_phone` → `ok`（1 passed）。详情页在 360×640 下把结论与异常项放在字段区之前，作者测试另覆盖 412×600。

## 6. Findings

### F1（high）失败路径没有退避：持续失败会被无限重拉，单条失败时轮间间隔为 0

- 位置：`src/ui/jobs.rs:730`（`ensure_record_badges` 守卫）/ `src/ui/records.rs:188`（每帧调用）/ `src/ui/record_badges.rs:325`（`plan` 只按缓存命中过滤）/ `src/ui/record_badges.rs:63`（`BadgePacer::new` 重置 `last_started_ms`）
- 问题：失败不写缓存、不写冷却，`ensure_record_badges` 又只挡 `in_flight`；一轮收尾后的下一帧（≤200 ms）立刻重新规划同一批目标。于是：
  1. 部分/单条持续失败 → 每轮都重打同一 rrid，**永不停止**（浏览记录页期间持续 1 req/s）；
  2. 只剩 1 条失败时每轮都是「立即发 1 条」，`BadgePacer` 每轮重置 → 轮间间隔 **0 ms**，实际请求频率受 200 ms 轮询上限驱动（≈5 req/s），比设计值高 5 倍。
  这与本档的硬约束（详情接口计数、"宁可慢 4 秒，也不要一次并发 5 条"）直接冲突，等于把「一次拉 9 条」换成「一次失败后无限重拉」。
  现有测试没抓到，是因为没有一个用例检查「一轮失败后再 `plan()`」。
- 证据：第 3 节两个探针；`git diff` 未见任何失败冷却逻辑。
- requiredFix：为失败加退避/负缓存（例如 `BadgeState` 记 `failed_until_ms`，`plan` 在冷却窗口内跳过；冷却可取 5 分钟或指数退避），并把节拍器提升为跨轮共享（在 `App` 里持有 `last_started_ms`，或把 `BADGE_INTERVAL_MS` 的检查放在 `ensure_record_badges` 起轮处），保证**任意两次详情请求**间隔 ≥1 s；连续失败 N 次后停止自动重试，只在用户显式刷新/重进页面时重试。补两条测试：一轮全失败后 `plan()` 为空（冷却期内）、单条失败跨轮请求间隔 ≥1 s。

### F2（low）文案退路会把「否定式」措辞判成异常（假红风险）

- 位置：`src/ui/record_status.rs:99`（`ANOMALY_KEYWORDS` 含裸词「异常」）/ `:159`（`kind_from_text`）
- 问题：真实 payload 没有规则标识字段，退路就是主路径；若服务端某天给出「运动数据无异常」「未见异常」这类**通过**措辞，会命中「异常」→ `NonRule` → 规则全过却被标红，正是本档最怕的假红。
- 证据：`kind_from_text` 仅做 `contains`，无否定词处理；现有测试只覆盖「运动数据有问题」「APP参数数据异常201」。
- requiredFix：退路里遇到否定前缀（无 / 未 / 没有 / 未见）先剔除或直接判 `Unclassified`；或把「异常」从严（要求同时 `complete==false`）。补一条「运动数据无异常 + complete=true → Passed」的测试。

## 7. 遗留（不构成本次判定）

- 真机未验（360×640/412×600 为 egui 离屏布局断言，非真机截图）。
- 服务端是否因「异常」标签二次处罚未知；本次只验本地展示，无新增提交/生成请求。
- `cargo fmt` 当前全绿，captain 计划中的「统一跑 fmt」不再需要为 `mod.rs` 顺序做修正。
