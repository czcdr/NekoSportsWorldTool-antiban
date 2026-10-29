//! 轨迹几何与随机工具。
//!
//! round 封装、RNG、打卡点 Catmull-Rom 拟合环 + 弧长表 + 弧长插值。

use chrono::{FixedOffset, TimeZone};
use rand::rngs::StdRng;
use rand::SeedableRng;
use rand_distr::{Distribution, Normal};

pub const MET_PER_DEG_LAT: f64 = 111_132.0;
/// 旧常量：北京附近每度经度的米数（≈ 111 320·cos 39°）。
///
/// **只给展示/排序用**（UI 像素换算、点数估算这类不需要高精度的场合）。
/// 轨迹生成一律用 [`met_per_deg_lng`]：固定值在南方会把东西向位移放大约 9%、
/// 在北方缩小约 5%，平面上的「圆」会被拉成椭圆（见该函数文档）。
pub const MET_PER_DEG_LNG: f64 = 86_600.0;

/// 赤道处每度经度的米数（WGS84 平均半径口径）。
pub const EQUATOR_MET_PER_DEG: f64 = 111_320.0;

/// 当前纬度下**每度经度**对应的米数：`111 320 · cos(lat)`。
///
/// 为什么必须按纬度算：`MET_PER_DEG_LNG` 是按北京（39°N）取的定值，
/// 用它做「米 → 度」的换算时，23.1°N 会偏 +9.3%、45.8°N 偏 −5.1%
/// （100 m 半径的圆实测：广州 +9.27%、上海 +5.00%、北京 −0.72%、
/// 包头 −1.29%、哈尔滨 −5.14%）。轨迹生成里所有 BD↔平面 的换算都走这里，
/// 传该点（或该环中心）的纬度。
///
/// 纬度方向仍用 [`MET_PER_DEG_LAT`]（111 132 m，与 111 320 差 0.17%），
/// 刻意不动它：它同时被上传锚点、点位请求、UI 显示共用，改它等于改协议口径。
pub fn met_per_deg_lng(lat: f64) -> f64 {
    EQUATOR_MET_PER_DEG * lat.to_radians().cos()
}

/// round(x, n)：按精确二进制值四舍五入。
pub fn round_to(x: f64, n: usize) -> f64 {
    let s = format!("{x:.n$}");
    s.parse().unwrap_or(x)
}

/// 可播种 RNG 封装。
pub struct Rng {
    inner: StdRng,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self {
            inner: StdRng::seed_from_u64(seed),
        }
    }
    pub fn random(&mut self) -> f64 {
        rand::Rng::gen_range(&mut self.inner, 0.0..1.0)
    }
    pub fn uniform(&mut self, a: f64, b: f64) -> f64 {
        rand::Rng::gen_range(&mut self.inner, a..=b)
    }
    pub fn randint(&mut self, a: i64, b: i64) -> i64 {
        rand::Rng::gen_range(&mut self.inner, a..=b)
    }
    pub fn gauss(&mut self, mu: f64, sigma: f64) -> f64 {
        Normal::new(mu, sigma).unwrap().sample(&mut self.inner)
    }
    pub fn choice<T>(&mut self, items: &[T]) -> T
    where
        T: Copy,
    {
        items[rand::Rng::gen_range(&mut self.inner, 0..items.len())]
    }
    pub fn weighted<T>(&mut self, items: &[(T, u32)]) -> T
    where
        T: Copy,
    {
        let total: u32 = items.iter().map(|(_, w)| *w).sum();
        let mut u = self.uniform(0.0, total as f64);
        for (item, w) in items {
            u -= *w as f64;
            if u < 0.0 {
                return *item;
            }
        }
        items[items.len() - 1].0
    }
}

/// 打卡点 BD 系 (lat, lng) → 闭合平面路径 + 弧长表 + 中心。
pub type PointRing = (Vec<(f64, f64)>, Vec<f64>, (f64, f64));
pub fn make_point_ring(bd_points: &[(f64, f64)]) -> PointRing {
    let n = bd_points.len();
    let cx = bd_points.iter().map(|q| q.0).sum::<f64>() / n as f64;
    let cy = bd_points.iter().map(|q| q.1).sum::<f64>() / n as f64;
    let mut ordered = bd_points.to_vec();
    ordered.sort_by(|a, b| {
        let ka = (a.0 - cx).atan2(a.1 - cy);
        let kb = (b.0 - cx).atan2(b.1 - cy);
        ka.partial_cmp(&kb).unwrap()
    });
    let plane: Vec<(f64, f64)> = ordered
        .iter()
        // 经度按**本环中心纬度**折算：一个校区的纬度跨度只有几十米，用中心值即可，
        // 而用定值 86 600 会让南方校区的环整体胖 9%（打卡点被拉出椭圆）。
        // 注意 `cx` 是纬度、`cy` 是经度（元组是 (lat, lng)）：cos() 只吃纬度。
        .map(|q| {
            (
                (q.1 - cy) * met_per_deg_lng(cx),
                (q.0 - cx) * MET_PER_DEG_LAT,
            )
        })
        .collect();
    let samples = 18usize;
    let mut dense = Vec::with_capacity(n * samples);
    for i in 0..n {
        let p0 = plane[(i + n - 1) % n];
        let p1 = plane[i];
        let p2 = plane[(i + 1) % n];
        let p3 = plane[(i + 2) % n];
        for j in 0..samples {
            let t = j as f64 / samples as f64;
            let (t2, t3) = (t * t, t * t * t);
            let x = 0.5
                * ((2.0 * p1.0)
                    + (-p0.0 + p2.0) * t
                    + (2.0 * p0.0 - 5.0 * p1.0 + 4.0 * p2.0 - p3.0) * t2
                    + (-p0.0 + 3.0 * p1.0 - 3.0 * p2.0 + p3.0) * t3);
            let y = 0.5
                * ((2.0 * p1.1)
                    + (-p0.1 + p2.1) * t
                    + (2.0 * p0.1 - 5.0 * p1.1 + 4.0 * p2.1 - p3.1) * t2
                    + (-p0.1 + 3.0 * p1.1 - 3.0 * p2.1 + p3.1) * t3);
            dense.push((x, y));
        }
    }
    let mut arcs = vec![0.0f64];
    for i in 1..=dense.len() {
        let a = dense[i - 1];
        let b = dense[i % dense.len()];
        arcs.push(arcs[i - 1] + ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt());
    }
    (dense, arcs, (cx, cy))
}

/// 环线弧长 → 坐标（线性插值）。
pub fn ring_point_at(dense: &[(f64, f64)], arcs: &[f64], s: f64) -> (f64, f64) {
    let total = *arcs.last().unwrap_or(&1.0);
    let s = s.rem_euclid(total);
    let mut lo = 0usize;
    let mut hi = arcs.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        if arcs[mid] < s {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let i = lo.max(1);
    let a = dense[(i - 1) % dense.len()];
    let b = dense[i % dense.len()];
    let seg = arcs[i] - arcs[i - 1];
    let t = if seg > 0.0 {
        (s - arcs[i - 1]) / seg
    } else {
        0.0
    };
    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)
}

pub fn to_bd(x: f64, y: f64, c_lat: f64, c_lng: f64) -> (f64, f64) {
    (
        c_lat + y / MET_PER_DEG_LAT,
        c_lng + x / met_per_deg_lng(c_lat),
    )
}

// ── 坐标基准转换：WGS84 ↔ GCJ-02 ↔ BD-09 ─────────────────────────
// 打卡点来自服务端为 BD-09；OSM 路网为 WGS84。真实道路路由需先把路网
// 逐节点转成 BD-09，与经典模式（直接在 BD-09 上拟合打卡点环）保持一致。

const X_PI: f64 = std::f64::consts::PI * 3000.0 / 180.0;
const WGS_A: f64 = 6378245.0;
const WGS_EE: f64 = 0.00669342162296594323;

fn out_of_china(lat: f64, lng: f64) -> bool {
    lng < 72.004 || lng > 137.8347 || lat < 0.8293 || lat > 55.8271
}

fn transform_lat(x: f64, y: f64) -> f64 {
    let pi = std::f64::consts::PI;
    let mut ret = -100.0 + 2.0 * x + 3.0 * y + 0.2 * y * y + 0.1 * x * y + 0.2 * x.abs().sqrt();
    ret += (20.0 * (6.0 * x * pi).sin() + 20.0 * (2.0 * x * pi).sin()) * 2.0 / 3.0;
    ret += (20.0 * (y * pi).sin() + 40.0 * (y / 3.0 * pi).sin()) * 2.0 / 3.0;
    ret += (160.0 * (y / 12.0 * pi).sin() + 320.0 * (y * pi / 30.0).sin()) * 2.0 / 3.0;
    ret
}

fn transform_lng(x: f64, y: f64) -> f64 {
    let pi = std::f64::consts::PI;
    let mut ret = 300.0 + x + 2.0 * y + 0.1 * x * x + 0.1 * x * y + 0.1 * x.abs().sqrt();
    ret += (20.0 * (6.0 * x * pi).sin() + 20.0 * (2.0 * x * pi).sin()) * 2.0 / 3.0;
    ret += (20.0 * (x * pi).sin() + 40.0 * (x / 3.0 * pi).sin()) * 2.0 / 3.0;
    ret += (150.0 * (x / 12.0 * pi).sin() + 300.0 * (x / 30.0 * pi).sin()) * 2.0 / 3.0;
    ret
}

/// WGS84 → GCJ-02（火星坐标，境内偏移）。
pub fn wgs84_to_gcj02(lat: f64, lng: f64) -> (f64, f64) {
    if out_of_china(lat, lng) {
        return (lat, lng);
    }
    let pi = std::f64::consts::PI;
    let mut dlat = transform_lat(lng - 105.0, lat - 35.0);
    let mut dlng = transform_lng(lng - 105.0, lat - 35.0);
    let radlat = lat / 180.0 * pi;
    let mut magic = radlat.sin();
    magic = 1.0 - WGS_EE * magic * magic;
    let sqrtmagic = magic.sqrt();
    dlat = (dlat * 180.0) / ((WGS_A * (1.0 - WGS_EE)) / (magic * sqrtmagic) * pi);
    dlng = (dlng * 180.0) / (WGS_A / sqrtmagic * radlat.cos() * pi);
    (lat + dlat, lng + dlng)
}

/// GCJ-02 → BD-09（`wire::bd09_to_gcj02` 的逆变换）。
pub fn gcj02_to_bd09(gcj_lat: f64, gcj_lng: f64) -> (f64, f64) {
    let z = (gcj_lng * gcj_lng + gcj_lat * gcj_lat).sqrt() + 0.00002 * (gcj_lat * X_PI).sin();
    let theta = gcj_lat.atan2(gcj_lng) + 0.000003 * (gcj_lng * X_PI).cos();
    (z * theta.sin() + 0.006, z * theta.cos() + 0.0065)
}

/// WGS84 → BD-09（OSM 路网对齐打卡点用）。
pub fn wgs84_to_bd09(lat: f64, lng: f64) -> (f64, f64) {
    let (g_lat, g_lng) = wgs84_to_gcj02(lat, lng);
    gcj02_to_bd09(g_lat, g_lng)
}

pub fn fmt_cst(ms: i64, format: &str) -> String {
    FixedOffset::east_opt(8 * 3600)
        .unwrap()
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.format(format).to_string())
        .unwrap_or_default()
}

pub fn fmt_gain_time(ms: i64) -> String {
    fmt_cst(ms, "%Y-%m-%d %H:%M:%S")
}

#[cfg(test)]
mod projection_tests {
    use super::*;

    /// 球面大圆距离（米）。用**独立**的半径/公式算实测距离，
    /// 这样探针不是拿同一个近似去验证同一个近似。
    fn haversine_m(a: (f64, f64), b: (f64, f64)) -> f64 {
        const EARTH_R: f64 = 6_371_008.8;
        let (lat1, lat2) = (a.0.to_radians(), b.0.to_radians());
        let dlat = lat2 - lat1;
        let dlon = (b.1 - a.1).to_radians();
        let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
        2.0 * EARTH_R * h.sqrt().clamp(-1.0, 1.0).asin()
    }

    /// 在平面上画一个半径 100 m 的圆，用 `project` 换成经纬度后量 haversine 弧长，
    /// 返回相对偏差（正 = 报远了）。
    fn circle_error(project: impl Fn(f64, f64) -> (f64, f64)) -> f64 {
        let n = 720;
        let mut plane_len = 0.0;
        let mut geo_len = 0.0;
        let mut prev: Option<((f64, f64), (f64, f64))> = None;
        for i in 0..=n {
            let angle = std::f64::consts::TAU * i as f64 / n as f64;
            let (x, y) = (100.0 * angle.cos(), 100.0 * angle.sin());
            let point = project(x, y);
            if let Some((last_point, (px, py))) = prev {
                plane_len += ((x - px).powi(2) + (y - py).powi(2)).sqrt();
                geo_len += haversine_m(last_point, point);
            }
            prev = Some((point, (x, y)));
        }
        geo_len / plane_len - 1.0
    }

    /// C2 探针：经度方向「米/度」必须随纬度走。
    ///
    /// 旧常量 `MET_PER_DEG_LNG = 86 600` 是北京（39°）附近的近似值。
    /// 100 m 圆实测偏差：广州 23.1°N +9.27%、上海 31.2°N +5.00%、
    /// 北京 39.9°N −0.72%、包头 40.7°N −1.29%、哈尔滨 45.8°N −5.14%。
    ///
    /// 修完后三个探针都要回到容差内（39.9° 要求 ±0.5%，其余 ±1%）：
    /// 剩下的偏差只来自纬度方向仍用 111 132 m/度（与 111 320 差 0.17%）。
    #[test]
    fn longitude_scale_tracks_latitude() {
        for (lat, c_lng, tol) in [
            (39.9, 116.4, 0.005),
            (23.1, 113.3, 0.01),
            (45.8, 126.6, 0.01),
        ] {
            let old = circle_error(|x, y| (lat + y / MET_PER_DEG_LAT, c_lng + x / MET_PER_DEG_LNG));
            let new = circle_error(|x, y| to_bd(x, y, lat, c_lng));
            println!(
                "{lat:>5.1}°N 半径100m圆：旧常量 {:+.2}% → 新公式 {:+.2}%",
                old * 100.0,
                new * 100.0
            );
            assert!(
                new.abs() < tol,
                "{lat}°N 偏差 {:.3}% 超过容差 {:.1}%",
                new * 100.0,
                tol * 100.0
            );
            // 探针本身要能测出旧常量的问题，否则这条测试等于没测。
            if (lat - 39.9).abs() > 1e-9 {
                assert!(
                    old.abs() > tol,
                    "{lat}°N 旧常量只偏 {:.2}%，探针无效",
                    old * 100.0
                );
            }
        }
    }

    /// 环线的**经度方向宽度**必须按纬度折算。
    ///
    /// 用「同一形状在两个纬度的宽/高比值」判定：Catmull-Rom 会把方角抹圆，
    /// 绝对周长不等于多边形周长，但两个纬度上的几何构造完全相同 ——
    /// 所以宽度的比值可以精确对上 `met_per_deg_lng` 的比值，高度则不受影响。
    #[test]
    fn point_ring_width_follows_latitude() {
        // 用**非对称**四边形：`make_point_ring` 按「原始度」的方位角排序，
        // 正十字对称的方角在浮点噪声下排序可能翻转（环自交），那会让这条测试偶然失败。
        let quad = |lat: f64, lng: f64| {
            [
                (lat - 0.0010, lng - 0.0010),
                (lat - 0.0012, lng + 0.0013),
                (lat + 0.0015, lng + 0.0011),
                (lat + 0.0013, lng - 0.0009),
            ]
        };
        let bounds = |ring: &PointRing| {
            ring.0.iter().fold(
                (
                    f64::INFINITY,
                    f64::NEG_INFINITY,
                    f64::INFINITY,
                    f64::NEG_INFINITY,
                ),
                |(x0, x1, y0, y1), p| (x0.min(p.0), x1.max(p.0), y0.min(p.1), y1.max(p.1)),
            )
        };
        let (sx0, sx1, sy0, sy1) = bounds(&make_point_ring(&quad(23.1, 113.3)));
        let (nx0, nx1, ny0, ny1) = bounds(&make_point_ring(&quad(45.8, 126.6)));
        let (south_w, north_w) = (sx1 - sx0, nx1 - nx0);
        let expect = met_per_deg_lng(23.1) / met_per_deg_lng(45.8);
        println!(
            "环宽：广州 {south_w:.1} m / 哈尔滨 {north_w:.1} m = {:.4}（期望 {expect:.4}）",
            south_w / north_w
        );
        assert!(
            ((south_w / north_w) - expect).abs() < expect * 0.005,
            "环宽比 {:.4} 与经度折算比 {expect:.4} 不符",
            south_w / north_w
        );
        // 纬度方向不受经度常量影响：两个纬度的环高相同。
        assert!(
            ((sy1 - sy0) - (ny1 - ny0)).abs() < 1.0,
            "环高不该随纬度变化：{:.1} vs {:.1}",
            sy1 - sy0,
            ny1 - ny0
        );
    }
}
