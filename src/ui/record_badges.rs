//! 列表徽标回填（0.9.1 / B 档）：让用户在**列表页**就看出哪条「通过但异常」。
//!
//! # 为什么要限速（这不是性能问题，是账号安全问题）
//! 判断「通过但异常」必须读详情（`get_one_record` 的 `reasonList`）。但**详情接口是会计数
//! 的**：一次请求风暴本身就是这个项目要避免的脚本特征。所以这里做三件事，缺一不可：
//! 1. 只读最近 [`BADGE_LIMIT`] 条 —— 列表首屏能看见的那几条够了，不碰更早的；
//! 2. 串行、两次请求之间至少 [`BADGE_INTERVAL_MS`] —— 不并发、不排队轰炸；
//! 3. 结果按 rrid 缓存 [`BADGE_TTL_MS`] —— 来回切页/重进页面**不重拉**。
//!
//! 另外三条底线：
//! - **失败只是没有徽标**：错误写进 `BadgeState::error()`，绝不覆盖列表数据；
//! - **切页即停**：`BadgeState::cancel()` 打到 worker 检查的同一个 [`Control`] 上，
//!   取消后不再发下一条请求；
//! - **旧回包一律丢弃**：每条消息带 `generation`，取消/重启都会换代。
//!
//! # 怎么测
//! 节奏与取消都不靠真 sleep：时钟、`sleep`、取数动作全部由 [`run_backfill`] 的调用方注入，
//! 测试用「假时钟 + 假 sleep」断言间隔与取消，既确定又快（见本文件 tests）。

use super::record_status::{self, RecordStatus};
use crate::api::records::{RecordRow, RunDetail};
use crate::task_control::Control;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

/// 只回填最近 N 条。
///
/// 5 = 列表首屏（360×640 上能看见 3–4 张卡片，留一点余量）与「别多打详情接口」的折中。
/// 调大它不是「体验优化」，是让账号多挨几次计数。
pub const BADGE_LIMIT: usize = 5;

/// 两次详情请求之间的最小间隔（毫秒）。
///
/// 1 条/秒：详情接口会计数，连发就是脚本节奏。宁可慢 4 秒，也不要一次并发 5 条。
pub const BADGE_INTERVAL_MS: u64 = 1_000;

/// 徽标缓存有效期（毫秒）。
///
/// 5 分钟：同一条记录的判定不会在几分钟内变来变去；这个窗口让「切页→回来」「刷新列表」
/// 都命中缓存，不再重复读详情。
pub const BADGE_TTL_MS: i64 = 5 * 60 * 1_000;

/// 回填队列的节拍器：只回答「下一条该发谁、现在能不能发」。
pub struct BadgePacer {
    queue: VecDeque<i64>,
    last_started_ms: Option<i64>,
}

impl BadgePacer {
    pub fn new(targets: Vec<i64>) -> Self {
        Self {
            queue: targets.into(),
            last_started_ms: None,
        }
    }

    pub fn pending(&self) -> usize {
        self.queue.len()
    }

    /// 现在可以发下一条吗？可以就出队并记下时刻（调用方负责真的去发）。
    pub fn next_ready(&mut self, now_ms: i64) -> Option<i64> {
        // 未满间隔一律不放行：串行 + 间隔都是硬约束，不因「队列还剩几条」而放宽。
        if self.last_started_ms.is_some() && self.wait_ms(now_ms) > 0 {
            return None;
        }
        let rrid = self.queue.pop_front()?;
        self.last_started_ms = Some(now_ms);
        Some(rrid)
    }

    /// 距下一次可发还需等多少毫秒（0 = 现在就能发）。
    pub fn wait_ms(&self, now_ms: i64) -> u64 {
        let Some(last) = self.last_started_ms else {
            return 0; // 还没发过：第一条立即发
        };
        let elapsed = now_ms.saturating_sub(last).max(0) as u64;
        BADGE_INTERVAL_MS.saturating_sub(elapsed)
    }
}

/// 一条回填结果（直接对应一条消息）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BadgeOutcome {
    /// 读到了详情：给出三态结论；顺带带上第一条非规则文案（有的话）。
    Fetched {
        rrid: i64,
        status: RecordStatus,
        anomaly: Option<String>,
    },
    /// 这条没读到（超时/网络/服务端拒绝）：只是没徽标，不动列表。
    Failed { rrid: i64, error: String },
}

impl BadgeOutcome {
    /// 消息体（`{generation}` 是丢弃旧回包用的代数）。
    pub fn to_json(&self, generation: u64) -> String {
        match self {
            BadgeOutcome::Fetched {
                rrid,
                status,
                anomaly,
            } => serde_json::json!({
                "generation": generation,
                "rrid": rrid,
                "status": status.code(),
                "anomaly": anomaly,
            })
            .to_string(),
            BadgeOutcome::Failed { rrid, error } => serde_json::json!({
                "generation": generation,
                "rrid": rrid,
                "error": error,
            })
            .to_string(),
        }
    }
}

/// 一次回填的收尾统计（只用于日志，不参与任何判定）。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BackfillReport {
    pub fetched: usize,
    pub failed: usize,
    pub cancelled: bool,
}

/// 回填驱动：按 [`BadgePacer`] 的节奏逐条读，`control` 一取消就立刻停手。
///
/// 时钟 / 等待 / 取数 / 上报全部由调用方注入 —— 这样「1 条/秒」和「取消即停」都能在
/// 测试里用假时钟断言，不需要真的 sleep 5 秒，也不需要假服务器。
pub fn run_backfill<N, S, F, E>(
    pacer: &mut BadgePacer,
    control: &Control,
    now_ms: N,
    sleep: S,
    mut fetch: F,
    mut emit: E,
) -> BackfillReport
where
    N: Fn() -> i64,
    S: Fn(u64),
    F: FnMut(i64) -> Result<RunDetail, String>,
    E: FnMut(BadgeOutcome),
{
    let mut report = BackfillReport::default();
    loop {
        // 取消检查放在「发请求之前」：切走页面后一条都不许再发。
        if control.check().is_err() {
            report.cancelled = true;
            return report;
        }
        let now = now_ms();
        let Some(rrid) = pacer.next_ready(now) else {
            if pacer.pending() == 0 {
                return report;
            }
            let wait = pacer.wait_ms(now);
            debug_assert!(wait > 0, "队列非空时 next_ready 已放行，不应无等待");
            if wait > 0 {
                sleep(wait);
            }
            continue;
        };
        match fetch(rrid) {
            Ok(detail) => {
                // M1（盲审）：详情页与徽标必须走**同一个入口**。`classify_detail_parts`
                // 在「既没有 complete、也没有 reasonList」的 Ok 空壳上返回 None ——
                // 那种 payload 不能替服务端下结论，更不能写成「达标」；
                // 当作本轮未取到，走失败路径（B1 的退避会兜住重复重试）。
                let Some(status) =
                    record_status::classify_detail_parts(detail.complete, &detail.reason_list)
                else {
                    report.failed += 1;
                    emit(BadgeOutcome::Failed {
                        rrid,
                        error: "详情没有可用的判定信息（complete 与 reasonList 都缺失）".into(),
                    });
                    continue;
                };
                let anomaly = record_status::anomaly_reasons(&detail.reason_list)
                    .first()
                    .map(|item| item.reason.clone())
                    .filter(|text| !text.is_empty());
                report.fetched += 1;
                emit(BadgeOutcome::Fetched {
                    rrid,
                    status,
                    anomaly,
                });
            }
            Err(error) => {
                report.failed += 1;
                emit(BadgeOutcome::Failed { rrid, error });
            }
        }
    }
}

/// 一条徽标的缓存项。
#[derive(Debug, Clone, PartialEq, Eq)]
struct BadgeEntry {
    status: RecordStatus,
    anomaly: Option<String>,
    at_ms: i64,
}

/// 单条记录的失败记录（退避用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Failure {
    at_ms: i64,
    count: u32,
}

/// 失败退避：30s / 2min / 10min。
///
/// BLOCKER-1（盲审）：早先失败不写缓存、也不记失败，于是「失败 → 不写缓存 →
/// 下一帧 plan 又把它排进去 → 新建 pacer（首条立发）」，应用每 200 ms 重绘一次，
/// 就等于**每 200 ms 重启一轮**：单条失败约 5 请求/秒，且永不停止。
/// 指数退避把重试间隔拉开，让"1 条/秒"在跨轮时也成立。
pub const BADGE_BACKOFF_MS: [i64; 3] = [30_000, 120_000, 600_000];

/// 同一 rrid 连续失败到这个次数就**停手**（保留 error，不再自动重试）。
pub const BADGE_FAIL_LIMIT: u32 = 3;

/// 列表页的徽标状态：缓存 + 在飞任务 + 取消把手 + 失败退避。全部是内存状态，不碰网络。
#[derive(Default)]
pub struct BadgeState {
    entries: HashMap<i64, BadgeEntry>,
    /// rrid → 最近一次失败（时刻 + 连续次数）。成功一次即清除。
    failures: HashMap<i64, Failure>,
    /// 上一轮 `done` 的时刻。用于**跨轮**节流（见 [`plan`]）。
    last_round_end_ms: i64,
    pending: bool,
    generation: u64,
    error: Option<String>,
    handle: Option<Arc<Control>>,
}

impl BadgeState {
    /// 某条记录已确认的结论（过期视为没有，绝不拿旧徽标糊弄）。
    pub fn status_of(&self, rrid: i64, now_ms: i64) -> Option<RecordStatus> {
        self.entry(rrid, now_ms).map(|entry| entry.status)
    }

    /// 非规则文案（红色徽标的说明用）。
    pub fn anomaly_of(&self, rrid: i64, now_ms: i64) -> Option<&str> {
        self.entry(rrid, now_ms)
            .and_then(|entry| entry.anomaly.as_deref())
    }

    fn entry(&self, rrid: i64, now_ms: i64) -> Option<&BadgeEntry> {
        self.entries
            .get(&rrid)
            .filter(|entry| now_ms.saturating_sub(entry.at_ms) < BADGE_TTL_MS)
    }

    /// 是否有回填在飞（含已取消但线程还没收尾的那一小段）。
    pub fn in_flight(&self) -> bool {
        self.pending || self.handle.is_some()
    }

    pub fn pending(&self) -> bool {
        self.pending
    }

    /// 最近一次失败原因（只影响徽标，不影响列表）。
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// 开一轮回填：换代并把取消把手交给后台线程。
    pub fn begin(&mut self, handle: Arc<Control>) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.pending = true;
        self.error = None;
        self.handle = Some(handle);
        self.generation
    }

    /// 取消回填（切页/登出）。缓存保留 —— 回来时不该重拉。
    pub fn cancel(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.cancel();
        }
        self.generation = self.generation.wrapping_add(1);
        self.pending = false;
    }

    /// 登出/会话失效：连缓存与失败记录一起收掉。
    pub fn clear(&mut self) {
        self.cancel();
        self.entries.clear();
        self.failures.clear();
        self.last_round_end_ms = 0;
        self.error = None;
    }

    /// 列表换代后丢掉不再展示的缓存与失败记录（保留的条目不重拉）。
    pub fn retain(&mut self, rows: &[RecordRow]) {
        self.entries
            .retain(|rrid, _| rows.iter().any(|row| row.rrid == *rrid));
        self.failures
            .retain(|rrid, _| rows.iter().any(|row| row.rrid == *rrid));
    }

    /// 这条现在能不能重试（失败退避 + 熔断）。
    fn ready_for_retry(&self, rrid: i64, now_ms: i64) -> bool {
        let Some(failure) = self.failures.get(&rrid) else {
            return true;
        };
        // 连续失败到上限：停手。继续重试只会白花请求，并在服务端看来像脚本。
        if failure.count >= BADGE_FAIL_LIMIT {
            return false;
        }
        let index = (failure.count as usize)
            .saturating_sub(1)
            .min(BADGE_BACKOFF_MS.len() - 1);
        now_ms.saturating_sub(failure.at_ms) >= BADGE_BACKOFF_MS[index]
    }

    /// 处理一条回填消息。返回是否被采纳（旧代数的回包一律丢）。
    pub fn apply(&mut self, payload: &Value, now_ms: i64) -> bool {
        let generation = payload.get("generation").and_then(Value::as_u64);
        if generation != Some(self.generation) {
            return false; // 已取消 / 已换轮：迟到的结果不许写进当前状态
        }
        if payload.get("done").and_then(Value::as_bool) == Some(true) {
            self.pending = false;
            self.handle = None;
            // B1：记下本轮结束时刻，让下一轮至少等 `BADGE_INTERVAL_MS` 才开始。
            self.last_round_end_ms = now_ms;
            return true;
        }
        let Some(rrid) = payload.get("rrid").and_then(Value::as_i64) else {
            return false;
        };
        if let Some(error) = payload.get("error").and_then(Value::as_str) {
            // 只是这条没有徽标：不动列表、不动已有缓存，但要记失败供退避使用。
            self.error = Some(format!("rrid={rrid} 判定没取到：{error}"));
            let failure = self.failures.entry(rrid).or_insert(Failure {
                at_ms: now_ms,
                count: 0,
            });
            failure.at_ms = now_ms;
            failure.count = failure.count.saturating_add(1);
            return true;
        }
        let Some(status) = payload
            .get("status")
            .and_then(Value::as_str)
            .and_then(RecordStatus::from_code)
        else {
            return false;
        };
        // 成功一次就把失败计数清掉（下次失败从 30s 重新算）。
        self.failures.remove(&rrid);
        self.entries.insert(
            rrid,
            BadgeEntry {
                status,
                anomaly: payload
                    .get("anomaly")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                at_ms: now_ms,
            },
        );
        true
    }
}

/// 最近 N 条（按 `start_time` 倒序；同一时刻保持服务端原顺序）。
///
/// 不依赖「列表接口本来就按时间倒序」这个假设：这里自己排一次，避免服务端顺序一变就
/// 悄悄去读了更早的记录。
pub fn recent(rows: &[RecordRow], limit: usize) -> Vec<i64> {
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by(|a, b| rows[*b].start_time.cmp(&rows[*a].start_time));
    order
        .into_iter()
        .take(limit)
        .map(|index| rows[index].rrid)
        .collect()
}

/// 本轮该拉哪几条：最近 N 条里，缓存没命中（或已过期）、且不在失败退避期内的那些。
///
/// **跨轮节流**：上一轮刚结束（不足 [`BADGE_INTERVAL_MS`]）就返回空 —— 否则
/// 「失败 → 应用每 200 ms 重绘 → 立刻开新一轮」会让详情接口跑到 5 次/秒
/// （盲审 BLOCKER-1）。"1 条/秒"必须在**轮与轮之间**也成立，而不只是轮内。
pub fn plan(rows: &[RecordRow], badges: &BadgeState, now_ms: i64) -> Vec<i64> {
    if badges.last_round_end_ms > 0
        && now_ms.saturating_sub(badges.last_round_end_ms) < BADGE_INTERVAL_MS as i64
    {
        return Vec::new();
    }
    recent(rows, BADGE_LIMIT)
        .into_iter()
        .filter(|rrid| badges.status_of(*rrid, now_ms).is_none())
        .filter(|rrid| badges.ready_for_retry(*rrid, now_ms))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::{Cell, RefCell};

    /// BLOCKER-1 的回归锁：失败后**不许**在下一轮立刻重发。
    ///
    /// 盲审给的触发链是「失败 → 不写缓存 → 下一帧 plan 又排进去 → 新 pacer 首条立发」，
    /// 应用每 200 ms 重绘一次，于是单条失败约 5 请求/秒。这条测试把三个闸门钉住：
    /// 轮间 ≥1s、失败退避 30s→2min→10min、连续 3 次熔断。
    #[test]
    fn a_failed_row_backs_off_and_stops_retrying() {
        let now = 1_700_000_000_000i64;
        let mut badges = BadgeState::default();
        let rows = vec![row(11, now, false)];

        let run_failing_round = |badges: &mut BadgeState, at: i64| {
            let control = std::sync::Arc::new(crate::task_control::Control::default());
            let generation = badges.begin(control);
            badges.apply(
                &json!({"generation": generation, "rrid": 11, "error": "超时"}),
                at,
            );
            badges.apply(&json!({"generation": generation, "done": true}), at);
        };

        // 第 1 轮：首条立即发（轮内允许）
        assert_eq!(plan(&rows, &badges, now), vec![11]);
        run_failing_round(&mut badges, now);

        // 轮刚结束 200 ms：跨轮节流挡住 —— 这正是原来 "5 请求/秒" 的来源
        assert!(
            plan(&rows, &badges, now + 200).is_empty(),
            "轮与轮之间也必须 ≥1s，否则每 200ms 重绘就等于 5 请求/秒"
        );

        // 过了轮间节流，但仍在 30s 退避内
        assert!(
            plan(&rows, &badges, now + 1_100).is_empty(),
            "第一次失败要退避 30s"
        );

        // 30s 后允许重试，第 2 轮再失败 → 退避升到 2min
        assert_eq!(plan(&rows, &badges, now + 30_000), vec![11]);
        run_failing_round(&mut badges, now + 30_000);
        assert!(
            plan(&rows, &badges, now + 31_000).is_empty(),
            "第二次失败要退避 2min"
        );
        assert_eq!(plan(&rows, &badges, now + 150_000), vec![11]);

        // 第 3 次失败 → 熔断，永远不再自动重试，但失败原因保留
        run_failing_round(&mut badges, now + 150_000);
        assert!(
            plan(&rows, &badges, now + 150_000 + 900_000).is_empty(),
            "连续失败 3 次后必须停手，不能无限重试"
        );
        assert!(badges.error().is_some(), "停手后仍要保留失败原因");
    }

    /// 成功一次就把失败计数清掉：下次失败从 30s 重新算，不会一路熔断。
    #[test]
    fn a_successful_fetch_clears_the_failure_streak() {
        let now = 1_700_000_000_000i64;
        let mut badges = BadgeState::default();
        let rows = vec![row(11, now, true)];

        let control = std::sync::Arc::new(crate::task_control::Control::default());
        let generation = badges.begin(control);
        badges.apply(
            &json!({"generation": generation, "rrid": 11, "error": "超时"}),
            now,
        );
        badges.apply(&json!({"generation": generation, "done": true}), now);
        assert!(plan(&rows, &badges, now + 1_100).is_empty());

        // 模拟 30s 后成功
        let control = std::sync::Arc::new(crate::task_control::Control::default());
        let generation = badges.begin(control);
        badges.apply(
            &json!({"generation": generation, "rrid": 11, "status": "passed"}),
            now + 30_000,
        );
        badges.apply(
            &json!({"generation": generation, "done": true}),
            now + 30_000,
        );

        // 缓存命中 → 不再排；失败计数也已清空（若再失败会从 30s 重新开始）
        assert!(plan(&rows, &badges, now + 40_000).is_empty());
        assert_eq!(
            badges.status_of(11, now + 40_000),
            Some(RecordStatus::Passed)
        );
    }

    fn row(rrid: i64, start_time: i64, complete: bool) -> RecordRow {
        RecordRow {
            rrid,
            total_dis: 2000.0,
            total_time: 840,
            start_time,
            complete,
            avg_step_freq: 160,
            calorie: 0,
            avg_power: 0,
            total_steps: 0,
            uuid: String::new(),
        }
    }

    fn rows_for(rrids: &[i64]) -> Vec<RecordRow> {
        // start_time 与 rrid 同序递增：rrid 越大越新。
        rrids
            .iter()
            .map(|rrid| row(*rrid, 1_700_000_000_000 + rrid * 1_000, true))
            .collect()
    }

    fn detail(complete: bool, rules: &[(&str, bool)]) -> RunDetail {
        RunDetail {
            rrid: 0,
            complete: Some(complete),
            reason_list: rules
                .iter()
                .map(|(text, ok)| crate::api::records::ReasonItem {
                    reason: (*text).into(),
                    complete: Some(*ok),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    /// Ok 空壳：既没有 `complete`、也没有 `reasonList`。
    /// 徽标路径必须把它当作「本轮没取到」，**不得**写成「达标」或「未达标」（盲审 M1）。
    fn detail_shell() -> RunDetail {
        RunDetail {
            rrid: 0,
            complete: None,
            reason_list: Vec::new(),
            ..Default::default()
        }
    }

    /// 只取最近 5 条：第 6 条及更早一条都不许碰（详情接口会计数）。
    #[test]
    fn only_the_five_most_recent_rows_are_planned() {
        let rows = rows_for(&[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(recent(&rows, BADGE_LIMIT), vec![9, 8, 7, 6, 5]);
        let badges = BadgeState::default();
        let planned = plan(&rows, &badges, 0);
        assert_eq!(planned, vec![9, 8, 7, 6, 5]);
        assert!(
            !planned.iter().any(|rrid| *rrid <= 4),
            "更早的记录一条都不许拉"
        );
        // 少于 5 条时就是全部，不多不少。
        assert_eq!(recent(&rows_for(&[1, 2]), BADGE_LIMIT), vec![2, 1]);
    }

    /// 1 条/秒：用假时钟 + 假 sleep 断言间隔，测试本身**不真 sleep**。
    #[test]
    fn detail_requests_are_paced_one_per_second_without_real_sleeping() {
        let control = Control::default();
        let mut pacer = BadgePacer::new(vec![9, 8, 7, 6, 5]);
        let clock = Cell::new(0_i64);
        let sleeps = RefCell::new(Vec::<u64>::new());
        let started_at = RefCell::new(Vec::<i64>::new());

        let report = run_backfill(
            &mut pacer,
            &control,
            || clock.get(),
            |ms| {
                sleeps.borrow_mut().push(ms);
                clock.set(clock.get() + ms as i64);
            },
            |rrid| {
                started_at.borrow_mut().push(clock.get());
                let _ = rrid;
                Ok(detail(true, &[("步频 40–300", true)]))
            },
            |_| {},
        );

        assert_eq!(report.fetched, BADGE_LIMIT);
        assert!(!report.cancelled);
        let started = started_at.borrow();
        assert_eq!(started.len(), BADGE_LIMIT);
        assert_eq!(started[0], 0, "第一条立即发，不许先等一秒");
        for pair in started.windows(2) {
            assert!(
                pair[1] - pair[0] >= BADGE_INTERVAL_MS as i64,
                "两次详情请求间隔只有 {} ms，必须 ≥ {BADGE_INTERVAL_MS} ms",
                pair[1] - pair[0]
            );
        }
        // 5 条 = 4 次等待，总耗时 4 秒级（不是并发）。
        assert_eq!(sleeps.borrow().len(), BADGE_LIMIT - 1);
        assert!(sleeps.borrow().iter().all(|ms| *ms >= BADGE_INTERVAL_MS));
    }

    /// 缓存命中不重拉；5 分钟后过期才重新拉。
    #[test]
    fn cache_hit_skips_refetch_and_expires_after_five_minutes() {
        let rows = rows_for(&[1, 2, 3, 4, 5, 6]);
        let mut badges = BadgeState::default();
        let generation = badges.begin(Arc::new(Control::default()));
        for rrid in [6, 5, 4, 3, 2] {
            assert!(badges.apply(
                &serde_json::json!({
                    "generation": generation, "rrid": rrid, "status": "passed"
                }),
                0
            ));
        }
        assert!(
            plan(&rows, &badges, 0).is_empty(),
            "刚拉到的最近 5 条不许重拉"
        );
        assert!(
            plan(&rows, &badges, BADGE_TTL_MS - 1).is_empty(),
            "缓存 5 分钟内都算新鲜"
        );
        assert_eq!(
            plan(&rows, &badges, BADGE_TTL_MS),
            vec![6, 5, 4, 3, 2],
            "过期后只重拉这 5 条，第 1 条永远不碰"
        );
    }

    /// 取消后一条都不再发，并且报告如实说「已取消」。
    #[test]
    fn cancel_stops_the_backfill_before_the_next_request() {
        let control = Arc::new(Control::default());
        let mut pacer = BadgePacer::new(vec![5, 4, 3, 2, 1]);
        let clock = Cell::new(0_i64);
        let calls = RefCell::new(Vec::<i64>::new());
        let handle = control.clone();

        let report = run_backfill(
            &mut pacer,
            &control,
            || clock.get(),
            |ms| clock.set(clock.get() + ms as i64),
            |rrid| {
                calls.borrow_mut().push(rrid);
                if calls.borrow().len() == 2 {
                    // 模拟「读到第 2 条时用户切走了页面」
                    handle.cancel();
                }
                Ok(detail(true, &[]))
            },
            |_| {},
        );

        assert_eq!(*calls.borrow(), vec![5, 4], "取消后不许再发第 3 条");
        assert_eq!(report.fetched, 2);
        assert!(report.cancelled);
        assert_eq!(pacer.pending(), 3, "队列里剩下的不许被消费掉");
    }

    /// 某一条失败不影响其它条：失败只是没有徽标，列表数据一律不碰。
    #[test]
    fn one_failure_does_not_discard_the_other_results() {
        let control = Control::default();
        let mut pacer = BadgePacer::new(vec![5, 4, 3]);
        let clock = Cell::new(0_i64);
        let outcomes = RefCell::new(Vec::new());

        let report = run_backfill(
            &mut pacer,
            &control,
            || clock.get(),
            |ms| clock.set(clock.get() + ms as i64),
            |rrid| {
                if rrid == 4 {
                    Err("超时".into())
                } else {
                    Ok(detail(true, &[("运动数据有问题", false)]))
                }
            },
            |outcome| outcomes.borrow_mut().push(outcome),
        );

        assert_eq!(report.fetched, 2);
        assert_eq!(report.failed, 1);
        let outcomes = outcomes.borrow();
        assert!(matches!(
            &outcomes[0],
            BadgeOutcome::Fetched {
                rrid: 5,
                status: RecordStatus::PassedButAnomalous,
                anomaly: Some(text),
            } if text == "运动数据有问题"
        ));
        assert!(matches!(&outcomes[1], BadgeOutcome::Failed { rrid: 4, .. }));
    }

    /// UI 的取消必须打到 worker 检查的同一个开关上；换轮后的旧回包一律丢弃。
    #[test]
    fn cancel_flips_the_worker_flag_and_drops_late_replies() {
        let handle = Arc::new(Control::default());
        let mut badges = BadgeState::default();
        let generation = badges.begin(handle.clone());
        assert!(badges.in_flight());

        assert!(badges.apply(
            &serde_json::json!({
                "generation": generation,
                "rrid": 7,
                "status": "anomalous",
                "anomaly": "运动数据有问题"
            }),
            1_000
        ));
        assert_eq!(
            badges.status_of(7, 1_000),
            Some(RecordStatus::PassedButAnomalous)
        );
        assert_eq!(badges.anomaly_of(7, 1_000), Some("运动数据有问题"));

        badges.cancel();
        assert!(
            handle.check().is_err(),
            "取消必须让 worker 的 check() 立刻失败"
        );
        assert!(!badges.in_flight());
        // 缓存保留：切回记录页不该重拉。
        assert_eq!(
            badges.status_of(7, 2_000),
            Some(RecordStatus::PassedButAnomalous)
        );
        // 迟到回包（旧代数）不许写进当前状态。
        assert!(!badges.apply(
            &serde_json::json!({
                "generation": generation, "rrid": 6, "status": "passed"
            }),
            3_000
        ));
        assert_eq!(badges.status_of(6, 3_000), None);
    }

    /// 错误消息只记录原因，不清缓存、不清在飞状态（done 才收尾）。
    #[test]
    fn failure_message_only_records_the_reason() {
        let mut badges = BadgeState::default();
        let generation = badges.begin(Arc::new(Control::default()));
        assert!(badges.apply(
            &serde_json::json!({
                "generation": generation, "rrid": 3, "error": "网络中断"
            }),
            0
        ));
        assert_eq!(badges.error(), Some("rrid=3 判定没取到：网络中断"));
        assert!(badges.pending(), "单条失败不算整轮结束");
        assert_eq!(badges.status_of(3, 0), None);
        assert!(badges.apply(
            &serde_json::json!({"generation": generation, "done": true}),
            0
        ));
        assert!(!badges.pending());
        assert!(!badges.in_flight());
    }

    /// 列表换代只保留仍在列表里的缓存，避免无限增长。
    #[test]
    fn retain_prunes_cache_for_records_that_left_the_list() {
        let mut badges = BadgeState::default();
        let generation = badges.begin(Arc::new(Control::default()));
        for rrid in [1, 2] {
            badges.apply(
                &serde_json::json!({"generation": generation, "rrid": rrid, "status": "passed"}),
                0,
            );
        }
        badges.retain(&rows_for(&[2]));
        assert_eq!(badges.status_of(1, 0), None);
        assert_eq!(badges.status_of(2, 0), Some(RecordStatus::Passed));
    }
}
