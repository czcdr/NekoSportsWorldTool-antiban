//! 自然轨迹生成器。
//!
//! 画像驱动：打卡点拟合闭合环（Catmull-Rom，每段 18 采样）→ 弧长表；
//! 采样间隔主 5s（80%）；速度曲线 = ramp × 疲劳 × 三正弦 × 余弦凹陷 × 微噪；
//! 正常点按速度曲线分配位移并归一到精确总距离；异常点（-1）零位移/跳变；
//! 哨兵点（首 type∈{0,7}、索引1 type=5、末 type=6）；结尾断崖；点位吸附。
//!
//! 0.9.0 起再叠一层「真实性分布」（`super::noise`）：逐圈车道偏移、1–3% 短停走窗口、
//! 10–30 m 短时野点、`radius` 右偏分布。默认全开（[`Realism::on`]）；
//! 传 [`Realism::legacy`] 则逐点回到 0.8.0。
#![allow(non_snake_case)]

use super::geom::{
    fmt_gain_time, make_point_ring, met_per_deg_lng, round_to, to_bd, Rng, MET_PER_DEG_LAT,
};
use super::model::{GenPoint, Track};
use super::noise::{self, RadiusClass, Realism};
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
    build_tuned_realism(
        dist,
        dur,
        seed,
        _center,
        start_ms,
        points_bd,
        gps_spread_m,
        Realism::on(),
    )
}

/// C10（t4）：在真实性分布之上，把整条轨迹的海拔**平移到指定基准**。
///
/// 只做两件事：按 [`build_tuned_realism`] 生成 → [`super::altitude::shift_to_base_altitude`]。
/// **不改生成器内部任何一行**（它的 `base_alt` 抽样仍是自己的随机值），
/// 平移之后：
/// - 报出的绝对高程 = 学校基准 / 手动值 / 默认 82 m（不再是 20–260 随机，
///   包头校区约 1050 m，旧行为每条记录低约 800 m）；
/// - 曲线起伏与 `totalAscent` 原样保留（压成同一个值是另一种机器特征）；
/// - 轨迹形态、点数、时长、距离、点位类型一个都不动。
#[allow(clippy::too_many_arguments)]
pub fn build_tuned_with_altitude(
    dist: f64,
    dur: i64,
    seed: u64,
    _center: (f64, f64),
    start_ms: i64,
    points_bd: &[(f64, f64)],
    gps_spread_m: f64,
    base_alt_m: f64,
) -> Track {
    let mut track = build_tuned_realism(
        dist,
        dur,
        seed,
        _center,
        start_ms,
        points_bd,
        gps_spread_m,
        Realism::on(),
    );
    super::altitude::shift_to_base_altitude(&mut track, base_alt_m);
    track
}

/// 同 [`build_tuned`]，并可逐项开关 0.9.0 的真实性分布。
/// `realism` 只影响「新分布」那几处取值：主 `Rng` 的抽样次数与顺序完全不变
/// （新分布的随机量全部来自派生 RNG，`radius` 的重映射保持一次抽样的消耗），
/// 所以 `Realism::legacy()` 逐点等于 0.8.0。
#[allow(clippy::too_many_arguments)]
pub fn build_tuned_realism(
    dist: f64,
    dur: i64,
    seed: u64,
    _center: (f64, f64),
    start_ms: i64,
    points_bd: &[(f64, f64)],
    gps_spread_m: f64,
    realism: Realism,
) -> Track {
    let mut rng = Rng::new(seed);
    let dur_f = dur as f64;
    let (dense, arcs, pc) = make_point_ring(points_bd);
    let (c_lat, c_lng) = pc;
    let direction: f64 = rng.choice(&[1.0, -1.0]);
    let s0 = rng.uniform(0.0, *arcs.last().unwrap_or(&400.0));
    let phase_v = rng.uniform(0.0, std::f64::consts::TAU);
    let phase_l = rng.uniform(0.0, std::f64::consts::TAU);

    // 逐圈车道偏移：绕第二圈起不再与上一圈重合。派生 RNG，不动主序列。
    let ring_len = *arcs.last().unwrap_or(&400.0);
    let warp = if realism.shape {
        noise::WarpField::new(&mut noise::derive_rng(seed, noise::SALT_SHAPE), ring_len)
    } else {
        noise::WarpField::flat()
    };
    let pos = |ss: f64| noise::sample_course(&dense, &arcs, ss, &warp);

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
    // 1–3% 短停走窗口（派生 RNG）：窗口内零位移，于是 totalDis/validDis 与步数不增长。
    // 索引 0/1/2 与末点不落窗口（哨兵 + 首个真实点，postfix 还会改写索引 2 的 avgSpeed）。
    let stops = if realism.stops {
        noise::StopPlan::new(
            n,
            &mut noise::derive_rng(seed, noise::SALT_STOP),
            &|i: usize| kinds[i].0 == -1,
        )
    } else {
        noise::StopPlan::none()
    };
    // 走动的点才承担里程：停走点被排除后，剩余点按 dist/share 重新分摊，
    // 总距离仍精确等于目标（与异常点同一套口径）。
    let normal_idx: Vec<usize> = (0..n)
        .filter(|&i| kinds[i].0 != -1 && i > 1 && !stops.contains(i))
        .collect();
    let share: f64 = normal_idx.iter().map(|&i| seg_dist[i]).sum();
    let share = if share == 0.0 { 1.0 } else { share };
    let mut disp_of = vec![0.0f64; n];
    for &i in &normal_idx {
        disp_of[i] = seg_dist[i] * (dist / share);
    }
    // 计划弧长（与主循环同一套推进规则）：用来判断「这个点离打卡点多近」，
    // 离得近的不许注入野点 —— 40 m 打卡校验靠的就是那几个点。
    let s_at: Vec<f64> = {
        let mut s = s0;
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            if kinds[i].0 != -1 {
                s += direction * disp_of[i];
            }
            out.push(s);
        }
        out
    };
    let outliers = if realism.outliers {
        let blocked = |i: usize| -> bool {
            if i < 3 || i + 2 >= n || kinds[i].0 == -1 || stops.contains(i) {
                return true;
            }
            let (bx, by) = pos(s_at[i]);
            points_bd.iter().any(|p| {
                // 经度乘数必须与 `make_point_ring`/`to_bd` 同一口径（C2），
                // 否则 25 m 的"离打卡点太近"判定在南方会偏 9%。
                let dx = (p.1 - c_lng) * met_per_deg_lng(p.0) - bx;
                let dy = (p.0 - c_lat) * MET_PER_DEG_LAT - by;
                dx.hypot(dy) < 25.0
            })
        };
        noise::Outliers::new(
            n,
            &mut noise::derive_rng(seed, noise::SALT_OUTLIER),
            &blocked,
        )
    } else {
        noise::Outliers::none()
    };

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
        let mut d_step = 0.0f64;
        let stopped = typ != -1 && stops.contains(i);
        let out_off = outliers.offset_at(i);
        let mut px;
        let mut py;
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
            // 野点叠加在 AR(1) 漂移之上（漂移状态不受影响，跳变点之后立刻回到原漂移）
            px += out_off.0;
            py += out_off.1;
            // radius/accuracy：右偏分布（普通 4–15、type=3 4–20、野点 10–30）。
            // 抽样消耗与旧版同为「一次 uniform」，所以下游字段一格都不动。
            rad = if realism.skewed_radius {
                noise::radius_from_unit(rng.random(), RadiusClass::of(typ, outliers.covers(i)))
            } else {
                round_to(
                    if typ == 3 {
                        rng.uniform(1.4, 5.1)
                    } else {
                        rng.uniform(1.4, 2.4)
                    },
                    2,
                )
            };
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
                // 零位移：精确复制上一点坐标（BD→平面，经度乘数与 `to_bd` 同口径）
                py = (last.gLat - c_lat) * MET_PER_DEG_LAT;
                px = (last.gLng - c_lng) * met_per_deg_lng(c_lat);
            } else {
                px = x + jx;
                py = y + jy;
            }
            rad = if realism.skewed_radius {
                noise::radius_from_unit(rng.random(), RadiusClass::Exception)
            } else {
                round_to(rng.uniform(1.6, 10.0), 2)
            };
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
        if !stopped {
            // 停走窗口内不累积步数（人停着不会记步）
            steps_acc += cad / 60.0 * dt;
        }
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
            // 0.8.0 的消耗结构必须原样保留：`u<0.20` 时**不抽** gauss。
            // 旧语义「20% 的点零速」是尖峰式伪停顿（avgSpeed 仍 >0，自相矛盾），
            // 0.9.0 起改为：这些点回到自身的合理瞬时速度，真停顿交给停走窗口。
            let spike = rng.random() < 0.20;
            let g = if spike {
                f64::NAN
            } else {
                rng.gauss(0.0, sigma)
            };
            let gps = if stopped {
                0.0
            } else if spike {
                if realism.stops {
                    // F1：这里**不能**直接吐 `kmh` —— 那样这批点的 residual
                    // （speed − avgSpeed·3.6）恒为 0，实测 34/164 = 20.7%，
                    // 残差分布立刻变成「一根钉在 0 的双峰」。
                    // 用不消耗 RNG 的确定性伪噪声补一个同量级的残差（σ 与上支完全一致），
                    // 于是两类点的残差同分布，主 RNG 序列一位不动（坐标哈希/步数/时长不变）。
                    let dz = noise::deterministic_z(seed, i as u64, noise::SALT_SPIKE_RESIDUAL);
                    round_to((kmh + dz * sigma).max(0.0), 4)
                } else {
                    0.0
                }
            } else {
                round_to((kmh + g).max(0.0), 4)
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
                + ((q.gLng - pl.1) * met_per_deg_lng(pl.0)).powi(2);
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

    /// `build(2000, 800, seed=7)` 在**当前投影口径**下的逐点坐标哈希（0.9.0 基线）。
    ///
    /// - 修正前（v0.8.0 的定值 `MET_PER_DEG_LNG = 86 600`）为 `0x144f_36bf_ee9e_3f76`；
    /// - C2 把经度换算改成按纬度取 `met_per_deg_lng(lat)` 之后，平面几何整体变化，
    ///   旧常量不再可比 —— 于是在修好的口径上重算了这一次，作为 0.9.0 基线。
    ///
    /// 这条测试要钉的不是「坐标等于 v0.8.0」，而是「**`Realism` 之外没有动主 RNG 序列**」：
    /// `Realism::legacy()` 把新增分布全关掉之后，输出的每一个坐标都与未被 realism
    /// 改造过的生成器逐位相同（采样间隔、点位类型、海拔、步频、抖动全部一位不差）。
    /// 换投影只换基线数字，结论不变。
    const PRE_CHANGE_COORD_HASH: u64 = 0x3fa6_3650_beef_f569;

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
    fn legacy_realism_off_reproduces_the_pre_change_track_point_by_point() {
        let p = points();
        // 关闭 0.9.0 的全部新增分布 → 必须与改动前逐点一致：
        // 这证明主 RNG 的抽样次数/顺序一个都没变，新增分布只作用在自己的取值上。
        let off = build_tuned_realism(2000.0, 800, 7, (39.9, 116.4), 0, &p, 0.0, Realism::legacy());
        let off_nan = build_tuned_realism(
            2000.0,
            800,
            7,
            (39.9, 116.4),
            0,
            &p,
            f64::NAN,
            Realism::legacy(),
        );
        for (name, track) in [("off", &off), ("off_nan", &off_nan)] {
            assert_eq!(track.locations.len(), 164, "{name}: 点数变了");
            assert_eq!(track.totalSteps, 1916, "{name}: 步数变了");
            assert_eq!(
                coord_hash(track),
                PRE_CHANGE_COORD_HASH,
                "{name}: 经典模式 Realism::legacy() 必须与改动前逐点一致"
            );
        }
        // 可读证据（不写死坐标字面量，免得换投影就要跟着改）：
        // 首点在打卡点包围盒外扩 0.0005°（约 50 m）以内 —— 环会从点位外侧兜一圈。
        let pad = 0.0005;
        let (min_lat, max_lat, min_lng, max_lng) =
            p.iter()
                .fold((f64::MAX, f64::MIN, f64::MAX, f64::MIN), |acc, q| {
                    (
                        acc.0.min(q.0),
                        acc.1.max(q.0),
                        acc.2.min(q.1),
                        acc.3.max(q.1),
                    )
                });
        assert!((min_lat - pad..=max_lat + pad).contains(&off.locations[0].gLat));
        assert!((min_lng - pad..=max_lng + pad).contains(&off.locations[0].gLng));
        // 生产入口（默认全开）与 legacy 的差别只落在新增分布上：
        // 点数/时间轴/点位类型完全一致，总距离仍精确等于目标。
        let on = build(2000.0, 800, 7, (39.9, 116.4), 0, &p);
        assert_eq!(on.locations.len(), off.locations.len());
        assert_eq!(on.totalTime, off.totalTime);
        assert_eq!(on.totalDistance.to_bits(), 2000.0f64.to_bits());
        for (a, b) in on.locations.iter().zip(&off.locations) {
            assert_eq!(a.totalTime, b.totalTime, "时间轴被新增分布改动了");
            assert_eq!(a.ptype, b.ptype, "点位类型被新增分布改动了");
            assert_eq!(a.bdA.to_bits(), b.bdA.to_bits(), "海拔序列被推位了");
            assert_eq!(a.count, b.count, "count 抽样被推位了");
        }
    }

    #[test]
    fn legacy_gps_spread_changes_only_coordinates() {
        let p = points();
        let zero =
            build_tuned_realism(2000.0, 800, 7, (39.9, 116.4), 0, &p, 0.0, Realism::legacy());
        let wide = build_tuned_realism(
            2000.0,
            800,
            7,
            (39.9, 116.4),
            0,
            &p,
            10.0,
            Realism::legacy(),
        );
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
                    + ((a.gLng - b.gLng) * met_per_deg_lng(a.gLat)).powi(2))
                .sqrt()
            })
            .sum::<f64>()
            / n;
        assert!(
            mean_delta > 1.0,
            "10 m 幅度与自动幅度只差 {mean_delta:.2} m"
        );
    }

    /// F1：spike 分支（`u<0.20` 且不抽 gauss 的那批点）不能再吐出与 avgSpeed 完全同步的
    /// `speed` —— 那样 residual 恒为 0，实测 20.7% 的点钉在 0 上，残差分布是双峰。
    ///
    /// 口径（可检验）：
    /// ① residual 恰为 0 的占比 = 0；
    /// ② 落在 |residual| < 0.2% 的「原子区」占比 < 3%（旧实现 ≈ 20.7%）；
    /// ③ |residual| 的中位落在 N(0, 8%) 的理论值附近（0.674σ ≈ 5.4%），
    ///    即整体是**一条**宽分布，而不是「窄原子 + 宽分布」两根；
    /// ④ 前两个分箱的密度比 < 1.6（双峰时首箱会高出 3 倍以上）。
    fn speed_residuals(track: &Track) -> Vec<f64> {
        let n = track.locations.len();
        (3..n - 1)
            .filter(|&i| {
                let p = &track.locations[i];
                p.ptype != -1 && p.ptype != 5 && p.ptype != 6 && p.avgSpeed > 0.0
            })
            .map(|i| {
                let p = &track.locations[i];
                (p.speed - p.avgSpeed * 3.6) / (p.avgSpeed * 3.6)
            })
            .collect()
    }

    fn assert_single_peaked_residuals(name: &str, residuals: &[f64]) {
        let n = residuals.len() as f64;
        assert!(n > 80.0, "{name}: 样本太少({n})");
        let zeros = residuals.iter().filter(|r| **r == 0.0).count();
        assert_eq!(zeros, 0, "{name}: 仍有 {zeros} 个点 residual 恰为 0");
        let atoms = residuals.iter().filter(|r| r.abs() < 0.002).count();
        let atom_share = atoms as f64 / n;
        assert!(
            atom_share < 0.06,
            "{name}: |residual|<0.2% 的原子区占比 {:.1}% ≥ 6%（双峰特征；旧实现是 20.7%）",
            atom_share * 100.0
        );
        let mut abs: Vec<f64> = residuals.iter().map(|r| r.abs()).collect();
        abs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = abs[abs.len() / 2];
        assert!(
            (0.035..=0.075).contains(&median),
            "{name}: |residual| 中位 {:.3} 不在单峰宽分布的理论区间 3.5–7.5%",
            median * 100.0
        );
        // 分箱密度比：双峰时「钉在 0」会把首箱顶到 3 倍以上
        let bins = |lo: f64, hi: f64| {
            residuals
                .iter()
                .filter(|r| (lo..hi).contains(&r.abs()))
                .count() as f64
                / n
        };
        let first = bins(0.0, 0.02);
        let second = bins(0.02, 0.04);
        assert!(
            second > 0.0 && first / second < 1.6,
            "{name}: 首箱/次箱 = {:.2}（>1.6 说明 0 附近又长出一根尖峰）",
            first / second.max(1e-9)
        );
    }

    #[test]
    fn spike_points_carry_a_real_measurement_residual_not_an_atom_at_zero() {
        let p = points();
        let on = build(2000.0, 800, 7, (39.9, 116.4), 0, &p);
        assert_single_peaked_residuals("经典模式", &speed_residuals(&on));
        // 对照：0.8.0 的 spike 分支把 speed 直接置 0（avgSpeed>0），
        // 那些点的 residual = −1，这里只确认对照样本确实存在这种点。
        let off = build_tuned_realism(2000.0, 800, 7, (39.9, 116.4), 0, &p, 0.0, Realism::legacy());
        let legacy_zero_speed = off
            .locations
            .iter()
            .filter(|q| q.ptype != -1 && q.speed == 0.0 && q.avgSpeed > 0.0)
            .count();
        assert!(legacy_zero_speed > 10, "对照样本没有复现旧的零速尖峰");
    }

    /// 与投影无关的强证明：`radius` 的重映射（均匀 → 截断对数正态）与旧版消耗
    /// 同样多次抽样，所以只开 `skewed_radius` 时坐标与 `Realism::legacy()` 完全相同。
    /// 这条不依赖任何几何基线，换成任何一版坐标口径都成立。
    #[test]
    fn radius_remap_alone_never_moves_a_coordinate() {
        let p = points();
        let off = build_tuned_realism(2000.0, 800, 7, (39.9, 116.4), 0, &p, 0.0, Realism::legacy());
        let rad_only = build_tuned_realism(
            2000.0,
            800,
            7,
            (39.9, 116.4),
            0,
            &p,
            0.0,
            Realism {
                shape: false,
                stops: false,
                outliers: false,
                skewed_radius: true,
            },
        );
        assert_eq!(
            coord_hash(&off),
            coord_hash(&rad_only),
            "只换 radius 的分布不该动坐标（抽样消耗必须一致）"
        );
        assert_eq!(off.totalSteps, rad_only.totalSteps);
        assert_eq!(off.totalTime, rad_only.totalTime);
        let differing = off
            .locations
            .iter()
            .zip(&rad_only.locations)
            .filter(|(a, b)| a.radius.to_bits() != b.radius.to_bits())
            .count();
        assert!(
            differing * 2 > off.locations.len(),
            "radius 几乎没变（只有 {differing} 点不同）"
        );
    }

    /// 停走点：`speed=0` 且 `avgSpeed=0`，且累计距离/步数相对上一点不增长。
    fn stop_indices(track: &Track) -> Vec<usize> {
        let n = track.locations.len();
        (3..n - 1)
            .filter(|&i| {
                let p = &track.locations[i];
                p.ptype != -1 && p.ptype != 5 && p.ptype != 6 && p.speed == 0.0 && p.avgSpeed == 0.0
            })
            .collect()
    }

    #[test]
    fn stop_windows_cover_one_to_three_percent_and_freeze_the_counters() {
        let p = points();
        let on = build(2000.0, 800, 7, (39.9, 116.4), 0, &p);
        let off = build_tuned_realism(2000.0, 800, 7, (39.9, 116.4), 0, &p, 0.0, Realism::legacy());
        let n = on.locations.len();
        let stops = stop_indices(&on);
        assert!(!stops.is_empty(), "没有生成任何停走窗口");
        let ratio = stops.len() as f64 / n as f64;
        assert!(
            (0.008..=0.035).contains(&ratio),
            "停走覆盖率 {:.1}% 不在 1–3%（{}/{n}）",
            ratio * 100.0,
            stops.len()
        );
        // 每段 1–3 个点（连续段长度分布）
        let mut runs = Vec::new();
        let mut cur = 0usize;
        for i in 3..n - 1 {
            if stops.contains(&i) {
                cur += 1;
            } else if cur > 0 {
                runs.push(cur);
                cur = 0;
            }
        }
        if cur > 0 {
            runs.push(cur);
        }
        for r in &runs {
            assert!((1..=3).contains(r), "停走段长度 {r} 越界（应 1–3 点）");
        }
        // 窗口内累计距离与步数不增长
        for &i in &stops {
            assert_eq!(
                on.locations[i].totalDis,
                on.locations[i - 1].totalDis,
                "第 {i} 点停走但 totalDis 增长了"
            );
            assert_eq!(on.locations[i].validDis, on.locations[i - 1].validDis);
            assert_eq!(
                on.locations[i].steps,
                on.locations[i - 1].steps,
                "第 {i} 点停走但步数增长了"
            );
        }
        // 总距离仍在目标误差内（里程由走动的点重新分摊）
        assert!((on.totalDistance - 2000.0).abs() < 1e-6);
        // 对照：0.8.0 的「20% 点零速尖峰」（avgSpeed>0）被替换掉了
        let old_spikes = (3..n - 1)
            .filter(|&i| off.locations[i].speed == 0.0 && off.locations[i].avgSpeed > 0.0)
            .count();
        assert!(
            old_spikes as f64 / n as f64 > 0.10,
            "对照样本没有复现旧的零速尖峰（{old_spikes}/{n}）"
        );
        for &i in &stops {
            assert!(
                on.locations[i].avgSpeed == 0.0,
                "停走点的 avgSpeed 必须为 0（旧实现自相矛盾）"
            );
        }
    }

    #[test]
    fn outliers_are_short_excursions_and_never_land_on_checkpoints() {
        let p = points();
        let on = build(2000.0, 800, 7, (39.9, 116.4), 0, &p);
        let no_out = build_tuned_realism(
            2000.0,
            800,
            7,
            (39.9, 116.4),
            0,
            &p,
            0.0,
            Realism {
                outliers: false,
                ..Realism::on()
            },
        );
        let n = on.locations.len();
        let displaced: Vec<(usize, f64)> = (0..n)
            .map(|i| {
                let a = &on.locations[i];
                let b = &no_out.locations[i];
                let d = (((a.gLat - b.gLat) * MET_PER_DEG_LAT).powi(2)
                    + ((a.gLng - b.gLng) * met_per_deg_lng(a.gLat)).powi(2))
                .sqrt();
                (i, d)
            })
            .filter(|(_, d)| *d > 5.0)
            .collect();
        assert!(
            (1..=6).contains(&displaced.len()),
            "野点点数 {} 不在预期（1–2 次 × 1–2 点）",
            displaced.len()
        );
        for (i, d) in &displaced {
            assert!(
                (10.0..=30.0).contains(d),
                "第 {i} 点野点幅度 {d:.1} m 不在 10–30 m"
            );
            assert!(*i >= 3, "第 {i} 点是起末段，不许注入野点");
            assert!(*i + 2 < n, "第 {i} 点太靠后（哨兵区），不许注入野点");
            assert_ne!(on.locations[*i].ptype, -1, "异常点不叠野点");
            // 野点该按异常点报精度
            assert!(
                on.locations[*i].radius >= 10.0,
                "野点 radius {:.2} 没有按异常点上报",
                on.locations[*i].radius
            );
        }
        let injected: Vec<usize> = displaced.iter().map(|(i, _)| *i).collect();
        // 每个打卡点仍然由**非野点**精确落位（40 m 校验不受影响）
        for cp in &p {
            let (best_i, best_d) = on
                .locations
                .iter()
                .enumerate()
                .map(|(i, q)| {
                    let d = (((q.gLat - cp.0) * MET_PER_DEG_LAT).powi(2)
                        + ((q.gLng - cp.1) * met_per_deg_lng(cp.0)).powi(2))
                    .sqrt();
                    (i, d)
                })
                .fold(
                    (0usize, f64::INFINITY),
                    |acc, x| {
                        if x.1 < acc.1 {
                            x
                        } else {
                            acc
                        }
                    },
                );
            assert!(best_d < 1.0, "打卡点落位失败：{best_d:.2} m");
            assert!(
                !injected.contains(&best_i),
                "打卡点被野点顶替（索引 {best_i}）"
            );
        }
    }

    #[test]
    fn radius_follows_class_bounds_and_is_right_skewed() {
        let p = points();
        // 关掉野点：野点会按「异常点」报精度（10–30 m），但点位类型仍是 0/3，
        // 留在同一张表里会把两个类别混在一起。
        let track = build_tuned_realism(
            2000.0,
            800,
            7,
            (39.9, 116.4),
            0,
            &p,
            0.0,
            Realism {
                outliers: false,
                ..Realism::on()
            },
        );
        let n = track.locations.len();
        let mut normal = Vec::new();
        let mut t3 = Vec::new();
        let mut exc = Vec::new();
        for (i, q) in track.locations.iter().enumerate() {
            if i == 0 || i == 1 || i + 1 == n || q.ptype == 5 || q.ptype == 6 {
                continue; // 哨兵点由 postfix 单独给值（1.5–3.5 m）
            }
            assert_eq!(q.radius, q.accuracy, "radius/accuracy 必须同源");
            match q.ptype {
                -1 => exc.push(q.radius),
                3 => t3.push(q.radius),
                _ => normal.push(q.radius),
            }
        }
        for (name, v, lo, hi) in [
            ("普通点", &normal, 4.0, 15.0),
            ("type=3", &t3, 4.0, 20.0),
            ("异常点", &exc, 10.0, 30.0),
        ] {
            assert!(!v.is_empty(), "{name} 没有样本");
            for x in v.iter() {
                assert!(*x >= lo && *x <= hi, "{name} radius {x} 越界（{lo}–{hi}）");
            }
            let mean = v.iter().sum::<f64>() / v.len() as f64;
            let mut sorted = v.clone();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let median = sorted[sorted.len() / 2];
            assert!(
                mean > median,
                "{name} 不是右偏：mean={mean:.2} median={median:.2}"
            );
            let mut uniq = sorted.clone();
            uniq.dedup();
            assert!(
                uniq.len() >= v.len() / 3,
                "{name} 取值过于集中（固定值特征）：{} / {}",
                uniq.len(),
                v.len()
            );
        }
    }

    #[test]
    fn closed_loop_course_has_no_repeated_laps_and_no_needle_fold() {
        // 2 km 闭环：方形环长 ≈786 m → 2.5 圈。旧行为是同一圈原样重跑。
        let corners = [
            (39.9077201, 116.4126209),
            (39.9077201, 116.4146209),
            (39.9097201, 116.4146209),
            (39.9097201, 116.4126209),
        ];
        let (dense, arcs, _) = make_point_ring(&corners);
        let total = *arcs.last().unwrap();
        assert!(
            (700.0..=900.0).contains(&total),
            "环长 {total:.0} m 与预期不符"
        );
        let n = 400;
        let course = |warp: &noise::WarpField| -> Vec<(f64, f64)> {
            (0..n)
                .map(|i| noise::sample_course(&dense, &arcs, i as f64 * 5.0, warp))
                .collect()
        };
        let flat = course(&noise::WarpField::flat());
        let warp = noise::WarpField::new(&mut noise::derive_rng(7, noise::SALT_SHAPE), total);
        let curved = course(&warp);

        // 重合段判据：段中点落在「沿程相隔 >300 m 的另一段」0.5 m 内 —— 0.5 m 是
        // 「同一条线的复制」的量级；300 m 大于本路网最长直段，避免把长直路自身算成重合。
        let overlap_flat = noise::overlap_ratio(&flat, 0.5, 300.0);
        let overlap_on = noise::overlap_ratio(&curved, 0.5, 300.0);
        assert!(
            overlap_flat > 0.9,
            "旧行为的重合段占比只有 {overlap_flat:.2}，样本不像「同一圈重跑」"
        );
        assert!(
            overlap_on < 0.20,
            "加了车道偏移后重合段占比仍有 {overlap_on:.2}"
        );
        assert!(
            overlap_on < overlap_flat * 0.3,
            "车道偏移没消掉重合：{overlap_flat:.2} -> {overlap_on:.2}"
        );
        // 圈间分离：这是「两张重合线」最直接的度量
        let sep_flat = noise::lap_separation(&flat, total);
        let sep_on = noise::lap_separation(&curved, total);
        assert!(sep_flat < 0.2, "旧行为第二圈离第一圈已有 {sep_flat:.2} m");
        assert!(
            sep_on > 1.5,
            "第二圈离第一圈只有 {sep_on:.2} m（仍会被画成重合线）"
        );

        // 最小折返角：方形闭环是 90°，任何 <30° 的针状折返都不允许
        let fold = noise::min_fold_angle_deg(&curved, 1.0).expect("折返角");
        assert!(fold >= 30.0, "最小折返角 {fold:.1}° < 30°（针状折返）");

        // 往返段：闭环同向绕圈，不该出现「反着重跑」
        for (name, c) in [("flat", &flat), ("warp", &curved)] {
            let runs = noise::retrace_runs(c, 1.5);
            let longest = runs.iter().cloned().fold(0.0, f64::max);
            assert!(longest <= 80.0, "{name} 出现 {longest:.0} m 往返段");
        }
    }
}
