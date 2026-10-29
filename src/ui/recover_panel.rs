//! 「异常记录诊断（实验）」：`recover` 的 App 内入口。
//!
//! 功能来自既有的 `src/api/recover.rs`（`diagnose()` / `fix_one()`），这里只做界面与
//! 后台任务编排，**不重写任何修复逻辑、不碰常量、不改提交载荷**。
//!
//! # 为什么这里以安全边界为主
//! 「红转绿」本质上是用原记录的开始时刻再提交一次合规轨迹，能否真的覆盖旧记录
//! **没有抓包证据**（见 `api::recover` 模块注释）。所以界面必须：
//! - 默认只读：进页面只做诊断，不提交；
//! - 单条：一次只提交一条，没有批量、没有「一键全绿」；
//! - 二次确认：文案写明「覆盖假设未验证，失败可能产生同日两条记录」；
//! - 禁跑中（`Diagnosis::frozen()`）把试修按钮置灰：CLI 也是先拦住，不让用户白挨一次风控计数；
//! - 冻结状态未知（策略接口没取到）按「不可提交」处理 —— 未知不等于未冻结；
//! - 每提交过一条就必须重新诊断，否则「连点几条」在效果上就是批量。
//!
//! 文案不另写一套：禁跑状态、判断结论、记录原因都直接取 `api::recover` 里
//! `freeze_text()` / `verdict()` / `reason_text()` 的产出，与 CLI 打印的是同一批字符串。

use super::{mobile, theme, App};
use eframe::egui;
use serde::{Deserialize, Serialize};

pub const RECOVER_TITLE: &str = "异常记录诊断（实验）";

/// 面板上的固定警示（第 2 条要求：明确标出覆盖假设）。
pub const COVER_ASSUMPTION: &str =
    "此功能依赖假设：同日同 sportType 提交会覆盖旧记录（未抓包验证）";

/// 二次确认弹窗里必须出现的那句话。
pub const CONFIRM_RISK: &str = "覆盖假设未验证，失败可能产生同日两条记录";

/// 诊断快照：`api::recover::Diagnosis` 不能跨线程/序列化，这里把**展示所需**的字段
/// 取出来；其中 `freeze_text` / `verdict` / `reason_text` 是调用 `Diagnosis` 自己的
/// 方法得到的原文，保证与 CLI 完全一致。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DiagSnapshot {
    /// `None` = 策略接口没取到（未知），不是「未冻结」。
    pub freeze_seconds: Option<i64>,
    pub freeze_message: String,
    pub freeze_text: String,
    pub verdict: String,
    pub flagged: bool,
    pub cheat_label: String,
    pub cheat_reason: String,
    pub cheat_time: String,
    pub total_records: usize,
    pub rows: Vec<RowSnapshot>,
    pub checked_at_ms: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RowSnapshot {
    pub rrid: i64,
    pub start_ms: i64,
    pub total_dis: f64,
    pub total_time: i64,
    pub reasons: Vec<String>,
    /// `Abnormal::reason_text()` 的原文：没取到详情时是「详情未取到（原因未知）」。
    pub reason_text: String,
}

impl RowSnapshot {
    /// 时间列，与 CLI 用的同一个格式化函数（`%Y-%m-%d %H:%M`）。
    pub fn time_text(&self) -> String {
        crate::track::geom::fmt_cst(self.start_ms, "%Y-%m-%d %H:%M")
    }
}

impl DiagSnapshot {
    pub fn from_diagnosis(d: &crate::api::recover::Diagnosis) -> Self {
        Self {
            freeze_seconds: d.freeze_seconds,
            freeze_message: d.freeze_message.clone(),
            freeze_text: d.freeze_text(),
            verdict: d.verdict(),
            flagged: d.flagged(),
            cheat_label: d.cheat.label().to_string(),
            cheat_reason: d.cheat.reason(),
            cheat_time: d.cheat.occurred_at(),
            total_records: d.total_records,
            rows: d
                .abnormal
                .iter()
                .map(|a| RowSnapshot {
                    rrid: a.rrid,
                    start_ms: a.start_ms,
                    total_dis: a.total_dis,
                    total_time: a.total_time,
                    reasons: a.reasons.clone(),
                    reason_text: a.reason_text(),
                })
                .collect(),
            checked_at_ms: crate::crypto::envelope::now_ms(),
        }
    }

    /// 与 `Diagnosis::frozen()` 同口径：拿不到策略不算冻结（但也不算未冻结）。
    pub fn frozen(&self) -> bool {
        self.freeze_seconds.unwrap_or(0) > 0
    }

    pub fn freeze_unknown(&self) -> bool {
        self.freeze_seconds.is_none()
    }

    pub fn row(&self, rrid: i64) -> Option<&RowSnapshot> {
        self.rows.iter().find(|r| r.rrid == rrid)
    }

    /// 还原成 `fix_one` 需要的结构 —— 提交时仍走 `api::recover` 的原逻辑。
    pub fn to_abnormal(&self, rrid: i64) -> Option<crate::api::recover::Abnormal> {
        self.row(rrid).map(|r| crate::api::recover::Abnormal {
            rrid: r.rrid,
            start_ms: r.start_ms,
            total_dis: r.total_dis,
            total_time: r.total_time,
            reasons: r.reasons.clone(),
        })
    }

    /// 快照是否已超过违规自查的新鲜窗口（只影响展示，不影响任何提交判断）。
    pub fn stale(&self) -> bool {
        let now = crate::crypto::envelope::now_ms();
        now.saturating_sub(self.checked_at_ms) >= crate::api::cheat::FRESH_MS
    }
}

/// 单条修复的复查结果（修复后**再读一次原记录**）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RecheckReport {
    pub complete: Option<bool>,
    pub reasons: Vec<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FixReport {
    pub rrid: i64,
    pub new_rrid: i64,
    /// 本次提交服务端给的当场判定（`Verification::label()`）。
    pub verdict: String,
    pub complete: Option<bool>,
    pub failure_reasons: Vec<String>,
    pub recheck: Option<RecheckReport>,
    pub error: Option<String>,
}

impl FixReport {
    /// 覆盖假设是否看得见地成立：原记录复查达标 = Some(true)。
    pub fn turned_green(&self) -> Option<bool> {
        let recheck = self.recheck.as_ref()?;
        if recheck.error.is_some() {
            return None;
        }
        recheck.complete
    }

    /// 结论行。不说「已修复」——能确定的只有「这次提交的判定」与「原记录复查」。
    pub fn conclusion(&self) -> String {
        if let Some(error) = &self.error {
            return format!(
                "× rrid={} 本次提交失败：{error}（是否已在服务端留痕不确定，请先重新诊断）",
                self.rrid
            );
        }
        match self.turned_green() {
            Some(true) => format!(
                "√ rrid={} 复查已达标：这是覆盖假设成立的有效证据，但仍以服务端违规榜为准",
                self.rrid
            ),
            Some(false) => format!(
                "⚠ rrid={} 复查仍未达标：覆盖可能没生效（假设不成立），或服务端判定条件未满足",
                self.rrid
            ),
            None => format!(
                "？ rrid={} 复查读不到原记录（可能已被覆盖成新 rrid={}）：请重新诊断确认",
                self.rrid, self.new_rrid
            ),
        }
    }
}

/// 单条试修的闸门结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FixGate {
    Ready,
    Blocked(String),
}

impl FixGate {
    pub fn is_ready(&self) -> bool {
        matches!(self, FixGate::Ready)
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            FixGate::Blocked(reason) => Some(reason),
            FixGate::Ready => None,
        }
    }
}

/// 面板状态机。所有判断都是纯函数式入口，测试不需要网络也不需要 egui。
#[derive(Default)]
pub struct RecoverPage {
    pub snapshot: Option<DiagSnapshot>,
    pub diag_busy: bool,
    pub diag_error: Option<String>,
    pub fix_busy: bool,
    pub fix_rrid: Option<i64>,
    pub fix_result: Option<FixReport>,
    /// 二次确认弹窗当前挂着的那一条（唯一一条）。
    pub confirm: Option<i64>,
    /// 已经提交过一次：必须重新诊断才能再选下一条。
    ///
    /// 这条状态是「不做批量」的执行机制：没有它，用户连点 N 次「试修」在效果上
    /// 就是一次批量覆盖，只是把批量从代码里挪到了手指上。
    pub needs_rediagnose: bool,
}

impl RecoverPage {
    pub fn busy(&self) -> bool {
        self.diag_busy || self.fix_busy
    }

    pub fn begin_diag(&mut self) {
        self.diag_busy = true;
        self.diag_error = None;
        self.confirm = None;
    }

    pub fn apply_diag(&mut self, snapshot: DiagSnapshot) {
        self.diag_busy = false;
        self.diag_error = None;
        self.snapshot = Some(snapshot);
        self.needs_rediagnose = false;
        self.confirm = None;
    }

    pub fn apply_diag_error(&mut self, error: String) {
        self.diag_busy = false;
        self.diag_error = Some(error);
        self.snapshot = None;
        self.confirm = None;
    }

    /// 能否对某一条动手。顺序即优先级：先挡「不可能提交」，再挡「不该提交」。
    pub fn decide_fix(&self, rrid: i64, logged_in: bool) -> FixGate {
        if !logged_in {
            return FixGate::Blocked("请先登录".into());
        }
        if self.diag_busy {
            return FixGate::Blocked("只读诊断进行中：等它跑完再选记录".into());
        }
        if self.fix_busy {
            return FixGate::Blocked("上一条试修还在进行：每次只能提交一条".into());
        }
        let Some(snapshot) = &self.snapshot else {
            return FixGate::Blocked("先点「只读诊断」：没有诊断结果不提交".into());
        };
        if snapshot.frozen() {
            return FixGate::Blocked(format!(
                "账号处于禁跑期（{}）：服务端会拒绝提交，等倒计时归零再试",
                snapshot.freeze_text
            ));
        }
        if snapshot.freeze_unknown() {
            return FixGate::Blocked(
                "冻结状态未知（策略接口这次没取到）：无法确认账号没被禁跑，先重新诊断".into(),
            );
        }
        // 快照过期就等于拿旧状态去提交：冻结可能是这几分钟里才生效的。
        // CLI 每次都是现读策略，界面必须用同一标准，不能靠「几分钟前没冻结」赌。
        if snapshot.stale() {
            return FixGate::Blocked(
                "诊断结果已过期（超过 5 分钟）：冻结状态可能已变，先重新诊断".into(),
            );
        }
        if self.needs_rediagnose {
            return FixGate::Blocked(
                "上一条已提交：请先重新诊断确认结果，再决定下一条（不做连续批量）".into(),
            );
        }
        if snapshot.row(rrid).is_none() {
            return FixGate::Blocked("这条记录已不在未达标列表：先重新诊断".into());
        }
        FixGate::Ready
    }

    /// 打开二次确认（不改任何提交状态）。
    pub fn open_confirm(&mut self, rrid: i64, logged_in: bool) -> Result<(), String> {
        match self.decide_fix(rrid, logged_in) {
            FixGate::Ready => {
                self.confirm = Some(rrid);
                Ok(())
            }
            FixGate::Blocked(reason) => Err(reason),
        }
    }

    pub fn cancel_confirm(&mut self) {
        self.confirm = None;
    }

    /// 二次确认通过，进入提交阶段。
    pub fn begin_fix(&mut self, rrid: i64) {
        self.confirm = None;
        self.fix_busy = true;
        self.fix_rrid = Some(rrid);
        self.fix_result = None;
    }

    /// 提交结束（成功或失败都算「动过一次」）。
    ///
    /// 失败也要重新诊断：`fix_one` 的 Err 大多发生在提交之前，但「提交在飞时断线」
    /// 只有服务端知道结果，这时继续点下一条就是拿风控计数赌运气。
    pub fn apply_fix(&mut self, report: FixReport) {
        self.fix_busy = false;
        self.fix_rrid = None;
        self.fix_result = Some(report);
        self.needs_rediagnose = true;
    }

    /// 登录失效：所有在飞的提交都不再有回执，状态必须一起收掉，否则按钮永远灰着。
    pub fn session_lost(&mut self) {
        self.diag_busy = false;
        self.fix_busy = false;
        self.fix_rrid = None;
        self.confirm = None;
        self.snapshot = None;
        self.needs_rediagnose = false;
        self.diag_error = Some("登录已失效，请重新登录后诊断".into());
    }
}

/// 面板上要执行的动作：先画完再把动作抛出去，避免在遍历快照时可变借用 App。
enum PanelAction {
    Diagnose,
    Confirm(i64),
}

impl App {
    /// 「数据」页里的折叠区内容。
    pub fn draw_recover_section(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new(COVER_ASSUMPTION).color(theme::warn()));
        ui.label(
            egui::RichText::new(
                "只读优先：诊断不提交任何记录；试修一次只提交一条，且必须先二次确认。",
            )
            .color(theme::text_dim()),
        );
        ui.label(egui::RichText::new(format!(
            "命令行等价：recover（只诊断）/ recover --only <rrid> --yes（单条试修）；合规距离区间 {:.0}–{:.0} m 与 CLI 共用同一常量。",
            crate::api::recover::MIN_DIST,
            crate::api::recover::MAX_DIST
        )).small().color(theme::text_dim()));

        let logged_in = self.session.is_some();
        let busy = self.data_page.recover.busy();
        let mut action = None;
        mobile::row(ui, |ui| {
            if ui
                .add_enabled(!busy && logged_in, theme::primary_btn("只读诊断"))
                .clicked()
            {
                action = Some(PanelAction::Diagnose);
            }
            if self.data_page.recover.diag_busy {
                ui.label("诊断中…（后台线程，界面可继续操作）");
            }
            if self.data_page.recover.fix_busy {
                ui.label(format!(
                    "单条试修提交中：rrid={}",
                    self.data_page.recover.fix_rrid.unwrap_or(0)
                ));
            }
            if !logged_in {
                ui.colored_label(theme::err(), "未登录：诊断与试修都不可用");
            }
        });

        if let Some(error) = &self.data_page.recover.diag_error {
            ui.colored_label(theme::err(), format!("诊断失败：{error}"));
        }

        // 快照复制一份再遍历：动作要可变借用 App，不能一边借一边改。
        let snapshot = self.data_page.recover.snapshot.clone();
        let Some(snapshot) = snapshot else {
            ui.label(egui::RichText::new("（还没有诊断结果）").color(theme::text_dim()));
            self.apply_recover_action(action);
            return;
        };

        // ── 现状（字段与顺序按 CLI 的「—— 现状 ——」段）────────────
        ui.add_space(4.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(format!(
                "禁跑状态：{}{}",
                snapshot.freeze_text,
                if snapshot.freeze_message.is_empty() {
                    String::new()
                } else {
                    format!("（服务端文案：{}）", snapshot.freeze_message)
                }
            ));
            ui.label(format!("违规榜：{}", snapshot.cheat_label));
            if snapshot.flagged {
                ui.label(format!(
                    "  原因：{}；发生时间：{}",
                    snapshot.cheat_reason, snapshot.cheat_time
                ));
            }
            ui.label(format!(
                "记录：共 {} 条，其中未达标 {} 条",
                snapshot.total_records,
                snapshot.rows.len()
            ));
            let mut checked = format!(
                "诊断时间：{}",
                crate::track::geom::fmt_cst(snapshot.checked_at_ms, "%Y-%m-%d %H:%M:%S 北京时间")
            );
            if snapshot.stale() {
                checked.push_str("（快照已过期，请重新诊断）");
            }
            ui.label(egui::RichText::new(checked).color(theme::text_dim()));
        });
        ui.add_space(4.0);
        ui.label("判断：");
        ui.label(&snapshot.verdict);

        // ── 修复闸门：为什么所有按钮都灰着 ────────────────────────
        let gate = match snapshot.rows.first() {
            Some(first) => self.data_page.recover.decide_fix(first.rrid, logged_in),
            None => FixGate::Blocked("没有未达标记录：当前没有可修的东西".into()),
        };
        if let Some(reason) = gate.reason() {
            ui.colored_label(theme::warn(), format!("单条试修已置灰：{reason}"));
        }
        if snapshot.frozen() {
            ui.colored_label(
                theme::err(),
                "禁跑期间服务端会独立拒绝提交：客户端强行提交只会白挨一次风控计数，所以这里不给点。",
            );
        }
        if self.data_page.recover.needs_rediagnose {
            ui.colored_label(
                theme::warn(),
                "已提交过一条：先重新诊断确认结果，再决定下一条（这是「不做批量」的执行机制）。",
            );
        }

        // ── 异常记录列表 ─────────────────────────────────────────
        if snapshot.rows.is_empty() {
            ui.label("（无未达标记录）");
        } else {
            let ready_rows: Vec<RowSnapshot> = snapshot.rows.clone();
            mobile::list_area(ui, "recover_rows", 240.0, |ui| {
                if mobile::compact_ui(ui) {
                    for row in &ready_rows {
                        let row_gate = self.data_page.recover.decide_fix(row.rrid, logged_in);
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            ui.label(format!("rrid={} · {}", row.rrid, row.time_text()));
                            ui.label(format!("{:.0} m / {}s", row.total_dis, row.total_time));
                            ui.label(format!("原因：{}", row.reason_text));
                            if ui
                                .add_enabled(
                                    row_gate.is_ready(),
                                    theme::primary_btn(&format!("试修 rrid={}", row.rrid)),
                                )
                                .clicked()
                            {
                                action = Some(PanelAction::Confirm(row.rrid));
                            }
                        });
                        ui.add_space(4.0);
                    }
                } else {
                    egui::Grid::new("recover_grid")
                        .num_columns(6)
                        .striped(true)
                        .show(ui, |ui| {
                            ui.strong("rrid");
                            ui.strong("时间");
                            ui.strong("距离");
                            ui.strong("时长");
                            ui.strong("原因");
                            ui.strong("操作");
                            ui.end_row();
                            for row in &ready_rows {
                                let row_gate =
                                    self.data_page.recover.decide_fix(row.rrid, logged_in);
                                ui.monospace(row.rrid.to_string());
                                ui.label(row.time_text());
                                ui.monospace(format!("{:.0} m", row.total_dis));
                                ui.monospace(format!("{}s", row.total_time));
                                ui.label(&row.reason_text);
                                if ui
                                    .add_enabled(
                                        row_gate.is_ready(),
                                        theme::primary_btn(&format!("试修 rrid={}", row.rrid)),
                                    )
                                    .clicked()
                                {
                                    action = Some(PanelAction::Confirm(row.rrid));
                                }
                                ui.end_row();
                            }
                        });
                }
            });
        }

        // ── 上一次试修的结果 ─────────────────────────────────────
        if let Some(report) = self.data_page.recover.fix_result.clone() {
            ui.add_space(4.0);
            egui::CollapsingHeader::new(format!("上次单条试修结果（rrid={}）", report.rrid))
                .default_open(true)
                .show(ui, |ui| {
                    if report.error.is_none() {
                        ui.label(format!(
                            "原记录 rrid={} → 本次提交 rrid={}",
                            report.rrid, report.new_rrid
                        ));
                        ui.label(format!("本次提交服务端判定：{}", report.verdict));
                        for reason in &report.failure_reasons {
                            ui.label(format!("  仍未达标：{reason}"));
                        }
                        match &report.recheck {
                            Some(recheck) => {
                                if let Some(error) = &recheck.error {
                                    ui.colored_label(
                                        theme::warn(),
                                        format!("复查原记录失败：{error}"),
                                    );
                                } else {
                                    ui.label(format!(
                                        "复查原记录：{}",
                                        match recheck.complete {
                                            Some(true) => "已达标",
                                            Some(false) => "仍未达标",
                                            None => "状态未知",
                                        }
                                    ));
                                    for reason in &recheck.reasons {
                                        ui.label(format!("  仍未达标：{reason}"));
                                    }
                                }
                            }
                            None => {
                                ui.label("复查原记录：未执行");
                            }
                        }
                    }
                    let color = if report.turned_green() == Some(true) {
                        theme::ok()
                    } else {
                        theme::warn()
                    };
                    ui.colored_label(color, report.conclusion());
                });
        }

        self.apply_recover_action(action);
    }

    fn apply_recover_action(&mut self, action: Option<PanelAction>) {
        match action {
            Some(PanelAction::Diagnose) => self.start_recover_diag(),
            Some(PanelAction::Confirm(rrid)) => {
                if let Err(reason) = self
                    .data_page
                    .recover
                    .open_confirm(rrid, self.session.is_some())
                {
                    self.status = reason;
                }
            }
            None => {}
        }
    }

    /// 二次确认弹窗。挂在 shell 层（与 `popup` 同级），所以切页也不会让它消失。
    pub fn draw_recover_confirm(&mut self, ctx: &egui::Context) {
        let Some(rrid) = self.data_page.recover.confirm else {
            return;
        };
        let row = self
            .data_page
            .recover
            .snapshot
            .as_ref()
            .and_then(|s| s.row(rrid).cloned());
        let mut cancel = false;
        let mut confirm = false;
        let mut open = true;
        egui::Window::new(egui::RichText::new(format!("确认单条试修 rrid={rrid}")).strong())
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                ui.set_max_width((ctx.screen_rect().width() - 32.0).min(520.0));
                ui.colored_label(theme::err(), CONFIRM_RISK);
                if let Some(row) = &row {
                    ui.label(format!(
                        "将用原记录的开始时刻重新生成合规轨迹并提交，期望覆盖 rrid={}（{} / {:.0} m / {}s）。",
                        row.rrid,
                        row.time_text(),
                        row.total_dis,
                        row.total_time
                    ));
                    ui.label(format!("原记录原因：{}", row.reason_text));
                }
                ui.label("假设：同日同 sportType 提交会覆盖旧记录 —— 未抓包验证。");
                ui.label("本次只提交这一条；提交会消耗一次真实网络请求与风控计数。");
                ui.add_space(6.0);
                // 两个普通按钮：Android 没有右键/长按，确认不能藏在下拉或上下文菜单里。
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add_sized(
                            [ui.available_width().min(160.0), 40.0],
                            egui::Button::new("取消"),
                        )
                        .clicked()
                    {
                        cancel = true;
                    }
                    if ui
                        .add_sized(
                            [ui.available_width().min(200.0), 40.0],
                            theme::primary_btn("确认覆盖提交"),
                        )
                        .clicked()
                    {
                        confirm = true;
                    }
                });
            });
        if cancel || !open {
            self.data_page.recover.cancel_confirm();
        } else if confirm {
            self.start_recover_fix(rrid);
        }
    }

    /// 只读诊断：网络与记录遍历全在后台线程。
    pub fn start_recover_diag(&mut self) {
        if self.data_page.recover.diag_busy {
            return;
        }
        let Some(session) = self.session.clone() else {
            self.status = "请先登录".into();
            return;
        };
        let identity = self.identity.clone();
        self.data_page.recover.begin_diag();
        self.status = "正在只读诊断（禁跑状态 / 违规榜 / 未达标记录）…".into();
        #[cfg(test)]
        let route_fixture = self.route_fixture.clone();
        self.spawn_session_job(move |tx| {
            let mut log = App::logger(tx.clone());
            let mut client = tx.client(identity, Some(session));
            #[cfg(test)]
            if let Some((host, data_dir)) = route_fixture {
                client.set_test_host(host);
                crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = Some(data_dir));
            }
            let payload = match crate::api::recover::diagnose(&mut client, &mut log) {
                Ok(diag) => {
                    let snapshot = DiagSnapshot::from_diagnosis(&diag);
                    log(&format!(
                        "[recover] 诊断完成：禁跑 {}；违规榜 {}；未达标 {}/{} 条（只读，未提交任何记录）",
                        snapshot.freeze_text,
                        snapshot.cheat_label,
                        snapshot.rows.len(),
                        snapshot.total_records
                    ));
                    serde_json::json!({ "ok": true, "diag": snapshot })
                }
                Err(e) => {
                    log(&format!("× [recover] 诊断失败: {e}"));
                    serde_json::json!({ "ok": false, "error": e })
                }
            };
            tx.send(format!("{}{payload}", super::RECOVER)).ok();
        });
    }

    /// 单条试修：闸门在 UI 线程判定，提交与复查在后台线程。
    pub fn start_recover_fix(&mut self, rrid: i64) {
        let logged_in = self.session.is_some();
        match self.data_page.recover.decide_fix(rrid, logged_in) {
            FixGate::Ready => {}
            FixGate::Blocked(reason) => {
                self.status = reason;
                self.data_page.recover.cancel_confirm();
                return;
            }
        }
        let Some(abnormal) = self
            .data_page
            .recover
            .snapshot
            .as_ref()
            .and_then(|s| s.to_abnormal(rrid))
        else {
            self.status = "这条记录已不在未达标列表：先重新诊断".into();
            return;
        };
        let Some(session) = self.session.clone() else {
            self.status = "请先登录".into();
            return;
        };
        let identity = self.identity.clone();
        // 距离/配速沿用 `FixPlan::default()`（0 = 保持原距离或夹到合规区间，
        // 配速在合规区间内随机），不在这里另设一套参数。
        let plan = crate::api::recover::FixPlan {
            face_check: if self.config.face_check { 1 } else { 0 },
            ..Default::default()
        };
        let route_mode = crate::track::generate_road::RouteMode::from_str(&self.config.route_mode);
        #[cfg(test)]
        let route_fixture = self.route_fixture.clone();
        self.data_page.recover.begin_fix(rrid);
        self.status = format!("单条试修正提交 rrid={rrid}…");
        self.spawn_session_job(move |tx| {
            let mut log = App::logger(tx.clone());
            let mut client = tx.client(identity, Some(session));
            #[cfg(test)]
            if let Some((host, data_dir)) = route_fixture {
                client.set_test_host(host);
                crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = Some(data_dir));
            }
            let payload = match crate::api::recover::fix_one(
                &mut client,
                &abnormal,
                plan,
                route_mode,
                0,
                &mut log,
            ) {
                Ok(outcome) => {
                    let recheck = recheck_one(&mut client, abnormal.rrid, &mut log);
                    serde_json::json!({
                        "ok": true,
                        "rrid": abnormal.rrid,
                        "new_rrid": outcome.result.rrid,
                        "verdict": outcome.verification.label(),
                        "complete": outcome.verification.complete,
                        "failure_reasons": outcome.verification.failure_reasons(),
                        "recheck": recheck,
                    })
                }
                Err(e) => {
                    log(&format!("× [recover] rrid={} 提交失败: {e}", abnormal.rrid));
                    serde_json::json!({ "ok": false, "rrid": abnormal.rrid, "error": e })
                }
            };
            tx.send(format!("{}{payload}", super::RECOVER_FIX)).ok();
        });
    }
}

/// 修复后自动复查**这一条**：重读原 rrid 的详情。
///
/// 不用整轮 `diagnose()`：那会把所有记录再翻一遍（请求量翻倍、时间更长），
/// 而这里只需要回答一个问题 ——「原记录现在是否达标」。若覆盖已生效导致原 rrid
/// 读不到，如实返回错误，由界面提示重新诊断。
pub(crate) fn recheck_one(
    client: &mut crate::api::client::ApiClient,
    rrid: i64,
    log: &mut dyn FnMut(&str),
) -> RecheckReport {
    match crate::api::records::fetch_detail(client, rrid) {
        Ok(detail) => {
            let reasons: Vec<String> = detail
                .reason_list
                .iter()
                .filter(|r| !r.complete)
                .map(|r| r.reason.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            // 与 `Verification::from_result` 同口径：reasonList 里任一 false 就是未达标，
            // 哪怕顶层的 complete 自相矛盾。
            let complete = if reasons.is_empty() {
                Some(detail.complete)
            } else {
                Some(false)
            };
            log(&format!(
                "[recover] rrid={rrid} 复查：{}",
                match complete {
                    Some(true) => "已达标",
                    Some(false) => "仍未达标",
                    None => "未知",
                }
            ));
            RecheckReport {
                complete,
                reasons,
                error: None,
            }
        }
        Err(e) => {
            log(&format!("× [recover] rrid={rrid} 复查失败: {e}"));
            RecheckReport {
                complete: None,
                reasons: Vec::new(),
                error: Some(e),
            }
        }
    }
}
