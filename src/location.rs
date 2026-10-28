//! 运行位置的统一数据结构与校验。
//!
//! 设备锚点、点位请求和跑步记录使用同一套纬度/经度语义。

use serde::{Deserialize, Serialize};

const DEFAULT_ANCHOR_LAT: f64 = 38.901678;
const DEFAULT_ANCHOR_LON: f64 = 121.540241;
const DEFAULT_CITY: &str = "大连市";

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Coordinate {
    pub latitude: f64,
    pub longitude: f64,
    #[serde(default)]
    pub accuracy: f64,
}

impl Coordinate {
    pub fn new(latitude: f64, longitude: f64, accuracy: f64) -> Result<Self, String> {
        let coordinate = Self {
            latitude,
            longitude,
            accuracy,
        };
        coordinate.validate()?;
        Ok(coordinate)
    }

    pub fn validate(self) -> Result<(), String> {
        if !self.latitude.is_finite() || !(-90.0..=90.0).contains(&self.latitude) {
            return Err(format!("纬度超出范围: {}", self.latitude));
        }
        if !self.longitude.is_finite() || !(-180.0..=180.0).contains(&self.longitude) {
            return Err(format!("经度超出范围: {}", self.longitude));
        }
        if !self.accuracy.is_finite() || self.accuracy < 0.0 {
            return Err(format!("定位精度无效: {}", self.accuracy));
        }
        Ok(())
    }

    pub fn is_near(self, other: Self, tolerance: f64) -> bool {
        (self.latitude - other.latitude).abs() <= tolerance
            && (self.longitude - other.longitude).abs() <= tolerance
    }

    pub fn is_default_dalian(self) -> bool {
        self.is_near(
            Self {
                latitude: DEFAULT_ANCHOR_LAT,
                longitude: DEFAULT_ANCHOR_LON,
                accuracy: 0.0,
            },
            1e-6,
        )
    }
}

pub fn default_anchor() -> Coordinate {
    Coordinate {
        latitude: DEFAULT_ANCHOR_LAT,
        longitude: DEFAULT_ANCHOR_LON,
        accuracy: 0.0,
    }
}

/// 计划级上传锚点偏移：同一个 `plan_seed` 必然得到同一个偏移后的锚点。
///
/// 参考实现（ScienseRunner v1.0.10）公告里的做法：
/// - 设备信息里的锚点**本身保持不变**（围栏、界面、历史记录都还是精确位置）；
/// - 每生成一个新跑步计划，对**上传用的锚点**施加一次约 40–60 m 的随机方向偏移；
/// - 预览与提交复用同一份偏移，每个新计划重新抽取。
///
/// 为什么按计划而不是按请求随机：逐请求漂移会让「点位集合」与「围栏判断」
/// 指向两个不同的地方（同一条记录自相矛盾），按计划随机只制造一次一致的偏移。
///
/// `offset_m <= 0`（或非有限）时**逐位**返回精确锚点 —— 零偏移不能引入任何
/// 浮点噪声，否则「0 = 精确锚点」这条约定就没法用等号验证。
pub fn plan_anchor(exact: Coordinate, offset_m: f64, plan_seed: u64) -> Coordinate {
    if !offset_m.is_finite() || offset_m <= 0.0 {
        return exact;
    }
    let mut state = plan_seed ^ 0x9E37_79B9_7F4A_7C15;
    let angle = splitmix_unit(&mut state) * std::f64::consts::TAU;
    // 幅度取 0.8–1.2 × offset_m：默认 50 m 时落在公告说的「约 40–60 m」。
    let radius_m = offset_m * (0.8 + 0.4 * splitmix_unit(&mut state));
    let dlat = radius_m * angle.cos() / crate::track::geom::MET_PER_DEG_LAT;
    let dlon = radius_m * angle.sin() / crate::track::geom::MET_PER_DEG_LNG;
    Coordinate {
        latitude: (exact.latitude + dlat).clamp(-90.0, 90.0),
        longitude: wrap_longitude(exact.longitude + dlon),
        accuracy: exact.accuracy,
    }
}

/// splitmix64 取样到 [0, 1)。自带的确定性保证：同 seed 同结果，与平台/线程无关。
fn splitmix_unit(state: &mut u64) -> f64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    // 取高 53 位 → 均匀落在 [0,1) 且不损失精度。
    (z >> 11) as f64 / (1u64 << 53) as f64
}

/// 经度折回 [-180, 180)：偏移发生在 ±180° 附近时不产生非法值。
fn wrap_longitude(lon: f64) -> f64 {
    let wrapped = (lon + 180.0).rem_euclid(360.0) - 180.0;
    if wrapped.is_finite() {
        wrapped
    } else {
        lon
    }
}

pub fn default_city() -> &'static str {
    DEFAULT_CITY
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_bounds_and_accuracy() {
        assert!(Coordinate::new(39.9, 116.4, 5.0).is_ok());
        assert!(Coordinate::new(91.0, 116.4, 5.0).is_err());
        assert!(Coordinate::new(39.9, 181.0, 5.0).is_err());
        assert!(Coordinate::new(39.9, 116.4, -1.0).is_err());
    }

    #[test]
    fn detects_legacy_default_location() {
        assert!(default_anchor().is_default_dalian());
        assert!(!Coordinate::new(39.9, 116.4, 0.0)
            .unwrap()
            .is_default_dalian());
    }

    /// 用代码里同一套米/度换算量出偏移距离（米）。
    fn offset_m(a: Coordinate, b: Coordinate) -> f64 {
        let dy = (a.latitude - b.latitude) * crate::track::geom::MET_PER_DEG_LAT;
        let dx = (a.longitude - b.longitude) * crate::track::geom::MET_PER_DEG_LNG;
        dx.hypot(dy)
    }

    #[test]
    fn zero_offset_returns_the_exact_anchor_bit_for_bit() {
        let exact = Coordinate::new(39.9042, 116.4074, 3.5).unwrap();
        for seed in [0u64, 1, 42, u64::MAX] {
            let got = plan_anchor(exact, 0.0, seed);
            assert_eq!(got.latitude.to_bits(), exact.latitude.to_bits());
            assert_eq!(got.longitude.to_bits(), exact.longitude.to_bits());
            assert_eq!(got.accuracy.to_bits(), exact.accuracy.to_bits());
            // 负数/NaN 同样视为「关闭偏移」。
            assert_eq!(plan_anchor(exact, -5.0, seed), exact);
            assert_eq!(plan_anchor(exact, f64::NAN, seed), exact);
        }
    }

    #[test]
    fn same_plan_seed_reuses_one_offset_and_new_plans_draw_again() {
        let exact = Coordinate::new(39.9042, 116.4074, 0.0).unwrap();
        // 同一计划（同 seed）取两次：必须一模一样 —— 预览与提交各算一次也不会漂。
        let a = plan_anchor(exact, 50.0, 0xDEAD_BEEF);
        let b = plan_anchor(exact, 50.0, 0xDEAD_BEEF);
        assert_eq!(a, b);
        // 新计划（新 seed）：方向/幅度重抽，几乎不可能与上一份重合。
        let c = plan_anchor(exact, 50.0, 0xDEAD_BEF0);
        assert_ne!(a, c);
        let mut distinct = std::collections::HashSet::new();
        for seed in 0..64u64 {
            let p = plan_anchor(exact, 50.0, seed);
            distinct.insert((p.latitude.to_bits(), p.longitude.to_bits()));
        }
        assert_eq!(distinct.len(), 64, "64 个计划应抽到 64 份不同偏移");
    }

    #[test]
    fn offset_magnitude_stays_in_the_announced_band() {
        let exact = Coordinate::new(39.9042, 116.4074, 0.0).unwrap();
        for seed in 0..256u64 {
            let p = plan_anchor(exact, 50.0, seed);
            let d = offset_m(exact, p);
            assert!(
                (39.5..=60.5).contains(&d),
                "seed {seed} 的偏移 {d:.2} m 超出 40–60 m 区间"
            );
            // 偏移后的点仍是合法坐标。
            p.validate().expect("偏移后必须仍是合法坐标");
        }
        // offset_m 线性缩放：200 m 时落在 160–240 m。
        for seed in 0..64u64 {
            let d = offset_m(exact, plan_anchor(exact, 200.0, seed));
            assert!((159.0..=241.0).contains(&d), "200 m 偏移实际 {d:.2} m");
        }
        // 10 m 的小偏移同样按比例。
        let d = offset_m(exact, plan_anchor(exact, 10.0, 7));
        assert!((7.9..=12.1).contains(&d), "10 m 偏移实际 {d:.2} m");
    }

    #[test]
    fn offset_keeps_coordinates_valid_near_the_antimeridian() {
        let exact = Coordinate::new(10.0, 179.9999, 0.0).unwrap();
        let p = plan_anchor(exact, 200.0, 5);
        p.validate().expect("跨 180° 经线也必须合法");
    }
}
