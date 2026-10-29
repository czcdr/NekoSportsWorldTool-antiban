# 0.9.0 独立验收（t3 / gate-review，只读）

- 验收对象：工作区 `E:\rea\upstream`（`v0.8.0-1-g3de1214` + 0.9.0 未提交改动）
- 范围：**提交幂等 WAL（t1）**、**轨迹真实性 + 形态（t2）**、**参数自洽 C1/C2/C3/C10（t4）**
- 方式：全部结论由本次会话亲自执行得出；真实性/形态/经度/海拔另用**独立探针**（见 §4 开头的做法与 §9 的复现命令）复算，不引用实现者贴出的数字
- 结论：**needs_revision**（7 项里 1–5 通过；第 6 项「没有引入新的可识别特征」未能确认，见 §7）
- **复审（t11，收口）：F1 / F2 / F3 全部关闭，fmt 后无回归 → 终态结论 pass（详见 §11）**

> 不在本轮判定内（工作区里同时存在、且与三项改动无关的改动）：`src/ui/*`（t5 的 recover 面板 + 另一条 UI 抽检线）、`src/ui/tiles.rs`/`osm.rs`/`about.rs`/`device.rs` 等。这些**我没有审**，不影响本节结论，也不代表它们被放行。`src/api/recover.rs` 相对 v0.8.0 无改动（`git diff` 为空），与 t5 的声明一致。

---

## 1. 门禁命令（本人执行，原样输出）

工作目录 `E:\rea\upstream`，`rustc 1.99.1` / `cargo 1.99.1`，Windows。

```
$ cargo fmt --all -- --check
（无任何输出）
fmt exit=0

$ cargo check --all-targets
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.85s
    warning: `nekosportsworldtool` (lib) generated 9 warnings
    warning: `nekosportsworldtool` (lib test) generated 15 warnings (8 duplicates)
exit=0

$ cargo test --release --lib
    Running unittests src\lib.rs (target\release\deps\nekosportsworldtool-...exe)
test result: ok. 459 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 19.96s
exit=0
（ignored 的一条是 api::gt4::tests::live_gt4_probe —— 需要真实极验网络，属既有 ignore）

$ cargo test -p route-planner
running 22 tests
test result: ok. 22 passed; 0 failed; 0 ignored; ...
running 7 tests
test result: ok. 7 passed; 0 failed; 0 ignored; ...
running 0 tests
test result: ok. 0 passed; 0 failed; 0 ignored; ...
exit=0

$ cargo check --lib --no-default-features
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.78s
    warning: `nekosportsworldtool` (lib) generated 103 warnings
exit=0
```

警告全部是既有的 `dead_code`（`tile_road` 未被 no-gui 路径使用等），逐条比对**没有一条来自 0.9.0 新增代码**。

判定：**门禁通过**。

---

## 2. 提交载荷零变化

### 2.1 受保护文件逐字节相同

```
$ git diff v0.8.0 -- src/crypto src/api/obs.rs src/track/wire.rs
（0 字节，exit 0）
```

`src/api/client.rs`、`src/http.rs`、`src/tls.rs`、`src/api/login.rs`、`src/api/fingerprint.rs` 均不在 `git status` 的改动列表里 → 头部、加密、上报链路未动。

### 2.2 `submit.rs` 逐 hunk 判断

`git diff v0.8.0 -- src/api/submit.rs` 共 5 个 hunk：

| hunk | 内容 | 判定 |
|---|---|---|
| 1 | 新增 `UNCOMPLETE_REASON_DISTANCE` 常量与 `completion_flag()` | 只增函数，不碰报文 |
| 2 | `submit_record` 改成「先只读对账 → 闸门 → 落 Pending → 发请求」，Unknown 分支先落 Unknown 再对账 | WAL 本地逻辑（t1） |
| 3 | `send_record` 里 `complete` / `unCompleteReason` 由常量改成取值 | **允许**：任务明确允许改取值 |
| 4 | `totalDis` 的值表达式提成局部变量 `total_dis_i` | 值不变（`round_to(total_dis,0) as i64`） |
| 5 | 新增 `completion_flag_tracks_the_school_threshold` 测试 | 测试 |

**字段名 / 字段顺序 / 新增 / 删除**：用脚本把 `body.insert("…")` 序列（含跨行写法）抽出来对比：

```
v0.8.0 (43): allLocJson,sportType,policy,totalTime,startTime,stopTime,getPrize,status,uuid,uid,
             selectedUnid,selRunTime,selDistance,totalDis,speed,validDis,validTime,complete,
             unCompleteReason,calorie,totalSteps,avgStepFreq,useMobilityTools,faceCheck,totalAscent,
             avgPower,speedPerTenSec,stepsPerTenSec,isUpload,more,latitude,longitude,maxRunTime,
             minSteps,fivePointJson,errorCode,geeToken,unauthorized,themeId,goalId,address,
             signature,originalSign
current  (43): （同上，逐项一致）
identical sequence: True
only in old: （空）   only in new: （空）
```

判定：**载荷零变化成立**（唯一变化是 `complete`/`unCompleteReason` 的取值，符合 C3 的授权）。

### 2.3 其余 `src/api/**` 的改动

- `flow.rs`：C1（`reject_future_stop`）、C3（`reject_below_min_distance`）、C10（海拔基准日志 + 新包装函数）、对账调用点（`reconcile_if_needed` / `guard.resolve`）；均为本地逻辑或提前拦截，无报文改动。
- `model.rs`：新增 `Config.school_base_altitude`（`#[serde(default)]`，本地配置项，不进请求）。
- `submission_guard.rs` / `submission_tests.rs`：本地回执与其测试。
- 结论：**没有任何 `src/api/**` 改动把新字段送进请求**（另见 §3.4）。

---

## 3. WAL 复核

### 3.1 状态机与顺序（读码）

`src/api/submission_guard.rs` 的 `submit_record` 顺序固定为
`reconcile_if_needed → guard.begin(=check+落 Pending) → send_record`；`Unknown` 分支是
`guard.unknown()`（先落盘）→ `reconcile_if_needed()`（只读）→ 依结果给文案。顺序正确：崩溃点最多留下 Pending，不会出现「请求已发出但回执什么都没有」。

### 3.2 五项契约逐条

| 契约 | 证据 |
|---|---|
| Pending 拒提 | `pending_receipt_blocks_a_retry` ok：窗口内 `needs_reconcile()` 为假、`resolve(&[])` 不放行、重启后仍拒提；30 分钟后用列表核销（未命中→Abandoned）才放行 |
| Unknown 对账 | `unknown_receipt_closes_on_a_matching_record` ok（命中→Committed→继续挡同一时间窗）、`unknown_receipt_without_a_record_allows_the_retry` ok（确认缺失→Abandoned→放行）、`ambiguous_lookup_stays_blocked` ok（命中多条不同 rrid→保持未决）、`record_without_uuid_is_matched_by_start_and_distance` ok（uuid 缺失时退化匹配） |
| 回执损坏 fail-closed | `corrupt_or_torn_receipt_fails_closed_and_is_not_overwritten` ok：末尾半行 / 非法 JSON / 未知 state 一律 `Err`，且断言**原文件字节未变** |
| 并发写不丢 | `concurrent_writers_are_serialised_and_lose_nothing` ok（8 线程 × 2000 次抢锁，最后 `pending_count()==8`）；`exclusive_lock_and_account_isolation` ok（同 (uid,unid) 第二次 open 失败，不同账号/学校互不影响） |
| 回执不进请求 | 见 §3.4 |

端到端（**真实 loopback HTTP**，断言请求序列）：

- `submission_tests::lost_response_reconciles_uuid_without_reposting` ok → 请求序列 `[RECORD_PATH, RECORDS_PATH]`，第二次提交被「重复提交」挡住；
- `submission_tests::timeout_without_a_matching_record_allows_one_retry` ok → 序列 `[RECORD_PATH, RECORDS_PATH, RECORD_PATH]`（对账在重提前，且只出现一次）；
- `submission_tests::failed_reconciliation_keeps_blocking_the_retry` ok → 列表拉不到时序列只有 `[RECORD_PATH]`，**没有**第二次提交；
- `successful_post_then_same_plan_only_sends_once` ok、`explicit_rejection_can_be_retried_by_user_but_not_automatically` ok、`malformed_success_is_recorded_as_unknown` ok、`unauthorized_post_invalidates_session_and_does_not_send_again` ok、`ios_identity_is_rejected_before_any_request_or_receipt` ok。

### 3.3 两条既有不变量

- 「提交后清计划」：`ui/msgs.rs:395` 的 `clear_plan_cache()`（跑完才清）仍在；`run_done_clears_the_plan_cache_but_an_in_flight_run_keeps_it` ok。
- 「预览/提交同方案」：`preview_and_generation_share_required_points_options_and_seed` ok；`RunPlan{seed,start_ms,dist,pace,dur}` 结构未变，`location.rs` 相对 v0.8.0 **零改动**。

### 3.4 回执不进请求（本人复核）

- 报文 43 个 key 与 v0.8.0 完全一致（§2.2）；
- `plan_fp` / `at_ms` / `target_dist` 只出现在：回执 JSONL、`unlock` 清单、日志、错误文案（`grep -n 'plan_fp|submission_receipts|RECONCILE_WINDOW'` 在 `submission_guard.rs` 之外只有 `submit.rs` 的拼接文案与 `flow.rs`/`cli` 的 guard 调用）；
- `submission_guard` 未被 `client.rs` / `wire.rs` / `obs.rs` / `crypto` 引用 → 不参与签名、不参与请求体。

### 3.5 WAL 的残余风险（已知、非缺陷，但要写清）

1. **窗口 30 分钟是时间假设**：若服务端记录列表延迟 > 30 min（Pending 过期→对账未命中→Abandoned→放行），仍可能重复。模块文档已写明，`unlock` 文案也提示「未查到不代表未入账」。
2. **`unlock --yes` 无年龄限制**：人工释放后可立刻重提，完全依赖人先去看记录页。这是设计出口，非 bug，但它是这套保护里唯一「一键绕过」的路径。
3. **记录页刷新遇到提交在飞时**：`Guard::open` 会失败（文件锁），`run_records` 只记日志、不阻断列表刷新（`flow.rs:874-876`），处理正确。

---

## 4. 真实性（停走 / 野点 / radius / 形态）—— 独立探针实测

**探针**：把工作区**复制**到 `E:\rea\tmp\t3probe`（不含 `target/.git`），在副本里加一个 `#[cfg(test)] mod probe_t3`，调用 crate 内部 API（`build_tuned_realism` / `sample_course` / `noise::*`）自己算指标。**被审工作区一行未改**（`git status` 前后都是 31 项，无新增文件）。构建复用 `E:\rea\upstream\target`（只是编译产物）。

参数：经典模式 4 个打卡点（39.9077–39.9097N, 116.4126–116.4146E），`dist=2000 m / dur=800 s / seed=7`，`Realism::on()` 与 `Realism::legacy()` 对照。

### 4.1 停走窗口

| 指标 | 0.8.0（legacy） | 0.9.0（on） |
|---|---|---|
| 点数 / 总距离 / 时长 | 164 / 2000.000 / 800 | 164 / 2000.000 / 800（不变） |
| `speed==0 且 avgSpeed==0` 的点 | 0 | **2（1.22%）**，idx `[7,8]` |
| 这些点的 `totalDis`/`steps` 是否冻结 | — | **是**（与前一点完全相等） |
| `speed==0` 的点（任何含义） | 31（18.9%，avgSpeed>0） | 3（其中 2 个是停走点） |
| 总步数 | 1916 | 1892（停走不记步，差 24） |

**占比 1.22% 落在 1–3% 区间内**；停走点不在 index 0/1/2 与末点（哨兵），也不在异常点上。

### 4.2 野点（用消融法定位，不靠猜）

`Realism{outliers:false, ..on}` 与 `on` 逐点比对坐标差 → 得到**精确的野点位置与幅度**：

```
moved points: idx138 22.1 m @100°, idx139 22.1 m @100°, idx157 17.2 m @175°, idx158 17.2 m @175°
```

- **2 次事件 × 每 2 个连续点**，同一事件内位移向量完全相同（相邻点之间不会二次跳变）；
- 幅度 **22.1 m / 17.2 m**，落在 10–30 m；
- 4 个点的 `radius` 分别是 21.25 / 13.83 / 11.74 / 10.69（全部 ≥10，按异常点类取）；
- 距最近打卡点 77.5 / 65.1 / 63.6 / 54.4 m，**不在**起点（idx<3）、终点（idx≥n-2）、哨兵、必经点 25 m 内。

### 4.3 radius：非常量、右偏

| 指标 | 0.8.0 | 0.9.0 |
|---|---|---|
| 全局（164 点） | min 1.41 / median 2.15 / max 9.43 | min 1.64（哨兵）/ p25 5.71 / **median 7.66** / p90 14.10 / max 24.66 / skew **+1.22** |
| 不同取值个数 | 窄带 | **135 / 164** |
| 普通点（非哨兵/非异常） | 1.4–2.4 | min 4.00 / median 7.10 / max 21.25（含野点按异常类取的上尾） |

判定：**radius 不再是常量/窄带，右偏成立**（与实现者自述一致）。

### 4.4 形态：重合段 / 往返 / 折返角 / 自交

闭环课程（4 角环，环长 **827.8 m**，`dist=2000 m` ≈ 2.57 圈）：

| 样本 | 重合@0.5 m | 重合@1 m | 圈间分离 | 最小折返角 | 往返段最长 | 真交叉 |
|---|---|---|---|---|---|---|
| 课程 flat（0.8.0 语义） | **1.000** | 1.000 | 0.03 m | 169.1° | 0 m | 0 |
| 课程 warp（0.9.0） | **0.150** | 0.316 | **3.33 m** | 169.2° | 0 m | 0 |
| 生成轨迹 legacy | 0.301 | 0.595 | 1.33 m | 32.3°（min_seg 5 m） | 15 m | **18** |
| 生成轨迹 realism | **0.104** | 0.178 | **3.65 m** | 见下 | **2 m** | **3** |

- 「真交叉」我单独实现（线段真穿越 + 夹角 sin>0.15 排除平行/重合），不用实现者的度量；flat 课程因为整圈重跑，按这个口径是 0（重合不算穿越），warp 课程同样 0。
- 生成轨迹 realism 的 0.1° 折返角出现在 **idx 140**，即 22 m 野点事件的**出口**：野点本身是「跳出去再跳回来」的 V 形（真人 GPS 野点就是这个形状），不是路线设计出的针状折返。把它排除后，课程层面的最小折返角是 169°。
- 结论：**「同一段路原样重跑」的机器形态被消掉了**（1.000→0.150 / 0.03 m→3.33 m；生成轨迹 0.301→0.104、往返 15 m→2 m、交叉 18→3）。
- 残差解释：@0.5 m 仍有 10–15% 段重合、3 处真交叉，来源是「逐圈偏移在个别弧长处恰好相等」的切点（`WarpField` 是 3 个互质周期的正弦叠加，`offset(s+L)-offset(s)` 必然有零点）。这是「同一个人绕圈，某些点两圈会压在一起」的正常形态，不是整圈复制。

---

## 5. 参数自洽（C1 / C2 / C3 / C10）

### C1 未来 stopTime

- 代码路径：`ui/run.rs::pick_time()` 两种模式都 `.min(latest_start_ms(dur))`；`flow::run_full_flow_cancellable` 在 `params.dur = effective_duration(...)` **之后**再 `reject_future_stop(start_ms, params.dur, now)?`（顺序正确，不会用旧时长校验）。
- 测试（本人跑）：`api::flow::audit_param_tests::future_stop_time_is_rejected` ok、`check_uses_the_effective_duration` ok（后者构造「旧时长刚好放行、新时长必须拒绝」的反例，证明校验位置有效）。
- 复现结论：**「现在 12:05、填 12:00、用时 30 分钟」被拒**，文案含结束时刻与可操作出口；溢出走 `checked_*` 返回 `Err`，不 panic。

### C2 经度常量（三纬度探针，我自己算）

参考值用 **WGS84 椭球**每度经度 `π/180·N(φ)·cosφ`（`a=6378137`，`e²=0.00669437999014`）：

| 纬度 | 1° 经度真值 | `met_per_deg_lng` | 偏差 | 旧常量 86600 的偏差 | 100 m 圆（旧→新） |
|---|---|---|---|---|---|
| 39.9°N | 85 518.3 m | 85 400.8 m | **−0.137%** | +1.265% | −0.72% → **−0.03%** |
| 23.1°N | 102 446.9 m | 102 394.5 m | **−0.051%** | −15.468% | +9.27% → **−0.03%** |
| 45.8°N | 77 741.9 m | 77 608.4 m | **−0.172%** | +11.394% | −5.14% → **−0.03%** |

- 与实现者自述的「−0.72%/+9.27%/−5.14% → −0.03%」**完全对上**（`cargo test --release --lib longitude_scale_tracks_latitude -- --nocapture` 也是这三行）。
- 更严格的「每度经度」口径下最大偏差 −0.17%，**仍在 ±1% 内**；剩余偏差来自纬度方向仍用 111 132（与 111 320 差 0.17%），这是被刻意保留的协议口径，可接受。

### C3 `complete` 与门槛绑定

```
totalDis=2000 minDistance=2000 → (true, 0)
totalDis=2001 minDistance=2000 → (true, 0)
totalDis=1200 minDistance=2000 → (false, 1)
totalDis=1200 minDistance=0    → (true, 0)
totalDis=1200 minDistance=-1   → (true, 0)
```

- `send_record` 用 `completion_flag(total_dis_i, p.min_distance)` 填 `complete`/`unCompleteReason`；未达标时日志显式告警。
- **没有降低门槛凑达标**：`min_distance` 只有 `pol.min_distance` 一个来源（`flow.rs:753`），全仓无「下调/取小」的写法；并且 `flow` 在 `fetch_policy` 之后立刻 `reject_below_min_distance`，计划距离 < 学校门槛时**根本不发请求**。
- 边角：`min_distance<=0`（策略没给）按完成处理，不虚构未达标 —— 与文案一致。

### C10 海拔基准

| seed | 生成器内部 `bdA` 均值（20–260 抽样） | `build_tuned_with_altitude(base=1050)` 后 |
|---|---|---|
| 0 | 254.4 | **1050.0** |
| 1 | 256.0 | 1050.0 |
| 2 | 105.3 | 1050.0 |
| 3 | 238.0 | 1050.0 |
| 4 | 22.5 | 1050.0 |
| 5 | 225.4 | 1050.0 |

- 起伏保留：`Δrange = 0.000 m`，`Δascent = 0.000 m`（平移不压平曲线，与 `override_bd_a` 的「每点同值 → ascent=0」不同）。
- 平移后**均值 = 基准 ± 0.005 m**（逐点 2 位小数四舍五入的残差；实测 base=82 时 5 个 seed 为 `82.001 / 81.996 / 81.996 / 82.001 / 82.001`，base=1050 同量级）。即「基准」是精确可控的，不是一个抽样值。
- 未配置时回落 **82.0 m**（`BaseAltitudeOrigin::Default`），不再是 20–260 抽样。
- 判定：**C10 的目标（不再用 20–260 均匀抽样）成立**；残余问题见 §7 F3。

---

## 6. 风控面：本次改动**没有**修掉的既有特征

顺手确认（不是本轮引入、也不要求本轮改动）：时间间隔仍是「80% 5 s + 20% 1–8 s」、`totalDis/validDis` 仍按 4 位小数累积、`count/state/bdG` 的加权分布未动 —— 这些都是 0.8.0 的既有形态，本轮没有加重新特征（除了 §7 的两条）。

---

## 7. Findings

### F1（medium，必须修）`speed` 与 `avgSpeed` 在 ~20% 点位上**精确相等**，形成双峰残差 —— **已关闭（§11.2）**

- 位置：`src/track/generator.rs:435-451`、`src/track/generate_road.rs:783-799`
- 事实（本人实测）：`speed == round(avgSpeed*3.6, 4)` 的点 **34/164 = 20.7%**（其中非零 31 个），残差恰为 `0`；其余 79% 是 `kmh + gauss` 的连续噪声。
- 来源：0.8.0 的「20% 点 `speed=0`」尖峰分支被改成「`speed=kmh`（去掉噪声）」，为了**不改变 RNG 消耗**选择「不抽 gauss 直接输出精确值」。旧问题（18.9% 零速）确实消掉了，但换成了一个可被单条记录检验的等式子集：服务端只要算 `speed/avgSpeed` 就会发现 1/5 的点比值**恰好** 3.6，其余点带 ±8% 噪声。
- requiredFix：在 `spike` 分支用**不消耗 RNG** 的确定性微扰替代精确值，例如
  `let dither = 1.0 + 0.006 * ((i as u64).wrapping_mul(2654435761) % 2001) as f64 / 1000.0 - 0.006;` 再 `round_to((kmh * dither).max(0.0), 4)`。要求：残差不再出现精确 0；主 `Rng` 抽样次数/顺序不变（`Realism::legacy()` 坐标哈希必须仍是 `0x3fa6…f569` / `0x55b5…062b`）。

### F2（medium，必须修）`radius` 硬截断在边界堆出**原子值** —— **已关闭（§11.3）**

- 位置：`src/track/noise.rs:154-161`（`radius_quantile` 的 `.clamp(lo, hi)`）、`noise.rs:118-124`（各类边界）
- 事实（本人实测，153 个非哨兵点）：`4.00` **×9**、`15.00` ×3、`20.00` ×4、`10.00` ×2。理论值也对得上：普通类 8.1% 落在 4.00、2.9% 落在 15.00，异常类 18.7% 落在 10.00。
- 影响：0.8.0 的经典模式是 `uniform(1.4,2.4)`（连续、无原子），0.9.0 引入 `clamp` 后出现「恰好等于下界」的堆积 —— 这正是验收项 6 点名的「分布边界突变」；且同类边界在道路模式原本就有（3.00/15.00），本轮把它扩展到了所有模式和四个边界。
- requiredFix：把硬 `clamp` 换成**软膝**映射，例如 `lo + (hi-lo)*smoothstep`（或对越界分位做 `1-exp(-x)` 压缩），保证仍是一次抽样、仍在 `[lo,hi]` 内、但**不产生等于边界的取值**；同步更新 `noise::tests::radius_is_right_skewed_and_inside_class_bounds`（现在只断言「在区间内」，没有断言「边界处无原子」），建议加一条 `边界值占比 < 1%` 的断言。

### F3（low，建议修）未配置学校基准时，所有用户仍报**同一条 82.0 m** —— **已关闭（§11.4）**

- 位置：`src/api/flow.rs:543-547`（只写日志 `[alt] 海拔基准 82.0 m（默认基准）`）、`src/api/model.rs:342`（`resolved_base_altitude`）
- 事实：C10 的目标是消掉「海拔参数异常」，但只要用户没填 `--school-altitude`，整条记录的 `bdA` 均值就固定在 **82.00 ± 0.005 m**（实测 5 个 seed：`82.001 / 81.996 / 81.996 / 82.001 / 82.001`；base=1050 时同样收在 1050.00 ± 0.005）。不同用户/校区之间仍然是同一个常数 → 服务端看到的仍是「人人 82 m」的聚类，只是从「20–260 随机」变回「固定 82」。实现者已把这个取舍写进文档（「报一个说得清的默认值」），但**界面上没有把「你正在用默认值」提升为警告**（只在折叠区里一行灰字）。
- requiredFix：`origin == Default` 时把「未设置学校基准海拔，将按 82 m 上报，可能与校区实际高程不符」同时推入 `warnings`（现有 `warnings` 会在跑完页/日志醒目展示），而不是只打一条普通日志；文案里给出一句可操作出口（`--school-altitude <米>` 或「高级生成参数 → 手动海拔」）。不改任何请求字段。

### 观察（不要求修，但请写进发布说明）

1. **野点会制造一个 0.1° 的 V 形折返**（本人实测 idx 140）：这是「GPS 野点跳出去再跳回来」的固有形态，真人样本里也有；但 0.9.0 的报告里「最小折返角 ≥30°」只对**课程**成立，对**注入野点后的轨迹**不成立。建议在报告与测试注释里写明口径（排除野点事件后复算），避免下次被人拿这一条当「自相矛盾」。
2. `ui/run.rs::pick_time` 的注释写「同一天内、不跨日」，但在「现在 00:0x + 用时 1 h」时，`latest_start_ms` 会落到**前一天**，`min()` 之后提交的是一条前一天的记录。行为可接受（stopTime 不会在未来），注释应改。
3. 手动单值海拔（`manual_altitude`）仍走 `override_bd_a` → 每个点同值 → 该条记录 `totalAscent=0`。这是 0.8.0 的既有行为、且是用户显式输入，**不算本轮缺陷**；但与本轮新增的「平移保形」注释放在一起容易被误读，建议在 UI 文案里点明「单值 = 全条同高，爬升 0」。

---

## 8. 结论

| 验收项 | 结论 |
|---|---|
| 1 门禁（fmt / check / test lib / route-planner / no-default-features） | **通过**（459 passed / 0 failed / 1 ignored；22+7 passed；exit 全 0） |
| 2 载荷零变化 | **通过**（crypto/obs/wire 逐字节相同；43 个 body 字段名字与顺序完全一致；只有 `complete`/`unCompleteReason` 取值变化） |
| 3 WAL（Pending 拒提 / Unknown 对账 / 损坏 fail-closed / 并发不丢 / 回执不进请求 / 两条不变量） | **通过**（含真实 loopback 请求序列断言） |
| 4 真实性 + 形态 | **通过**（停走 1.22%；野点 2 次×2 点 17.2/22.1 m 且不在关键点；radius 135/164 不同值、median 7.66、右偏 1.22；重合 1.000→0.150、圈间 0.03→3.33 m、往返 0、课程交叉 0） |
| 5 参数自洽 C1/C2/C3/C10 | **通过**（三纬度 ±0.17% 内；complete 与门槛绑定且不下调门槛；海拔均值精确落在基准且保形） |
| 6 风控面「没有引入新的可识别特征」 | **未通过**：F1（20.7% 点位 `speed` 与 `avgSpeed` 精确成 3.6 倍）、F2（radius 边界原子值） |
| 7 结论 | **needs_revision**（F1/F2 必修，F3 建议修；均为本地生成侧的小改动，不涉及请求字段与架构） |

修完 F1/F2 后建议只跑：`cargo test --release --lib`（要看到 459+ 全绿、`legacy_realism_off_*` 两条坐标哈希不变）+ 一条新的「残差无精确 0 / 边界无原子」断言，即可复审。

---

## 9. 复现方式

- 门禁与测试：见 §1（日志在 `E:\rea\t3logs\*.log`）。
- 独立探针：`E:\rea\tmp\t3probe`（工作区副本 + `src/track/probe_t3.rs`），
  `CARGO_TARGET_DIR=E:\rea\upstream\target cargo test --lib probe_t3 -- --nocapture --test-threads=1`。
  该副本**不在 git 里**，可随时删除；被审工作区 `git status` 在验收前后都是同样的 31 项。

## 10. 已知遗留（按任务约定，不算本轮失败项）

- 真机验证 / 服务端判定规则（无抓包，F1/F2 的「可被服务端检出」只能到统计层面）。
- C4（点位 `totalTime` 与 `startTime` 的关系）与 C5（零埋点）——架构级暴露面，改代码解决不了。
- §7 的三条「观察」是文档/口径问题，不影响本轮判定。

---

## 11. 复审（t11，收口）：F1 / F2 / F3 关闭 + fmt 后回归

复审方式与 §4/§9 相同：**重新复制**当前工作区到 `E:\rea\tmp\t11probe`（不含 `target/.git`），在副本里加 `#[cfg(test)] mod probe_t11b/probe_t11d`，调用 crate 内 API 自己复算；**被审工作区未改**（`git status` 复审前后一致：27 改 + 7 未跟踪，其中新增 `review/UI-ALTITUDE-EXIT.md`）。日志在 `E:\rea\t11logs\`。

### 11.1 门禁（fmt 之后，本人重跑）

```
$ cargo fmt --all -- --check
（无输出）exit=0

$ cargo check --all-targets
Finished `dev` profile ... in 0.81s   exit=0   （9 / 15 warnings，全部既有 dead_code）

$ cargo test --release --lib
test result: ok. 468 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 19.22s
exit=0

$ cargo test -p route-planner
test result: ok. 22 passed; 0 failed; ...
test result: ok. 7 passed; 0 failed; ...
test result: ok. 0 passed; 0 failed; ...
exit=0

$ cargo check --lib --no-default-features
Finished `dev` profile ... in 0.69s   exit=0   （103 warnings，全部既有 dead_code）
```

与 captain 报的 **468 / 0 / 1** 一致。另单跑 5 条修复用例 + 1 条布局用例，全 ok：

```
api::flow::audit_param_tests::unset_school_base_altitude_is_warned_with_an_exit ... ok
track::noise::tests::deterministic_z_behaves_like_a_normal_sample_but_is_pure ... ok
track::noise::tests::soft_knee_removes_boundary_atoms_without_narrowing_the_distribution ... ok
track::generator::gps_spread_tests::spike_points_carry_a_real_measurement_residual_not_an_atom_at_zero ... ok
track::generate_road::preview_tests::road_spike_points_carry_a_real_residual_not_an_atom_at_zero ... ok
ui::layout_tests::run_primary_action_is_visible_without_scrolling_on_a_phone ... ok
```

### 11.2 F1 关闭：spike 分支不再有「钉在 0」的残差

门禁口径（`i ∈ [3, n-1)`、`ptype ∉ {-1,5,6}`、`avgSpeed > 0`），我自己算：

| 样本 | n | residual==0 | `\|r\|<0.2%` | `\|r\|` 中位 | 首箱/次箱 | std | 奇/偶均值 |
|---|---|---|---|---|---|---|---|
| 经典 on | 152 | **0** | 1（0.7%） | 5.30% | **0.68** | 0.082 | 0.0068 / −0.0046 |
| 道路 on | 153 | **0** | 5（3.3%） | 5.61% | **1.18** | 0.080 | −0.0003 / −0.0000 |
| 经典 seed 1 | 156 | 0 | 3.8% | 4.35% | — | — | — |
| 经典 seed 11 | 151 | 0 | 2.0% | 5.64% | — | — | — |
| 经典 seed 42 | 155 | 0 | 0.0% | 4.95% | — | — | — |
| 经典 seed 2024 | 148 | 0 | 2.0% | 5.74% | — | — | — |
| 对照：legacy | 154 | — | — | — | — | — | residual==−1 占 **19.5%**（旧的零速尖峰） |

四条口径全部落在门禁区间内（0 / <6% / 3.5–7.5% / <1.6）；`|r|` 直方是平滑单峰（0–1% 与 1–2% 两箱分别 9%/5% 与 7%/10%，不是「首箱顶到 3 倍」）。确定性伪噪声没有留下结构：std ≈ 0.08（= σ/kmh）、lag-1 量级 0、奇偶均值对称。

**没有靠多消耗 RNG**：`Realism::legacy()` 逐点坐标哈希仍等于基线
`0x3fa63650beeff569`（经典）/ `0x55b51ec5898d062b`（道路），并由我自己的探针独立算了一遍（不是引用测试常量）。

一个容易被误判成「F1 没修干净」的现象，先说明：**整条轨迹（含前三个哨兵位）还剩恰好 1 个 residual==0 的点，是 idx=2**：
`ptype=2/0, avgSpeed=1.9, speed=6.84`。来源是 `postfix.rs:52-60` 的既有边界修正（`p2.speed = round_to(p2.avgSpeed*3.6, 4)`），0.8.0 就有 —— `legacy` 对照同样是 1 个。它不属于 spike 分支（门禁口径排除 idx<3），也不构成本轮回归。

### 11.3 F2 关闭：radius 不再在 clamp 边界堆原子值

每档 4000 样本（与实现方同种子），加轨迹级复核：

| 档 | 下界命中 | 上界命中 | min–max | 中位 | skew | 唯一值 | 单值最大重复 |
|---|---|---|---|---|---|---|---|
| Normal（4–15） | **0** | **0** | 4.15–14.39 | 7.01 | 0.68 | 893/4000 | 16（0.4%） |
| Type3（4–20） | **0** | **0** | 4.20–19.07 | 8.50 | 0.64 | 1222/4000 | 13 |
| Exception（10–30） | **0** | **0** | 10.21–29.04 | 14.15 | 0.95 | 1350/4000 | 14 |

- 轨迹级（经典 164 点 / 道路 164 点）：`4.00 / 10.00 / 15.00 / 20.00 / 30.00` **全部 0 个**；经典唯一值 152/164、中位 7.66（旧实现 135/164 —— 软膝之后取值反而更多样，没有被压窄）。
- 仍右偏（skew 0.64–0.95 > 0.5），中位与理论值偏差 ≤1.1%。
- 说明：那部分原本被硬 clamp 成 `10.00` 的 19% 质量并没有消失，它现在铺在 10.2–11.0 这一段（对数正态自身的下尾），密度连续、单值最大重复只有 0.4%，不再是「一个光秃秃的原子值」。

### 11.4 F3 关闭：未配置学校基准 → 警告 + 两个出口 + App 内控件

- 逻辑：`flow.rs:576-579` 在解析出 `(base_alt_m, alt_origin)` 之后 `log("⚠ [alt] …")` 且 `warnings.push(warning)`；`RunOutcome::summary()`（`flow.rs:109-137`）把 `warnings` 拼在结论后面 → UI 结论弹窗与 CLI 结论都会带出。
- 文案：`default_altitude_warning` 只在 `Default` 返回 `Some`；断言覆盖「82」「--school-altitude」「school_base_altitude」「手动海拔」「未配置学校基准海拔」，以及 `School/Manual/ManualRange → None`（用例 ok，我复核了文案本身）。
- t10 的 App 内出口**确实存在且可达**：`ui/run.rs:654-686` 在「高级生成参数」折叠区内有「学校基准海拔（米）」输入框 + 行内状态徽标 + 「当前生效：<describe_base_altitude>」优先级说明；`parse_school_altitude` 对非数字/越界/`"1 050"` 保留原值并报错（红字），不静默覆盖；`ui/msgs.rs:756-760` 在结论弹窗里追加「海拔出口（App 内）：跑步页 →『高级生成参数』→『学校基准海拔（米）』」。
- 布局未回退：`run_primary_action_is_visible_without_scrolling_on_a_phone` ok（新行在默认收起的折叠区内）。
- `--school-altitude` 也已补进 `src/cli/mod.rs` 的 usage（`git diff v0.8.0 -- src/cli/mod.rs` 只有 usage 那 4 行）。

### 11.5 fmt 回归检查：只有排版变化

- `cargo fmt --all -- --check` 干净；全量 468/0/1。
- **行为层面的等价证明**：我的探针在 fmt 之后复现了 t3（fmt 之前）逐项测到的数字 —— 164 点 / `2000.000 m` / 800 s / 1892 步（经典）、2322 步（道路）/ 停走 idx `[7,8]` / 野点 idx `138,139,157,158` 幅度 `22.1 / 22.1 / 17.2 / 17.2 m` / 两条坐标哈希。数字一位不差，说明 fmt 只改了排版。
- 断言常量逐条对齐门禁要求，没有被 `fmt` 或修 F1/F2 时悄悄放宽：`zeros == 0`、`atom_share < 0.06`、`median ∈ 0.035..=0.075`、`first/second < 1.6`、`boundary share < 1%`、`uniq/n > 0.2`、`skew > 0.5`。道路那条 spike 用例的门槛也是 6%（与门禁一致；3–4% 在单峰下属于正常抽样波动，n≈150 时 1 个样本就是 0.65%）。

### 11.6 有没有新问题（三类逐个查）

1. **新的恒定性 / 边界突变**：没有。
   - 残差：0 个精确 0、无奇偶/自相关结构（§11.2）；
   - radius：0 个边界原子，单值最大重复 0.4%，唯一值反而增加（§11.3）；
   - 时间轴未动（仍是 80% 5 s + 20% 1–8 s），停走/野点位置与 t3 完全一致；
   - 断面观察（非缺陷）：`soft_knee` 让「声明区间」不再可达（Normal 实际 4.15–14.39，声明 4–15）；若 UI/文案写「4–15 m」会与实测最大值差 0.6 m，属文案口径，不影响判定。
2. **提交载荷**：`git diff v0.8.0 -- src/crypto src/api/obs.rs src/track/wire.rs` = **0 字节**；`send_record` 的 `body.insert` 序列仍是 **43 个 key、名字与顺序完全一致**；`submit.rs` 的 diff 是 6 个 hunk（此处更正 §2.2 里写的「5 个 hunk」—— 内容是 5 类，hunk 数是 6），逐条仍是「C3 取值 + WAL 本地逻辑 + 测试」，没有任何字段增删。
3. **回归**：点数 164、距离 `2000.000 m`、时长 800 s、步数 1892/2322、停走 2 点（idx `[7,8]`，距离/步数冻结，由既有测试守住）、野点 4 点且**不在**起点/终点/哨兵/必经点 25 m 内、两条坐标哈希不变 —— 与 t3 的基线一一相同。

### 11.7 复审结论

| 项 | 状态 |
|---|---|
| F1 spike 双峰残差 | **关闭**（0 个精确 0；原子区 0.7%/3.3%；中位 5.30%/5.61%；首箱比 0.68/1.18；RNG 消耗未变） |
| F2 radius 边界原子 | **关闭**（三档边界命中 0/0；轨迹级 5 个边界值全为 0；右偏/唯一值未退化） |
| F3 未配置海拔警告 | **关闭**（Default→warnings+两个出口；其它 origin→None；App 内输入框可达、非法值不静默覆盖） |
| fmt 回归 | **无**（468/0/1；行为数字与 fmt 前逐项一致） |
| 新的可识别特征 | **无**（仅一条「声明区间不可达」的文案级观察） |
| 提交载荷 | **未变**（crypto/obs/wire 0 字节；43 key 顺序一致） |
| 已知遗留（真机 / 服务端规则 / C4 / C5） | 单列，不计入本轮失败项 |

**最终结论：pass。** 0.9.0 的 F1/F2/F3 三条 findings 全部关闭，fmt 之后无回归、无新增恒定性，提交载荷保持零变化。
