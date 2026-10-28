//! 自然轨迹生成器。
//!
//! 画像驱动：打卡点拟合闭合环（Catmull-Rom，每段 18 采样）→ 弧长表；
//! 采样间隔主 5s（80%）；速度曲线 = ramp × 疲劳 × 三正弦 × 余弦凹陷 × 微噪；
//! 正常点按速度曲线分配位移并归一到精确总距离；异常点（-1）零位移/跳变；
//! 哨兵点（首 type∈{0,7}、索引1 type=5、末 type=6）；结尾断崖；点位吸附。
#![allow(non_snake_case)]

use super::geom::{
    fmt_gain_time, make_point_ring, ring_point_at, round_to, to_bd, Rng, MET_PER_DEG_LAT,
    MET_PER_DEG_LNG,
};
use super::model::{GenPoint, Track};
use super::postfix::apply_post_fixes;

/// Limit generated measurement noise to a local neighborhood.
pub const MAX_EXCEPTION_DRIFT_M: f64 = 6.0;
pub const MAX_EXCEPTION_GPS_SPEED_KMH: f64 = 6.0;

/// 有效配速窗口（m/s），保留原有生成器边界。
pub const SPEED_FLOOR: f64 = 1.90;
pub const SPEED_CEIL: f64 = 6.30;

/// 等比缩放逐点速度至目标总距：越界点钳在窗口边界，剩余差量由未饱和点分摊（迭代收敛）。
/// 与整体等比缩放的区别：任何一点的瞬时配速都不会越出有效窗口。
fn fit_speeds(w: &mut [f64], dts: &[f64], target: f64) {
    for _ in 0..24 {
        let cur: f64 = w.iter().zip(dts).map(|(x, dt)| x * dt).sum();
        if (cur - target).abs() <= 1.0 {
            break;
        }
        let k = target / cur;
        for x in w.iter_mut() {
            *x = (*x * k).clamp(SPEED_FLOOR, SPEED_CEIL);
        }
    }
    for x in w.iter_mut() {
        *x = x.clamp(SPEED_FLOOR, SPEED_CEIL);
    }
}

/// 轨迹生成主入口。points_bd 为 BD 系打卡点。
///
/// GPS 抖动沿用旧行为（AR(1) α=0.72、σ=0.75 → 单点稳态 ≈1.08 m）。
/// 需要指定幅度（配置项 `gps_spread_m`）用 [`build_tuned`]。
pub fn build(
    dist: f64,
    dur: i64,
    seed: u64,
    _center: (f64, f64),
    start_ms: i64,
    points_bd: &[(f64, f64)],
) -> Track {
    build_tuned(dist, dur, seed, _center, start_ms, points_bd, 0.0)
}

/// 同 [`build`]，但可指定 GPS 抖动幅度（单点稳态标准差，米）。
///
/// - `0.0` 沿用旧行为：α=0.72、σ=0.75（稳态 ≈1.08 m），逐点输出与改动前一致；
/// - `>0` 由目标稳态值反解 σ：`σ = S·√(1−α²)`，α 与旧版相同。
///
/// 与 Road 路径的区别要记住：经典模式的「旧行为」是**固定** 1.08 m（不是 2.8–4.2
/// 抽样），所以把配置改成非 0 时两条路径才第一次变得完全一致 —— 配置默认值因此是 0。
///
/// 这里不消耗任何额外随机数，所以 RNG 序列与旧版完全一致，只有抖动幅度变。
pub fn build_tuned(
    dist: f64,
    dur: i64,
    seed: u64,
    _center: (f64, f64),
    start_ms: i64,
    points_bd: &[(f64, f64)],
    gps_spread_m: f64,
) -> Track {
    let mut rng = Rng::new(seed);
    let dur_f = dur as f64;
    let (dense, arcs, pc) = make_point_ring(points_bd);
    let (c_lat, c_lng) = pc;
    let direction: f64 = rng.choice(&[1.0, -1.0]);
    let s0 = rng.uniform(0.0, *arcs.last().unwrap_or(&400.0));
    let phase_v = rng.uniform(0.0, std::f64::consts::TAU);
    let phase_l = rng.uniform(0.0, std::f64::consts::TAU);

    let mut times = Vec::new();
    let mut t = 0.0;
    while t < dur_f {
        times.push(t);
        t += if rng.random() < 0.80 {
            5.0
        } else {
            rng.choice(&[1.0, 2.0, 3.0, 4.0, 6.0, 7.0, 8.0])
        };
    }
    let n = times.len();

    let n_dips = rng.choice(&[1, 1, 2]);
    let mut dips = Vec::new();
    for _ in 0..n_dips {
        dips.push((
            rng.uniform(0.15, 0.75) * dur_f,
            rng.uniform(25.0, 55.0),
            rng.uniform(0.08, 0.16),
        ));
    }
    let dip_factor = |t: f64| -> f64 {
        let mut f = 1.0;
        for &(c, hw, d) in &dips {
            if (t - c).abs() < hw {
                f *= 1.0 - d * 0.5 * (1.0 + std::f64::consts::PI * (t - c) / hw).cos();
            }
        }
        f
    };
    let base = dist / dur_f;
    let mut w = Vec::with_capacity(n);
    for &tt in &times {
        let ramp = if tt < 8.0 {
            (0.85 + 0.15 * (tt / 1.0f64.max(8.0f64.min(dur_f / 20.0)))).min(1.0)
        } else {
            1.0
        } * if tt > dur_f - 8.0 {
            1.0 + 0.03 * (tt - (dur_f - 8.0)) / 8.0
        } else {
            1.0
        };
        let km_done = (tt / dur_f) * dist / 1000.0;
        let fatigue = if km_done <= 0.5 {
            1.03
        } else {
            (1.03 - 0.06 * (km_done - 0.5)).max(0.80)
        };
        let wave = (1.0
            + 0.010 * (std::f64::consts::TAU * tt / 115.0 + phase_v).sin()
            + 0.035 * (std::f64::consts::TAU * tt / 47.0 + phase_v * 2.3).sin()
            + 0.015 * (std::f64::consts::TAU * tt / 19.0 + phase_v * 3.7).sin())
            * dip_factor(tt);
        let noise = 1.0 + rng.gauss(0.0, 0.008);
        w.push(base * ramp * fatigue * wave * noise);
    }
    let mut dts: Vec<f64> = (0..n - 1).map(|i| times[i + 1] - times[i]).collect();
    dts.push(1.0f64.max(dur_f - times[n - 1]));
    // 逐点速度全部约束在有效配速窗口内，并精确命中目标距离
    fit_speeds(&mut w, &dts, dist);
    let seg_dist: Vec<f64> = (0..n).map(|i| w[i] * dts[i]).collect();

    let tl_w = [
        ((-1i64, 4i64), 54u32),
        ((-1, 1), 27),
        ((-1, 12), 13),
        ((-1, 5), 3),
        ((-1, 6), 2),
    ];
    let mut kinds: Vec<(i64, i64)> = Vec::with_capacity(n);
    for _ in 0..n {
        let u = rng.random();
        if u < 0.39 {
            kinds.push((3, 1));
        } else if u < 0.93 {
            kinds.push((0, 1));
        } else if u < 0.96 {
            kinds.push(rng.weighted(&tl_w));
        } else {
            kinds.push((rng.choice(&[1, 1, 1, 1, 2, 2]), 1));
        }
    }
    for i in 1..kinds.len() {
        let prev_drift = (-1 == kinds[i - 1].0) || kinds[i - 1].0 == 5 || kinds[i - 1].0 == 6;
        if kinds[i].0 != -1 && prev_drift && rng.random() < 0.08 {
            kinds[i] = (if rng.random() < 0.75 { 7 } else { 8 }, 1);
        }
    }
    for i in 1..n {
        if kinds[i].0 == -1 && kinds[i - 1].0 == -1 {
            kinds[i] = (rng.choice(&[3, 0]), 1);
        }
    }
    let normal_idx: Vec<usize> = (0..n).filter(|&i| kinds[i].0 != -1 && i > 1).collect();
    let share: f64 = normal_idx.iter().map(|&i| seg_dist[i]).sum();
    let share = if share == 0.0 { 1.0 } else { share };
    let mut disp_of = vec![0.0f64; n];
    for &i in &normal_idx {
        disp_of[i] = seg_dist[i] * (dist / share);
    }

    let mut locs: Vec<GenPoint> = Vec::with_capacity(n);
    let mut s = s0;
    let mut t_acc = 0.0f64;
    let mut dist_acc = 0.0f64;
    let mut steps_acc = 0.0f64;
    // AR(1) 抖动系数与旧版一致（α=0.72）；幅度按目标稳态标准差反解。
    // 旧版 σ 恒为 0.75（稳态 = 0.75/√(1−0.72²) ≈ 1.08 m），这里 0 时原样保留。
    const JITTER_ALPHA: f64 = 0.72;
    let jitter_sigma = if gps_spread_m.is_finite() && gps_spread_m > 0.0 {
        gps_spread_m.clamp(0.0, crate::api::model::GPS_SPREAD_MAX_M)
            * (1.0 - JITTER_ALPHA * JITTER_ALPHA).sqrt()
    } else {
        0.75
    };
    let mut jx = 0.0f64;
    let mut jy = 0.0f64;
    // 基准海拔按每次跑步抽一次：常量 82m 会让所有记录的 bdA 都围绕同一个值，
    // 而真实海拔随城市/校区变化（同一片区域是几十米到几百米）。这里只做区间抽样，
    // 不假装知道具体城市高程 —— 目的是消掉「所有账号海拔都是 82m」这个聚类特征。
    let base_alt = rng.uniform(20.0, 260.0);
    let mut alt = base_alt + rng.uniform(-1.0, 1.0);
    let n_est = 1.max((dur_f / 5.0) as i64);
    let alt_sigma = rng.uniform(3.8, 6.2) / (0.40 * n_est as f64);
    for i in 0..n {
        let dt = dts[i];
        let (typ, lt) = kinds[i];
        t_acc += dt;
        let pos = |ss: f64| ring_point_at(&dense, &arcs, ss);
        let mut d_step = 0.0f64;
        let px;
        let py;
        let x;
        let y;
        let rad;
        let state;
        if typ != -1 {
            d_step = disp_of[i];
            s += direction * d_step;
            let (bx, by) = pos(s);
            x = bx;
            y = by;
            jx = JITTER_ALPHA * jx + rng.gauss(0.0, jitter_sigma);
            jy = JITTER_ALPHA * jy + rng.gauss(0.0, jitter_sigma);
            px = bx + jx;
            py = by + jy;
            rad = round_to(
                if typ == 3 {
                    rng.uniform(1.4, 5.1)
                } else {
                    rng.uniform(1.4, 2.4)
                },
                2,
            );
            state = if typ == 0 {
                rng.weighted(&[(1, 145), (2, 45), (3, 164)])
            } else {
                rng.weighted(&[(1, 145), (2, 256), (3, 151)])
            };
        } else {
            if rng.random() >= 0.20 {
                d_step = rng.uniform(0.0, super::generator::MAX_EXCEPTION_DRIFT_M);
            }
            let (bx, by) = pos(s);
            x = bx;
            y = by;
            if d_step > 0.0 {
                let ang = rng.uniform(0.0, std::f64::consts::TAU);
                px = x + d_step * ang.sin();
                py = y + d_step * ang.cos();
            } else if let Some(last) = locs.last() {
                // 零位移：精确复制上一点坐标（BD→平面）
                py = (last.gLat - c_lat) * MET_PER_DEG_LAT;
                px = (last.gLng - c_lng) * MET_PER_DEG_LNG;
            } else {
                px = x + jx;
                py = y + jy;
            }
            rad = round_to(rng.uniform(1.6, 10.0), 2);
            state = rng.weighted(&[(1, 102), (2, 124), (3, 136)]);
        }
        if typ != -1 {
            dist_acc += d_step; // 异常点漂移不计入累计距离
        }
        let (lat, lng) = to_bd(px, py, c_lat, c_lng);
        alt += 0.04 * (base_alt - alt) + rng.gauss(0.0, alt_sigma);
        let v_now = dist / dur_f;
        let stride_target = 0.62
            + 0.17 * v_now
            + 0.03 * (std::f64::consts::TAU * t_acc / 200.0 + phase_l).sin()
            + rng.gauss(0.0, 0.008);
        let v_cad = v_now * (1.0 + 0.03 * (std::f64::consts::TAU * t_acc / 70.0 + phase_v).sin());
        let cad = (v_cad / stride_target * 60.0).clamp(100.0, 200.0);
        steps_acc += cad / 60.0 * dt;
        let nxt = pos(s + direction * 2.0);
        let brg =
            ((nxt.0 - x).atan2(nxt.1 - y).to_degrees() + rng.gauss(0.0, 35.0)).rem_euclid(360.0);
        // 异常点（-1）：avgSpeed 为累计均值（真人与此一致，不为 0）；
        // GPS 瞬时速度限制在低速范围，避免额外制造尖峰
        let (avg_sp, gps_speed) = if typ == -1 {
            let avg = round_to(dist_acc / t_acc.max(1.0), 4);
            let gps = rng.uniform(0.5, super::generator::MAX_EXCEPTION_GPS_SPEED_KMH);
            (avg, round_to(gps, 4))
        } else {
            let avg = round_to(d_step / dt, 4);
            let kmh = avg * 3.6;
            let sigma = (kmh * 0.08).max(0.05);
            let gps = if rng.random() < 0.20 {
                0.0
            } else {
                round_to((kmh + rng.gauss(0.0, sigma)).max(0.0), 4)
            };
            (avg, gps)
        };

        locs.push(GenPoint {
            id: i as i64 + 1,
            flag: start_ms,
            lat: -1.0,
            lng: -1.0,
            gLat: round_to(lat, 7),
            gLng: round_to(lng, 7),
            speed: round_to(gps_speed, 4),
            avgSpeed: avg_sp,
            radius: rad,
            accuracy: rad,
            ptype: typ,
            locType: lt,
            hasAltitude: true,
            totalTime: round_to(t_acc, 0) as i64,
            totalDis: round_to(dist_acc, 4),
            validDis: round_to(dist_acc, 4),
            validTime: round_to(t_acc, 0) as i64,
            steps: steps_acc as i64,
            stepDistance: 0.0,
            gainTime: fmt_gain_time(start_ms + (t_acc * 1000.0) as i64),
            gainTimeMs: start_ms + (t_acc * 1000.0) as i64,
            queueNum: 0,
            coorType: "gcj02".into(),
            bdA: round_to(alt, 2),
            bdD: round_to(brg, 2),
            bdS: round_to((avg_sp * rng.uniform(0.6, 0.95)).max(0.0), 3),
            bdG: rng.choice(&[1, 1, 1, -1]),
            count: rng.randint(20, 88),
            dtr: 0.0,
            state,
            locationId: String::new(),
        });
    }
    apply_post_fixes(&mut locs, &mut rng, start_ms);

    // 点位吸附：<40m 精确落位
    for pl in points_bd {
        let mut best_i = None;
        let mut best_d = 1e18f64;
        for (i, q) in locs.iter().enumerate().filter(|(_, q)| q.ptype != -1) {
            let dd = ((q.gLat - pl.0) * MET_PER_DEG_LAT).powi(2)
                + ((q.gLng - pl.1) * MET_PER_DEG_LNG).powi(2);
            if dd < best_d {
                best_d = dd;
                best_i = Some(i);
            }
        }
        if let Some(i) = best_i {
            if best_d < 40.0 * 40.0 {
                locs[i].gLat = round_to(pl.0, 7);
                locs[i].gLng = round_to(pl.1, 7);
            }
        }
    }

    let total_dis = round_to(dist, 3);
    let mut track = Track {
        totalTime: round_to(t_acc, 0) as i64,
        totalDistance: total_dis,
        validDistance: total_dis,
        validTime: round_to(t_acc, 0) as i64,
        startTime: start_ms,
        startLatitude: locs[0].gLat,
        startLongitude: locs[0].gLng,
        totalSteps: steps_acc as i64,
        locations: locs,
        speedPerTenSec: Vec::new(),
        stepsPerTenSec: Vec::new(),
        segments: Vec::new(),
    };
    super::stats::rebuild(&mut track);
    track
}

#[cfg(test)]
mod gps_spread_tests {
    use super::*;

    /// 改动前（HEAD）的 `build(2000, 800, seed=7)` 逐点坐标哈希。
    /// 该数字来自 `git archive HEAD` 的干净副本，不是改完之后自己算的。
    const PRE_CHANGE_COORD_HASH: u64 = 0x144f_36bf_ee9e_3f76;

    fn points() -> [(f64, f64); 4] {
        [
            (39.9077201, 116.4126209),
            (39.9077201, 116.4146209),
            (39.9097201, 116.4146209),
            (39.9097201, 116.4126209),
        ]
    }

    fn coord_hash(track: &Track) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for p in &track.locations {
            for v in [p.gLat, p.gLng] {
                h ^= v.to_bits();
                h = h.wrapping_mul(0x100_0000_01b3);
            }
        }
        h
    }

    #[test]
    fn legacy_gps_spread_zero_reproduces_the_pre_change_track() {
        let p = points();
        let old_api = build(2000.0, 800, 7, (39.9, 116.4), 0, &p);
        let zero = build_tuned(2000.0, 800, 7, (39.9, 116.4), 0, &p, 0.0);
        let nan = build_tuned(2000.0, 800, 7, (39.9, 116.4), 0, &p, f64::NAN);
        for (name, track) in [("old_api", &old_api), ("zero", &zero), ("nan", &nan)] {
            assert_eq!(track.locations.len(), 164, "{name}: 点数变了");
            assert_eq!(track.totalSteps, 1916, "{name}: 步数变了");
            assert_eq!(
                coord_hash(track),
                PRE_CHANGE_COORD_HASH,
                "{name}: 经典模式 gps_spread_m=0 必须与改动前逐点一致"
            );
        }
        assert!((zero.locations[0].gLat - 39.9083108).abs() < 1e-9);
        assert!((zero.locations[0].gLng - 116.4148486).abs() < 1e-9);
    }

    #[test]
    fn legacy_gps_spread_changes_only_coordinates() {
        let p = points();
        let zero = build_tuned(2000.0, 800, 7, (39.9, 116.4), 0, &p, 0.0);
        let wide = build_tuned(2000.0, 800, 7, (39.9, 116.4), 0, &p, 10.0);
        assert_ne!(coord_hash(&zero), coord_hash(&wide), "幅度没生效");
        assert_eq!(zero.locations.len(), wide.locations.len());
        assert_eq!(zero.totalSteps, wide.totalSteps);
        assert_eq!(zero.totalDistance, wide.totalDistance);
        for (a, b) in zero.locations.iter().zip(&wide.locations) {
            assert_eq!(a.totalTime, b.totalTime);
            assert_eq!(a.ptype, b.ptype);
            assert_eq!(a.steps, b.steps);
            assert_eq!(a.speed.to_bits(), b.speed.to_bits());
            assert_eq!(a.bdA.to_bits(), b.bdA.to_bits());
        }
        let n = zero.locations.len() as f64;
        let mean_delta: f64 = zero
            .locations
            .iter()
            .zip(&wide.locations)
            .map(|(a, b)| {
                (((a.gLat - b.gLat) * MET_PER_DEG_LAT).powi(2)
                    + ((a.gLng - b.gLng) * MET_PER_DEG_LNG).powi(2))
                .sqrt()
            })
            .sum::<f64>()
            / n;
        assert!(
            mean_delta > 1.0,
            "10 m 幅度与自动幅度只差 {mean_delta:.2} m"
        );
    }
}
