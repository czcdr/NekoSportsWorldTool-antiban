//! 全链编排：policy → 实时点位 → 轨迹生成 → 提交 → OBS → 详情验证。
//! 由 UI 后台线程调用，log 闭包回传日志。

use super::client::ApiClient;
use super::model::Session;
use super::points;
use super::policy::fetch_policy;
use super::records::fetch_one_record;
use super::submit::{submit_record, SubmitParams, SubmitResult};
use crate::location::Coordinate;
use crate::track::generate_road::build_road_tuned_with_altitude as build_road_with_altitude;
use crate::track::generate_road::RouteMode;
use crate::track::generator::build_tuned_with_altitude as gen_track_tuned_with_altitude;
use crate::track::wire::{build_obs_object, five_point_wrapper, obs_keys};
use serde_json::Value;

#[derive(Clone, Copy)]
pub struct RunParams {
    /// 距离（米）与时长（秒）已由 UI 参数解析。
    pub dist: f64,
    pub dur: i64,
    /// 开始时间（毫秒）。
    pub start_ms: i64,
    pub face_check: i64,
    /// 用户手动填写的绝对海拔（米）；None 使用生成器海拔。
    pub manual_altitude: Option<f64>,
    /// 用户手动填写的海拔范围；与单值字段兼容，范围优先。
    pub manual_altitude_range: Option<crate::track::altitude::AltitudeRange>,
    pub seed: u64,
    /// 路线算法模式。
    pub route_mode: RouteMode,
}

pub struct RunOutcome {
    pub result: SubmitResult,
    pub obs_ok: usize,
    pub verification: Verification,
    /// 如实记录的「这条记录可能不算数」的原因（A4 围栏外采样点 / A8 要求人脸核验
    /// 但没提供核验数据）。不改变提交行为，只让 UI 与 CLI 都能看见。
    pub warnings: Vec<String>,
}

/// 「请求被接受」之外的最终状态。
///
/// 提交成功 ≠ OBS 完整 ≠ 详情可读 ≠ 服务端判定达标。以前只要提交被接受就
/// 返回 `Ok`，CLI 以 0 退出、UI 显示「提交成功」—— 定时任务会把这种半成品
/// 当成完成。现在把四种结果分开，调用方按此决定退出码与文案。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunVerdict {
    /// OBS 2/2 且详情明确达标。
    Complete,
    /// 服务端已接受，但详情明确判定未达标（记录在、不计数）。
    NotCounted,
    /// 提交已被接受，但 OBS 没传全（对象缺失，详情页大概率读不到轨迹）。
    ObsIncomplete,
    /// 详情读不到或没有判定字段：结果未知，不能算成功。
    Unverified,
}

impl RunOutcome {
    pub fn verdict(&self) -> RunVerdict {
        // 服务端的明确否决优先于其它信号。
        if self.verification.complete == Some(false) {
            return RunVerdict::NotCounted;
        }
        if self.obs_ok < 2 {
            return RunVerdict::ObsIncomplete;
        }
        if self.verification.complete == Some(true) {
            RunVerdict::Complete
        } else {
            RunVerdict::Unverified
        }
    }

    /// 只有一条完整记录才算成功：OBS 齐全 + 详情可读 + 服务端判定达标。
    pub fn is_success(&self) -> bool {
        self.verdict() == RunVerdict::Complete
    }

    /// 进程退出码：0 完成 / 2 OBS 不全 / 3 详情未知 / 4 未达标。
    ///
    /// 用不同码而不是统一非零，方便定时脚本区分「重试可能有用」（3）与
    /// 「重试也没用，得改参数」（4）。
    pub fn exit_code(&self) -> i32 {
        match self.verdict() {
            RunVerdict::Complete => 0,
            RunVerdict::ObsIncomplete => 2,
            RunVerdict::Unverified => 3,
            RunVerdict::NotCounted => 4,
        }
    }

    /// 机器可读的状态码（写进 UI 结果 JSON）。
    pub fn verdict_code(&self) -> &'static str {
        match self.verdict() {
            RunVerdict::Complete => "complete",
            RunVerdict::NotCounted => "not_counted",
            RunVerdict::ObsIncomplete => "obs_incomplete",
            RunVerdict::Unverified => "unverified",
        }
    }

    /// 一句话结论（CLI/UI 共用口径）。
    ///
    /// `warnings`（A4 围栏外采样点 / A8 要求核验但无核验数据）追加在结论后面，
    /// 这样 CLI 的 `结论：…` 与 UI 的结果弹窗都会原样带出这句 —— 不用每个调用方
    /// 各自记得打印。
    pub fn summary(&self) -> String {
        let base = match self.verdict() {
            RunVerdict::Complete => "记录完成：OBS 2/2，详情达标".into(),
            RunVerdict::NotCounted => {
                let reasons = self.verification.failure_reasons();
                if reasons.is_empty() {
                    "服务端判定未达标，本条不计入".into()
                } else {
                    format!("服务端判定未达标：{}", reasons.join("；"))
                }
            }
            RunVerdict::ObsIncomplete => format!(
                "提交已被接受，但 OBS 只上传 {}/2：轨迹对象不完整，记录大概率无效",
                self.obs_ok
            ),
            RunVerdict::Unverified => {
                let reason = self
                    .verification
                    .error
                    .clone()
                    .unwrap_or_else(|| "响应没有达标判定字段".into());
                format!("提交已被接受，但无法确认是否达标：{reason}")
            }
        };
        if self.warnings.is_empty() {
            return base;
        }
        format!("{base}；{}", self.warnings.join("；"))
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Verification {
    pub detail: Option<Value>,
    pub error: Option<String>,
    pub complete: Option<bool>,
}

impl Verification {
    pub fn from_result(result: Result<Value, String>) -> Self {
        match result {
            Ok(detail) => {
                // An explicit failure wins over a contradictory top-level flag.
                let failed = detail
                    .get("reasonList")
                    .and_then(Value::as_array)
                    .map(|rows| {
                        rows.iter()
                            .any(|r| r.get("complete").and_then(Value::as_bool) == Some(false))
                    })
                    .unwrap_or(false);
                let complete = if failed {
                    Some(false)
                } else {
                    detail.get("complete").and_then(Value::as_bool)
                };
                Self {
                    complete,
                    detail: Some(detail),
                    error: None,
                }
            }
            Err(error) => Self {
                detail: None,
                complete: None,
                error: Some(error),
            },
        }
    }
    pub fn label(&self) -> &'static str {
        match self.complete {
            Some(true) => "达标",
            Some(false) => "未达标",
            None => "未知",
        }
    }

    /// 未达标时服务端给出的具体原因（`reasonList` 里 `complete=false` 的 `reason`）。
    ///
    /// 这是「记录还在但不算数」最直接的诊断：服务端会说明是哪一项没满足
    /// （里程、有效时间、点位、步频……），比只看「未达标」有用得多。
    pub fn failure_reasons(&self) -> Vec<String> {
        let Some(detail) = &self.detail else {
            return Vec::new();
        };
        detail
            .get("reasonList")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter(|r| r.get("complete").and_then(Value::as_bool) == Some(false))
                    .filter_map(|r| r.get("reason").and_then(Value::as_str))
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// 固定的 2s / 1s / 2s 间隔本身就是一种机器节奏。抖动到 [base, 2*base) 毫秒，
/// 让"拉策略→拉点位→提交→传 OBS→读详情"这条链的时序不像脚本。
fn sleep_jitter(base_ms: u64) {
    let extra = rand::random::<u64>() % base_ms.max(1);
    std::thread::sleep(std::time::Duration::from_millis(base_ms + extra));
}

/// 用 policy 响应的服务端时间戳估算本地时钟偏差（毫秒）。
///
/// 服务端可能给秒或毫秒，按量级判断；给 0 表示未知，不报偏差。
/// 返回正值 = 服务器比本地快。
fn server_skew_ms(server_ts: i64) -> i64 {
    if server_ts <= 0 {
        return 0;
    }
    let server_ms = if server_ts > 1_000_000_000_000 {
        server_ts
    } else {
        server_ts.saturating_mul(1000)
    };
    server_ms.saturating_sub(crate::crypto::envelope::now_ms())
}

/// A7：给「整分」的起点补上真随机的秒与毫秒（仍落在同一分钟内）。
///
/// `ui::run::specified_time` 产出的时刻秒与毫秒都是 0；不补的话 `startTime` 恒为
/// 「秒 ∈ 0-4、ms = 0」，而它是 flag / beginTime / gainTime 的基准 —— 一次跑步几十个窗、
/// 连点几次提交，整条时间轴都带着同一条机器痕迹。
///
/// - 只在**整分**（`ms % 60_000 == 0`）时补，也就是「指定时刻」模式：随机模式本来是
///   均匀抽样、自带毫秒，再平移反而会破坏它与 7:00–20:00 窗口的对齐；
/// - 增量 < 60s：分钟由用户选，不跨分钟/不跨日，`totalTime` 与 10s 窗口径都不受影响；
/// - 用 `draw` 传入随机数，方便测试可复现。
fn second_precision_start(base_ms: i64, draw: u32, now_ms: i64) -> i64 {
    if base_ms.rem_euclid(60_000) != 0 {
        return base_ms;
    }
    // F4：指定时刻被钳到「现在」时，再加最多 59.999s 会把开始时间推到未来 ——
    // 服务端看到 startTime 晚于 createTime 会直接判异常。夹住不超过 now。
    (base_ms + (draw % 60_000) as i64).min(now_ms)
}

/// C1：开始时间 + 用时不得越过现在。
///
/// 指定时刻模式下 `specified_time()` / CLI 的 `--time` 只把**开始时间**钳到今天或现在，
/// 于是「现在 12:05、指定 12:00、用时 30 分钟」会得到 `stopTime = 12:30`（未来）。
/// 真机不可能提交一段还没跑完的记录，服务端看到 stopTime 晚于创建时间就是硬矛盾。
///
/// `dur` 必须是**最终**时长（`effective_duration` 之后的值）：它会二次改变时长，
/// 先校验再改等于白校验。
fn reject_future_stop(start_ms: i64, dur: i64, now_ms: i64) -> Result<(), String> {
    let stop = dur
        .checked_mul(1000)
        .and_then(|ms| start_ms.checked_add(ms));
    match stop {
        Some(stop) if stop <= now_ms => Ok(()),
        Some(stop) => Err(format!(
            "开始时间 {} + 用时 {dur}s = 结束 {} 落在未来（现在 {}）：\
             指定时刻太靠近现在，请把开始时间提前到至少 {dur} 秒之前，或改用随机时刻",
            crate::track::geom::fmt_cst(start_ms, "%m-%d %H:%M:%S"),
            crate::track::geom::fmt_cst(stop, "%m-%d %H:%M:%S"),
            crate::track::geom::fmt_cst(now_ms, "%m-%d %H:%M:%S"),
        )),
        None => Err("时间范围溢出".into()),
    }
}

/// F3（t3 验收 low）：没配置学校基准时的海拔警告。
///
/// C10 把「随机 20–260 m」改回了固定 82 m —— 这只是把「随机错」换成「固定错」：
/// 校区在包头（约 1050 m）时整条记录仍低几百米，而服务端只要知道校区高程就能看出来。
/// 更重要的是**用户看不见**：它此前只是一条普通日志，UI 结论里没有。
///
/// 所以这条走 `warnings`（`RunOutcome::summary()` 会带进 UI 弹窗与 CLI 结论），
/// 并明确写出两个出口：跑步页的「手动海拔」，或 `run --school-altitude`（落盘一次）。
///
/// 不改默认值：82 m 是 v0.8.0 的既有语义，换成任何别的常量只是换一种错。
fn default_altitude_warning(origin: crate::track::altitude::BaseAltitudeOrigin) -> Option<String> {
    use crate::track::altitude::BaseAltitudeOrigin;
    if origin != BaseAltitudeOrigin::Default {
        return None;
    }
    Some(
        "未配置学校基准海拔：本次记录按默认 82 m 生成。校区不在地势平坦的平原时\
         （例如包头约 1050 m），这条记录的海拔会与真实高程差出几百米，\
         是服务端一眼可查的参数异常。出口：跑步页「手动海拔」填本次校区的绝对海拔，\
         或执行 `run --school-altitude <米>`（写入 config.json 的 school_base_altitude，\
         填一次之后所有记录都按它生成）"
            .to_string(),
    )
}

/// C3：本次计划距离不得低于学校门槛。
///
/// 门槛由校方下发（策略 `minDistance`），工具**不**下调它来凑达标；
/// 计划比门槛短就直接不发请求 —— 那条记录无论如何都不会计入有效次数，
/// 白跑一次还多一次风控计数。
fn reject_below_min_distance(dist_m: f64, min_distance_m: i64) -> Result<(), String> {
    if min_distance_m > 0 && dist_m < min_distance_m as f64 {
        return Err(format!(
            "本次计划 {dist_m:.0} m 低于学校要求的最低距离 {min_distance_m} m（策略 minDistance）：\
             请把跑步距离下限调到 ≥ {min_distance_m} m 后再提交；工具不会下调学校门槛"
        ));
    }
    Ok(())
}

/// A4：只读的围栏校验 —— 数一遍有多少采样点落在学校围栏外。
///
/// Legacy（默认模式）只保证「经过打卡点」，生成的环还会向外鼓；Manual 拿不到路网时
/// 退回同一条连线路径。两者都不使用围栏选线，这里只把事实数出来交给调用方警告，
/// **不改轨迹、不拦提交**（改选线属于另一个量级的改动）。
///
/// `track.locations[].gLat/gLng` 与围栏同为 BD09（见 `generate_road::to_bd`），
/// 可以直接比。
fn count_points_outside_fences(
    track: &crate::track::model::Track,
    fences: &[Vec<(f64, f64)>],
) -> usize {
    if fences.is_empty() {
        return 0;
    }
    // 围栏是 (lat, lon) 元组；boundary 用 Coord::new(lon, lat)。
    let polys: Vec<Vec<route_planner::Coord>> = fences
        .iter()
        .map(|ring| {
            ring.iter()
                .map(|&(lat, lon)| route_planner::Coord::new(lon, lat))
                .collect()
        })
        .collect();
    track
        .locations
        .iter()
        .filter(|p| {
            !route_planner::boundary::contains(&polys, route_planner::Coord::new(p.gLng, p.gLat))
        })
        .count()
}

/// A4：围栏外采样点的告警文案（0 个时不告警）。
///
/// 抽成函数是为了让文案本身可测：这句话会同时出现在日志、UI 弹窗与 CLI 结论里。
fn fence_warning(outside: usize) -> Option<String> {
    (outside > 0).then(|| format!("本路线有 {outside} 个采样点在围栏外，可能被判无效"))
}

/// N4（外部复审 0.9.0）：轨迹点云的**短长轴比**（短轴/长轴的标准差比，米制平面）。
///
/// 1.0 = 各向同性（圆形/方形环、绕建筑的多圈都接近 1）；越小越「瘦长」。
/// 用协方差主轴而不是「长边/短边」：真实轨迹不是多边形，轴比才能同时覆盖
/// 三角形环、细长跑道环这类形状，且不受采样密度影响。
///
/// 经度按纬度折算（`met_per_deg_lng`），否则高纬度会把东西向压扁、伪造出「瘦长」。
fn shape_axis_ratio(pts: &[(f64, f64)]) -> f64 {
    let n = pts.len() as f64;
    if pts.len() < 8 {
        return 1.0;
    }
    let lat0 = pts.iter().map(|p| p.0).sum::<f64>() / n;
    let lng0 = pts.iter().map(|p| p.1).sum::<f64>() / n;
    let kx = crate::track::geom::met_per_deg_lng(lat0);
    let (mut sxx, mut syy, mut sxy) = (0.0f64, 0.0f64, 0.0f64);
    for p in pts {
        let x = (p.1 - lng0) * kx;
        let y = (p.0 - lat0) * crate::track::geom::MET_PER_DEG_LAT;
        sxx += x * x;
        syy += y * y;
        sxy += x * y;
    }
    let (sxx, syy, sxy) = (sxx / n, syy / n, sxy / n);
    let tr = sxx + syy;
    let det = sxx * syy - sxy * sxy;
    let disc = (0.25 * tr * tr - det).max(0.0).sqrt();
    let long = 0.5 * tr + disc;
    let short = (0.5 * tr - disc).max(0.0);
    if long <= 1e-12 {
        1.0
    } else {
        (short / long).sqrt()
    }
}

/// 瘦长告警的阈值（短长轴比）。
///
/// 依据：用 [`shape_axis_ratio`] 对同一套 Legacy 生成器（2000 m / seed 7）跑了五种
/// 打卡点布局，实测出一张「形 → 轴比」表（见测试
/// `shape_axis_ratio_separates_thin_triangle_from_round_ring` 的标定断言）：
///
/// | 打卡点布局 | 短长轴比 |
/// |---|---|
/// | 4 角方形环（≈1:1） | **0.91** |
/// | 6 点近圆环 | **0.98** |
/// | 长宽 2:1 的细长跑道环 | **0.57** |
/// | 长宽 3:1 | **0.35** |
/// | 长宽 4:1 | **0.23** |
/// | 3 点近似共线（真实被封样本那类瘦长三角） | **0.01** |
///
/// 取 **0.45**：落在「2:1 细长跑道」（0.57，余量 24%）与「3:1 以上、图上已经是一条
/// 粗线」（0.35，余量 22%）之间；方形/近圆环有 2 倍余量，不会误报；瘦长三角比阈值
/// 低 45 倍，必抓。**这个阈值只决定提示与否，不阻断提交、不改默认模式。**
const THIN_SHAPE_AXIS_RATIO: f64 = 0.45;

/// N4：瘦长形态告警。
///
/// 背景（`review/SERVER-EVIDENCE.md` 的真实样本）：被判可疑那条是「瘦长三角形 +
/// 与已有边重合的重复段」，达标那条是「复杂多圈、绕建筑、多次折返」。
/// 0.9.0 的逐圈车道偏移解决了「重合段」，但**没解决「瘦长」**：Legacy 仍把打卡点按
/// 质心角排序做 Catmull-Rom 闭环，3 个近似共线的点位必然生出瘦长三角环。
///
/// 这里只提示、不阻断：默认模式（Legacy）不变，改模式是用户的选择。
/// **只对 Legacy 提示** —— Road / Tiles / Manual 沿真实路网走，形状由路网决定。
fn thin_shape_warning(pts: &[(f64, f64)], route_mode: RouteMode) -> Option<String> {
    if route_mode != RouteMode::Legacy {
        return None;
    }
    let ratio = shape_axis_ratio(pts);
    if ratio >= THIN_SHAPE_AXIS_RATIO {
        return None;
    }
    Some(format!(
        "当前轨迹偏瘦长（短长轴比 {ratio:.2}，阈值 {THIN_SHAPE_AXIS_RATIO:.2}）：\
         真实跑步很少是这种形状，被人工复核的样本里「瘦长三角形」正是被判可疑的那类。\
         建议切到「道路」或「瓦片」模式；没有路网时可在路网页手绘一条轨迹"
    ))
}

/// N5（外部复审 0.9.0）：计划距离区间上限低于学校门槛时的告警。
///
/// `reject_below_min_distance` 会在提交前直接拦下（这是对的：比提交一条不计入的记录好），
/// 但默认区间仍是 1.0–1.5 km，而多数学校门槛是 1600 / 2000 m —— 新用户首次必失败，
/// 而且要到「点开始」之后才看到错误。这里把冲突提前说出来。
///
/// 单位口径：学校门槛一律用 **米**（策略 `minDistance` 就是米），计划区间用 **km**
/// （与跑步页 UI 一致），并在括号里给出换算后的米数，避免用户对不上。
///
/// 不改用户配置（改配置要用户确认），也不下调门槛。
fn distance_floor_warning(
    dist_min_km: f64,
    dist_max_km: f64,
    min_distance_m: i64,
) -> Option<String> {
    if min_distance_m <= 0 || !dist_max_km.is_finite() {
        return None;
    }
    let floor_km = min_distance_m as f64 / 1000.0;
    if dist_max_km >= floor_km {
        return None;
    }
    let lo_m = (dist_min_km * 1000.0).round() as i64;
    let hi_m = (dist_max_km * 1000.0).round() as i64;
    Some(format!(
        "学校要求单次 ≥ {min_distance_m} m，当前计划区间 {dist_min_km:.1}–{dist_max_km:.1} km\
         （{lo_m}–{hi_m} m）的上限就低于门槛，提交会被直接拒绝。\
         请把距离下限/上限调到 ≥ {floor_km:.1} km（{min_distance_m} m）以上\
         ——工具不会替你改配置，也不会下调学校门槛"
    ))
}

/// 跑步全链。
pub fn run_full_flow(
    client: &mut ApiClient,
    params: &RunParams,
    log: &mut dyn FnMut(&str),
) -> Result<RunOutcome, String> {
    run_full_flow_cancellable(
        client,
        params,
        log,
        &crate::task_control::Control::default(),
    )
}

pub fn run_full_flow_cancellable(
    client: &mut ApiClient,
    params: &RunParams,
    log: &mut dyn FnMut(&str),
    control: &crate::task_control::Control,
) -> Result<RunOutcome, String> {
    control.check()?;
    if !params.dist.is_finite() || params.dist <= 0.0 || params.dur <= 0 {
        return Err("距离和时长必须为正数".into());
    }
    let sess: Session = client.login.clone().ok_or("未登录")?;

    // 行为护栏：深夜提交、同日多次提交是最容易被人工审查的特征。
    // 纯本地判断，不发任何请求；被拦时立刻返回，不浪费出口与额度。
    // 这里**占位而不是只检查**：锁内完成「校验 + 计数 +1」，提交失败由 RAII 归还，
    // 既堵住并发提交同时读到 0 条的竞态，也避免写盘失败变成"永远 0 条"的 fail-open。
    let safety = super::model::load_config().safety;
    // 时区不是 UTC+8 时直接停：只警告会让「今天/昨天」与深夜判定错一天。
    crate::safety::check_timezone(&safety, chrono::Local::now().offset().local_minus_utc())?;
    // 0-4 秒抖动先落定，再按**最终开始时间**占位：否则午夜边界会把额度记到
    // 前一天，而提交的轨迹落在后一天（关掉深夜拦截时就会真的错位）。
    // A7（审计 medium）：指定时刻模式下 `run.rs::specified_time` 用 `with_ymd_and_hms(..., 0)`
    // 把秒与毫秒都钉成 0，原来这里只补 `0..4` 秒 —— 于是 `startTime` 恒为「秒 ∈ 0-4、ms = 0」，
    // 而它同时是每个点的 `flag`、每个窗的 `beginTime`、`gainTime` 的基准，整条时间轴
    // 都带着这条机器痕迹（连续多次提交尤其明显）。
    //
    // 现在把秒与毫秒都取真随机，但仍**落在用户选的那一分钟内**（< 60s：不跨日、不改时长、
    // 不动 10s 窗口径）。值在 `reserve_run` 之前落定：护栏看到的、写进轨迹与报文的是同一个
    // `start_ms`，flag/beginTime/gainTime 全部由它派生，不各算各的。
    //
    // 随机模式的时刻本来就是均匀抽样（带毫秒），原样保留，不再二次平移。
    let now_for_start = crate::crypto::envelope::now_ms();
    let start_ms = second_precision_start(params.start_ms, rand::random::<u32>(), now_for_start);
    let reservation = crate::safety::reserve_run(&safety, start_ms)?;

    // 出口与轨迹同城：策略 / 点位 / 提交 / OBS / 详情整条链路共用同一出口 IP。
    // 服务端把「轨迹所在城市」和「请求来源 IP」做对比，是最廉价的风控手段。
    let city = client.identity.city.clone();
    match client.bind_egress_for_city(&city)? {
        Some(entry) => log(&format!(
            "[net] 出口已绑定 {}（同城：{city}）",
            if entry.id.is_empty() {
                &entry.url
            } else {
                &entry.id
            }
        )),
        None => {
            // 走到这里只剩「本来就没配代理」：配置坏了/没有同城节点都会返回 Err，
            // 不会静默直连。
            log(&format!(
                "[net] 未配置出口池，本次直连 —— 来源 IP 可能与轨迹城市（{city}）不符"
            ));
        }
    }

    super::submission_guard::preflight(client, params.start_ms, params.dur, log)?;

    // ① policy
    log("[policy] 拉取跑步策略…");
    let pol = fetch_policy(client)?;
    log(&format!(
        "√ [policy] ts={} policy={} minDistance={} validTime={}",
        pol.timestamp, pol.policy, pol.min_distance, pol.valid_time
    ));
    // N5：门槛与**配置里的计划区间**冲突时，先说出来（此时还没生成轨迹/预览）。
    //
    // 必须放在下面那条 reject 之前：默认区间撞门槛时那条会直接 return Err，
    // 用户就永远看不到「为什么每次都失败、该把区间调到多少」这段说明。
    // 这里只警告不拦：本次计划可能已被用户手调到 ≥ 门槛，真正该拦的交给下面。
    // `warnings` 在后面才建（要等 policy 的人脸核验分支），所以先记在本地，稍后并入。
    let pending_floor_warning = {
        let config = super::model::load_config();
        match distance_floor_warning(
            config.dist_min as f64,
            config.dist_max as f64,
            pol.min_distance,
        ) {
            Some(warning) => {
                log(&format!("⚠ [policy] {warning}"));
                Some(warning)
            }
            None => None,
        }
    };
    // C3：计划距离低于学校门槛 → 就地拦下，不生成、不提交。
    //
    // 不改这一条，报文里就是「selDistance = 学校门槛 / totalDis = 更短 / complete = true」
    // 三件互相矛盾的事实（默认距离区间 1.0–1.5 km 撞上 2000 m 门槛，每跑一次矛盾一次）。
    // 学校门槛是校方规则，工具不下调它来凑达标；提前拦住也省得白跑一次还吃一次风控计数。
    reject_below_min_distance(params.dist, pol.min_distance)?;
    // 人脸核验：服务端要不要，本工具都**不执行**核验。这里只把事实摆出来 ——
    // 让「这条记录可能被判无效」在跑之前就可见，而不是等详情页异常才发现。
    //
    // A8（审计 medium）：原来只打一条日志就算了，结论里看不出来。现在同时记进
    // `warnings`，由 `RunOutcome::summary()` 带进 UI 弹窗与 CLI 输出 —— 提交行为不变
    // （不拒绝、不改 faceCheck），只是别让用户以为这是一条干净的记录。
    let mut warnings: Vec<String> = Vec::new();
    // N5：门槛与计划区间冲突（在拿到 policy 时就已判定并打过日志）—— 并入 warnings，
    // 让它随 RunOutcome::summary() 进 UI 结论弹窗与 CLI 输出。
    if let Some(warning) = pending_floor_warning {
        warnings.push(warning);
    }
    match pol.face_required {
        Some(true) => {
            warnings
                .push("已提交，但校方要求人脸核验而未提供核验数据，可能不计入有效次数".to_string());
            log(&format!(
                "⚠ [policy] 服务端要求本次人脸核验（faceVerifyType={}）：本工具不执行核验，\
                 提交只有 faceCheck 标记、没有核验数据，记录可能被判无效",
                pol.face_verify_type
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "未下发".into())
            ));
        }
        Some(false) => log("[policy] 服务端本次未要求人脸核验"),
        None => log("[policy] 响应未下发人脸核验配置（不代表不要求）"),
    }
    // 服务端时钟对齐：本地时间偏差会让请求 timeStamp 与服务端对不上，
    // 是成本很低的异常特征，也是「提交成功却不算数」的常见原因之一。
    let skew_ms = server_skew_ms(pol.timestamp);
    if skew_ms.abs() > 60_000 {
        log(&format!(
            "⚠ [time] 本地时间与服务器相差约 {} 分钟 —— 请校准设备时间，否则 timeStamp 会异常",
            skew_ms / 60_000
        ));
    } else if skew_ms.abs() > 5_000 {
        log(&format!(
            "[time] 本地与服务器时钟偏差 {}s（在可接受范围内）",
            skew_ms / 1000
        ));
    }
    // 时区在入口处已强制（check_timezone）：走到这里只可能是 UTC+8，或用户显式放行。
    if safety.allow_timezone_mismatch && chrono::Local::now().offset().local_minus_utc() != 8 * 3600
    {
        log("[time] 已按 safety.allow_timezone_mismatch 放行非 UTC+8 时区；日期与时段判断可能与北京时间错位");
    }
    sleep_jitter(1500);

    control.check()?;
    log("[road] 获取并核对当前校区围栏…");
    let fences = super::fence::fetch_geo_fence_response(client)
        .map_err(|e| format!("无法确认本次围栏：{e}"))?;
    let fences = if params.route_mode == RouteMode::Road && !fences.polygons.is_empty() {
        super::campus::selected_response(
            fences,
            super::model::load_config().school_campus.as_ref(),
            &sess.unid,
        )?
    } else {
        fences
    };

    // ② 实时点位（拒绝本地样本兜底）
    log("[points] 拉取实时点位…");
    if client.identity.has_unconfigured_default_location() {
        return Err("请先在设备信息页填写本次跑步所在城市和定位锚点，不能使用大连默认配置".into());
    }
    let exact_anchor: Coordinate = client.identity.anchor_coordinate()?;
    // 计划级上传锚点偏移（配置 anchor_offset_m，0 = 精确锚点）：
    // 只改点位请求里的 latitude/longitude（`runec` 输入串同源派生，因此自动一致），
    // 设备身份锚点、围栏、轨迹生成用的锚点都还是精确值。
    // 偏移由本次计划的 seed 派生 → 同一计划内预览与提交拿到同一份偏移。
    let anchor = crate::location::plan_anchor(
        exact_anchor,
        super::model::load_config().resolved_anchor_offset_m(),
        params.seed,
    );
    if anchor != exact_anchor {
        let dy = (anchor.latitude - exact_anchor.latitude) * crate::track::geom::MET_PER_DEG_LAT;
        let dx = (anchor.longitude - exact_anchor.longitude) * crate::track::geom::MET_PER_DEG_LNG;
        log(&format!(
            "[points] 上传锚点按计划偏移 {:.1} m（设备锚点本身不变）",
            dx.hypot(dy)
        ));
    }
    let area_id = (fences.area.run_area_id >= 0).then(|| fences.area.run_area_id.to_string());
    // 点位集合随目标距离变化（官方路径 `/distance/<米>`），这里传本次目标距离
    let pts = points::fetch_points_ext(client, anchor, area_id, params.dist, log)?;
    if pts.is_empty() {
        return Err("实时点位为空 —— 拒绝本地样本兜底".into());
    }
    log(&format!("√ [points] {} 个点位", pts.len()));
    for p in pts.iter().take(5) {
        log(&format!(
            "  [points] {} BD=({:.6},{:.6}) GCJ=({},{})",
            p.get("pointName").and_then(|v| v.as_str()).unwrap_or(""),
            p.get("lat").and_then(|v| v.as_f64()).unwrap_or(0.0),
            p.get("lon").and_then(|v| v.as_f64()).unwrap_or(0.0),
            p.get("glat").map(|v| v.to_string()).unwrap_or_default(),
            p.get("glon").map(|v| v.to_string()).unwrap_or_default(),
        ));
    }

    // ③ 轨迹生成（必经点 + 打卡点）
    if points::points_bd(&pts).len() != pts.len() {
        return Err("实时点位包含无效坐标".into());
    }
    let route_pts = points::route_points(&pol.must_points, &pts);
    log(&format!(
        "[points] 去重后 {} 个路线点位（包含全部策略必经点）",
        route_pts.len()
    ));
    // Keep the user's location stable so preview and submission refer to the same campus.
    let mut params = *params;
    // C1：`effective_duration` 会**二次改变**时长（配速越界时把 dur 拉回有效窗口），
    // 所以任何「开始时间 + 时长是否越过现在」的判断都必须放在它之后 ——
    // 放在前面就会漏掉「用户填 12:00、现在 12:05、dist/dur 又让 dur 变大」这一档。
    params.dur = crate::track::generate_road::effective_duration(params.dist, params.dur);
    reject_future_stop(start_ms, params.dur, crate::crypto::envelope::now_ms())?;
    control.check()?;
    log(&format!(
        "[track] 生成轨迹 {:.0}m / {}s（{} 点位）…",
        params.dist,
        params.dur,
        route_pts.len()
    ));
    // 随机 0-4 秒偏移（终端上报的 flag 与首点差 <5s），轨迹/提交/OBS/五点统一使用；
    // 已在占位之前算好（`start_ms`），这里不再二次抽样。
    // GPS 抖动幅度（配置 gps_spread_m，0 = 旧行为）；只读一次，供下面各模式共用。
    let gps_spread_m = super::model::load_config().resolved_gps_spread_m();
    // C10：海拔基准（手动 > 学校基准 > 默认 82 m）。接口不带高程，只能由配置给；
    // 来源必须写进日志，否则用户不知道自己这条记录报了多少米。
    let (base_alt_m, alt_origin) = super::model::load_config().resolved_base_altitude();
    log(&format!(
        "[alt] 海拔基准 {}",
        crate::track::altitude::describe_base_altitude(base_alt_m, alt_origin)
    ));
    // F3（t3 验收 low）：没配置学校基准时，C10 只是把「随机 20–260 m」换成了
    // 「固定 82 m」—— 包头校区（约 1050 m）照样差几百米，而且用户看不见。
    // 这里升级成 warnings：进 UI 结论弹窗与 CLI 结论，并给出可操作的出口。
    if let Some(warning) = default_altitude_warning(alt_origin) {
        log(&format!("⚠ [alt] {warning}"));
        warnings.push(warning);
    }
    let mut track = match params.route_mode {
        RouteMode::Road => {
            if !fences.polygons.is_empty() {
                super::school_roads::validate_fences(&fences.polygons)?;
            }
            let config = super::model::load_config();
            let mut net = super::school_roads::load_for_route_cancellable(
                &config.osm_path,
                &sess.unid,
                &fences.polygons,
                &route_pts,
                false,
                control,
                log,
            )?;
            crate::track::generate_road::align_network(&mut net);
            let filtered = crate::track::generate_road::apply_fences(&net, &fences.polygons);
            log(if fences.polygons.is_empty() {
                "[road] 学校边界未提供；沿点位闭环重复补足距离，无法保证校内范围"
            } else {
                "[road] 在学校围栏内沿点位闭环重复补足距离"
            });
            build_road_with_altitude(
                params.dist,
                params.dur,
                params.seed,
                start_ms,
                &route_pts,
                &route_pts,
                &filtered,
                gps_spread_m,
                base_alt_m,
            )
            .map_err(|e| format!("道路路由失败：{e}；请修复路网或主动切换经典模式"))?
        }
        RouteMode::Manual => {
            // 自定义点位：只用实时点位，**不追加**策略必经点（`route_pts` 含 must_points）。
            // 有可用路网就沿道路串起来；拿不到路网就退回按点位连线，
            // 而不是像 Road 那样直接失败 —— 这是 Manual 存在的意义。
            let manual_pts = points::points_bd(&pts);
            log(&format!(
                "[manual] 自定义点位 {} 个（不含策略必经点），优先沿道路串联",
                manual_pts.len()
            ));
            let config = super::model::load_config();
            let loaded = super::school_roads::load_for_route_cancellable(
                &config.osm_path,
                &sess.unid,
                &fences.polygons,
                &manual_pts,
                false,
                control,
                log,
            );
            match loaded {
                Ok(mut net) => {
                    crate::track::generate_road::align_network(&mut net);
                    let filtered =
                        crate::track::generate_road::apply_fences(&net, &fences.polygons);
                    log("[manual] 沿道路按点位顺序串联；不要求经过策略必经点");
                    build_road_with_altitude(
                        params.dist,
                        params.dur,
                        params.seed,
                        start_ms,
                        &manual_pts,
                        &[],
                        &filtered,
                        gps_spread_m,
                        base_alt_m,
                    )
                    .map_err(|e| format!("自定义点位路由失败：{e}；可留空路网退回按点位连线"))?
                }
                Err(e) => {
                    log(&format!("[manual] 无可用路网（{e}），退回按点位连线"));
                    gen_track_tuned_with_altitude(
                        params.dist,
                        params.dur,
                        params.seed,
                        (anchor.latitude, anchor.longitude),
                        start_ms,
                        &manual_pts,
                        gps_spread_m,
                        base_alt_m,
                    )
                }
            }
        }
        RouteMode::Legacy => gen_track_tuned_with_altitude(
            params.dist,
            params.dur,
            params.seed,
            (anchor.latitude, anchor.longitude),
            start_ms,
            &route_pts,
            gps_spread_m,
            base_alt_m,
        ),
        // 第 4 种模式：瓦片矢量化路网。与 Road 的唯一差别是「路网从哪来」——
        // 这里读本地瓦片缓存（`tile_road::load`），之后的裁剪/路由完全复用同一套。
        RouteMode::Tiles => {
            let cache = crate::track::tile_road::default_cache_path();
            let mut net = match crate::track::tile_road::load(&cache) {
                Ok((stats, graph)) => {
                    log(&format!("[tiles] 瓦片路网：{}", stats.summary()));
                    if graph.graph.edge_count() == 0 {
                        return Err(
                            "瓦片路网里没有可用道路：请在跑步页重新点「生成瓦片路网」".into()
                        );
                    }
                    graph
                }
                Err(e) => {
                    return Err(format!(
                        "瓦片路网不可用（{e}）。请到跑步页点「生成瓦片路网」，\
                         或切回「真实道路 / 经典」模式；人工导入 OSM 路网也仍然可用"
                    ))
                }
            };
            // 缓存里的节点已是 BD-09（`align_applied = true`），align_network 会自己跳过。
            crate::track::generate_road::align_network(&mut net);
            let filtered = crate::track::generate_road::apply_fences(&net, &fences.polygons);
            log(if fences.polygons.is_empty() {
                "[tiles] 学校边界未提供；沿点位闭环重复补足距离，无法保证校内范围"
            } else {
                "[tiles] 在瓦片路网里沿点位闭环重复补足距离"
            });
            build_road_with_altitude(
                params.dist,
                params.dur,
                params.seed,
                start_ms,
                &route_pts,
                &route_pts,
                &filtered,
                gps_spread_m,
                base_alt_m,
            )
            .map_err(|e| {
                format!("瓦片路网路由失败：{e}；可在跑步页重新生成瓦片路网，或切回经典模式")
            })?
        }
    };
    control.check()?;
    // A4（审计 medium）：Legacy（默认模式）完全不用围栏 —— 只保证过打卡点，环的
    // Catmull-Rom 还会向外鼓；Manual 在拿不到路网时也退化成同一条连线路径。
    // 这里补一道**只读**校验：数出围栏外的采样点，如实警告，不改轨迹、不拦提交
    // （改选线是另一个量级的改动，审计也只要求可见）。
    let outside = count_points_outside_fences(&track, &fences.polygons);
    if let Some(message) = fence_warning(outside) {
        log(&format!(
            "⚠ [fence] {message}（{}）",
            if params.route_mode == RouteMode::Legacy {
                "经典模式只过打卡点、不避让围栏"
            } else {
                "当前模式无路网时按点位连线"
            }
        ));
        warnings.push(message);
    }
    // N4（复审 0.9.0）：瘦长形态提示。只对 Legacy 提示、只警告不拦提交。
    // 放在这里是因为此时轨迹点已定稿（后续只有海拔覆盖，不改平面形状）。
    {
        let path: Vec<(f64, f64)> = track.locations.iter().map(|p| (p.gLat, p.gLng)).collect();
        if let Some(warning) = thin_shape_warning(&path, params.route_mode) {
            log(&format!("⚠ [shape] {warning}"));
            warnings.push(warning);
        }
    }
    if let Some(range) = params.manual_altitude_range {
        crate::track::altitude::override_bd_a_range(&mut track, range)?;
        log(&format!(
            "√ [track] 已将海拔曲线映射到 {:.2}-{:.2}m，覆盖 {} 个点，爬升/圈数据将按覆盖值计算",
            range.min_m,
            range.max_m,
            track.locations.len()
        ));
    } else if let Some(altitude_m) = params.manual_altitude {
        crate::track::altitude::override_bd_a(&mut track, altitude_m)?;
        log(&format!(
            "√ [track] 已用手动海拔 {:.2}m 覆盖 {} 个点，爬升/圈数据将按覆盖值计算",
            altitude_m,
            track.locations.len()
        ));
    }
    log(&format!(
        "√ [track] {} 点 totalDis={:.0}m steps={} 起点={}",
        track.locations.len(),
        track.totalDistance,
        track.totalSteps,
        crate::track::geom::fmt_cst(track.startTime, "%H:%M:%S"),
    ));

    // ④ 五点 wrapper（跑完态）
    track.validate_consistency()?;
    for point in &pol.must_points {
        if !crate::track::wire::passes_point(&track, *point) {
            return Err("最终轨迹未经过策略必经点，请调整路线".into());
        }
    }
    let five = five_point_wrapper(&pts, &track, &fences.area);

    // ⑤ 提交（sportType=1）
    control.begin_submit()?;
    // 点位拉取与提交落在同一秒是纯机器节奏：真机「拉点位」发生在记录开始前后，
    // 提交在跑完之后，两者不可能相隔零秒。这里给一段抖动，至少把它们拆开。
    sleep_jitter(2500);
    log("[record] 提交跑步记录（已进入提交，不能撤回）…");
    let sp = SubmitParams {
        track,
        uid: sess.uid,
        selected_unid: sess.unid.parse().unwrap_or(0),
        policy: pol.policy,
        policy_ts: pol.timestamp,
        min_distance: pol.min_distance,
        weight: if sess.weight > 0.0 { sess.weight } else { 68.0 },
        face_check: params.face_check,
        five_point_json: five,
        address: client.identity.city.clone(),
    };
    let result = submit_record(client, &sp, log)?;
    // 服务端已接受：保留这次额度。前面的任何失败路径都已经由 Drop 归还。
    reservation.commit();
    sleep_jitter(700);

    // ⑥ OBS 上传（双 key）
    log("[obs] 上传 OBS 对象（gzip+base64，10 键）…");
    // 从提交结果回填 track.startTime（含随机秒偏移），保证 body/OBS/flag 全链一致
    let mut track_for_obs = sp.track.clone();
    track_for_obs.startTime = result.start_ms;
    let obj = build_obs_object(
        &track_for_obs,
        result.rrid,
        &result.uuid,
        sess.uid,
        &sp.five_point_json,
    );
    let payload = obj.to_string().into_bytes();
    let keys = obs_keys(&track_for_obs, result.rrid, &result.uuid);
    let obs_ok = super::obs::upload_both_keys(client, &keys, &payload, log);
    if obs_ok == 2 {
        log("√ [obs] 双 key 上传成功");
    } else {
        log(&format!("⚠ [obs] 上传成功 {obs_ok}/2"));
    }

    // ⑦ 详情验证
    sleep_jitter(1500);
    log("[verify] 拉取详情验证…");
    let verification = Verification::from_result(fetch_one_record(client, result.rrid));
    log(&format!(
        "[verify] 详情读取={}；服务端达标判定={}",
        if verification.detail.is_some() {
            "成功"
        } else {
            "失败"
        },
        verification.label()
    ));
    if let Some(error) = &verification.error {
        log(&format!("⚠ [verify] {error}"));
    }
    // 记录"不算数"的直接原因：服务端会写明是哪一项没满足。
    for reason in verification.failure_reasons() {
        log(&format!("⚠ [verify] 未计数原因：{reason}"));
    }
    Ok(RunOutcome {
        result,
        obs_ok,
        verification,
        warnings,
    })
}

/// AI 提交流（UI 线程用）。
pub fn run_ai_submit(
    client: &mut ApiClient,
    sport_id: i64,
    mode: super::ai::AiMode,
    log: &mut dyn FnMut(&str),
) -> Result<Value, String> {
    // AI 运动同样受行为护栏约束（深夜 / 每日条数）。
    let safety = super::model::load_config().safety;
    crate::safety::check_timezone(&safety, chrono::Local::now().offset().local_minus_utc())?;
    // 锁内「校验 + 占位」：并发提交不会同时读到同一个旧计数。请求失败时
    // reservation 在 Drop 里归还，不会白吃配额。
    let reservation = crate::safety::reserve_ai(&safety, 1)?;
    log(&format!("[ai] 提交 sportId={sport_id} mode={mode:?}…"));
    let biz = super::ai::upload(client, sport_id, mode, None)?;
    if !super::ai::upload_succeeded(&biz) {
        let message = biz
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("服务器未确认提交成功");
        return Err(format!("AI 提交被拒绝：{message}"));
    }
    reservation.commit();
    log("√ [ai] 提交成功");
    Ok(biz)
}

/// AI 列表（UI 线程用）。
pub fn run_ai_list(
    client: &mut ApiClient,
    log: &mut dyn FnMut(&str),
) -> Result<Vec<super::ai::AiSport>, String> {
    log("[ai] 拉取项目列表…");
    let list = super::ai::fetch_list(client)?;
    log(&format!("√ [ai] {} 个项目", list.len()));
    Ok(list)
}

/// 记录列表（UI 线程用）。
pub fn run_records(
    client: &mut ApiClient,
    log: &mut dyn FnMut(&str),
) -> Result<Vec<super::records::RecordRow>, String> {
    log("[records] 拉取跑步记录…");
    let rows = super::records::fetch_records(client)?;
    if let Some(session) = &client.login {
        let reconciliation = (|| {
            let mut guard = super::submission_guard::Guard::open(
                session.uid,
                session.unid.parse().map_err(|_| "学校编号无效")?,
            )?;
            // 记录页刷新就是任务里说的「人去看一眼」：能靠列表定论的回执在这里落终态。
            let now = crate::crypto::envelope::now_ms();
            for id in guard.resolve(&rows, now)? {
                log(&format!("[record] 已核对先前提交的记录 #{id}；未重复提交"));
            }
            if guard.pending_count() > 0 {
                log("[record] 仍有未决的提交回执（对账窗口内、或对账没有结论）；重复提交保护继续生效");
            }
            Ok::<_, String>(())
        })();
        if let Err(error) = reconciliation {
            log(&format!("[record] 提交保护核对未完成：{error}"));
        }
    }
    log(&format!("√ [records] {} 条记录", rows.len()));
    Ok(rows)
}

#[cfg(test)]
mod verification_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn reading_a_detail_is_not_a_successful_grade() {
        let failed = Verification::from_result(Ok(json!({"complete": false})));
        assert!(failed.detail.is_some());
        assert_eq!(failed.label(), "未达标");
        let unknown = Verification::from_result(Ok(json!({"reasonList": []})));
        assert_eq!(unknown.complete, None);
        assert_eq!(unknown.label(), "未知");
        let contradictory = Verification::from_result(Ok(
            json!({"complete": true, "reasonList": [{"complete": false}]}),
        ));
        assert_eq!(contradictory.complete, Some(false));
    }
    /// A7：整分起点要补真随机的秒/毫秒，且不能跨分钟、不能动随机模式。
    #[test]
    fn specified_start_gets_real_seconds_and_millis() {
        let minute = 1_792_730_760_000_i64; // 某个整分（= 29878846 * 60_000）
        assert_eq!(minute % 60_000, 0, "样本必须是整分");
        let draws: Vec<i64> = [0_u32, 7_321, 59_999, 30_000]
            .iter()
            .map(|d| second_precision_start(minute, *d, minute + 120_000))
            .collect();
        // ① 落在同一分钟内：分钟不变、仍属于这一分钟。
        for value in &draws {
            assert_eq!(value / 60_000, minute / 60_000, "跨分钟了：{value}");
            assert!(*value >= minute && *value < minute + 60_000);
        }
        // ② 秒与毫秒都不再恒定 0。
        assert!(draws.iter().any(|v| (v % 60_000) / 1000 != 0), "秒仍是 0");
        assert!(draws.iter().any(|v| v % 1000 != 0), "毫秒仍是 0");
        // ③ 不同抽样给出不同时刻（不是固定偏移）。
        assert!(
            draws.windows(2).any(|w| w[0] != w[1]),
            "不同抽样得到相同起点：{draws:?}"
        );

        // ⑤ F4：指定时刻就是「现在」时（run.rs 会把未来时刻钳到今天），
        //    补秒/毫秒不能把开始时间推到未来。
        let now = minute + 3_000; // 用户选的那一分钟已经过去 3 秒
        for draw in [0u32, 1, 30_000, 59_999] {
            let start = second_precision_start(minute, draw, now);
            assert!(start <= now, "开始时间不能晚于现在：{start} > {now}");
        }
        // ④ 随机模式（非整分）原样保留，避免破坏与 7:00–20:00 窗口的对齐。
        let sampled = minute + 12_345;
        assert_eq!(
            second_precision_start(sampled, 30_000, sampled + 60_000),
            sampled
        );
    }

    /// N4：形态度量的标定 —— 圆环 vs 瘦长三角环，短长轴比要拉开一个数量级。
    ///
    /// 这两个数字就是 `THIN_SHAPE_AXIS_RATIO = 0.40` 的依据：形状正常的环在 0.9 以上，
    /// 真实被封样本那类「三点近似共线」的瘦长三角环在 0.25 以下。
    #[test]
    fn shape_axis_ratio_separates_thin_triangle_from_round_ring() {
        let round: Vec<(f64, f64)> = crate::track::generator::build(
            2000.0,
            600,
            7,
            (39.9, 116.4),
            1_700_000_000_000,
            &[
                (39.9000, 116.4000),
                (39.9000, 116.4040),
                (39.9030, 116.4040),
                (39.9030, 116.4000),
            ],
        )
        .locations
        .iter()
        .map(|p| (p.gLat, p.gLng))
        .collect();
        // 三个近似共线的点位（纬度只差 0.0002°、经度拉满）→ 瘦长三角形
        let thin: Vec<(f64, f64)> = crate::track::generator::build(
            2000.0,
            600,
            7,
            (39.9, 116.4),
            1_700_000_000_000,
            &[
                (39.9000, 116.3990),
                (39.9002, 116.4040),
                (39.9004, 116.4090),
            ],
        )
        .locations
        .iter()
        .map(|p| (p.gLat, p.gLng))
        .collect();
        let round_ratio = shape_axis_ratio(&round);
        let thin_ratio = shape_axis_ratio(&thin);
        assert!(
            round_ratio > 0.80,
            "圆形环的短长轴比只有 {round_ratio:.2}（期望 >0.80）"
        );
        assert!(
            thin_ratio < 0.25,
            "瘦长三角环的短长轴比有 {thin_ratio:.2}（期望 <0.25）"
        );
        assert!(
            round_ratio > THIN_SHAPE_AXIS_RATIO && thin_ratio < THIN_SHAPE_AXIS_RATIO,
            "阈值 {THIN_SHAPE_AXIS_RATIO} 必须落在两者之间：round={round_ratio:.2} thin={thin_ratio:.2}"
        );

        // 阈值表另外两档必须钉住（阈值 0.45 的依据就是这两档之间的空档）：
        // 2:1 细长跑道环不能误报，3:1 以上就该提示「像一根线」。
        let ratio_of = |pts: &[(f64, f64)]| -> f64 {
            let t = crate::track::generator::build(
                2000.0,
                600,
                7,
                (39.9, 116.4),
                1_700_000_000_000,
                pts,
            );
            let path: Vec<(f64, f64)> = t.locations.iter().map(|p| (p.gLat, p.gLng)).collect();
            shape_axis_ratio(&path)
        };
        // 39.9°N：纬度 0.001° ≈ 111 m，经度 0.001° ≈ 85 m
        let rect_2to1 = [
            (39.9000, 116.4000),
            (39.9000, 116.4072),
            (39.9027, 116.4072),
            (39.9027, 116.4000),
        ];
        let rect_3to1 = [
            (39.9000, 116.4000),
            (39.9000, 116.4108),
            (39.9027, 116.4108),
            (39.9027, 116.4000),
        ];
        let r2 = ratio_of(&rect_2to1);
        let r3 = ratio_of(&rect_3to1);
        assert!(
            r2 > THIN_SHAPE_AXIS_RATIO,
            "2:1 细长跑道环（{r2:.2}）不该触发瘦长提示"
        );
        assert!(
            r3 < THIN_SHAPE_AXIS_RATIO,
            "3:1 长条环（{r3:.2}）应当触发瘦长提示"
        );
        assert!(r2 > r3, "轴比必须随长宽比单调：2:1={r2:.2} 3:1={r3:.2}");
    }

    /// N4：瘦长环在 Legacy 下告警；圆形环不告警；其它模式一律不提示。
    #[test]
    fn thin_legacy_ring_warns_but_never_blocks_and_other_modes_stay_silent() {
        let thin: Vec<(f64, f64)> = crate::track::generator::build(
            2000.0,
            600,
            7,
            (39.9, 116.4),
            1_700_000_000_000,
            &[
                (39.9000, 116.3990),
                (39.9002, 116.4040),
                (39.9004, 116.4090),
            ],
        )
        .locations
        .iter()
        .map(|p| (p.gLat, p.gLng))
        .collect();
        let warning = thin_shape_warning(&thin, RouteMode::Legacy).expect("瘦长环应当告警");
        assert!(warning.contains("瘦长"), "文案要说清形状：{warning}");
        assert!(
            warning.contains(&format!("{THIN_SHAPE_AXIS_RATIO:.2}")),
            "文案要给出阈值，用户才知道差多少：{warning}"
        );
        assert!(
            warning.contains("短长轴比 0."),
            "文案要给出实际比值：{warning}"
        );
        assert!(
            warning.contains("道路") && warning.contains("瓦片"),
            "文案要给出可操作的出口：{warning}"
        );
        // 只提示不阻断：函数返回的是 Option<String>，没有错误类型可用 —— 这里再钉一次语义。
        assert!(warning.len() > 20);
        // 沿真实路网的模式不该被这条误报
        for mode in [RouteMode::Road, RouteMode::Tiles, RouteMode::Manual] {
            assert!(
                thin_shape_warning(&thin, mode).is_none(),
                "{mode:?} 模式不该提示瘦长"
            );
        }
    }

    /// N4：圆形环不告警（阈值不能拍得太高，把正常环也报出来）。
    #[test]
    fn round_ring_does_not_warn_about_shape() {
        let round: Vec<(f64, f64)> = crate::track::generator::build(
            3000.0,
            900,
            11,
            (39.9, 116.4),
            1_700_000_000_000,
            &[
                (39.9000, 116.4000),
                (39.9000, 116.4040),
                (39.9020, 116.4055),
                (39.9030, 116.4040),
                (39.9030, 116.4000),
                (39.9020, 116.3985),
            ],
        )
        .locations
        .iter()
        .map(|p| (p.gLat, p.gLng))
        .collect();
        assert!(
            shape_axis_ratio(&round) > THIN_SHAPE_AXIS_RATIO,
            "正常环的轴比不该低于阈值"
        );
        assert_eq!(thin_shape_warning(&round, RouteMode::Legacy), None);
    }

    /// N5：门槛与计划区间的单位不能混 —— 门槛用米、区间用 km 并给出米换算。
    #[test]
    fn distance_floor_warning_keeps_metres_and_kilometres_straight() {
        let warning = distance_floor_warning(1.0, 1.5, 2000).expect("默认区间撞 2000 m 门槛");
        assert!(warning.contains("≥ 2000 m"), "门槛要用米：{warning}");
        assert!(
            warning.contains("1.0–1.5 km"),
            "计划区间要用 UI 的 km：{warning}"
        );
        assert!(
            warning.contains("1000–1500 m"),
            "同时给出米换算，避免用户对不上：{warning}"
        );
        assert!(
            warning.contains("2.0 km"),
            "出口要换算成用户填的 km：{warning}"
        );
        assert!(
            warning.contains("直接拒绝"),
            "要说清后果（会被拒跑）：{warning}"
        );
        assert!(
            warning.contains("不会替你改配置"),
            "要说明工具不擅自改配置：{warning}"
        );

        // 上限够得着门槛 → 不警告（等于门槛也算够）
        assert_eq!(distance_floor_warning(1.0, 2.5, 2000), None);
        assert_eq!(distance_floor_warning(1.0, 2.0, 2000), None);
        // 门槛未知（0/负数）→ 不警告
        assert_eq!(distance_floor_warning(1.0, 1.5, 0), None);
        assert_eq!(distance_floor_warning(1.0, 1.5, -1), None);
        // 1600 m 门槛同样按米说
        let w1600 = distance_floor_warning(1.0, 1.5, 1600).expect("1600 m 门槛也该提示");
        assert!(w1600.contains("≥ 1600 m") && w1600.contains("1.6 km"));
    }

    /// A4：只读围栏校验 —— 数点、给文案，但不改轨迹、不拦提交。
    #[test]
    fn fence_check_counts_outside_samples_and_warns_without_blocking() {
        let track = crate::track::generator::build(
            2000.0,
            600,
            7,
            (39.9, 116.4),
            1_700_000_000_000,
            &[(39.9, 116.4), (39.901, 116.401), (39.902, 116.4)],
        );
        // 没有围栏：不做判断、也不告警。
        assert_eq!(count_points_outside_fences(&track, &[]), 0);
        assert_eq!(fence_warning(0), None);

        // 围栏在很远的地方：所有采样点都在外面，文案要点明数量与后果。
        let far = vec![vec![
            (30.0, 100.0),
            (30.001, 100.0),
            (30.001, 100.001),
            (30.0, 100.001),
        ]];
        let all = track.locations.len();
        assert_eq!(count_points_outside_fences(&track, &far), all);
        let message = fence_warning(all).unwrap_or_default();
        assert!(
            message.contains(&all.to_string()),
            "文案要带数量：{message}"
        );
        assert!(message.contains("围栏外"), "文案要说清问题：{message}");
        assert!(
            message.contains("可能被判无效"),
            "文案要说清后果：{message}"
        );

        // 把整条轨迹包起来：一个都不在外面。
        let lats: Vec<f64> = track.locations.iter().map(|p| p.gLat).collect();
        let lngs: Vec<f64> = track.locations.iter().map(|p| p.gLng).collect();
        let (lat_lo, lat_hi) = (
            lats.iter().copied().fold(f64::INFINITY, f64::min) - 0.002,
            lats.iter().copied().fold(f64::NEG_INFINITY, f64::max) + 0.002,
        );
        let (lng_lo, lng_hi) = (
            lngs.iter().copied().fold(f64::INFINITY, f64::min) - 0.002,
            lngs.iter().copied().fold(f64::NEG_INFINITY, f64::max) + 0.002,
        );
        let around = vec![vec![
            (lat_lo, lng_lo),
            (lat_lo, lng_hi),
            (lat_hi, lng_hi),
            (lat_hi, lng_lo),
        ]];
        assert_eq!(count_points_outside_fences(&track, &around), 0);
        assert_eq!(fence_warning(0), None, "没问题时不能刷告警");
        // 校验是只读的：轨迹点一个没少、距离没被改。
        assert_eq!(track.locations.len(), all);
        assert!((track.totalDistance - 2000.0).abs() < 0.01);
    }

    /// A8：要求人脸核验而没提供核验数据时，结论里必须带出这句（UI/CLI 共用 summary）。
    /// 警告只加文案，不改判定与退出码 —— 提交行为不变。
    #[test]
    fn face_required_warning_is_visible_in_the_shared_summary() {
        let mut out = outcome(2, Verification::from_result(Ok(json!({"complete": true}))));
        assert_eq!(out.verdict(), RunVerdict::Complete);
        assert!(
            !out.summary().contains("人脸核验"),
            "没有警告时不该出现核验提示"
        );

        out.warnings
            .push("已提交，但校方要求人脸核验而未提供核验数据，可能不计入有效次数".to_string());
        let summary = out.summary();
        assert!(
            summary.contains("人脸核验"),
            "结论里看不到核验提示：{summary}"
        );
        assert!(
            summary.contains("可能不计入有效次数"),
            "要说明后果：{summary}"
        );
        assert!(
            summary.contains("记录完成"),
            "原有结论不能被警告替换掉：{summary}"
        );
        // 警告不是判定：退出码与机器码都不变。
        assert_eq!(out.exit_code(), 0);
        assert_eq!(out.verdict_code(), "complete");
    }

    #[test]
    fn server_skew_handles_seconds_and_millis() {
        let now = crate::crypto::envelope::now_ms();
        // 毫秒级
        let skew = server_skew_ms(now + 10_000);
        assert!((skew - 10_000).abs() <= 2, "毫秒单位换算有误: {skew}");
        // 秒级（精度 1 秒，允许 1 秒误差）
        let secs = (now + 10_000) / 1000;
        let skew = server_skew_ms(secs);
        assert!((skew - 10_000).abs() <= 1_000, "秒单位换算有误: {skew}");
        // 未知时间戳不报偏差
        assert_eq!(server_skew_ms(0), 0);
    }

    #[test]
    fn failure_reasons_expose_only_unmet_entries() {
        let v = Verification::from_result(Ok(json!({
            "complete": false,
            "reasonList": [
                {"reason": "里程未达标", "complete": false},
                {"reason": "时长达标", "complete": true},
                {"reason": "  ", "complete": false}
            ]
        })));
        assert_eq!(v.failure_reasons(), vec!["里程未达标".to_string()]);
        // 没有 detail 时不编造原因
        assert!(Verification::from_result(Err("offline".into()))
            .failure_reasons()
            .is_empty());
    }

    #[test]
    fn consecutive_results_do_not_share_details() {
        let first = Verification::from_result(Ok(json!({"complete": true, "rrid": 1})));
        let second = Verification::from_result(Err("offline".into()));
        assert_eq!(first.complete, Some(true));
        assert!(second.detail.is_none());
        assert_eq!(second.complete, None);
        assert_eq!(second.error.as_deref(), Some("offline"));
    }

    fn outcome(obs_ok: usize, verification: Verification) -> RunOutcome {
        RunOutcome {
            result: SubmitResult {
                rrid: 1,
                uuid: "u".into(),
                start_ms: 0,
                complete: None,
                total_dis: 3000.0,
                total_time: 1200,
                total_steps: 3000,
                avg_step_freq: 150,
                calorie: 200,
                avg_power: 100,
                sel_distance: 2000,
            },
            obs_ok,
            verification,
            warnings: Vec::new(),
        }
    }

    /// 审计 F-01 的回归：请求被接受 ≠ 记录完成。
    #[test]
    fn accepted_submission_is_only_success_when_every_stage_passed() {
        let graded = || Verification::from_result(Ok(json!({"complete": true})));
        let rejected = || {
            Verification::from_result(Ok(
                json!({"complete": false, "reasonList": [{"complete": false, "reason": "里程未达标"}]}),
            ))
        };
        let unreadable = || Verification::from_result(Err("timeout".into()));

        let complete = outcome(2, graded());
        assert_eq!(complete.verdict(), RunVerdict::Complete);
        assert!(complete.is_success());
        assert_eq!(complete.exit_code(), 0);

        // 服务端明确未达标：即使 OBS 全传也失败，且退出码单独可辨。
        let not_counted = outcome(2, rejected());
        assert_eq!(not_counted.verdict(), RunVerdict::NotCounted);
        assert!(!not_counted.is_success());
        assert_eq!(not_counted.exit_code(), 4);
        assert!(not_counted.summary().contains("里程未达标"));

        // OBS 没传全：详情即使达标也不算完成。
        let obs_short = outcome(1, graded());
        assert_eq!(obs_short.verdict(), RunVerdict::ObsIncomplete);
        assert_eq!(obs_short.exit_code(), 2);
        assert!(!obs_short.is_success());

        // 详情读不到：未知，不能当成功。
        let unknown = outcome(2, unreadable());
        assert_eq!(unknown.verdict(), RunVerdict::Unverified);
        assert_eq!(unknown.exit_code(), 3);
        assert!(!unknown.is_success());

        // 详情可读但没有判定字段：同样是未知。
        let no_flag = outcome(2, Verification::from_result(Ok(json!({"reasonList": []}))));
        assert_eq!(no_flag.verdict(), RunVerdict::Unverified);
    }
}

#[cfg(test)]
#[path = "fake_run_tests.rs"]
mod fake_run_tests;

#[cfg(test)]
mod audit_param_tests {
    use super::*;

    /// C1：指定时刻 + 时长越过现在 → 必须拒绝（stopTime 落在未来的复现）。
    ///
    /// 复现路径：`specified_time` 只把**开始时间**钳到 now，于是
    /// 「现在 12:05、填 12:00、用时 30 分钟」得到 stopTime = 12:30。
    /// 这里直接拿那一对值喂给校验函数。
    #[test]
    fn future_stop_time_is_rejected() {
        let now = 1_700_000_000_000;
        let start = now - 5 * 60_000; // 5 分钟前开始
        let dur = 30 * 60; // 但要跑 30 分钟
        let err = reject_future_stop(start, dur, now).unwrap_err();
        assert!(err.contains("落在未来"), "{err}");
        assert!(err.contains("12:30") || err.contains("未来"), "{err}");
        // 刚好跑完（stop == now）允许；早一秒也允许。
        assert!(reject_future_stop(now - dur * 1000, dur, now).is_ok());
        assert!(reject_future_stop(now - dur * 1000 - 1, dur, now).is_ok());
        // 溢出不能 panic。
        assert!(reject_future_stop(i64::MAX - 5, 3_600, now).is_err());
    }

    /// C1：`effective_duration` 之后时长会变，校验必须用**最终**时长。
    ///
    /// 反例（改前就会漏判）：dist/dur 的配速越出有效窗口时，`effective_duration`
    /// 会把 dur 拉大；按旧 dur 校验过的开始时间，在新 dur 下 stopTime 就落到未来。
    #[test]
    fn check_uses_the_effective_duration() {
        let now = 1_700_000_000_000;
        let dist = 3000.0;
        let requested = 300; // 10 m/s，远超 SPEED_CEIL
        let effective = crate::track::generate_road::effective_duration(dist, requested);
        assert!(effective > requested, "本用例需要一个变长的时长");
        let start = now - requested * 1000 - 1_000; // 按旧 dur 刚好放行
        assert!(
            reject_future_stop(start, requested, now).is_ok(),
            "旧时长下本应放行（说明用例前提变了）"
        );
        assert!(
            reject_future_stop(start, effective, now).is_err(),
            "用最终时长校验时必须拒绝"
        );
    }

    /// C3：计划距离低于学校门槛 → 本地拦截，且门槛原样保留（不下调凑达标）。
    #[test]
    fn plan_below_school_threshold_is_refused() {
        let err = reject_below_min_distance(1200.0, 2000).unwrap_err();
        assert!(err.contains("2000"), "要写明学校门槛：{err}");
        assert!(err.contains("1200"), "要写明本次计划距离：{err}");
        assert!(err.contains("不会下调学校门槛"), "要说明不改门槛：{err}");
        // 达标 / 恰好达标 / 门槛未知 → 放行。
        assert!(reject_below_min_distance(2000.0, 2000).is_ok());
        assert!(reject_below_min_distance(2000.5, 2000).is_ok());
        assert!(reject_below_min_distance(800.0, 0).is_ok());
    }

    /// F3：没配置学校基准时必须进 warnings（不是普通日志），且文案给出出口。
    ///
    /// 背景：C10 把「随机 20–260 m」改回固定 82 m，问题只是从「随机错」变成「固定错」。
    /// 校区在包头（约 1050 m）时这条记录仍低几百米，用户如果不被告知就永远不会去填。
    #[test]
    fn unset_school_base_altitude_is_warned_with_an_exit() {
        use crate::track::altitude::BaseAltitudeOrigin;
        let warning =
            default_altitude_warning(BaseAltitudeOrigin::Default).expect("默认基准必须给出警告");
        // 文案要同时点出「按 82 m 生成」和两个可操作的出口。
        assert!(warning.contains("82"), "要写明当前用的默认值：{warning}");
        assert!(
            warning.contains("--school-altitude"),
            "要给出 CLI 出口：{warning}"
        );
        assert!(
            warning.contains("school_base_altitude"),
            "要给出配置文件键名：{warning}"
        );
        assert!(
            warning.contains("手动海拔"),
            "要给出 UI 出口（跑步页手动海拔）：{warning}"
        );
        assert!(
            warning.contains("未配置学校基准海拔"),
            "要说明原因：{warning}"
        );

        // 配了基准就不该再唠叨：学校基准 / 手动单值 / 手动区间都算「已配置」。
        for origin in [
            BaseAltitudeOrigin::School,
            BaseAltitudeOrigin::Manual,
            BaseAltitudeOrigin::ManualRange,
        ] {
            assert!(
                default_altitude_warning(origin).is_none(),
                "{origin:?} 已配置基准，不该再警告"
            );
        }
    }
}
