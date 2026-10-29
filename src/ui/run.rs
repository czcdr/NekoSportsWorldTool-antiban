//! 跑步页：恒 sportType=1；距离/配速范围 + 开始时间（随机或指定，最多前 3 天）。
//!
//! 开始时间：「日期」下拉（今天 / 1-3 天前）两种模式共用；随机模式在该日 7:00-20:00
//! 内抽样，并保证「开始 + 用时」不越过当前时刻；指定模式完全按用户填写的时/分
//! （尚未到达时按当前时刻），「换一版」只重掷运动量，不动时刻。

use super::map::MapState;
use super::{mobile, theme, App};
use crate::track::generate_road::RouteMode;
use chrono::{Datelike, Duration, Local, TimeZone, Timelike};
use eframe::egui;

/// 随机时刻窗口（含端点）：7:00-20:00。
const RAND_HOUR_LO: u32 = 7;
const RAND_HOUR_HI: u32 = 20;
/// flow.rs 提交时会给开始时间加 0-4s 抖动，随机模式抽样时预留，避免「开始 + 用时」越过当前时刻。
const JITTER_MARGIN_MS: i64 = 5_000;

/// 指定日期（days_ago 天前）的 7:00-20:00 内均匀抽样一个时刻，且不晚于 latest_ms。
///
/// latest_ms = 当前时刻 - 用时 - 抖动余量。若该日窗口整体晚于 latest_ms（例如凌晨选「今天」），
/// 退化为 latest_ms，宁可贴近当前时间，也不产生「未来开跑」。
fn random_time_ago(days_ago: i64, latest_ms: i64) -> i64 {
    let base = Local::now() - Duration::days(days_ago.clamp(0, 3));
    let lo = Local
        .with_ymd_and_hms(base.year(), base.month(), base.day(), RAND_HOUR_LO, 0, 0)
        .single();
    let hi = Local
        .with_ymd_and_hms(base.year(), base.month(), base.day(), RAND_HOUR_HI, 0, 0)
        .single();
    let (lo_ms, hi_ms) = match (lo, hi) {
        (Some(l), Some(h)) => (l.timestamp_millis(), h.timestamp_millis()),
        _ => return latest_ms,
    };
    let hi_ms = hi_ms.min(latest_ms);
    let lo_ms = lo_ms.min(hi_ms);
    let span = (hi_ms - lo_ms).max(0) as f64;
    lo_ms + (rand::random::<f64>() * span) as i64
}

/// 指定时刻：days_ago(0=今天) + 时/分；超出 3 天或在未来时做钳制。
fn specified_time(days_ago: i64, hour: i64, minute: i64) -> i64 {
    let now = Local::now();
    let base = now - Duration::days(days_ago.clamp(0, 3));
    let h = hour.clamp(0, 23);
    let m = minute.clamp(0, 59);
    let t = Local
        .with_ymd_and_hms(base.year(), base.month(), base.day(), h as u32, m as u32, 0)
        .single()
        .map(|x| x.timestamp_millis())
        .unwrap_or_else(crate::crypto::envelope::now_ms);
    t.min(now.timestamp_millis())
}

/// 日期下拉的显示文本。
fn days_ago_label(days_ago: i64) -> String {
    match days_ago {
        0 => "今天".into(),
        1 => "昨天".into(),
        n => format!("{n} 天前"),
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct PreviewInputs {
    pub uid: i64,
    pub unid: String,
    pub anchor: crate::location::Coordinate,
    pub points: Vec<(f64, f64)>,
    pub fences: Vec<Vec<(f64, f64)>>,
    pub loaded_ms: i64,
}

#[derive(Default)]
pub struct RunPage {
    pub dist_min: f32,
    pub dist_max: f32,
    pub pace_min: f32,
    pub pace_max: f32,
    /// 手动绝对海拔（米）；空白表示使用生成器默认海拔。
    pub manual_altitude: String,
    /// 学校基准海拔（米）的输入缓冲；空白 = 未配置（`config.school_base_altitude == None`）。
    ///
    /// 这是 flow.rs「未配置学校基准海拔」警告在 App 内的出口：手机上不用连命令行
    /// 也能填一次，之后所有记录都按它生成（优先级低于手动海拔）。
    pub school_base_altitude: String,
    /// GPS 抖动幅度（单点稳态标准差，米）；0 = 旧的自动区间 2.8–4.2 m。
    pub gps_spread_m: f64,
    /// 上传锚点的计划级随机偏移（米）；0 = 精确锚点。
    pub anchor_offset_m: f64,
    /// 0=随机时刻 1=指定时刻
    pub start_mode: usize,
    pub days_ago: i64,
    pub hour: i64,
    pub minute: i64,
    pub face_check: bool,
    /// 本页参数有待落盘（`RunSettings` 快照变化后置真，写完清零）。
    ///
    /// 这些控件原来只在「开始跑步」时写 config.json：勾了 faceCheck 却直接退出，
    /// 重启就回到默认值。现在改动即标记，松手/输入提交后立刻落盘。
    pub settings_dirty: bool,
    /// 预计算方案：参数变更时重抽样，提交直接使用
    pub plan: Option<RunPlan>,
    /// 用户**显式钉住**的计划种子（复现用）；`None` = 每次重新抽。
    ///
    /// 故意**不落盘**：固定种子一旦写进 config.json，之后每次跑步都会复刻同一条
    /// 记录 —— 那正是本模块 `clear_plan_cache` 要消灭的形态。它只活在本次会话里。
    pub fixed_seed: Option<u64>,
    /// 路线算法模式
    pub route_mode: RouteMode,
    /// 地图视图状态
    pub map: MapState,
    /// 瓦片底图（显示层，默认关闭；与 `map` 相互独立，失败不影响矢量绘制）
    pub tiles: super::tiles::TileLayer,
    /// 路网预览（真实道路路由）
    pub preview: Option<crate::track::generate_road::RoadPlan>,
    /// 预览是否过期（参数变更后置真）
    pub preview_stale: bool,
    pub preview_inputs: Option<PreviewInputs>,
    pub check: Option<route_planner::check::Check>,
    pub progress: f32,
    pub preview_busy: bool,
    pub preview_cancel: std::sync::Arc<crate::task_control::Control>,
    pub preview_key: String,
    /// 上传锚点偏移改过、等松手后重取点位（拖动中不重取，见 `on_anchor_offset_changed`）。
    pub anchor_offset_dirty: bool,
    pub preview_changed_at: Option<std::time::Instant>,
    pub preview_status: String,
    /// 地图视野是否已适配
    pub map_fitted: bool,
    /// Request whose WGS84 base map currently owns the viewport; None means BD09 preview.
    pub base_map_request: Option<u64>,
}

/// 跑步页里会持久化进 `config.json` 的那部分参数。
///
/// 这些控件原先只在「开始跑步」时才写盘：勾了 faceCheck 不提交直接退出，
/// 下次启动仍是默认值（用户实测复现）。改为每帧比对快照，真正变化就标脏，
/// 松手或输入提交后立刻落盘——Android 的数字/文本输入走原生对话框，
/// 拿不到 `Response::changed()`，所以只能靠快照比对。
#[derive(Clone, Debug, PartialEq)]
pub struct RunSettings {
    pub dist_min: f32,
    pub dist_max: f32,
    pub pace_min: f32,
    pub pace_max: f32,
    pub face_check: bool,
    pub route_mode: RouteMode,
    pub manual_altitude: String,
    /// 学校基准海拔（米）；空白 = 未配置。与 `manual_altitude` 并存时后者优先。
    pub school_base_altitude: String,
    pub gps_spread_m: f64,
    pub anchor_offset_m: f64,
}

impl RunSettings {
    pub fn from_page(page: &RunPage) -> Self {
        Self {
            dist_min: page.dist_min,
            dist_max: page.dist_max,
            pace_min: page.pace_min,
            pace_max: page.pace_max,
            face_check: page.face_check,
            route_mode: page.route_mode,
            manual_altitude: page.manual_altitude.clone(),
            school_base_altitude: page.school_base_altitude.clone(),
            gps_spread_m: page.gps_spread_m,
            anchor_offset_m: page.anchor_offset_m,
        }
    }

    /// 写进配置。手动海拔解析失败时其余字段照写、海拔保留原值并回传错误，
    /// 避免「填错一个字符 → 整页设置都没保存」。
    pub fn apply_to(&self, config: &mut crate::api::model::Config) -> Result<(), String> {
        config.dist_min = self.dist_min;
        config.dist_max = self.dist_max;
        config.pace_min = self.pace_min;
        config.pace_max = self.pace_max;
        config.face_check = self.face_check;
        config.route_mode = self.route_mode.as_str().into();
        // 两个米制参数在写入前按各自上限归一，界面/磁盘/生成器读到的是同一个值。
        config.gps_spread_m = if self.gps_spread_m.is_finite() {
            self.gps_spread_m
                .clamp(0.0, crate::api::model::GPS_SPREAD_MAX_M)
        } else {
            config.gps_spread_m
        };
        config.anchor_offset_m = if self.anchor_offset_m.is_finite() {
            self.anchor_offset_m
                .clamp(0.0, crate::api::model::ANCHOR_OFFSET_MAX_M)
        } else {
            config.anchor_offset_m
        };
        match crate::track::altitude::parse_spec(&self.manual_altitude) {
            Ok(None) => {
                config.manual_altitude = None;
                config.manual_altitude_range = None;
            }
            Ok(Some(crate::track::altitude::AltitudeSpec::Single(value))) => {
                config.manual_altitude = Some(value);
                config.manual_altitude_range = None;
            }
            Ok(Some(crate::track::altitude::AltitudeSpec::Range(range))) => {
                config.manual_altitude = None;
                config.manual_altitude_range = Some(range);
            }
            Err(error) => return Err(error),
        }
        // 学校基准海拔：空 = 未配置；非法值保留配置里的原值并回传错误，
        // 不静默丢弃（否则用户填了 10500 却以为已经存上）。
        match parse_school_altitude(&self.school_base_altitude) {
            Ok(None) => config.school_base_altitude = None,
            Ok(Some(value)) => config.school_base_altitude = Some(value),
            Err(error) => return Err(error),
        }
        Ok(())
    }
}

/// 解析「学校基准海拔（米）」输入框。
///
/// 空 = 未配置（`None`，回落到默认 82 m）；范围与
/// `track::altitude` 的校验完全一致（`ALTITUDE_MIN_M..=ALTITUDE_MAX_M`），
/// 非法时返回错误文本——调用方保留旧值，并把这句话显示在行内与状态栏。
pub(crate) fn parse_school_altitude(text: &str) -> Result<Option<f64>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let value: f64 = text
        .parse()
        .map_err(|_| format!("学校基准海拔「{text}」不是数字，未保存"))?;
    if !value.is_finite()
        || !(crate::track::altitude::ALTITUDE_MIN_M..=crate::track::altitude::ALTITUDE_MAX_M)
            .contains(&value)
    {
        return Err(format!(
            "学校基准海拔 {value} 超出 {}–{} 米，未保存（原值保留）",
            crate::track::altitude::ALTITUDE_MIN_M as i64,
            crate::track::altitude::ALTITUDE_MAX_M as i64
        ));
    }
    Ok(Some(value))
}

/// 一次提交的确定方案（进入页面/参数变更时抽样生成）。
#[derive(Debug, Clone)]
pub struct RunPlan {
    pub dist_min: f32,
    pub dist_max: f32,
    pub pace_min: f32,
    pub pace_max: f32,
    pub start_mode: usize,
    pub days_ago: i64,
    pub hour: i64,
    pub minute: i64,
    /// 公里
    pub dist: f64,
    /// 秒/km
    pub pace: f32,
    /// 秒
    pub dur: i64,
    pub start_ms: i64,
    /// 本次方案的随机种子（预览与提交共用；提交时仍须重新校验策略和点位）
    pub seed: u64,
}

impl RunPage {
    /// 距离/配速是否与方案一致（不一致需重掷运动参数）。
    fn shape_matches(&self, p: &RunPlan) -> bool {
        p.dist_min == self.dist_min
            && p.dist_max == self.dist_max
            && p.pace_min == self.pace_min
            && p.pace_max == self.pace_max
    }

    /// 开始时间控件是否与方案一致。
    ///
    /// 随机模式的时刻是抽样结果（不是输入），只比对日期；指定模式的时/分是用户输入，需回比。
    fn time_matches(&self, p: &RunPlan) -> bool {
        if p.start_mode != self.start_mode || p.days_ago != self.days_ago {
            return false;
        }
        self.start_mode == 0 || (p.hour == self.hour && p.minute == self.minute)
    }

    /// 开跑时刻上限：当前时刻 - 用时 - 抖动余量（flow.rs 提交时还会再加 0-4s）。
    fn latest_start_ms(dur: i64) -> i64 {
        crate::crypto::envelope::now_ms() - dur * 1000 - JITTER_MARGIN_MS
    }

    /// 按当前参数抽样运动量（距离 / 配速 / 用时）。
    fn sample_shape(&self) -> (f64, f32, i64) {
        let (lo, hi) = (
            self.dist_min.min(self.dist_max),
            self.dist_min.max(self.dist_max),
        );
        let (plo, phi) = (
            self.pace_min.min(self.pace_max),
            self.pace_min.max(self.pace_max),
        );
        let pace = plo + (phi - plo) * rand::random::<f32>();
        let dist = (lo + (hi - lo) * rand::random::<f32>()) as f64;
        let dur = (dist * pace as f64).round() as i64;
        (dist, pace, dur)
    }

    /// 按当前模式与日期取一个开始时刻（随机模式抽样，指定模式采纳输入框）。
    ///
    /// C1：**两种模式都要按「现在 − 用时 − 抖动余量」封顶**。
    /// 原来只有随机模式走 `latest_start_ms`，指定模式直接采纳输入框
    /// （`specified_time` 只 `.min(now)`），于是「现在 12:05、填 12:00、用时 30 分钟」
    /// 会得到 stopTime = 12:30 —— 一段落在未来的记录。真机不可能提交还没跑完的记录。
    ///
    /// 用户填的那一刻仍然优先：只有它会越过上限时才回退到上限（同一天内、不跨日）。
    fn pick_time(&self) -> i64 {
        let latest = Self::latest_start_ms(self.dur_hint());
        if self.start_mode == 0 {
            random_time_ago(self.days_ago, latest)
        } else {
            specified_time(self.days_ago, self.hour, self.minute).min(latest)
        }
    }

    /// 用于随机抽样的时长上限：已抽样的方案优先，否则按当前配速区间上界估一个。
    fn dur_hint(&self) -> i64 {
        match &self.plan {
            Some(p) => p.dur,
            None => {
                let hi = self.dist_min.max(self.dist_max) as f64;
                let phi = self.pace_min.max(self.pace_max) as f64;
                (hi * phi).round() as i64
            }
        }
    }

    /// 运动参数变更时重抽距离/配速/用时，保留已选开始时刻（指定模式的时刻是用户输入，不动）。
    fn regen_shape(&mut self) {
        let (dist, pace, dur) = self.sample_shape();
        let start_ms = match (&self.plan, self.start_mode) {
            // 随机模式沿用已抽样的时刻（仅按新用时收紧上限），避免拖动距离时时刻乱跳
            (Some(p), 0) => p.start_ms.min(Self::latest_start_ms(dur)),
            _ => self.pick_time(),
        };
        self.write_plan(dist, pace, dur, start_ms);
    }

    /// 只重算开始时刻，保留已抽样的运动量（日期 / 模式变更时用）。
    fn resync_time(&mut self) {
        let start_ms = self.pick_time();
        if let Some(p) = self.plan.as_mut() {
            p.start_mode = self.start_mode;
            p.days_ago = self.days_ago;
            p.hour = self.hour;
            p.minute = self.minute;
            p.start_ms = start_ms;
        }
    }

    /// 把当前参数与已算好的量写成方案。
    fn write_plan(&mut self, dist: f64, pace: f32, dur: i64, start_ms: i64) {
        self.plan = Some(RunPlan {
            dist_min: self.dist_min,
            dist_max: self.dist_max,
            pace_min: self.pace_min,
            pace_max: self.pace_max,
            start_mode: self.start_mode,
            days_ago: self.days_ago,
            hour: self.hour,
            minute: self.minute,
            dist,
            pace,
            dur,
            start_ms,
            // 用户钉住就照用（复现用）；否则每次重新抽。
            seed: self.fixed_seed.unwrap_or_else(rand::random::<u64>),
        });
        self.preview_stale = true;
        // 种子换了 ⇒ 计划级锚点偏移跟着换 ⇒ 已经拉回来的点位不再属于这份计划：
        // 预览画的是「旧锚点拉的点」，提交却会按新锚点重拉（缓存按锚点索引，必然 miss），
        // 两边打卡点就对不上了。所以这里标记「待重取」，真正的网络请求留在 UI
        // 松手之后触发（见 `draw_run_content` 末尾对 `anchor_offset_dirty` 的消费），
        // 拖动滑块/连续改参数时不会每帧打一次点位接口。
        //
        // 偏移关着（默认 0）时不用重取：那时锚点就是精确值，点位与种子无关。
        // 钉住种子时也不用重取：`plan_seed_for_anchor` 让预览与提交都吃同一个
        // fixed_seed，偏移恒定（见该函数的注释）。
        if self.fixed_seed.is_none() && self.effective_anchor_offset_m() > 0.0 {
            self.anchor_offset_dirty = true;
        }
    }

    /// 派生锚点偏移用的计划种子 —— **预览与提交必须用同一个来源**。
    ///
    /// 提交侧的规则是 `write_plan` 里的 `fixed_seed.unwrap_or(random)`；
    /// 预览侧（`refresh_school_route`）在「当前还没有方案」时必须按同一规则回落，
    /// 而不是一律用 0。
    ///
    /// 反例（可复现）：开偏移 50 + 勾固定种子（勾选会 `clear_plan_cache`，此刻
    /// 还没有新方案）+ 立刻点『刷新路线点位与围栏』—— 若这里回落成 0，
    /// 预览按 `seed=0` 派生锚点、提交按 `fixed_seed` 派生另一个，
    /// 两边锚点差 40–60 m，预览的打卡点与提交的打卡点就对不上。
    pub fn plan_seed_for_anchor(&self) -> u64 {
        self.plan
            .as_ref()
            .map(|p| p.seed)
            .or(self.fixed_seed)
            .unwrap_or(0)
    }

    /// 当前生效的上传锚点偏移（米）—— **UI 值是唯一真值**。
    ///
    /// - 预览：`refresh_school_route` 用它算点位请求的锚点；
    /// - 提交：`start_run` 把它写进 config，`flow` 再从 config 读同一个数。
    ///
    /// 两处同源才不会有「预览按 A 锚点拉点位、提交按 B 锚点重拉」这种错位。
    /// 非有限值一律当 0（= 关闭偏移），宁可保守也不用脏值去挪上传坐标。
    pub fn effective_anchor_offset_m(&self) -> f64 {
        if self.anchor_offset_m.is_finite() {
            self.anchor_offset_m
                .clamp(0.0, crate::api::model::ANCHOR_OFFSET_MAX_M)
        } else {
            0.0
        }
    }

    /// 让「这一份方案」失效 —— **整个程序里唯一该清方案缓存的地方**。
    ///
    /// 为什么必须清：预览与提交共用同一份 `RunPlan`（start_ms / seed / 距离 / 配速），
    /// 这是「预览画的就是要提交的那条」的前提。但这张缓存一旦活到下一次点击，
    /// 同参数再跑一次就会逐字段复刻上一条记录（开始时间、轨迹、步频、总步数全一样）——
    /// 平台上最容易被判成「同一条记录重复提交」的形态，ScienseRunner v1.0.10 的公告
    /// 里专门修的就是这个。
    ///
    /// 清掉的是「这一次抽到的东西」，**不是用户的选择**：距离/配速区间、路线模式、
    /// `fixed_seed` 全部保留；下一次 `ensure_plan` 用同一组参数重新抽取距离/配速/
    /// 种子/开始时间（t4 的锚点偏移跟着新 seed 一起重抽）。
    ///
    /// 调用点只有两处，都必须在「没有提交在飞」的时候：
    /// - `poll_messages` 的 `__RUN_DONE__` 处理末尾（成功 / 部分成功 / 失败都走同一处）；
    /// - 用户改「固定种子」开关时。
    ///
    /// **提交进行中禁止调用**：清了它，预览与提交就不是同一份方案了。
    pub(crate) fn clear_plan_cache(&mut self) {
        self.preview_cancel.cancel();
        self.plan = None;
        self.preview = None;
        self.check = None;
        self.preview_key.clear();
        self.preview_stale = true;
    }

    /// 「换一版」：重掷运动量；随机模式下同时换一个开始时刻。
    ///
    /// 指定模式的时刻由用户填写，按钮不碰它（用户填什么就是什么）。
    pub fn regen_plan(&mut self) {
        let (dist, pace, dur) = self.sample_shape();
        let start_ms = if self.start_mode == 0 {
            random_time_ago(self.days_ago, Self::latest_start_ms(dur))
        } else {
            specified_time(self.days_ago, self.hour, self.minute)
        };
        self.write_plan(dist, pace, dur, start_ms);
    }

    /// 路线算法变了：把已经算好的预览标脏并丢掉。
    ///
    /// 三件事缺一不可，否则用户看到的就是「切了模式没反应」：
    /// - `preview_stale = true`：不置真后台不会重算（原来只有 plan 重掷/路网刷新会置）；
    /// - 清 `preview` / `check`：不清就会拿**上一种模式**的结果继续画这一种模式的线；
    /// - 清 `preview_key`：`prepare_preview` 的去重键里没有模式，键不变它就不会重来一遍，
    ///   旧结果回来时还会被当成这次的结果收下。
    ///
    /// 在飞的预览任务用 `cancel` 叫停：它的结果会因 key 不匹配被丢弃（见
    /// `poll_preview_results`），不必在这里改 `preview_busy`。
    pub(crate) fn on_route_mode_changed(&mut self) {
        self.preview_cancel.cancel();
        self.preview_key.clear();
        self.preview = None;
        self.check = None;
        self.preview_stale = true;
        self.map_fitted = false;
        self.base_map_request = None;
    }

    /// GPS 抖动幅度变了：已算好的预览是按旧幅度画的，必须重新规划。
    ///
    /// 与切模式同理，三件事缺一不可 —— 尤其 `preview_key.clear()`：
    /// `prepare_preview` 的去重键里没有幅度，键不变它就认为结果还新鲜，
    /// 用户会看着「改了幅度没反应」。
    pub(crate) fn on_gps_spread_changed(&mut self) {
        self.preview_cancel.cancel();
        self.preview_key.clear();
        self.preview = None;
        self.check = None;
        self.preview_stale = true;
    }

    /// 上传锚点偏移变了：点位是用**偏移后的锚点**拉回来的，旧点位不能再用。
    ///
    /// 这里只作废预览并置脏，真正重取点位留给松手后（`flush_refresh_on_settle`）：
    /// 拖动滑块时每帧重取会打爆点位接口。
    pub(crate) fn on_anchor_offset_changed(&mut self) {
        self.preview_cancel.cancel();
        self.preview_key.clear();
        self.preview = None;
        self.check = None;
        self.preview_stale = true;
        self.anchor_offset_dirty = true;
    }

    /// 确保方案与当前参数一致（参数变更补齐 / 提交前兜底）。
    ///
    /// 两个一致性判定都在改写方案「之前」求值：否则 regen_shape 会把当前参数写进方案，
    /// 同帧内「距离 + 日期」一起改时日期变化将被漏掉。
    pub fn ensure_plan(&mut self) {
        let shape_ok = self.plan.as_ref().map(|p| self.shape_matches(p)) == Some(true);
        let time_ok = self.plan.as_ref().map(|p| self.time_matches(p)) == Some(true);
        if !shape_ok {
            self.regen_shape();
        }
        if !time_ok {
            self.resync_time();
        }
    }

    /// 随机模式的抽样结果是否落在所选日期的 7:00-20:00 窗口内。
    ///
    /// 凌晨选「今天」时窗口尚未到达，`random_time_ago` 会退化到贴近当前时刻，
    /// 此时界面需要说明，避免用户以为刻意选了凌晨。
    fn random_window_ok(&self) -> bool {
        let Some(p) = &self.plan else { return true };
        let Some(t) = Local.timestamp_millis_opt(p.start_ms).single() else {
            return true;
        };
        let day = (Local::now() - Duration::days(self.days_ago.clamp(0, 3))).date_naive();
        t.date_naive() == day && (RAND_HOUR_LO..=RAND_HOUR_HI).contains(&t.hour())
    }
}

impl App {
    pub fn draw_run(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.draw_run_content(ui);
            });
    }

    fn draw_run_content(&mut self, ui: &mut egui::Ui) {
        let prev_route = self.run_page.route_mode;
        // 矮屏上把行距收到 2px：本页有 8~10 行参数，默认行距白吃 ~10px，
        // 而 A1 要求主操作进首屏——这是最便宜的一档。
        ui.spacing_mut().item_spacing.y = 2.0;
        // 进入本帧前的参数快照：画完比对，变了就标脏（见 draw_run_content 末尾）。
        let settings_before = RunSettings::from_page(&self.run_page);
        let gps_spread_before = self.run_page.gps_spread_m;
        let anchor_offset_before = self.run_page.anchor_offset_m;
        // C10：先把「这条记录会报多少米、依据是什么」算好再进渲染块
        // （块内 `page` 是 &mut self.run_page，不能再借 self.config）。
        // 界面不写来源，用户就不知道自己报的是 82 m 还是校区高程。
        let (base_alt_m, base_alt_origin) = self.config.resolved_base_altitude();
        let altitude_hint = format!(
            "留空自动；可填 17.2 或 11.6-22.8 · 当前生效：{}",
            crate::track::altitude::describe_base_altitude(base_alt_m, base_alt_origin)
        );
        // 学校基准海拔：flow.rs「未配置学校基准海拔」警告在 App 内的出口。
        // 优先级与 `track::altitude::resolve_base_altitude` 逐条一致；
        // 同时把当前 origin 摆出来，避免「填了学校基准、手动海拔却在生效」看不出来。
        let school_altitude_hint = format!(
            "留空 = 未配置。优先级：手动海拔（单值/区间）> 学校基准 > 默认 {:.0} m。当前生效：{}",
            crate::track::altitude::DEFAULT_BASE_ALTITUDE_M,
            crate::track::altitude::describe_base_altitude(base_alt_m, base_alt_origin)
        );
        {
            let page = &mut self.run_page;
            let compact = mobile::compact_ui(ui);
            let mut draw_distance_inputs = |ui: &mut egui::Ui| {
                mobile::drag_f32(
                    ui,
                    "run_dist_min",
                    &mut page.dist_min,
                    0.5..=20.0,
                    0.05,
                    2,
                    " km",
                );
                ui.label("至");
                mobile::drag_f32(
                    ui,
                    "run_dist_max",
                    &mut page.dist_max,
                    0.5..=20.0,
                    0.05,
                    2,
                    " km",
                );
            };
            if compact {
                ui.label("距离范围（km）：");
                ui.horizontal(draw_distance_inputs);
            } else {
                ui.horizontal(|ui| {
                    ui.label("距离范围（km）：");
                    draw_distance_inputs(ui);
                });
            }
            let mut draw_pace_inputs = |ui: &mut egui::Ui| {
                mobile::drag_f32(
                    ui,
                    "run_pace_min",
                    &mut page.pace_min,
                    180.0..=520.0,
                    5.0,
                    0,
                    "",
                );
                ui.label("至");
                mobile::drag_f32(
                    ui,
                    "run_pace_max",
                    &mut page.pace_max,
                    180.0..=520.0,
                    5.0,
                    0,
                    "",
                );
            };
            if compact {
                ui.label("配速范围（秒/km）：");
                ui.horizontal(draw_pace_inputs);
            } else {
                ui.horizontal(|ui| {
                    ui.label("配速范围（秒/km）：");
                    draw_pace_inputs(ui);
                });
            }
            // 高级生成参数：都不进协议字段、且都有「0/空 = 旧行为」的默认值，
            // 默认收起，把首屏让给主操作（A5，见 review/UI-IMPL.md）。
            egui::CollapsingHeader::new("高级生成参数（不填即用默认值）")
                .default_open(false)
                .show(ui, |ui| {
                    mobile::row(ui, |ui| {
                        ui.label("手动海拔（米/区间）：");
                        mobile::text_edit(
                            ui,
                            "run_manual_altitude",
                            &mut page.manual_altitude,
                            crate::platform::InputKind::Text,
                            120.0,
                        );
                    });
                    ui.colored_label(theme::text_dim(), &altitude_hint);
                    // ── 学校基准海拔（t10）──────────────────────────────
                    // flow.rs 的「未配置学校基准海拔」警告需要一个 App 内出口：
                    // 手机上没法跑 `run --school-altitude`，也不能靠「手动海拔」顶替
                    // （手动海拔是本次覆盖，学校基准是填一次长期生效）。
                    mobile::row(ui, |ui| {
                        ui.label("学校基准海拔（米）：");
                        let response = mobile::text_edit(
                            ui,
                            "run_school_base_altitude",
                            &mut page.school_base_altitude,
                            crate::platform::InputKind::Decimal,
                            120.0,
                        );
                        // 行内状态：填了才算「已配置」，空着就是 None（回落到默认）。
                        let state = match parse_school_altitude(&page.school_base_altitude) {
                            Ok(None) => ("未配置", theme::text_dim()),
                            Ok(Some(_)) => ("已配置", theme::ok()),
                            Err(_) => ("未生效", theme::err()),
                        };
                        response.on_hover_text(
                            "留空 = 未配置（回落到默认 82 m）。填一次后所有记录都按它生成。",
                        );
                        ui.colored_label(state.1, state.0);
                    });
                    ui.colored_label(theme::text_dim(), &school_altitude_hint);
                    if let Err(error) = parse_school_altitude(&page.school_base_altitude) {
                        ui.colored_label(theme::err(), format!("学校基准海拔未生效：{error}"));
                    }
                    ui.colored_label(
                        theme::text_dim(),
                        "提交结论里的「未配置学校基准海拔」警告，就是让你在这里填。",
                    );
                    // 每行只放一个数值框 + 说明另起一行：原来「抖动 / 分隔符 / 锚点偏移」
                    // 挤在同一个不换行的 horizontal 里，640 宽下溢出 76 px（A4 横向裁切）。
                    mobile::row(ui, |ui| {
                        ui.label("GPS 抖动（米）：");
                        mobile::drag_f64(
                            ui,
                            "run_gps_spread",
                            &mut page.gps_spread_m,
                            0.0..=crate::api::model::GPS_SPREAD_MAX_M,
                            0.1,
                            1,
                        );
                    });
                    ui.colored_label(
                        theme::text_dim(),
                        "0 = 旧行为（道路/瓦片：每 seed 2.8–4.2 m；经典：固定 1.08 m）",
                    );
                    mobile::row(ui, |ui| {
                        ui.label("锚点偏移（米）：");
                        mobile::drag_f64(
                            ui,
                            "run_anchor_offset",
                            &mut page.anchor_offset_m,
                            0.0..=crate::api::model::ANCHOR_OFFSET_MAX_M,
                            1.0,
                            0,
                        );
                    });
                    ui.colored_label(
                        theme::text_dim(),
                        "默认 0（关闭）；开启后每份计划偏移约 40–60 m，可能让点位更贴近围栏边缘",
                    );
                    // 固定种子（复现用，不落盘）：默认不钉，每次重新抽。
                    mobile::row(ui, |ui| {
                        let mut pinned = page.fixed_seed.is_some();
                        if ui.checkbox(&mut pinned, "固定种子").changed() {
                            page.fixed_seed = pinned.then_some(page.fixed_seed.unwrap_or(7));
                            // 抽取规则变了：旧方案是按旧规则算的，重来一份。
                            page.clear_plan_cache();
                        }
                        if let Some(seed) = page.fixed_seed.as_mut() {
                            let mut value = (*seed).min(i64::MAX as u64) as i64;
                            mobile::drag_i64(
                                ui,
                                "run_fixed_seed",
                                &mut value,
                                0..=i64::MAX,
                                1.0,
                                "",
                                "",
                            );
                            *seed = value.max(0) as u64;
                        }
                    });
                    ui.colored_label(theme::text_dim(), "不勾 = 每次重新抽取（提交后也会重抽）");
                    // 协议开关也归到「高级」：它有 config 默认值，收起不影响提交内容。
                    mobile::row(ui, |ui| {
                        ui.label("人脸校验标记：");
                        ui.checkbox(&mut page.face_check, "faceCheck=1");
                    });
                });
            mobile::row(ui, |ui| {
                ui.label("开始时间：");
                ui.radio_value(&mut page.start_mode, 0, "随机时刻");
                ui.radio_value(&mut page.start_mode, 1, "指定时刻");
            });
            // 日期下拉单独一行：它跟单选按钮挤在同一行时，egui 的
            // `horizontal_wrapped` 拿不到 ComboBox 的期望宽度，360 宽下会溢出 20 px
            // 并被页面右边界裁掉（A4 横向裁切）。
            mobile::row(ui, |ui| {
                ui.label("日期：");
                egui::ComboBox::from_id_salt("run_days_ago")
                    .width(96.0)
                    .selected_text(days_ago_label(page.days_ago))
                    .show_ui(ui, |ui| {
                        for d in 0..=3 {
                            ui.selectable_value(&mut page.days_ago, d, days_ago_label(d));
                        }
                    });
                if page.start_mode == 0 {
                    ui.label(format!("（{RAND_HOUR_LO}:00-{RAND_HOUR_HI}:00 内随机）"));
                } else {
                    ui.label("时刻：");
                    mobile::drag_i64(ui, "run_hour", &mut page.hour, 0..=23, 1.0, "", " 点");
                    ui.label(":");
                    mobile::drag_i64(ui, "run_minute", &mut page.minute, 0..=59, 1.0, "", "");
                }
            });
            if page.start_mode == 1 && page.days_ago == 0 {
                // 今天 + 指定时刻：提示是否落在未来
                let now = Local::now();
                let spec = Local
                    .with_ymd_and_hms(
                        now.year(),
                        now.month(),
                        now.day(),
                        page.hour as u32,
                        page.minute as u32,
                        0,
                    )
                    .single();
                if let Some(t) = spec {
                    if t.timestamp_millis() > crate::crypto::envelope::now_ms() {
                        ui.colored_label(
                            theme::warn(),
                            "指定时刻在今天且尚未到达，将按当前时间提交",
                        );
                    }
                }
            }
            ui.horizontal_wrapped(|ui| {
                ui.label("路线算法：");
                // 四个模式名收短（原「经典打卡点环/真实道路路由/自定义点位/瓦片矢量化（实验）」
                // 在 360 宽下要占两行）：含义由紧随其后的说明句承担，键位不变、默认值不变。
                ui.selectable_value(&mut page.route_mode, RouteMode::Legacy, "经典");
                ui.selectable_value(&mut page.route_mode, RouteMode::Road, "道路");
                ui.selectable_value(&mut page.route_mode, RouteMode::Manual, "自定义")
                    .on_hover_text(
                        "只用实时点位顺序串联，不要求经过策略必经点；没有路网时按点位连线",
                    );
                ui.selectable_value(&mut page.route_mode, RouteMode::Tiles, "瓦片")
                    .on_hover_text(
                        "用百度栅格瓦片自己识别校内道路并建图：不依赖 OSM 导入，\
                         但结果取决于底图清晰度（文字遮挡/隧道/高架会断线）",
                    );
                // 说清「现在是什么模式、还缺什么」：只靠三个按钮名，用户不知道自己
                // 选的东西眼下能不能出图。
                let (label, missing) = match page.route_mode {
                    RouteMode::Legacy => ("经典：按点位拟合，不画道路地图", None),
                    RouteMode::Road => (
                        "真实道路：沿校内道路行进，需要学校路网",
                        self.network.is_none().then_some("路网未就绪"),
                    ),
                    RouteMode::Manual => (
                        "自定义点位：按点位顺序串联，有路网时沿道路",
                        self.network.is_none().then_some("无路网，将按点位连线"),
                    ),
                    RouteMode::Tiles => (
                        "瓦片矢量化：从百度瓦片识别道路后沿路行进",
                        self.network.is_none().then_some("瓦片路网未生成"),
                    ),
                };
                ui.colored_label(theme::plain(), format!("（{label}）"));
                if let Some(missing) = missing {
                    ui.colored_label(theme::warn(), format!("（{missing}）"));
                }
            });
            // 瓦片矢量化要先有图：按钮单独一行，和模式说明分开，避免「选了模式就自动抓图」的误解。
            if self.run_page.route_mode == RouteMode::Tiles {
                ui.horizontal_wrapped(|ui| {
                    let busy = self.osm_page.busy || self.run_busy;
                    if ui
                        .add_enabled(!busy, egui::Button::new("生成瓦片路网"))
                        .on_hover_text(
                            "按当前学校围栏（没有围栏则用定位锚点 ±700m）抓瓦片，\
                             识别道路后建图；结果缓存到数据目录，下次直接复用",
                        )
                        .clicked()
                    {
                        self.generate_tile_network();
                    }
                    if !self.osm_page.status.is_empty() {
                        ui.colored_label(theme::plain(), &self.osm_page.status);
                    }
                });
                // 源说明：默认 OSM 免鉴权；百度匿名口子已封，要用就把浏览器里抓到的
                // 完整瓦片 URL 模板贴进来（投影按 bdimg.com 自动切）。
                ui.colored_label(
                    theme::text_dim(),
                    "瓦片源：默认 OSM 栅格（免鉴权）。要用百度/自建源，把浏览器里抓到的完整\
                     瓦片 URL 模板粘到下面（需含 {x}/{y}/{z}；百度还要在 config.json 的 \
                     tile_headers 里补 Referer 等请求头），主机不在白名单时先在 \
                     tile_allowed_hosts 里加。",
                );
                ui.horizontal_wrapped(|ui| {
                    ui.label("URL 模板：");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.config.tile_url_template)
                            .hint_text("留空 = https://tile.openstreetmap.org/{z}/{x}/{y}.png")
                            .desired_width(320.0),
                    );
                });
            }
        }

        // 路线算法变了：先标脏 + 丢掉旧预览，再决定要不要重新拉学校数据。
        // 顺序不能反：refresh_points 会重新灌 preview_inputs，旧结果留在那儿会被
        // 当成新模式的预览画出来。
        if self.run_page.route_mode != prev_route {
            self.run_page.on_route_mode_changed();
        }
        // 切到需要学校数据的模式时按需拉取围栏/点位/路网（经典模式不触发这些端点）。
        if self.run_page.route_mode.needs_school_route() && !prev_route.needs_school_route() {
            self.refresh_points();
        }

        ui.add_space(2.0);
        let fmt_pace = |s: f32| format!("{}:{:02}", (s / 60.0) as i64, (s as i64) % 60);
        let fmt_dur = |s: i64| {
            if s >= 3600 {
                format!("{}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
            } else {
                format!("{}:{:02}", s / 60, s % 60)
            }
        };
        // 参数变更时补方案；显示本次提交的确定方案
        self.run_page.ensure_plan();
        self.draw_run_warnings(ui);
        let plan_label = match &self.run_page.plan {
            Some(p) => {
                let start = chrono::Local
                    .timestamp_millis_opt(p.start_ms)
                    .single()
                    .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_default();
                let (dist, pace, dur) = (p.dist, p.pace, p.dur);
                format!(
                    "本次方案：距离 {dist:.2} km · 配速 {}/km · 用时 {} · 开始 {start}",
                    fmt_pace(pace),
                    fmt_dur(dur)
                )
            }
            None => String::new(),
        };
        mobile::row(ui, |ui| {
            ui.colored_label(theme::plain(), plan_label);
        });

        // A1：主操作提到地图之前 —— 手机上原来「开始跑步」在纸面下 1.5 屏。
        // 心智顺序仍是「确认方案摘要（距离/配速/用时/开始时刻）→ 开始跑步 → 下方地图复核」；
        // 预览没出来时在旁边明说，而不是把按钮藏掉（藏掉会让人不知道能不能点）。
        // 「换一版」也并进这一行：矮屏上少一整行 ≈ 20 px。
        ui.add_space(2.0);
        if self.run_busy
            && ui
                .add_enabled(
                    self.run_control.cancellable(),
                    egui::Button::new("取消准备/轨迹生成"),
                )
                .clicked()
        {
            if self.run_control.cancel() {
                self.status = "已请求取消；等待当前请求/计算结束，取消后不会进入记录提交".into();
            }
        }
        let enabled = !self.run_busy && self.session.is_some();
        let btn = if self.run_busy {
            theme::primary_btn("提交中…")
        } else {
            theme::primary_btn("开始跑步")
        };
        mobile::action_row(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.add_enabled(enabled, btn).clicked() {
                    self.start_run();
                }
                if ui.small_button("换一版").clicked() {
                    self.run_page.regen_plan();
                }
                if self.session.is_none() {
                    ui.label("（请先登录）");
                }
                if self.run_page.route_mode != RouteMode::Legacy && self.run_page.preview.is_none()
                {
                    ui.colored_label(theme::text_dim(), "（下方地图暂无路线预览）");
                }
            });
        });

        self.draw_run_map(ui);

        // 改动即落盘：拖拽期间指针仍按着，先标脏；松手/输入提交后写一次。
        // 不再依赖「开始跑步」——那正是 faceCheck 勾了却丢失的原因。
        if RunSettings::from_page(&self.run_page) != settings_before {
            self.run_page.settings_dirty = true;
        }
        // 两个本地生成参数改了要作废预览：抖动幅度改的是轨迹点位置，
        // 锚点偏移改的是点位请求的坐标 —— 两者都不能拿旧预览继续画。
        if self.run_page.gps_spread_m != gps_spread_before {
            self.run_page.on_gps_spread_changed();
        }
        if self.run_page.anchor_offset_m != anchor_offset_before {
            self.run_page.on_anchor_offset_changed();
        }
        // 偏移只在**松手**后重取点位，避免拖动滑块时每一帧都发一次点位请求。
        if self.run_page.anchor_offset_dirty && !ui.ctx().is_using_pointer() {
            self.run_page.anchor_offset_dirty = false;
            if self.run_page.route_mode.needs_school_route() {
                self.refresh_points();
            }
        }
        if self.run_page.settings_dirty && !ui.ctx().is_using_pointer() {
            self.persist_run_settings();
        }
    }

    /// 把跑步页参数写进 config.json；写盘失败与海拔格式错误都走状态栏。
    fn persist_run_settings(&mut self) {
        let settings = RunSettings::from_page(&self.run_page);
        let outcome = settings.apply_to(&mut self.config);
        // 失败也清脏：否则磁盘满/目录只读时每帧重试，界面卡在报错上。
        self.run_page.settings_dirty = false;
        if let Err(e) = crate::ui::save_config(&self.config) {
            self.status = e;
        } else if let Err(e) = outcome {
            self.status = e;
        }
    }

    /// 开始时间相关的提示。须在 `ensure_plan()` 之后调用，才对得上本次提交时刻。
    fn draw_run_warnings(&self, ui: &mut egui::Ui) {
        let page = &self.run_page;
        if page.start_mode == 0 {
            if !page.random_window_ok() {
                ui.colored_label(
                    theme::warn(),
                    format!(
                        "所选日期的 {RAND_HOUR_LO}:00-{RAND_HOUR_HI}:00 尚未到达，本次只能贴近当前时刻",
                    ),
                );
            }
            return;
        }
        // 今天 + 指定时刻：提示是否落在未来（与原有行为一致）
        if page.days_ago != 0 {
            return;
        }
        let now = Local::now();
        if let Some(t) = Local
            .with_ymd_and_hms(
                now.year(),
                now.month(),
                now.day(),
                page.hour.clamp(0, 23) as u32,
                page.minute.clamp(0, 59) as u32,
                0,
            )
            .single()
        {
            if t.timestamp_millis() > crate::crypto::envelope::now_ms() {
                ui.colored_label(theme::warn(), "指定时刻在今天且尚未到达，将按当前时间提交");
            }
        }
    }

    fn draw_run_map(&mut self, ui: &mut egui::Ui) {
        let mode = self.run_page.route_mode;
        // 只有经典模式没有地图可画：它按点位顺序拟合，不涉及道路/建筑。
        // 这里原来是 `!= RouteMode::Road`，于是「自定义点位」也掉进这个分支 ——
        // 选了自定点位既不出图、提示还写着「经典模式」，用户看到的就是「点了没用」。
        if mode == RouteMode::Legacy {
            ui.label("经典模式仅按点位拟合，不包含建筑避让；需要沿道路行进时请选择真实道路模式。");
            return;
        }
        if ui
            .add_enabled(!self.osm_page.busy, egui::Button::new("刷新路线点位与围栏"))
            .clicked()
        {
            self.refresh_points();
        }
        if !self.osm_page.status.is_empty() {
            ui.label(&self.osm_page.status);
        }
        ui.horizontal_wrapped(|ui| {
            if ui.button("查看路网与校区").clicked() {
                self.tab = 7;
            }
            if self.identity.has_unconfigured_default_location()
                && ui.button("设置定位锚点").clicked()
            {
                self.tab = 5;
            }
        });
        let Some(net) = self.network.clone() else {
            // 两种模式缺路网的后果不同，提示也要分开说：
            // Road 拿不到路网就**交不了**，Manual 会退回按点位连线（能交，但不沿道路）。
            let status = if self.osm_page.status.is_empty() {
                "尚未加载".to_string()
            } else {
                self.osm_page.status.clone()
            };
            let hint = if mode == RouteMode::Manual {
                format!(
                    "学校路网尚未就绪（{status}）：自定义点位将按点位连线；\
                     要沿校内道路行进，请点『刷新路线点位与围栏』或到路网页获取。"
                )
            } else {
                format!(
                    "学校路网尚未就绪（{status}）：真实道路模式需要路网。\
                     请点『刷新路线点位与围栏』，或到路网页获取。"
                )
            };
            ui.colored_label(theme::warn(), hint);
            return;
        };
        let Some(plan) = self.run_page.plan.clone() else {
            return;
        };

        let inputs = self
            .run_page
            .preview_inputs
            .as_ref()
            .filter(|input| {
                self.osm_page.offline
                    || (self
                        .session
                        .as_ref()
                        .map(|s| s.uid == input.uid && s.unid == input.unid)
                        .unwrap_or(false)
                        && self
                            .identity
                            .anchor_coordinate()
                            .map(|a| a == input.anchor)
                            .unwrap_or(false)
                        && (0..crate::api::model::POINTS_TTL_MS)
                            .contains(&(crate::crypto::envelope::now_ms() - input.loaded_ms)))
            })
            .cloned();
        let Some(inputs) = inputs else {
            self.run_page.preview = None;
            self.run_page.check = None;
            let message = if self.osm_page.busy {
                "正在刷新学校围栏、策略与点位，请查看上方进度。"
            } else if self.session.is_none() && !self.osm_page.offline {
                "本地地图已加载；请先登录，再刷新路线点位与围栏。"
            } else if self.identity.has_unconfigured_default_location() && !self.osm_page.offline {
                "本地地图已加载；请设置定位锚点，再刷新路线点位与围栏。"
            } else if self.run_page.preview_inputs.is_some() {
                "路线数据已过期或账号/定位已改变，请刷新路线点位与围栏。"
            } else {
                "地图已加载，但路线数据未就绪。请查看上方原因并刷新路线点位与围栏。"
            };
            ui.colored_label(theme::warn(), message);
            self.draw_run_base_map(ui, &net);
            return;
        };
        // A base map uses WGS84, while route inputs and previews use BD09.
        if self.run_page.base_map_request.take().is_some() {
            self.run_page.map_fitted = false;
        }
        self.prepare_preview(ui, net.clone(), &inputs, &plan);

        if inputs.fences.is_empty() {
            ui.colored_label(
                theme::warn(),
                "学校边界未提供：按道路闭环重复里程，无法确认路线始终在校内。",
            );
        }

        if let Some(check) = &self.run_page.check {
            ui.label(format!(
                "路线检查：{} 个道路连通分量；按当前顺序完整闭环最短距离 {}",
                check.components,
                check
                    .minimum_m
                    .map(|m| format!("{m:.0} 米"))
                    .unwrap_or("未知".into())
            ));
            for issue in &check.issues {
                ui.colored_label(theme::warn(), &issue.message);
            }
        }
        if let Some(p) = &self.run_page.preview {
            ui.label(format!(
                "总距离 {:.2} km · 预计 {} 圈（返回起点计一圈，含末圈）",
                plan.dist,
                p.laps.len()
            ));
            ui.label(format!(
                "点位顺序：{}",
                (1..=p.checkpoints.len())
                    .map(|i| i.to_string())
                    .collect::<Vec<_>>()
                    .join(" → ")
            ));
            ui.collapsing("每圈长度", |ui| {
                for (i, length) in p.laps.iter().enumerate() {
                    ui.label(format!(
                        "第 {} 圈：{:.1} 米{}",
                        i + 1,
                        length,
                        if i + 1 == p.laps.len() {
                            "（可能为末圈）"
                        } else {
                            ""
                        }
                    ));
                }
            });
            ui.add(egui::Slider::new(&mut self.run_page.progress, 0.0..=1.0).text("沿路线进度"));
        }
        if self.run_page.preview.is_some() {
            super::map::legend(
                ui,
                &[
                    ("道路", egui::Color32::from_rgb(200, 208, 204)),
                    ("建筑", egui::Color32::from_rgb(224, 194, 170)),
                    ("路线", egui::Color32::from_rgb(30, 111, 216)),
                    ("打卡点", egui::Color32::from_rgb(240, 180, 0)),
                    ("起点", egui::Color32::from_rgb(22, 160, 90)),
                    ("终点", egui::Color32::from_rgb(220, 38, 38)),
                ],
            );
        }

        // 底图工具条在画布上方：切换即落盘（默认 off，不联网）。
        if super::tiles::toolbar(
            ui,
            &mut self.config.basemap,
            &mut self.run_page.tiles,
            self.config.map_cache_limit_mb,
        ) {
            if let Err(error) = crate::ui::save_config(&self.config) {
                self.status = error;
            }
        }
        ui.add_space(4.0);
        let rect = ui.available_rect_before_wrap();
        let (response, painter) = ui.allocate_painter(
            egui::Vec2::new(rect.width().max(200.0), 240.0),
            egui::Sense::drag(),
        );
        let canvas = response.rect;
        painter.rect_filled(
            canvas,
            egui::Rounding::ZERO,
            egui::Color32::from_rgb(250, 252, 251),
        );
        // 底图垫在矢量图下面；加载失败只写状态栏，绝不挡矢量绘制。
        // 放在 fit 之后：否则首帧的瓦片按「未适配的视野」画、矢量按新视野画，会闪一下。
        let basemap = super::tiles::BasemapSource::parse(&self.config.basemap);
        self.run_page
            .tiles
            .draw(&painter, ui.ctx(), &self.run_page.map, canvas, basemap);
        if let Some(message) = self.run_page.tiles.take_notice() {
            self.status = message;
        }

        // 首次预览后适配视野：优先以电子围栏为中点/范围，无围栏时退化为路线+打卡点+道路
        if !self.run_page.map_fitted {
            let mut bounds: Vec<(f64, f64)> = Vec::new();
            if let Some(p) = &self.run_page.preview {
                for f in &p.fences {
                    bounds.extend(f.iter().copied());
                }
                if bounds.is_empty() {
                    bounds = super::map::collect_bounds(&p.edges);
                    bounds.extend(p.route.iter().copied());
                    bounds.extend(p.checkpoints.iter().copied());
                }
            }
            if bounds.is_empty() {
                bounds.extend(inputs.fences.iter().flatten().copied());
                bounds.extend(inputs.points.iter().copied());
            }
            if !bounds.is_empty() {
                self.run_page.map.fit(canvas, &bounds);
                self.run_page.map_fitted = true;
            }
        }

        if let Some(p) = &self.run_page.preview {
            let road = egui::Color32::from_rgb(200, 208, 204);
            for e in p.edges.iter().step_by(if self.run_page.map.scale < 0.1 {
                (p.edges.len() / 4000).max(1)
            } else {
                1
            }) {
                self.run_page
                    .map
                    .draw_polyline(&painter, canvas, e, road, 1.0);
            }
            let bld = egui::Color32::from_rgb(224, 194, 170);
            for b in &p.buildings {
                self.run_page
                    .map
                    .draw_polygon(&painter, canvas, b, bld, 1.0);
            }
            let fence_c = egui::Color32::from_rgb(180, 118, 0);
            for f in &p.fences {
                self.run_page
                    .map
                    .draw_polygon(&painter, canvas, f, fence_c, 2.0);
            }
            let route_c = egui::Color32::from_rgb(30, 111, 216);
            self.run_page
                .map
                .draw_polyline(&painter, canvas, &p.route, route_c, 2.5);
            let cp = egui::Color32::from_rgb(240, 180, 0);
            for &(la, lo) in &p.checkpoints {
                self.run_page
                    .map
                    .draw_point(&painter, canvas, la, lo, cp, 3.5);
            }
            if let Some(first) = p.route.first() {
                self.run_page.map.draw_point(
                    &painter,
                    canvas,
                    first.0,
                    first.1,
                    egui::Color32::from_rgb(22, 160, 90),
                    4.5,
                );
            }
            if let Some(last) = p.route.last() {
                self.run_page.map.draw_point(
                    &painter,
                    canvas,
                    last.0,
                    last.1,
                    egui::Color32::from_rgb(220, 38, 38),
                    4.5,
                );
            }
            if let Some(pos) = crate::track::route_view::position(
                &p.route,
                &p.arcs,
                plan.dist * 1000. * self.run_page.progress as f64,
            ) {
                self.run_page.map.draw_point(
                    &painter,
                    canvas,
                    pos.0,
                    pos.1,
                    egui::Color32::from_rgb(170, 20, 200),
                    7.0,
                );
            }
            let label = if p.length_m > 0.0 {
                let loops = plan.dist * 1000.0 / p.length_m;
                if loops > 1.15 {
                    format!(
                        "本次方案 {:.2} km · 单圈 {:.0} m × {:.1} 圈 · {} 打卡点",
                        plan.dist,
                        p.length_m,
                        loops,
                        p.checkpoints.len()
                    )
                } else {
                    format!(
                        "本次方案 {:.2} km · 路线 {:.0} m · {} 打卡点",
                        plan.dist,
                        p.length_m,
                        p.checkpoints.len()
                    )
                }
            } else {
                format!(
                    "本次方案 {:.2} km · {} 打卡点",
                    plan.dist,
                    p.checkpoints.len()
                )
            };
            painter.text(
                egui::Pos2::new(canvas.left() + 8.0, canvas.top() + 8.0),
                egui::Align2::LEFT_TOP,
                label,
                egui::FontId::proportional(12.0),
                egui::Color32::from_rgb(88, 104, 99),
            );
        } else {
            painter.text(
                canvas.center(),
                egui::Align2::CENTER_CENTER,
                "（无预览）",
                egui::FontId::proportional(14.0),
                egui::Color32::from_rgb(150, 160, 156),
            );
        }

        if self.run_page.preview.is_none() {
            for f in &inputs.fences {
                self.run_page.map.draw_polygon(
                    &painter,
                    canvas,
                    f,
                    egui::Color32::from_rgb(180, 118, 0),
                    2.0,
                );
            }
            for &(lat, lon) in &inputs.points {
                self.run_page
                    .map
                    .draw_point(&painter, canvas, lat, lon, egui::Color32::YELLOW, 4.);
            }
        }
        if let Some(check) = &self.run_page.check {
            for issue in &check.issues {
                if let Some(pos) = issue.position {
                    self.run_page.map.draw_point(
                        &painter,
                        canvas,
                        pos.lat,
                        pos.lon,
                        egui::Color32::RED,
                        7.,
                    );
                    painter.text(
                        self.run_page.map.to_screen(canvas, pos.lat, pos.lon),
                        egui::Align2::LEFT_BOTTOM,
                        issue
                            .point
                            .map(|i| format!("点位 {}", i + 1))
                            .unwrap_or("距离不足".into()),
                        egui::FontId::proportional(12.),
                        egui::Color32::RED,
                    );
                }
            }
        }
        self.run_page.map.interact(ui, canvas);
    }

    fn draw_run_base_map(&mut self, ui: &mut egui::Ui, net: &route_planner::RoadGraph) {
        ui.label("基础地图：道路与建筑已加载，路线尚未生成。可拖拽和滚轮缩放。");
        super::map::legend(
            ui,
            &[
                ("道路", egui::Color32::from_rgb(190, 200, 196)),
                ("建筑", egui::Color32::from_rgb(214, 182, 158)),
            ],
        );
        if super::tiles::toolbar(
            ui,
            &mut self.config.basemap,
            &mut self.run_page.tiles,
            self.config.map_cache_limit_mb,
        ) {
            if let Err(error) = crate::ui::save_config(&self.config) {
                self.status = error;
            }
        }
        let (response, painter) = ui.allocate_painter(
            egui::vec2(ui.available_width().max(200.), 240.),
            egui::Sense::drag(),
        );
        let canvas = response.rect;
        if self.run_page.base_map_request != Some(self.osm_page.request_id) {
            let mut bounds: Vec<_> = net
                .graph
                .node_weights()
                .map(|n| (n.coord.lat, n.coord.lon))
                .collect();
            bounds.extend(net.buildings.iter().flatten().map(|p| (p.lat, p.lon)));
            self.run_page.map.fit(canvas, &bounds);
            self.run_page.base_map_request = Some(self.osm_page.request_id);
            self.run_page.map_fitted = false;
        }
        painter.rect_filled(
            canvas,
            egui::Rounding::ZERO,
            egui::Color32::from_rgb(250, 252, 251),
        );
        let basemap = super::tiles::BasemapSource::parse(&self.config.basemap);
        self.run_page
            .tiles
            .draw(&painter, ui.ctx(), &self.run_page.map, canvas, basemap);
        if let Some(message) = self.run_page.tiles.take_notice() {
            self.status = message;
        }
        self.run_page.map.draw_network(&painter, canvas, net);
        self.run_page.map.interact(ui, canvas);
    }

    fn start_run(&mut self) {
        if self.run_busy {
            return;
        }
        if self.run_page.route_mode.requires_network() && self.network.is_none() {
            self.status = "请先加载路网并刷新路线点位".into();
            return;
        }
        // 参数变更时补齐方案；提交直接使用预计算值
        self.run_page.ensure_plan();
        let page = &mut self.run_page;
        let plan = match page.plan.clone() {
            Some(p) => p,
            None => return,
        };
        let altitude_spec = match crate::track::altitude::parse_spec(&page.manual_altitude) {
            Ok(spec) => spec,
            Err(e) => {
                self.status = e;
                return;
            }
        };
        let (manual_altitude, manual_altitude_range) = match altitude_spec {
            None => (None, None),
            Some(crate::track::altitude::AltitudeSpec::Single(value)) => (Some(value), None),
            Some(crate::track::altitude::AltitudeSpec::Range(range)) => (None, Some(range)),
        };
        let (dist, dur) = (plan.dist * 1000.0, plan.dur); // 米
        let start_ms = plan.start_ms;
        let face_check = if page.face_check { 1 } else { 0 };
        let route_mode = page.route_mode;
        self.config.dist_min = page.dist_min;
        self.config.dist_max = page.dist_max;
        self.config.pace_min = page.pace_min;
        self.config.pace_max = page.pace_max;
        self.config.face_check = page.face_check;
        self.config.manual_altitude = manual_altitude;
        self.config.manual_altitude_range = manual_altitude_range;
        self.config.route_mode = route_mode.as_str().into();
        // 两个本地生成参数也从**页面**取值并落盘：预览就是按这两个数算的，
        // 提交链路（flow.rs）读 config —— 同源才不会出现「预览按 A 锚点拉点位、
        // 提交按 B 锚点重拉」这种两边打卡点不一致。
        self.config.gps_spread_m = page.gps_spread_m;
        self.config.anchor_offset_m = page.effective_anchor_offset_m();
        if let Err(e) = crate::ui::save_config(&self.config) {
            // **写盘失败就中止本次提交**，而不是只写状态栏。
            //
            // 提交链路（`flow.rs`）是从**磁盘**读 `anchor_offset_m` / `gps_spread_m`
            // 和 `safety` 的：磁盘满/目录只读时它读到的还是上一次的旧值 ——
            // 开了锚点偏移就是「预览按新锚点拉点位、提交按旧锚点重拉」，
            // 护栏也会按旧策略放行。与其按两套参数跑一次，不如让用户先解决写盘问题。
            self.status = format!("{e}（配置未能写入，已取消本次提交）");
            return;
        }

        let identity = self.identity.clone();
        let session = match self.session.clone() {
            Some(s) => s,
            None => {
                self.status = "请先登录".into();
                return;
            }
        };
        self.run_busy = true;
        self.run_control = std::sync::Arc::new(crate::task_control::Control::default());
        let control = self.run_control.clone();
        self.status = "跑步提交中…".into();
        let seed = plan.seed;
        self.spawn_job(move |tx| {
            let mut log = App::logger(tx.clone());
            let mut client = tx.client(identity, Some(session));
            let params = crate::api::flow::RunParams {
                dist,
                dur,
                start_ms,
                face_check,
                manual_altitude,
                manual_altitude_range,
                seed,
                route_mode,
            };
            let payload = match crate::api::flow::run_full_flow_cancellable(
                &mut client,
                &params,
                &mut |s| {
                    if s.starts_with('[') && control.check().is_ok() {
                        let _ = tx.send(format!("__RUN_STAGE__{s}"));
                    }
                    log(s);
                },
                &control,
            ) {
                Ok(out) => {
                    log(&format!(
                        "全链结束 rrid={} obs={}/2 verdict={} uuid={}",
                        out.result.rrid,
                        out.obs_ok,
                        out.verdict_code(),
                        out.result.uuid
                    ));
                    log(&format!("结论：{}", out.summary()));
                    serde_json::json!({
                        "ok": out.is_success(),
                        "submitted": true,
                        "verdict": out.verdict_code(),
                        "exit_code": out.exit_code(),
                        "summary": out.summary(),
                        "rrid": out.result.rrid,
                        "obs_ok": out.obs_ok, "verification": out.verification,
                        "uuid": out.result.uuid,
                        "dist": out.result.total_dis, "dur": out.result.total_time,
                        "steps": out.result.total_steps, "avg_step_freq": out.result.avg_step_freq,
                        "calorie": out.result.calorie, "avg_power": out.result.avg_power,
                        "sel_distance": out.result.sel_distance, "start": out.result.start_ms,
                    })
                }
                Err(e) => {
                    log(&format!("跑步提交失败: {e}"));
                    serde_json::json!({ "ok": false, "message": crate::road_status::explain(&e) })
                }
            };
            tx.send(format!("__RUN_DONE__{payload}")).ok();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一份参数合理的页面状态（配速 6:00/km，距离 2 km 附近）。
    fn page(start_mode: usize, days_ago: i64) -> RunPage {
        RunPage {
            dist_min: 2.0,
            dist_max: 2.2,
            pace_min: 350.0,
            pace_max: 370.0,
            manual_altitude: String::new(),
            start_mode,
            days_ago,
            hour: 12,
            minute: 0,
            face_check: false,
            plan: None,
            ..Default::default()
        }
    }

    /// 开跑时刻本身不得落在未来（两种模式都成立）。
    fn assert_start_not_future(p: &RunPlan) {
        let now = crate::crypto::envelope::now_ms();
        assert!(p.start_ms <= now, "开始时刻 {} 落在未来", p.start_ms);
    }

    /// 用户实测：勾了 faceCheck 但没点「开始跑步」就退出，重启后又变回默认。
    /// 现在页面参数有独立快照，改动即写盘，与提交解耦。
    #[test]
    fn run_settings_persist_without_submitting() {
        let mut p = page(0, 0);
        let mut config = crate::api::model::Config::default();
        // Config::default 里 face_check=true，先翻掉才能证明是被页面写回来的。
        config.face_check = false;
        config.route_mode = "legacy".into();

        p.face_check = true;
        p.route_mode = RouteMode::Road;
        p.dist_min = 3.0;
        p.pace_max = 400.0;
        p.manual_altitude = "17.2".into();
        RunSettings::from_page(&p).apply_to(&mut config).unwrap();

        assert!(config.face_check, "faceCheck 没写进配置");
        assert_eq!(config.route_mode, "road");
        assert_eq!(config.dist_min, 3.0);
        assert_eq!(config.pace_max, 400.0);
        assert_eq!(config.manual_altitude, Some(17.2));
        assert!(config.manual_altitude_range.is_none());
    }

    /// 快照只认这几个字段：换方案（距离/时刻）不算参数变化，不该触发写盘。
    #[test]
    fn run_settings_snapshot_ignores_plan_state() {
        let mut a = page(0, 0);
        a.ensure_plan();
        let mut b = page(0, 0);
        b.plan = None;
        assert_eq!(RunSettings::from_page(&a), RunSettings::from_page(&b));

        b.face_check = !a.face_check;
        assert_ne!(RunSettings::from_page(&a), RunSettings::from_page(&b));
    }

    /// 海拔三种写法：单值、区间、留空清除；写错时保留原值但仍写其余字段。
    #[test]
    fn run_settings_manual_altitude_forms() {
        let mut p = page(0, 0);
        let mut config = crate::api::model::Config::default();

        p.manual_altitude = "11.6-22.8".into();
        RunSettings::from_page(&p).apply_to(&mut config).unwrap();
        assert!(config.manual_altitude.is_none());
        let range = config.manual_altitude_range.expect("区间海拔未写入");
        assert_eq!((range.min_m, range.max_m), (11.6, 22.8));

        p.manual_altitude = String::new();
        RunSettings::from_page(&p).apply_to(&mut config).unwrap();
        assert!(config.manual_altitude.is_none());
        assert!(config.manual_altitude_range.is_none(), "留空未清除海拔区间");

        config.manual_altitude = Some(12.0);
        p.manual_altitude = "abc".into();
        p.face_check = true;
        let error = RunSettings::from_page(&p).apply_to(&mut config);
        assert!(error.is_err(), "非法海拔应回传错误");
        assert_eq!(config.manual_altitude, Some(12.0), "非法输入覆盖了原海拔");
        assert!(config.face_check, "海拔写错连带其它字段没保存");
    }

    /// 学校基准海拔：填了写进 config、留空回落到 None。
    ///
    /// 这是 flow.rs「未配置学校基准海拔」警告在 App 内的出口，所以
    /// 「留空 = 未配置」必须真的清掉旧值，不能留着上一所学校的高度。
    #[test]
    fn school_base_altitude_writes_config_and_blank_clears_it() {
        let mut config = crate::api::model::Config::default();
        let mut p = page(0, 0);
        assert_eq!(
            config.resolved_base_altitude(),
            (82.0, crate::track::altitude::BaseAltitudeOrigin::Default),
            "默认配置必须是 82 m"
        );

        p.school_base_altitude = " 1050 ".into();
        RunSettings::from_page(&p).apply_to(&mut config).unwrap();
        assert_eq!(config.school_base_altitude, Some(1050.0));
        assert_eq!(
            config.resolved_base_altitude(),
            (1050.0, crate::track::altitude::BaseAltitudeOrigin::School),
            "填了学校基准后 origin 必须是 School"
        );

        p.school_base_altitude = "   ".into();
        RunSettings::from_page(&p).apply_to(&mut config).unwrap();
        assert_eq!(config.school_base_altitude, None, "空白必须清成未配置");
        assert_eq!(
            config.resolved_base_altitude(),
            (82.0, crate::track::altitude::BaseAltitudeOrigin::Default)
        );
    }

    /// 非法值：保留已存值（不是静默丢弃，也不是写进去），并把原因交回界面。
    #[test]
    fn invalid_school_base_altitude_keeps_previous_value_and_reports() {
        let mut config = crate::api::model::Config::default();
        config.school_base_altitude = Some(1050.0);
        for bad in ["abc", "-600", "9001", "NaN", "1 050"] {
            let mut p = page(0, 0);
            p.school_base_altitude = bad.into();
            let error = RunSettings::from_page(&p)
                .apply_to(&mut config)
                .expect_err("非法学校基准海拔必须回传错误");
            assert!(
                error.contains("未保存"),
                "提示要说明「没保存」而不是沉默：{error}"
            );
            assert_eq!(
                config.school_base_altitude,
                Some(1050.0),
                "{bad} 覆盖了已存的学校基准海拔"
            );
        }
    }

    /// 与手动海拔并存时，界面显示的「当前生效」必须跟着 origin 走：
    /// 学校基准还在配置里，但生效的是手动海拔——这正是学生容易误解的地方。
    #[test]
    fn manual_altitude_wins_over_school_base_but_school_stays_configured() {
        use crate::track::altitude::{describe_base_altitude, BaseAltitudeOrigin};
        let mut config = crate::api::model::Config::default();
        config.school_base_altitude = Some(1050.0);
        config.manual_altitude = Some(1200.0);

        let (value, origin) = config.resolved_base_altitude();
        assert_eq!((value, origin), (1200.0, BaseAltitudeOrigin::Manual));
        let shown = describe_base_altitude(value, origin);
        assert!(
            shown.contains("手动海拔"),
            "界面显示的是 origin 标签：{shown}"
        );
        assert_eq!(
            config.school_base_altitude,
            Some(1050.0),
            "学校基准不该被手动海拔抹掉"
        );

        // 手动海拔清空后，学校基准重新接管。
        config.manual_altitude = None;
        assert_eq!(
            config.resolved_base_altitude(),
            (1050.0, BaseAltitudeOrigin::School)
        );
    }

    /// 无头：折叠区点开后，输入框标签、优先级说明、当前 origin 三者都要真的画出来。
    #[test]
    fn school_base_altitude_row_shows_priority_and_current_origin() {
        let ctx = egui::Context::default();
        let mut app = mode_app(&ctx);
        app.tab = 0;
        app.config.school_base_altitude = Some(1050.0);
        app.run_page.school_base_altitude = "1050".into();
        let (w, h) = (900.0, 700.0);
        click_label(&mut app, &ctx, w, h, "高级生成参数（不填即用默认值）");
        let output = run_frame(&mut app, &ctx, w, h, vec![]);
        let texts = visible_texts(&output);
        assert!(
            texts.iter().any(|t| t.contains("学校基准海拔（米）")),
            "输入框标签没画出来：{texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|t| t.contains("优先级：手动海拔（单值/区间）> 学校基准 > 默认 82 m")),
            "优先级说明没画出来：{texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|t| t.contains("当前生效：1050.0 m（学校基准海拔）")),
            "当前 origin 没画出来：{texts:?}"
        );
    }

    /// 随机模式：整段跑步（含 flow.rs 的 0-4s 抖动）都不得越过当前时刻。
    ///
    /// 指定模式同样适用（C1）：用户填的时刻只决定「想什么时候跑」，
    /// 一旦 start + 用时越过现在，就必须回退到 latest_start_ms，
    /// 否则 stopTime 落在未来（真机不可能提交还没跑完的记录）。
    fn assert_random_not_future(p: &RunPlan) {
        let now = crate::crypto::envelope::now_ms();
        assert!(
            p.start_ms + p.dur * 1000 + JITTER_MARGIN_MS <= now,
            "时刻 {} + 用时 {}s 越过当前时刻",
            p.start_ms,
            p.dur
        );
    }

    /// C1：指定时刻靠近「现在」时，方案时刻必须被钳到「现在 − 用时 − 余量」。
    #[test]
    fn specified_time_near_now_is_clamped_to_a_finished_run() {
        use chrono::Timelike;
        let now = chrono::Local::now();
        let mut p = page(1, 0);
        p.start_mode = 1;
        p.days_ago = 0;
        p.hour = now.hour() as i64;
        p.minute = now.minute() as i64;
        // 10 km / 350–370 s/km → 用时约 1 小时，「现在」这一分钟必然跑不完。
        p.dist_min = 10.0;
        p.dist_max = 10.0;
        p.ensure_plan();
        let plan = p.plan.clone().unwrap();
        assert_random_not_future(&plan);
        assert_eq!(
            (plan.hour, plan.minute),
            (p.hour, p.minute),
            "用户填的时刻被改写了"
        );
    }

    /// C1 的反面：用户填一个足够早的时刻时，必须**原样**采纳，不能无脑钳到现在。
    #[test]
    fn specified_time_in_the_past_is_kept() {
        let mut p = page(1, 0);
        p.start_mode = 1;
        p.days_ago = 1;
        p.hour = 9;
        p.minute = 15;
        p.ensure_plan();
        let plan = p.plan.clone().unwrap();
        assert_eq!(plan.start_ms, specified_time(1, 9, 15));
        assert_eq!(plan.start_ms % 60_000, 0, "整分输入不该被二次平移");
    }

    /// 指定模式下「换一版」只换运动量，用户填的时刻必须原样保留。
    #[test]
    fn shuffle_keeps_user_time_in_specified_mode() {
        let mut p = page(1, 1);
        p.hour = 9;
        p.minute = 15;
        p.ensure_plan();
        for _ in 0..40 {
            p.regen_plan();
            let plan = p.plan.clone().unwrap();
            assert_eq!((p.hour, p.minute), (9, 15), "「换一版」改动了用户填的时刻");
            assert_eq!((plan.hour, plan.minute), (9, 15), "方案时刻与输入框不一致");
        }
    }

    /// 指定模式下「换一版」仍应换掉距离/配速（否则按钮看起来没反应）。
    #[test]
    fn shuffle_changes_shape_in_specified_mode() {
        let mut p = page(1, 1);
        p.hour = 9;
        p.minute = 15;
        p.ensure_plan();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..40 {
            p.regen_plan();
            seen.insert(p.plan.as_ref().unwrap().dist.to_bits());
        }
        assert!(seen.len() > 1, "「换一版」40 次仍未改变距离");
    }

    /// 日期由用户决定，「换一版」不应改动它。
    #[test]
    fn shuffle_keeps_days_ago_in_specified_mode() {
        let mut p = page(1, 2);
        for _ in 0..20 {
            p.regen_plan();
            assert_eq!(p.days_ago, 2);
            assert_eq!(p.plan.as_ref().unwrap().days_ago, 2);
        }
    }

    /// 随机模式：时刻应落在该日 7:00-20:00 内，且「换一版」每次都换新的。
    #[test]
    fn random_mode_within_window_and_varies() {
        let mut p = page(0, 1);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..40 {
            p.regen_plan();
            let plan = p.plan.clone().unwrap();
            let t = Local.timestamp_millis_opt(plan.start_ms).single().unwrap();
            assert!(
                (RAND_HOUR_LO..=RAND_HOUR_HI).contains(&t.hour()),
                "随机时刻 {}:{} 越出 {RAND_HOUR_LO}:00-{RAND_HOUR_HI}:00",
                t.hour(),
                t.minute()
            );
            seen.insert((t.hour(), t.minute()));
        }
        assert!(seen.len() > 1, "随机模式 40 次仍未改变时刻");
    }

    /// 任何模式下开始时刻都不得落在未来；随机模式还要求整段跑步不越过当前时刻。
    #[test]
    fn never_schedules_future_start() {
        for mode in [0, 1] {
            for days_ago in 0..=3 {
                let mut p = page(mode, days_ago);
                for _ in 0..25 {
                    p.regen_plan();
                    assert_start_not_future(p.plan.as_ref().unwrap());
                    if mode == 0 {
                        assert_random_not_future(p.plan.as_ref().unwrap());
                    }
                }
                // 改参数走 ensure_plan 的路径同样成立
                p.dist_max = 5.0;
                p.pace_max = 500.0;
                p.ensure_plan();
                let plan = p.plan.as_ref().unwrap();
                assert_start_not_future(plan);
                if mode == 0 {
                    assert_random_not_future(plan);
                }
            }
        }
    }

    /// 今天 + 已过去的指定时刻：按原时刻提交，不动它。
    #[test]
    fn specified_today_past_hour_is_kept() {
        let mut p = page(1, 0);
        p.regen_plan();
        // 用户在界面上填时刻（regen_plan 之后填，才是用户输入而非抽样结果）
        p.hour = 0;
        p.minute = 30;
        p.ensure_plan();
        let plan = p.plan.clone().unwrap();
        let today = Local::now().date_naive();
        let want = Local
            .with_ymd_and_hms(today.year(), today.month(), today.day(), 0, 30, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        // 凌晨 0:30 尚未到达时会被钳制，跳过断言
        if want <= crate::crypto::envelope::now_ms() {
            assert_eq!(plan.start_ms, want, "已过去的指定时刻不应被改动");
        }
        assert_start_not_future(&plan);
    }

    /// 未来的指定时刻被钳制到当前时刻；已过去的时刻原样保留。
    ///
    /// 断言直接对齐契约 `min(填入时刻, 现在)`，与运行时刻无关（23:59 跑也不会误报）。
    #[test]
    fn specified_time_is_clamped_to_now() {
        let now = Local::now();
        let mut p = page(1, 0);
        p.hour = 23;
        p.minute = 59;
        p.regen_plan();
        let plan = p.plan.clone().unwrap();

        // Generation reads the clock between these two observations; requiring
        // equality to a later millisecond makes this test intermittently fail.
        let expected_at = |at: chrono::DateTime<Local>| {
            Local
                .with_ymd_and_hms(at.year(), at.month(), at.day(), 23, 59, 0)
                .single()
                .unwrap()
                .timestamp_millis()
                .min(at.timestamp_millis())
        };
        assert!(
            (expected_at(now)..=expected_at(Local::now())).contains(&plan.start_ms),
            "指定时刻未按 min(填入, 现在) 处理"
        );
        assert_start_not_future(&plan);
    }

    /// 改距离/配速只重抽运动量，不应打乱用户指定的时刻。
    #[test]
    fn shape_change_keeps_specified_time() {
        let mut p = page(1, 1);
        p.regen_plan();
        // 用户在界面上填时刻
        p.hour = 9;
        p.minute = 15;
        p.ensure_plan();
        let before = p.plan.clone().unwrap();
        assert_eq!((before.hour, before.minute), (9, 15));

        p.dist_min = 3.0;
        p.dist_max = 3.5;
        p.ensure_plan();
        let after = p.plan.clone().unwrap();
        assert_ne!(
            (after.dist, after.pace),
            (before.dist, before.pace),
            "运动量未重抽"
        );
        assert_eq!(after.start_ms, before.start_ms, "改距离不应改动指定时刻");
        assert_eq!((p.hour, p.minute), (9, 15));
    }

    /// 改小时/分钟应立刻反映到方案，且不重抽运动量。
    #[test]
    fn editing_hhmm_resyncs_plan() {
        let mut p = page(1, 1);
        p.regen_plan();
        let before = p.plan.clone().unwrap();

        p.hour = 8;
        p.minute = 45;
        p.ensure_plan();
        let after = p.plan.clone().unwrap();
        assert_eq!((after.hour, after.minute), (8, 45));
        assert_eq!(
            (after.dist, after.pace, after.dur),
            (before.dist, before.pace, before.dur)
        );
        assert_start_not_future(&after);
    }

    /// 切换模式应立即换掉时刻，且不重抽运动量。
    #[test]
    fn mode_switch_resyncs_time_only() {
        let mut p = page(1, 1);
        p.regen_plan();
        let before = p.plan.clone().unwrap();

        p.start_mode = 0;
        p.ensure_plan();
        let after = p.plan.clone().unwrap();
        assert_eq!(after.start_mode, 0);
        assert_eq!(
            (after.dist, after.pace, after.dur),
            (before.dist, before.pace, before.dur)
        );
        assert_random_not_future(&after);
    }

    /// 日期下拉作用于两种模式。
    #[test]
    fn date_change_applies_to_both_modes() {
        for mode in [0, 1] {
            let mut p = page(mode, 0);
            p.regen_plan();
            p.days_ago = 3;
            p.ensure_plan();
            let plan = p.plan.clone().unwrap();
            assert_eq!(plan.days_ago, 3);
            let want = (Local::now() - Duration::days(3)).date_naive();
            let t = Local.timestamp_millis_opt(plan.start_ms).single().unwrap();
            assert_eq!(t.date_naive(), want, "mode={mode} 未落到 3 天前");
        }
    }

    /// 日期下拉文本。
    #[test]
    fn days_ago_labels() {
        assert_eq!(days_ago_label(0), "今天");
        assert_eq!(days_ago_label(1), "昨天");
        assert_eq!(days_ago_label(3), "3 天前");
    }

    /// 同一帧内「距离 + 日期」一起改：两处变化都不能漏。
    #[test]
    fn simultaneous_shape_and_date_change() {
        let mut p = page(0, 0);
        p.regen_plan();
        p.dist_min = 4.0;
        p.dist_max = 4.5;
        p.days_ago = 2;
        p.ensure_plan();
        let plan = p.plan.clone().unwrap();
        assert_eq!(plan.days_ago, 2, "日期变化被漏掉");
        assert!(plan.dist >= 4.0, "距离变化被漏掉");
        let want = (Local::now() - Duration::days(2)).date_naive();
        let t = Local.timestamp_millis_opt(plan.start_ms).single().unwrap();
        assert_eq!(t.date_naive(), want);
        assert_random_not_future(&plan);
    }

    /// 同一帧内「时刻 + 距离」一起改。
    #[test]
    fn simultaneous_shape_and_hhmm_change() {
        let mut p = page(1, 1);
        p.regen_plan();
        p.dist_min = 4.0;
        p.dist_max = 4.5;
        p.hour = 8;
        p.minute = 20;
        p.ensure_plan();
        let plan = p.plan.clone().unwrap();
        assert_eq!((plan.hour, plan.minute), (8, 20), "时刻变化被漏掉");
        assert!(plan.dist >= 4.0, "距离变化被漏掉");
        assert_eq!(plan.start_ms, specified_time(1, 8, 20));
        assert_start_not_future(&plan);
    }

    /// 今天 + 指定时刻且该时刻已过：照常提交，不再回退（用户指定的时刻说了算）。
    #[test]
    fn specified_past_time_is_committed_as_is() {
        let now = Local::now();
        // 取一个今天已过去足够久的整点，保证整段用时都已结束
        let h = now.hour().saturating_sub(3) as i64;
        let mut p = page(1, 0);
        p.hour = h;
        p.minute = 0;
        p.regen_plan();
        let plan = p.plan.clone().unwrap();
        let today = now.date_naive();
        let want = Local
            .with_ymd_and_hms(today.year(), today.month(), today.day(), h as u32, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        assert_eq!(plan.start_ms, want, "用户填的过去时刻被改动了");
        assert_start_not_future(&plan);
    }

    /// 今天最后一刻（23:59）作为指定时刻：必然被钳到今天之内；C1 之后还要保证
    /// **整段跑步已经跑完**（start + 用时 ≤ 现在），这是本用例新增的判据。
    ///
    /// 用「今天 23:59」构造，避免依赖当前钟点（23:xx 跑也不会失效）。
    #[test]
    fn future_specified_time_is_clamped_and_detectable() {
        let mut p = page(1, 0);
        p.hour = 23;
        p.minute = 59;
        p.ensure_plan();
        let plan = p.plan.clone().unwrap();

        let now = Local::now();
        let today = now.date_naive();
        let typed = Local
            .with_ymd_and_hms(today.year(), today.month(), today.day(), 23, 59, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        let now_ms = crate::crypto::envelope::now_ms();
        // ensure_plan() 读时钟的时刻早于这里的 now_ms；允许 2ms 的时钟误差。
        assert!(
            plan.start_ms <= typed.min(now_ms) + 2,
            "start_ms={} 晚于 min(typed, now)={}",
            plan.start_ms,
            typed.min(now_ms)
        );
        // C1：不只是「不晚于现在」，还要留够整个用时 —— 否则 stopTime 落在未来。
        assert_random_not_future(&plan);
        // 界面提示条件：填入时刻晚于现在（23:59 总成立，除非正好在那一分钟）
        if typed > now_ms {
            assert!(plan.start_ms < typed, "未来时刻未被钳制");
        }
    }

    /// 反复调用 ensure_plan 应当是幂等的（不改动任何已定值）。
    #[test]
    fn ensure_plan_is_idempotent() {
        for mode in [0, 1] {
            let mut p = page(mode, 1);
            p.regen_plan();
            let first = p.plan.clone().unwrap();
            for _ in 0..5 {
                p.ensure_plan();
            }
            let again = p.plan.clone().unwrap();
            assert_eq!(
                (again.dist, again.pace, again.dur, again.start_ms),
                (first.dist, first.pace, first.dur, first.start_ms),
                "mode={mode} ensure_plan 不稳定"
            );
        }
    }

    /// 随机模式抽样结果应落在所选日期的窗口内；凌晨选「今天」时窗口未到，
    /// 此时允许退化，但必须能被 random_window_ok 识别出来（界面据此提示）。
    #[test]
    fn random_window_detection() {
        let mut p = page(0, 1);
        p.regen_plan();
        assert!(p.random_window_ok(), "昨天的随机时刻应当落在窗口内");

        // 构造一个「今天但早于 7:00」的方案，窗口判定应为 false
        let now = Local::now();
        let early = Local
            .with_ymd_and_hms(now.year(), now.month(), now.day(), 3, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        p.days_ago = 0;
        if let Some(plan) = p.plan.as_mut() {
            plan.days_ago = 0;
            plan.start_ms = early;
        }
        assert!(
            !p.random_window_ok(),
            "今天 03:00 不应被判为在 7:00-20:00 窗口内"
        );
    }

    // ── 「路由模式点了好像没用」的两条回归 ───────────────────────────────
    //
    // 用户反馈的两件事：① 选「自定义点位」不出图、提示还写着「经典模式」；
    // ② 切模式不重算，画面留旧结果。两条都用无头 egui 跑真实的 draw_run。

    use crate::api::model::{Config, HeaderIdentity, Session};

    /// 某个价位都能跑的一页（空会话/空配置即可：本组用例不碰网络）。
    fn mode_app(ctx: &egui::Context) -> App {
        App::from_state(
            ctx,
            HeaderIdentity::default(),
            Session::default(),
            Config::default(),
        )
    }

    fn run_frame(
        app: &mut App,
        ctx: &egui::Context,
        w: f32,
        h: f32,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(w, h),
                )),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| app.draw_run(ui));
            },
        )
    }

    /// 画面上所有可见文字（只取未被裁掉的）。
    fn visible_texts(output: &egui::FullOutput) -> Vec<String> {
        output
            .shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                egui::Shape::Text(text)
                    if clipped.clip_rect.contains(
                        egui::Rect::from_min_size(text.pos, text.galley.size()).center(),
                    ) =>
                {
                    Some(text.galley.text().to_string())
                }
                _ => None,
            })
            .collect()
    }

    fn label_position(output: &egui::FullOutput, label: &str) -> Option<egui::Pos2> {
        output.shapes.iter().find_map(|clipped| {
            if let egui::Shape::Text(text) = &clipped.shape {
                let rect = egui::Rect::from_min_size(text.pos, text.galley.size());
                if text.galley.text() == label && clipped.clip_rect.contains(rect.center()) {
                    return Some(rect.center());
                }
            }
            None
        })
    }

    /// 滚到能看见该标签，再点一下（布局测试同一套做法）。
    fn click_label(app: &mut App, ctx: &egui::Context, w: f32, h: f32, label: &str) {
        let mut at = None;
        for _ in 0..60 {
            let output = run_frame(app, ctx, w, h, vec![]);
            if let Some(pos) = label_position(&output, label) {
                at = Some(pos);
                break;
            }
            let _ = run_frame(
                app,
                ctx,
                w,
                h,
                vec![
                    egui::Event::PointerMoved(egui::pos2(w / 2.0, h - 70.0)),
                    egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point,
                        delta: egui::vec2(0.0, -60.0),
                        modifiers: Default::default(),
                    },
                ],
            );
            for _ in 0..12 {
                let _ = run_frame(app, ctx, w, h, vec![]);
            }
        }
        let Some(at) = at else {
            assert!(false, "滚动不到「{label}」");
            return;
        };
        for pressed in [true, false] {
            let _ = run_frame(
                app,
                ctx,
                w,
                h,
                vec![
                    egui::Event::PointerMoved(at),
                    egui::Event::PointerButton {
                        pos: at,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: Default::default(),
                    },
                ],
            );
        }
    }

    /// ① 自定义点位必须画地图：只有经典模式才该出现「经典模式仅按点位拟合」，
    /// 而且选了自定义点位后地图区域（基础地图/画布）要真的画出来。
    #[test]
    fn manual_route_mode_draws_the_map_instead_of_the_legacy_hint() {
        let ctx = egui::Context::default();
        let mut app = mode_app(&ctx);
        app.run_page.route_mode = RouteMode::Manual;
        // 空路网也算「有路网」：本用例只关心走没走到画地图那条分支。
        app.network = Some(std::sync::Arc::new(route_planner::RoadGraph::new()));

        let output = run_frame(&mut app, &ctx, 900.0, 700.0, vec![]);
        let texts = visible_texts(&output);
        assert!(
            !texts.iter().any(|t| t.contains("经典模式仅按点位拟合")),
            "自定义点位不该再显示经典模式提示：{texts:?}"
        );
        // 地图区域被绘制：没有点位数据时会画「基础地图」+ 说明，这就是画布存在的证据。
        assert!(
            texts
                .iter()
                .any(|t| t.contains("基础地图：道路与建筑已加载") || t.contains("（无预览）")),
            "自定义点位必须出地图区域：{texts:?}"
        );
        // 模式说明也要在场，别让用户猜「现在是什么模式、还缺什么」。
        assert!(
            texts
                .iter()
                .any(|t| t.contains("自定义点位：按点位顺序串联")),
            "缺少当前模式说明：{texts:?}"
        );
    }

    /// 经典模式仍然只给那句提示（回归保护：别把 Legacy 也改成画图）。
    #[test]
    fn legacy_route_mode_keeps_the_classic_hint_without_a_map() {
        let ctx = egui::Context::default();
        let mut app = mode_app(&ctx);
        app.run_page.route_mode = RouteMode::Legacy;
        app.network = Some(std::sync::Arc::new(route_planner::RoadGraph::new()));
        let output = run_frame(&mut app, &ctx, 900.0, 700.0, vec![]);
        let texts = visible_texts(&output);
        assert!(texts.iter().any(|t| t.contains("经典模式仅按点位拟合")));
        assert!(!texts
            .iter()
            .any(|t| t.contains("基础地图：道路与建筑已加载")));
    }

    /// ③ 缺路网时的提示要指出「还缺什么、下一步点哪里」，且两种模式后果不同。
    #[test]
    fn missing_network_hints_name_the_next_step_per_mode() {
        let ctx = egui::Context::default();
        // 自定义点位：能交（按点位连线），提示给出可选的补路网入口。
        let mut manual = mode_app(&ctx);
        manual.run_page.route_mode = RouteMode::Manual;
        let texts = visible_texts(&run_frame(&mut manual, &ctx, 900.0, 700.0, vec![]));
        assert!(
            texts.iter().any(|t| t.contains("将按点位连线")),
            "自定义点位缺路网时要说明会按点位连线：{texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("刷新路线点位与围栏")),
            "要告诉用户从哪儿补路网：{texts:?}"
        );

        // 真实道路：没路网就没法走，提示要说清必须先取得路网。
        let mut road = mode_app(&ctx);
        road.run_page.route_mode = RouteMode::Road;
        let texts = visible_texts(&run_frame(&mut road, &ctx, 900.0, 700.0, vec![]));
        assert!(
            texts.iter().any(|t| t.contains("真实道路模式需要路网")),
            "真实道路缺路网时要说明这是硬前提：{texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|t| t.contains("刷新路线点位与围栏") || t.contains("路网页")),
            "要告诉用户从哪儿补路网：{texts:?}"
        );
    }

    /// ② 切模式要标脏并丢掉旧预览：否则画面留着上一种模式的结果。
    #[test]
    fn switching_route_mode_invalidates_the_previous_preview() {
        let ctx = egui::Context::default();
        let mut app = mode_app(&ctx);
        app.run_page.route_mode = RouteMode::Road;
        app.network = Some(std::sync::Arc::new(route_planner::RoadGraph::new()));
        // 造一份「上一种模式算出来的」预览。
        app.run_page.preview = Some(crate::track::generate_road::RoadPlan {
            arcs: vec![0.0, 1.0],
            laps: vec![500.0],
            route: vec![(39.9, 116.4)],
            length_m: 500.0,
            edges: vec![],
            buildings: vec![],
            checkpoints: vec![(39.9, 116.4)],
            fences: vec![],
        });
        app.run_page.preview_key = "旧模式的键".into();
        app.run_page.preview_stale = false;
        let _ = run_frame(&mut app, &ctx, 900.0, 700.0, vec![]);

        click_label(&mut app, &ctx, 900.0, 700.0, "自定义");

        assert_eq!(app.run_page.route_mode, RouteMode::Manual, "点击没切换模式");
        assert!(app.run_page.preview_stale, "切模式后预览必须标脏");
        assert!(app.run_page.preview.is_none(), "旧模式的预览必须清掉");
        assert!(app.run_page.check.is_none(), "旧模式的检查结果必须清掉");
        assert!(
            app.run_page.preview_key.is_empty(),
            "去重键必须清空，否则 prepare_preview 不会重算"
        );
    }

    /// 改 GPS 抖动幅度必须让预览失效：`prepare_preview` 的去重键里没有幅度，
    /// 键不清空它就认为结果还新鲜，用户会看着「改了幅度没反应」。
    #[test]
    fn changing_gps_spread_invalidates_the_previous_preview() {
        let mut page = RunPage::default();
        page.preview = Some(crate::track::generate_road::RoadPlan {
            arcs: vec![0.0, 1.0],
            laps: vec![500.0],
            route: vec![(39.9, 116.4)],
            length_m: 500.0,
            edges: vec![],
            buildings: vec![],
            checkpoints: vec![(39.9, 116.4)],
            fences: vec![],
        });
        page.preview_key = "旧幅度的键".into();
        page.preview_stale = false;
        page.gps_spread_m = 6.0;

        page.on_gps_spread_changed();

        assert!(page.preview_stale, "改幅度后预览必须标脏");
        assert!(page.preview.is_none(), "旧幅度的预览必须清掉");
        assert!(page.check.is_none(), "旧的检查结果必须清掉");
        assert!(page.preview_key.is_empty(), "去重键必须清空");
    }

    /// 改锚点偏移：只标脏 + 置「待重取点位」，不当场发请求（拖动中不重取）。
    #[test]
    fn changing_anchor_offset_defers_the_points_refresh_until_settle() {
        let mut page = RunPage::default();
        page.preview_key = "旧锚点的键".into();
        page.preview_stale = false;
        page.anchor_offset_m = 80.0;

        page.on_anchor_offset_changed();

        assert!(page.preview_stale);
        assert!(page.preview_key.is_empty());
        assert!(page.anchor_offset_dirty, "必须留下「松手后重取点位」的标记");
        assert!(page.preview.is_none());
    }

    // ---------------------------------------------------------------
    // 提交结束后清空计划缓存（避免同参数两次记录完全一致）
    // ---------------------------------------------------------------

    /// 提交结束 → 方案作废 → 下一次重新抽取。
    ///
    /// `days_ago = 1` 是刻意的：随机时刻的窗口是「那一天的 7:00–20:00」，
    /// 用今天的话凌晨跑 CI 会把窗口压成一个点，`start_ms` 就不再是随机的了，
    /// 断言会变成偶发失败。
    #[test]
    fn finished_run_clears_the_plan_so_the_next_one_differs() {
        let mut p = page(0, 1);
        p.ensure_plan();
        let first = p.plan.clone().expect("方案应已抽出");
        p.preview = Some(crate::track::generate_road::RoadPlan {
            arcs: vec![0.0, 1.0],
            laps: vec![500.0],
            route: vec![(39.9, 116.4)],
            length_m: 500.0,
            edges: vec![],
            buildings: vec![],
            checkpoints: vec![(39.9, 116.4)],
            fences: vec![],
        });
        p.check = Some(Default::default());
        p.preview_key = "旧方案的键".into();
        p.preview_stale = false;

        p.clear_plan_cache();

        assert!(p.plan.is_none(), "提交结束后 plan 必须清空");
        assert!(p.preview.is_none(), "提交结束后预览必须清掉");
        assert!(p.check.is_none(), "旧的检查结果必须清掉");
        assert!(p.preview_key.is_empty(), "去重键必须清空");
        assert!(p.preview_stale, "预览必须标脏，下一帧才会重算");

        p.ensure_plan();
        let second = p.plan.clone().expect("应重新抽出方案");
        assert_ne!(second.seed, first.seed, "新方案必须重新抽种子");
        assert_ne!(
            second.start_ms, first.start_ms,
            "新方案必须重新抽开始时间（否则两条记录的开始时间一样）"
        );
        // 用户选的区间与模式不能被「清理」改掉。
        assert_eq!(second.dist_min, first.dist_min);
        assert_eq!(second.dist_max, first.dist_max);
        assert_eq!(second.pace_min, first.pace_min);
        assert_eq!(second.pace_max, first.pace_max);
        assert_eq!(second.start_mode, first.start_mode);
    }

    /// 显式钉住种子时，清理只清掉「这一次的抽样」，种子必须照用。
    #[test]
    fn pinned_seed_survives_the_post_run_clear() {
        let mut p = page(0, 1);
        p.fixed_seed = Some(424_242);
        p.ensure_plan();
        assert_eq!(p.plan.as_ref().unwrap().seed, 424_242, "固定种子没生效");

        p.clear_plan_cache();
        assert!(p.plan.is_none());
        assert_eq!(p.fixed_seed, Some(424_242), "清理不该动用户的固定种子");

        p.ensure_plan();
        assert_eq!(
            p.plan.as_ref().unwrap().seed,
            424_242,
            "固定种子下重新抽也必须用同一个种子"
        );
    }

    /// 端到端：提交在飞时 plan 原样保留；`__RUN_DONE__` 到达（失败也算）才清。
    #[test]
    fn run_done_clears_the_plan_cache_but_an_in_flight_run_keeps_it() {
        let ctx = egui::Context::default();
        let mut app = mode_app(&ctx);
        app.run_page.ensure_plan();
        let planned = app.run_page.plan.clone().expect("方案应已抽出");
        app.run_page.preview_key = "旧方案的键".into();

        // ① 提交进行中：只来 stage 消息，方案必须原样留着 —— 预览与提交共用它。
        app.run_busy = true;
        app.tx
            .send("__RUN_STAGE__[track] 生成轨迹".to_string())
            .unwrap();
        app.poll_messages();
        assert!(app.run_busy, "stage 消息不该结束提交");
        assert!(app.run_page.plan.is_some(), "提交进行中不能清方案");
        assert_eq!(
            app.run_page.plan.as_ref().unwrap().seed,
            planned.seed,
            "提交进行中方案被换掉了，预览与提交会指向两条轨迹"
        );
        assert_eq!(app.run_page.preview_key, "旧方案的键");

        // ② 提交结束（失败分支也算「结束」）：清空，下一次重新抽。
        let payload = serde_json::json!({"ok": false, "message": "fixture 失败"}).to_string();
        app.tx
            .send(format!("{}{}", crate::ui::RUN_DONE, payload))
            .unwrap();
        app.poll_messages();
        assert!(!app.run_busy);
        assert!(app.run_page.plan.is_none(), "提交结束后必须清空方案");
        assert!(app.run_page.preview.is_none());
        assert!(app.run_page.preview_key.is_empty());

        // ③ 下一帧 `ensure_plan` 会抽到新种子（同参数也不复刻上一条记录）。
        app.run_page.ensure_plan();
        assert_ne!(
            app.run_page.plan.as_ref().unwrap().seed,
            planned.seed,
            "同参数再跑必须换一个种子"
        );
    }

    // ---------------------------------------------------------------
    // F3：换 seed ⇒ 计划级锚点偏移变 ⇒ 必须重取点位（否则预览≠提交）
    // ---------------------------------------------------------------

    /// 新方案 = 新 seed = 新偏移：必须标记「点位要重取」，否则预览手里的点位
    /// 还是按旧锚点拉的，而提交会按新锚点重拉（缓存按锚点索引，必然 miss）。
    #[test]
    fn new_plan_marks_points_for_refetch_only_when_the_offset_is_on() {
        let mut p = page(0, 1);
        p.anchor_offset_m = 50.0;
        p.ensure_plan();
        assert!(p.anchor_offset_dirty, "开了偏移后新方案必须重取点位");

        // 消费掉标记（等价于 UI 在松手后跑完 `refresh_points`）。
        p.anchor_offset_dirty = false;
        // 参数没变：不该每帧都重取。
        p.ensure_plan();
        assert!(!p.anchor_offset_dirty, "方案没变就不该重取点位");

        // 换一版 = 新 seed = 新偏移 → 又要重取。
        p.regen_plan();
        assert!(p.anchor_offset_dirty, "换一版必须重取点位");

        // 偏移关着（默认）：点位与 seed 无关，不该白打一次点位接口。
        let mut off = page(0, 1);
        assert_eq!(off.effective_anchor_offset_m(), 0.0, "默认必须是关闭");
        off.ensure_plan();
        assert!(!off.anchor_offset_dirty, "偏移关闭时重取点位是白费请求");
        off.regen_plan();
        assert!(!off.anchor_offset_dirty);

        // 固定种子：偏移不随计划变（同 seed 同偏移），也不用重取。
        let mut pinned = page(0, 1);
        pinned.anchor_offset_m = 50.0;
        pinned.fixed_seed = Some(7);
        pinned.ensure_plan();
        assert!(
            !pinned.anchor_offset_dirty,
            "固定种子下偏移恒定，重取点位是白费请求"
        );
    }

    /// 预览（页面值）与提交（启动时写进 config 的值）必须读同一个偏移。
    #[test]
    fn preview_and_submit_read_the_same_anchor_offset() {
        let mut p = page(0, 1);
        p.anchor_offset_m = 80.0;
        p.gps_spread_m = 6.0;
        let mut config = crate::api::model::Config::default();
        assert_eq!(config.resolved_anchor_offset_m(), 0.0, "默认关闭");

        // `start_run` 落盘用的是这两个值，预览读的是 `effective_anchor_offset_m`。
        RunSettings::from_page(&p).apply_to(&mut config).unwrap();
        assert_eq!(
            config.resolved_anchor_offset_m(),
            p.effective_anchor_offset_m(),
            "预览与提交的偏移量必须同源"
        );
        assert_eq!(config.gps_spread_m, p.gps_spread_m);

        // 同一个计划 seed 派生出同一个锚点：预览与提交各算一次也一致。
        let exact = crate::location::Coordinate::new(39.9042, 116.4074, 0.0).unwrap();
        assert_eq!(
            crate::location::plan_anchor(exact, p.effective_anchor_offset_m(), 7),
            crate::location::plan_anchor(exact, config.resolved_anchor_offset_m(), 7),
            "同一 seed + 同一偏移量 → 同一锚点"
        );

        // 越界/脏值一律当关闭，不会把上传坐标挪走。
        let mut dirty = page(0, 1);
        dirty.anchor_offset_m = f64::NAN;
        assert_eq!(dirty.effective_anchor_offset_m(), 0.0);
        dirty.anchor_offset_m = -5.0;
        assert_eq!(dirty.effective_anchor_offset_m(), 0.0);
        dirty.anchor_offset_m = 1e9;
        assert_eq!(
            dirty.effective_anchor_offset_m(),
            crate::api::model::ANCHOR_OFFSET_MAX_M
        );
    }

    // ---------------------------------------------------------------
    // M1：固定种子 + 开启偏移 + 还没有方案 —— 预览与提交必须同一锚点
    // ---------------------------------------------------------------

    /// 盲审抓到的真实分叉：`plan` 为空时预览曾把种子回落成 0，而提交用
    /// `fixed_seed`，两份锚点差 40–60 m（打卡点跟着分叉）。
    #[test]
    fn pinned_seed_without_a_plan_keeps_preview_and_submit_on_one_anchor() {
        let mut p = page(0, 1);
        p.anchor_offset_m = 50.0;
        p.fixed_seed = Some(424_242);
        // 勾选「固定种子」→ clear_plan_cache 之后的真实状态：有种子、没方案。
        p.plan = None;
        assert!(p.preview.is_none() && p.plan.is_none());

        // 预览侧：此刻用来派生锚点的种子。
        let preview_seed = p.plan_seed_for_anchor();
        assert_eq!(
            preview_seed, 424_242,
            "没有方案时必须回落到 fixed_seed，而不是 0"
        );

        // 提交侧：ensure_plan 写进 RunPlan 的种子。
        p.ensure_plan();
        let submit_seed = p.plan.as_ref().unwrap().seed;
        assert_eq!(submit_seed, 424_242);

        let exact = crate::location::Coordinate::new(39.9042, 116.4074, 0.0).unwrap();
        let offset = p.effective_anchor_offset_m();
        let preview_anchor = crate::location::plan_anchor(exact, offset, preview_seed);
        let submit_anchor = crate::location::plan_anchor(exact, offset, submit_seed);
        assert_eq!(
            preview_anchor.latitude.to_bits(),
            submit_anchor.latitude.to_bits(),
            "预览与提交的锚点必须逐位相同"
        );
        assert_eq!(
            preview_anchor.longitude.to_bits(),
            submit_anchor.longitude.to_bits(),
            "预览与提交的锚点必须逐位相同"
        );

        // 回归守卫：旧的 `.unwrap_or(0)` 回落确实会算出另一个锚点，
        // 否则这条用例证明不了任何东西。
        let old_fallback = crate::location::plan_anchor(exact, offset, 0);
        assert_ne!(
            old_fallback.latitude.to_bits(),
            submit_anchor.latitude.to_bits(),
            "0 回落应产生不同锚点，说明这条守卫有效"
        );
    }

    /// 有方案时依旧取方案种子（固定种子与随机种子两种情形都成立）。
    #[test]
    fn plan_seed_for_anchor_prefers_the_plan_then_the_pin() {
        let mut p = page(0, 1);
        p.ensure_plan();
        let planned = p.plan.as_ref().unwrap().seed;
        assert_eq!(p.plan_seed_for_anchor(), planned, "有方案时取方案种子");

        p.fixed_seed = Some(9);
        // 方案还在：仍以方案种子为准（下一份方案才会变成 9）。
        assert_eq!(p.plan_seed_for_anchor(), planned);

        p.plan = None;
        assert_eq!(p.plan_seed_for_anchor(), 9, "没有方案时用钉住的种子");

        p.fixed_seed = None;
        assert_eq!(p.plan_seed_for_anchor(), 0, "两者都没有时才回落 0");
    }

    // ---------------------------------------------------------------
    // L1：config 写不进去就不提交
    // ---------------------------------------------------------------

    /// 磁盘写不进去时 `flow` 会读到旧参数（锚点偏移/抖动/护栏），
    /// 宁可中止提交并说清原因，也不要按两套参数跑一次。
    #[test]
    fn failed_config_write_aborts_the_submission() {
        // 把数据目录指到一个**普通文件**上：写 `<file>/config.json` 必然 ENOTDIR。
        let blocker =
            std::env::temp_dir().join(format!("neko-run-blocker-{}", uuid::Uuid::new_v4()));
        std::fs::write(&blocker, b"x").unwrap();
        crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = Some(blocker.clone()));

        let ctx = egui::Context::default();
        let mut app = mode_app(&ctx);
        // 有会话：若代码没被拦住，它会真的起提交任务（run_busy 变 true）。
        app.session = Some(crate::api::model::Session {
            uid: 42,
            token: "fixture-token".into(),
            unid: "77".into(),
            ..Default::default()
        });
        app.run_page.ensure_plan();
        let planned = app.run_page.plan.clone().expect("方案应已抽出");

        app.start_run();

        let busy = app.run_busy;
        let status = app.status.clone();
        let plan_alive = app.run_page.plan.is_some();
        crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = None);
        let _ = std::fs::remove_file(&blocker);

        assert!(!busy, "配置写不进去时不该起提交任务（状态栏：{status}）");
        assert!(
            status.contains("已取消本次提交"),
            "要明说为什么没提交，实际：{status}"
        );
        assert!(plan_alive, "取消提交不该顺手清掉方案");
        assert_eq!(
            app.run_page.plan.as_ref().unwrap().seed,
            planned.seed,
            "取消提交不该换方案"
        );
    }
}
