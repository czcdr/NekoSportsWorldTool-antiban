//! 轨迹海拔覆盖。
//!
//! 海拔是轨迹点的一部分，而不是提交时临时拼出来的字段。统一在生成器
//! 完成后覆盖 `bdA`，这样提交体的 totalAscent、OBS 的 run_data 和每圈
//! elevationGain 都会读取同一份数据。

#![allow(non_snake_case)]

use super::geom::round_to;
use super::model::Track;

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AltitudeRange {
    pub min_m: f64,
    pub max_m: f64,
}

/// 海拔（绝对高程）允许范围：从地下场地到高原，覆盖所有真实校区。
pub const ALTITUDE_MIN_M: f64 = -500.0;
pub const ALTITUDE_MAX_M: f64 = 9000.0;

/// 既没有手动海拔、也没有学校基准时使用的默认海拔基准（米）。
///
/// 这是 0.5.0 之前的老行为。0.6.x 为了消掉「所有账号都是 82 m」的聚类特征改成了
/// `rng.uniform(20.0, 260.0)`，结果错得更离谱：包头校区实测约 1050 m，
/// 每条记录低约 800 m —— 服务端只要知道校区高程，这就是一眼可查的参数异常
/// （管理员复核意见「APP参数数据异常201」）。现在改回**已知的固定值**：
/// 报一个说得清的默认值，比编造一个谁也解释不了的随机高程强。
pub const DEFAULT_BASE_ALTITUDE_M: f64 = 82.0;

/// 海拔基准来自哪里 —— 必须能被日志与界面说清楚。
///
/// 用户看到的 bdA 是 82 m 还是 1050 m，取决于这一项；不写出来，
/// 用户根本不知道自己报了多少米（审计 C10 的另一半）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BaseAltitudeOrigin {
    /// config.manual_altitude（单值）
    Manual,
    /// config.manual_altitude_range（取中值作为 OU 回归基准，生成后再映射到区间）
    ManualRange,
    /// config.school_base_altitude（学校预设）
    School,
    /// 都没配：默认 82 m
    Default,
}

impl BaseAltitudeOrigin {
    pub fn label(self) -> &'static str {
        match self {
            Self::Manual => "手动海拔",
            Self::ManualRange => "手动海拔区间（中值）",
            Self::School => "学校基准海拔",
            Self::Default => "默认基准（未设置学校基准）",
        }
    }
}

/// 把基准值与来源拼成一句人能读懂的话，供日志与 UI 共用。
pub fn describe_base_altitude(value_m: f64, origin: BaseAltitudeOrigin) -> String {
    format!("{value_m:.1} m（{}）", origin.label())
}

/// 解析本次轨迹的海拔基准。
///
/// 优先级（审计 C10 的要求）：
/// 1. `manual_altitude`（用户显式填的绝对海拔）；
/// 2. `manual_altitude_range`（区间模式，取中值当回归基准，生成后再整体映射到区间）；
/// 3. `school_base_altitude`（学校预设，填一次）；
/// 4. 都没有 → [`DEFAULT_BASE_ALTITUDE_M`]（82 m），**不是随机区间**。
///
/// 非法值（NaN / 超出 -500–9000）一律跳过并继续往下找，不生成不可序列化的 bdA。
pub fn resolve_base_altitude(
    manual: Option<f64>,
    range: Option<AltitudeRange>,
    school: Option<f64>,
) -> (f64, BaseAltitudeOrigin) {
    if let Some(value) = manual.filter(|v| is_valid_altitude(*v)) {
        return (value, BaseAltitudeOrigin::Manual);
    }
    if let Some(range) = range.filter(|r| is_valid_altitude(r.min_m) && is_valid_altitude(r.max_m))
    {
        return (
            (range.min_m + range.max_m) / 2.0,
            BaseAltitudeOrigin::ManualRange,
        );
    }
    if let Some(value) = school.filter(|v| is_valid_altitude(*v)) {
        return (value, BaseAltitudeOrigin::School);
    }
    (DEFAULT_BASE_ALTITUDE_M, BaseAltitudeOrigin::Default)
}

fn is_valid_altitude(value: f64) -> bool {
    value.is_finite() && (ALTITUDE_MIN_M..=ALTITUDE_MAX_M).contains(&value)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AltitudeSpec {
    Single(f64),
    Range(AltitudeRange),
}

pub fn parse_spec(text: &str) -> Result<Option<AltitudeSpec>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    if let Some((left, right)) = text.split_once('-') {
        let min_m = left
            .trim()
            .parse::<f64>()
            .map_err(|_| "手动海拔区间格式应为 min-max，例如 11.6-22.8".to_string())?;
        let max_m = right
            .trim()
            .parse::<f64>()
            .map_err(|_| "手动海拔区间格式应为 min-max，例如 11.6-22.8".to_string())?;
        return Ok(Some(AltitudeSpec::Range(validate_range(min_m, max_m)?)));
    }
    let altitude_m = text
        .parse::<f64>()
        .map_err(|_| "手动海拔应为数字或 min-max 区间，例如 17.2 或 11.6-22.8".to_string())?;
    validate_altitude(altitude_m)?;
    Ok(Some(AltitudeSpec::Single(altitude_m)))
}

fn validate_altitude(altitude_m: f64) -> Result<(), String> {
    if !is_valid_altitude(altitude_m) {
        return Err(format!(
            "手动海拔必须是 {ALTITUDE_MIN_M:.0} 到 {ALTITUDE_MAX_M:.0} 米之间的数字"
        ));
    }
    Ok(())
}

fn validate_range(min_m: f64, max_m: f64) -> Result<AltitudeRange, String> {
    validate_altitude(min_m)?;
    validate_altitude(max_m)?;
    if min_m > max_m {
        return Err("手动海拔区间的最小值不能大于最大值".into());
    }
    Ok(AltitudeRange { min_m, max_m })
}

/// 将轨迹所有点的百度海拔覆盖为用户输入的绝对海拔（米）。
///
/// 返回 `Err` 而不是静默接受 NaN/无穷值，避免生成不可序列化或被服务端
/// 拒绝的提交。海拔范围采用常见地表范围，既能覆盖地下场地也能覆盖高原。
pub fn override_bd_a(track: &mut Track, altitude_m: f64) -> Result<(), String> {
    validate_altitude(altitude_m)?;
    for point in &mut track.locations {
        point.bdA = round_to(altitude_m, 2);
        point.hasAltitude = true;
    }
    Ok(())
}

/// 把整条轨迹的海拔**平移到**指定基准（保形，不改爬升）。
///
/// 生成器内部的基准是随机的（`rng.uniform(20.0, 260.0)`，C10 的老问题）；
/// 生成之后把整条曲线整体平移 `base − mean(bdA)` 米：
/// - 报出的绝对高程落在学校基准 / 手动值 / 默认 82 m 上；
/// - 曲线原有的起伏与 `totalAscent` **原样保留** —— 直接 `override_bd_a` 会把
///   每个点压成同一个值，于是每条记录的 ascent 都是 0，那本身就是另一种机器特征。
///
/// 非法基准或空轨迹时原样返回（调用方已用 [`resolve_base_altitude`] 归一过）。
pub fn shift_to_base_altitude(track: &mut Track, base_m: f64) {
    if !is_valid_altitude(base_m) || track.locations.is_empty() {
        return;
    }
    let count = track.locations.len() as f64;
    let mean = track.locations.iter().map(|p| p.bdA).sum::<f64>() / count;
    if !mean.is_finite() {
        return;
    }
    let delta = base_m - mean;
    for point in &mut track.locations {
        point.bdA = round_to(point.bdA + delta, 2);
        point.hasAltitude = true;
    }
}

/// 将生成器的海拔曲线平滑映射到用户指定的绝对海拔区间。
pub fn override_bd_a_range(track: &mut Track, range: AltitudeRange) -> Result<(), String> {
    let range = validate_range(range.min_m, range.max_m)?;
    let (mut current_min, mut current_max) = (f64::INFINITY, f64::NEG_INFINITY);
    for point in &track.locations {
        current_min = current_min.min(point.bdA);
        current_max = current_max.max(point.bdA);
    }
    let span = current_max - current_min;
    let target_span = range.max_m - range.min_m;
    for point in &mut track.locations {
        let mapped = if span.abs() < f64::EPSILON {
            (range.min_m + range.max_m) / 2.0
        } else {
            range.min_m + ((point.bdA - current_min) / span) * target_span
        };
        point.bdA = round_to(mapped.clamp(range.min_m, range.max_m), 2);
        point.hasAltitude = true;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::submit::total_ascent;
    use crate::track::generator::build;

    fn points() -> Vec<(f64, f64)> {
        vec![(38.901678, 121.540241), (38.902564, 121.541233)]
    }

    #[test]
    fn override_replaces_every_bd_a_and_clears_ascent() {
        let mut track = build(1200.0, 600, 7, (38.9, 121.54), 1_700_000_000_000, &points());
        override_bd_a(&mut track, 36.75).unwrap();
        assert!(track.locations.iter().all(|p| p.bdA == 36.75));
        assert_eq!(total_ascent(&track.locations), 0.0);
    }

    #[test]
    fn rejects_non_finite_or_out_of_range_values() {
        let mut track = build(1000.0, 500, 1, (38.9, 121.54), 1_700_000_000_000, &points());
        assert!(override_bd_a(&mut track, f64::NAN).is_err());
        assert!(override_bd_a(&mut track, 9001.0).is_err());
    }

    #[test]
    fn parses_single_value_and_range() {
        assert_eq!(
            parse_spec("17.2").unwrap(),
            Some(AltitudeSpec::Single(17.2))
        );
        assert_eq!(
            parse_spec("11.6-22.8").unwrap(),
            Some(AltitudeSpec::Range(AltitudeRange {
                min_m: 11.6,
                max_m: 22.8
            }))
        );
        assert!(parse_spec("22.8-11.6").is_err());
    }

    #[test]
    fn range_mapping_stays_inside_requested_bounds() {
        let mut track = build(1200.0, 600, 7, (38.9, 121.54), 1_700_000_000_000, &points());
        override_bd_a_range(
            &mut track,
            AltitudeRange {
                min_m: 11.6,
                max_m: 22.8,
            },
        )
        .unwrap();
        assert!(track
            .locations
            .iter()
            .all(|p| (11.6..=22.8).contains(&p.bdA)));
    }

    /// C10：基准海拔的优先级与「没有就回退 82、不抽样」的契约。
    ///
    /// 优先级：手动单值 > 手动区间（中值）> 学校基准 > 默认 82。
    #[test]
    fn base_altitude_priority_and_fallback() {
        let range = AltitudeRange {
            min_m: 100.0,
            max_m: 140.0,
        };
        assert_eq!(
            resolve_base_altitude(Some(1050.0), Some(range), Some(200.0)),
            (1050.0, BaseAltitudeOrigin::Manual),
            "手动单值必须最高优先"
        );
        assert_eq!(
            resolve_base_altitude(None, Some(range), Some(200.0)),
            (120.0, BaseAltitudeOrigin::ManualRange),
            "手动区间排第二（基准取中值）"
        );
        assert_eq!(
            resolve_base_altitude(None, None, Some(1050.0)),
            (1050.0, BaseAltitudeOrigin::School),
            "学校基准排第三（包头校区 1050 m）"
        );
        assert_eq!(
            resolve_base_altitude(None, None, None),
            (DEFAULT_BASE_ALTITUDE_M, BaseAltitudeOrigin::Default),
            "三者都没有时必须回退 82 m"
        );
        assert_eq!(DEFAULT_BASE_ALTITUDE_M, 82.0, "默认基准被改动过");
        // 非法值跳过往下找，而不是生成不可序列化的高程。
        assert_eq!(
            resolve_base_altitude(Some(f64::NAN), None, Some(1050.0)),
            (1050.0, BaseAltitudeOrigin::School)
        );
        assert_eq!(
            resolve_base_altitude(None, None, Some(9001.0)),
            (DEFAULT_BASE_ALTITUDE_M, BaseAltitudeOrigin::Default)
        );
        // 说明文案必须带来源，否则用户不知道自己报了什么。
        let text = describe_base_altitude(1050.0, BaseAltitudeOrigin::School);
        assert!(text.contains("1050.0"), "{text}");
        assert!(text.contains("学校基准"), "{text}");
    }

    /// C10 的输出侧验证：包装函数之后**报出去的海拔**才是关键。
    ///
    /// ① 整条轨迹的均值落在给定基准上（包头 1050 m），不是 20–260 随机；
    /// ② 同一 seed 两次运行逐点一致（随机基准已被覆盖）；
    /// ③ 起伏与 totalAscent 不被压平 —— 这是与 `override_bd_a`（每点同值、
    ///    于是每条记录 ascent=0）的区别，也是选「平移」而不是「覆盖」的理由。
    #[test]
    fn base_altitude_shifting_keeps_the_curve_and_is_deterministic() {
        use crate::track::generator::build_tuned_with_altitude;
        let pts = points();
        let run = |base: f64| {
            build_tuned_with_altitude(
                1200.0,
                600,
                7,
                (38.9, 121.54),
                1_700_000_000_000,
                &pts,
                0.0,
                base,
            )
        };
        let a = run(1050.0);
        let b = run(1050.0);
        let mean = a.locations.iter().map(|p| p.bdA).sum::<f64>() / a.locations.len() as f64;
        assert!(
            (mean - 1050.0).abs() < 0.01,
            "均值 {mean:.3} m 没有落在学校基准 1050 m 上"
        );
        let hi = a.locations.iter().map(|p| p.bdA).fold(f64::MIN, f64::max);
        let lo = a.locations.iter().map(|p| p.bdA).fold(f64::MAX, f64::min);
        assert!(hi - lo > 0.05, "海拔曲线被压平了（起伏 {:.3} m）", hi - lo);
        assert_eq!(
            a.locations
                .iter()
                .map(|p| p.bdA.to_bits())
                .collect::<Vec<_>>(),
            b.locations
                .iter()
                .map(|p| p.bdA.to_bits())
                .collect::<Vec<_>>(),
            "同一 seed 两次运行的海拔不一致 —— 说明基准仍是随机的"
        );
        assert_eq!(
            total_ascent(&a.locations),
            total_ascent(&b.locations),
            "平移后爬升必须逐位一致"
        );
        // 默认路径：82 m（不是随机区间）。
        let default = run(DEFAULT_BASE_ALTITUDE_M);
        let mean_default =
            default.locations.iter().map(|p| p.bdA).sum::<f64>() / default.locations.len() as f64;
        assert!(
            (mean_default - 82.0).abs() < 0.01,
            "默认基准 {mean_default:.3} m 不是 82 m"
        );
        // 轨迹形态不受海拔平移影响。
        assert_eq!(a.locations.len(), default.locations.len());
        assert_eq!(a.totalDistance, default.totalDistance);
        assert_eq!(a.totalTime, default.totalTime);
    }
}
