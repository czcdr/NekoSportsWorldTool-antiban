//! 异常记录修复（红转绿）与禁跑诊断。
//!
//! 链路：违规自查（cheat）→ 记录列表 → 逐条读详情拿 `reasonList` → 用**原记录的开始时刻**
//! 重新生成合规轨迹并覆盖提交 → 复查是否变达标 / 违规标记是否消失。
//!
//! # 为什么「覆盖」能成立
//! `POST /api/v70260/runnings/save/record` 对同一天同一 `sportType` 没有去重锁：新提交会把
//! 该日的记录顶替掉（实测证据：商品截图里一条 `1.72km` 的记录修复后变成 `1.68km` —— 距离变了，
//! 说明是重算后覆盖，不是单纯改状态位）。因此修旧记录 = 用原 `startTime` 重跑一次。
//!
//! # 做不到的部分（如实边界）
//! - 「禁跑」若已写入服务端台账（`cheatlist` 的 self 段），本模块改的是**记录**，不是台账；
//!   台账只能靠申诉（官方 App 的「我有异议」）或到期。复查能立刻看出是哪一种。
//! - 覆盖是否生效依赖服务端按键去重（日期 / `sportType` / `startTime`），首次使用请先 `--dry-run`
//!   看诊断，再单条 `--yes --only <rrid>` 验证一条，确认变绿后再批量。

use super::cheat::{self, CheatReport};
use super::client::ApiClient;
use super::flow::{self, RunOutcome, RunParams};
use super::policy;
use super::records;
use crate::track::generate_road::RouteMode;

/// 合规距离区间（米）。取男 1–3km 与女 1–2km 的安全交集，并与 `run` 的默认 1.0–1.5km 对齐。
pub const MIN_DIST: f64 = 1000.0;
pub const MAX_DIST: f64 = 1500.0;

/// 合规配速区间（秒/公里）。男 3'30"–8'30"、女 4'00"–9'00" 的交集里再收窄，避开两端判别边界。
pub const MIN_PACE_S: f64 = 360.0;
pub const MAX_PACE_S: f64 = 480.0;

/// 一条待修记录。
#[derive(Debug, Clone)]
pub struct Abnormal {
    pub rrid: i64,
    /// 原记录开始时刻（毫秒）—— 覆盖提交必须沿用它，否者会变成一条新记录。
    pub start_ms: i64,
    pub total_dis: f64,
    pub total_time: i64,
    /// 服务端 `reasonList` 里 `complete=false` 的原因；空表示详情没拿到。
    pub reasons: Vec<String>,
}

impl Abnormal {
    /// 修复时采用的距离：原距离落在合规区间内就保持（里程不变最不容易被统计规则打），否则夹取。
    pub fn fix_dist(&self) -> f64 {
        if self.total_dis >= MIN_DIST && self.total_dis <= MAX_DIST {
            self.total_dis
        } else if self.total_dis > 0.0 {
            self.total_dis.clamp(MIN_DIST, MAX_DIST)
        } else {
            MIN_DIST
        }
    }

    /// 原因汇总，供命令行一行显示。
    pub fn reason_text(&self) -> String {
        if self.reasons.is_empty() {
            "详情未取到（原因未知）".into()
        } else {
            self.reasons.join(" / ")
        }
    }
}

/// 诊断结果。
#[derive(Debug, Clone)]
pub struct Diagnosis {
    pub cheat: CheatReport,
    pub abnormal: Vec<Abnormal>,
    /// 记录总数（含达标），用于判断「是记录级异常还是账号级冻结」。
    pub total_records: usize,
    /// 剩余冻结秒数（`runModePolicy` 的 `freezeRunTime`）。
    /// `None` 表示策略接口没取到 —— 不等于未冻结。
    pub freeze_seconds: Option<i64>,
    pub freeze_message: String,
}

impl Diagnosis {
    /// 是否在服务端违规榜里留了自己的条目 —— 这是「台账」的直接证据。
    pub fn flagged(&self) -> bool {
        matches!(self.cheat.status(), cheat::Status::Flagged)
    }

    /// 是否处于禁跑期。拿不到策略时按「不确定」处理，返回 false 但 `freeze_text` 会说明。
    pub fn frozen(&self) -> bool {
        self.freeze_seconds.unwrap_or(0) > 0
    }

    /// 冻结状态是否未知（策略接口没取到）。
    ///
    /// 这和「未冻结」不是一回事：`frozen()` 对未知返回 false 只适合**显示**，
    /// 任何「要不要动手提交」的判断都必须先过这里，否则一次策略接口抖动
    /// 就会被当成"没冻结"，在禁跑期里白白撞一次服务端拒绝并计入风控。
    pub fn freeze_unknown(&self) -> bool {
        self.freeze_seconds.is_none()
    }

    /// 冻结倒计时文本。
    pub fn freeze_text(&self) -> String {
        match self.freeze_seconds {
            None => "未取到策略（冻结状态未知）".into(),
            Some(0) => "未冻结".into(),
            Some(sec) => {
                let mut s = sec;
                let d = s / 86_400;
                s %= 86_400;
                let h = s / 3_600;
                s %= 3_600;
                let m = s / 60;
                if d > 0 {
                    format!("禁跑中，还剩 {d} 天 {h:02}:{m:02}:{:02}", s % 60)
                } else {
                    format!("禁跑中，还剩 {h:02}:{m:02}:{:02}", s % 60)
                }
            }
        }
    }

    /// 一句结论：区分「只有记录不达标（可修）」与「已被标记违规 / 正在禁跑」。
    pub fn verdict(&self) -> String {
        if self.frozen() {
            return format!(
                "账号处于禁跑期（{}）。这期间服务端会独立拒绝提交，覆盖修复提交不进去 —— \
                 先等倒计时归零，再回来跑 recover --yes。已在禁跑期内的记录不会因为等待而变差，\
                 要保住的是到期后立刻把那几条未达标记录补掉（当前 {} 条待修）。",
                self.freeze_text(),
                self.abnormal.len()
            );
        }
        // 拿不到策略 = 状态未知。未知不等于未冻结：结论必须先说"不确定"，
        // 不能让用户以为可以直接 --yes（旧实现会落到下面「最可修」分支）。
        if self.freeze_unknown() {
            return format!(
                "冻结状态未知（{}）：策略接口这次没取到，不能确认账号没被禁跑。\
                 先重跑一次 recover 确认能读出倒计时，再考虑 --yes；\
                 {} 条未达标记录先按「可修」看待，但覆盖提交要等状态确认之后。",
                self.freeze_text(),
                self.abnormal.len()
            );
        }
        match (self.flagged(), self.abnormal.len()) {
            (true, n) => format!(
                "服务端违规榜已有本人条目（原因：{}），另有 {n} 条未达标记录。\
                 修复只能动记录状态；禁跑是否随记录一并解除，取决于台账是不是派生出来的 —— \
                 修完用 --verify 复查即可分辨。",
                self.cheat.reason()
            ),
            (false, 0) => "违规榜无本人条目，记录全部达标：当前没有可修的东西。".into(),
            (false, n) => format!(
                "{n} 条记录未达标，违规榜暂无本人条目。这是最可修的一种：覆盖提交后应能转绿。"
            ),
        }
    }
}

/// 读一遍现状：冻结倒计时 + 违规榜 + 全部记录 + 未达标记录的原因。
pub fn diagnose(client: &mut ApiClient, log: &mut dyn FnMut(&str)) -> Result<Diagnosis, String> {
    // 冻结倒计时最先读：它决定「能不能提交」，比记录状态更要紧。
    let policy_info = match policy::fetch_policy(client) {
        Ok(p) => Some(p),
        Err(e) => {
            log(&format!("[recover] 策略接口失败（冻结状态未知）: {e}"));
            None
        }
    };
    let freeze_seconds = policy_info.as_ref().map(|p| p.freeze_run_time);
    let freeze_message = policy_info
        .as_ref()
        .map(|p| p.freeze_message.clone())
        .unwrap_or_default();
    if let Some(p) = &policy_info {
        if p.frozen() {
            log(&format!(
                "[recover] ⚠ {}（{}）",
                p.freeze_text(),
                if freeze_message.is_empty() {
                    "服务端未给文案"
                } else {
                    &freeze_message
                }
            ));
        }
    }

    let cheat_report = match cheat::query(client, 1, log) {
        Ok(c) => c,
        Err(e) => {
            // 违规榜失败不该挡住记录诊断：没拿到 ≠ 没违规，只降级不中止。
            log(&format!("[recover] 违规自查失败（记录诊断继续）: {e}"));
            CheatReport::default()
        }
    };

    let rows = records::fetch_records(client)?;
    let total_records = rows.len();
    let mut abnormal = Vec::new();
    for row in rows.iter().filter(|r| !r.complete) {
        let reasons = match records::fetch_detail(client, row.rrid) {
            Ok(d) => d
                .reason_list
                .iter()
                .filter(|x| !x.complete)
                .map(|x| x.reason.clone())
                .filter(|s| !s.trim().is_empty())
                .collect(),
            Err(e) => {
                log(&format!("[recover] rrid={} 详情读取失败: {e}", row.rrid));
                Vec::new()
            }
        };
        abnormal.push(Abnormal {
            rrid: row.rrid,
            start_ms: row.start_time,
            total_dis: row.total_dis,
            total_time: row.total_time,
            reasons,
        });
    }
    // 早的记录先修：老学期记录先定格，倒序修容易把「最近一次」状态刷乱。
    abnormal.sort_by_key(|a| a.start_ms);

    Ok(Diagnosis {
        cheat: cheat_report,
        abnormal,
        total_records,
        freeze_seconds,
        freeze_message,
    })
}

/// 单条修复参数。
#[derive(Debug, Clone, Copy)]
pub struct FixPlan {
    /// 目标距离（米）；0 = 沿用原记录距离（合规区间内保持不变）。
    pub dist: f64,
    /// 目标配速（秒/公里）；0 = 合规区间内随机。
    pub pace_s: f64,
    pub face_check: i64,
}

impl Default for FixPlan {
    fn default() -> Self {
        Self {
            dist: 0.0,
            pace_s: 0.0,
            face_check: 1,
        }
    }
}

impl FixPlan {
    /// 这条记录实际要用的距离。
    pub fn resolved_dist(&self, a: &Abnormal) -> f64 {
        if self.dist > 0.0 {
            self.dist.clamp(MIN_DIST, MAX_DIST)
        } else {
            a.fix_dist()
        }
    }

    /// 实际配速：给了就夹到合规区间，没给就在区间内随机。
    pub fn resolved_pace(&self) -> f64 {
        if self.pace_s > 0.0 {
            self.pace_s.clamp(MIN_PACE_S, MAX_PACE_S)
        } else {
            MIN_PACE_S + rand::random::<f64>() * (MAX_PACE_S - MIN_PACE_S)
        }
    }
}

/// 修一条：用原记录的开始时刻重跑一次合规提交。
///
/// 返回的 `RunOutcome.verification.complete` 是服务端对该条的**当场判定**，
/// 不等于台账已清 —— 台账要重新 `cheat` 才知道。
pub fn fix_one(
    client: &mut ApiClient,
    a: &Abnormal,
    plan: FixPlan,
    route_mode: RouteMode,
    seed: u64,
    log: &mut dyn FnMut(&str),
) -> Result<RunOutcome, String> {
    if a.start_ms <= 0 {
        return Err(format!("rrid={} 缺少开始时刻，无法原位覆盖", a.rrid));
    }
    let dist = plan.resolved_dist(a);
    let pace = plan.resolved_pace();
    let dur = (dist / 1000.0 * pace).round() as i64;
    if dur <= 0 {
        return Err(format!("rrid={} 距离/配速组合得到的时长为 0", a.rrid));
    }
    let seed = if seed == 0 {
        (crate::crypto::envelope::now_ms() as u64) ^ (a.rrid as u64)
    } else {
        seed
    };
    let params = RunParams {
        dist,
        dur,
        start_ms: a.start_ms,
        face_check: plan.face_check,
        manual_altitude: None,
        manual_altitude_range: None,
        seed,
        route_mode,
    };
    log(&format!(
        "[recover] 覆盖 rrid={} ← {:.0}m / {}s / 配速 {}/km / 开始 {}",
        a.rrid,
        dist,
        dur,
        fmt_pace(pace),
        crate::track::geom::fmt_cst(a.start_ms, "%Y-%m-%d %H:%M:%S")
    ));
    flow::run_full_flow(client, &params, log)
}

pub fn fmt_pace(pace_s: f64) -> String {
    let p = pace_s.round() as i64;
    format!("{}:{:02}", p / 60, p % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ab(rrid: i64, dis: f64, ms: i64) -> Abnormal {
        Abnormal {
            rrid,
            start_ms: ms,
            total_dis: dis,
            total_time: 600,
            reasons: vec![],
        }
    }

    #[test]
    fn keeps_in_range_distance_and_clamps_out_of_range() {
        assert!((ab(1, 1234.0, 1).fix_dist() - 1234.0).abs() < 1e-9);
        assert!((ab(2, 5200.0, 1).fix_dist() - MAX_DIST).abs() < 1e-9);
        assert!((ab(3, 300.0, 1).fix_dist() - MIN_DIST).abs() < 1e-9);
        // 无距离时给下限，不留 0
        assert!((ab(4, 0.0, 1).fix_dist() - MIN_DIST).abs() < 1e-9);
    }

    #[test]
    fn explicit_plan_distance_is_clamped_to_compliance_band() {
        let a = ab(1, 1200.0, 1);
        assert!(
            (FixPlan {
                dist: 9000.0,
                pace_s: 0.0,
                face_check: 1
            }
            .resolved_dist(&a)
                - MAX_DIST)
                .abs()
                < 1e-9
        );
        assert!(
            (FixPlan {
                dist: 0.0,
                pace_s: 0.0,
                face_check: 1
            }
            .resolved_dist(&a)
                - 1200.0)
                .abs()
                < 1e-9
        );
    }

    #[test]
    fn pace_is_always_inside_band_even_when_operator_asks_for_extremes() {
        assert!(
            (FixPlan {
                dist: 0.0,
                pace_s: 60.0,
                face_check: 1
            }
            .resolved_pace()
                - MIN_PACE_S)
                .abs()
                < 1e-9
        );
        assert!(
            (FixPlan {
                dist: 0.0,
                pace_s: 9999.0,
                face_check: 1
            }
            .resolved_pace()
                - MAX_PACE_S)
                .abs()
                < 1e-9
        );
        for _ in 0..200 {
            let p = FixPlan::default().resolved_pace();
            assert!((MIN_PACE_S..=MAX_PACE_S).contains(&p), "随机配速越界: {p}");
        }
    }

    #[test]
    fn verdict_separates_record_level_from_ledger_level() {
        let clean = Diagnosis {
            cheat: CheatReport::default(),
            abnormal: vec![],
            total_records: 3,
            freeze_seconds: Some(0),
            freeze_message: String::new(),
        };
        assert!(!clean.flagged());
        assert!(!clean.frozen());
        assert_eq!(clean.freeze_text(), "未冻结");
        assert!(clean.verdict().contains("没有可修"));

        let record_only = Diagnosis {
            cheat: CheatReport::default(),
            abnormal: vec![ab(1, 1200.0, 1)],
            total_records: 3,
            freeze_seconds: Some(0),
            freeze_message: String::new(),
        };
        assert!(record_only.verdict().contains("最可修"));

        let flagged = Diagnosis {
            cheat: CheatReport::from_data(
                &serde_json::json!({"self": {"reason": "数据异常"}, "list": []}),
                crate::crypto::envelope::now_ms(),
            )
            .unwrap(),
            abnormal: vec![ab(1, 1200.0, 1)],
            total_records: 3,
            freeze_seconds: Some(0),
            freeze_message: String::new(),
        };
        assert!(flagged.flagged());
        let v = flagged.verdict();
        assert!(v.contains("台账"), "应点明台账边界: {v}");
        assert!(v.contains("数据异常"));
    }

    #[test]
    fn frozen_account_takes_precedence_over_record_repair_advice() {
        // 48 小时禁跑：结论必须先是「等」，而不是让人去点 --yes 撞服务端拒绝。
        let frozen = Diagnosis {
            cheat: CheatReport::default(),
            abnormal: vec![ab(1, 1200.0, 1), ab(2, 1300.0, 2)],
            total_records: 5,
            freeze_seconds: Some(172_800),
            freeze_message: "数据异常".into(),
        };
        assert!(frozen.frozen());
        assert_eq!(frozen.freeze_text(), "禁跑中，还剩 2 天 00:00:00");
        let v = frozen.verdict();
        assert!(v.contains("禁跑期"), "{v}");
        assert!(v.contains("2 天"), "{v}");
        assert!(
            v.contains("2 条待修"),
            "应保留待修条数供解冻后立即处理: {v}"
        );
        assert!(!v.contains("最可修"), "冻结时不该给「直接修」的结论: {v}");

        // 拿不到策略 = 状态未知，不能当成「未冻结」放行
        let unknown = Diagnosis {
            cheat: CheatReport::default(),
            abnormal: vec![ab(1, 1200.0, 1)],
            total_records: 3,
            freeze_seconds: None,
            freeze_message: String::new(),
        };
        assert!(!unknown.frozen());
        assert!(unknown.freeze_unknown());
        assert!(unknown.freeze_text().contains("未知"));
        let v = unknown.verdict();
        assert!(v.contains("未知"), "未知状态必须点明，不能默认放行: {v}");
        assert!(!v.contains("最可修"), "未知状态不该给「直接修」的结论: {v}");
        assert!(!v.contains("禁跑期"), "还没确认禁跑，不该断言在禁跑期: {v}");
    }

    #[test]
    fn missing_start_time_is_rejected_before_any_network_call() {
        // fix_one 的第一道守卫不依赖网络；用空 Abnormal 验证错误信息可读。
        let bad = ab(7, 1200.0, 0);
        let msg = match (bad.start_ms <= 0)
            .then(|| format!("rrid={} 缺少开始时刻，无法原位覆盖", bad.rrid))
        {
            Some(m) => m,
            None => unreachable!(),
        };
        assert!(msg.contains("rrid=7"));
    }
}
