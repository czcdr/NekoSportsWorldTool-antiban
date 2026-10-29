//! 本地生成专用的「真实性」分布层（0.9.0）。
//!
//! 三类「真人一定有、生成器原先没有」的分布，外加轨迹形态去几何化：
//!
//! 1. **短停走窗口** [`StopPlan`]：1–3% 的点、每段 1–3 点，窗口内零位移/零步数，
//!    优先落在起末段 —— 替掉旧实现里「20% 的点被单独置零速」的尖峰式伪停顿
//!    （零速点与 `avgSpeed>0` 自相矛盾，是机器特征而非人的行为）。
//! 2. **GPS 野点** [`Outliers`]：1–2 次、每次 1–2 个连续点的 10–30 m 短时跳变。
//!    起点/终点/哨兵点/必经点一律不注入 —— 那些点要落进 40 m 打卡校验，
//!    野点会把校验打坏。
//! 3. **`radius`/`accuracy` 右偏分布** [`radius_from_gauss`]：截断对数正态，
//!    普通点 4–15 m、`type=3` 点 4–20 m、异常点 10–30 m；旧实现是均匀区间
//!    （1.4–2.4 m），所有用户所有点挤在同一窄带里。
//! 4. **逐圈车道偏移** [`WarpField`]：里程不够时绕圈不再把同一段路原样重跑一遍，
//!    而是沿弧长做低频横向偏移（周期 2L/3L/5L 的正弦叠加，连续、无折角），
//!    于是相邻两圈是两条不同的线（约 2–3 m 的横向间隔），既不是重合线，
//!    也不会制造 <30° 的针状折返。
//!
//! ## 随机数约定（很重要）
//!
//! 本模块的所有抽样都走**派生 RNG**（[`derive_rng`]，由 `seed ^ SALT_*` 得到），
//! 一个都不消耗主 `Rng`。所以主抽样序列（采样间隔、点位类型、海拔、步频、抖动）
//! 与 0.8.0 完全一致 —— [`Realism::legacy`] 能逐点复现改动前的坐标哈希，
//! 这就是「新增分布之外没有动别的随机序列」的机器证据。
//!
//! 需要重映射既有抽样（`radius`）时，也刻意保持**同样的抽样消耗**：
//! 道路模式继续用一次 `gauss`（只换映射），经典模式继续用一次 `uniform`
//! （再经 [`probit`] 变成正态分位），因此下游字段一个都不会被推位。
//!
//! 形态度量（[`overlap_ratio`] / [`min_fold_angle_deg`] / [`retrace_runs`] /
//! [`lap_separation`]）与若干观测用取值器只在测试与统计报告里调用，
//! 生产路径不读它们，因此整模块允许 dead_code（而不是为了消警告去改调用点）。
#![allow(dead_code)]

use super::geom::{ring_point_at, round_to, Rng};

use std::f64::consts::TAU;

/// 形态偏移（车道）用的派生盐。
pub const SALT_SHAPE: u64 = 0x51ED_0F5A_9E37_79B9;
/// 停走窗口用的派生盐。
pub const SALT_STOP: u64 = 0x2709_A19B_0F5A_51ED;
/// 野点用的派生盐。
pub const SALT_OUTLIER: u64 = 0xC2B2_AE35_3C6E_F373;
/// spike 点残差用的派生盐（[`deterministic_z`]，不消耗 RNG）。
pub const SALT_SPIKE_RESIDUAL: u64 = 0x7A31_5C0D_2B8F_44E1;

/// 新增分布的开关。`legacy()` 全关 → 生成结果与 0.8.0 逐点一致。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Realism {
    /// 逐圈车道偏移（去「同一段路重跑一遍」）。
    pub shape: bool,
    /// 1–3% 短停走窗口。
    pub stops: bool,
    /// 10–30 m 短时野点。
    pub outliers: bool,
    /// `radius`/`accuracy` 右偏分布。
    pub skewed_radius: bool,
}

impl Realism {
    /// 生产默认：四项全开。
    pub const fn on() -> Self {
        Self {
            shape: true,
            stops: true,
            outliers: true,
            skewed_radius: true,
        }
    }

    /// 全关：用于回归证明「新增分布之外一行都没变」。
    pub const fn legacy() -> Self {
        Self {
            shape: false,
            stops: false,
            outliers: false,
            skewed_radius: false,
        }
    }
}

impl Default for Realism {
    fn default() -> Self {
        Self::on()
    }
}

/// 派生 RNG：与主序列完全独立，主 `Rng` 的消耗次数不受任何影响。
pub fn derive_rng(seed: u64, salt: u64) -> Rng {
    Rng::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ salt)
}

// ── radius / accuracy：截断对数正态 ───────────────────────────────

/// `radius`/`accuracy` 的取数类别。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RadiusClass {
    /// 普通点：声明区间 4–15 m。经 `soft_knee` 软膝后**实际落在约 4.15–14.39 m**，
    /// 永远碰不到边界值（硬 clamp 会在边界堆原子值，本身就是一种指纹）。
    /// 断言里提到的「4–15」是声明区间，不是可达区间。
    Normal,
    /// `type=3` 点：声明 4–20 m，软膝后实际约 4.20–19.07 m（这类点常出现在弱信号/室内边缘，长尾更长）。
    Type3,
    /// 异常点（`type=-1` 与 GPS 野点）：声明 10–30 m，软膝后实际约 10.21–29.04 m。
    Exception,
}

impl RadiusClass {
    /// 按点位类型 + 是否为野点归类。
    pub fn of(ptype: i64, outlier: bool) -> Self {
        if outlier {
            RadiusClass::Exception
        } else if ptype == 3 {
            RadiusClass::Type3
        } else {
            RadiusClass::Normal
        }
    }

    /// (中位数, 对数标准差, 下限, 上限)
    fn params(self) -> (f64, f64, f64, f64) {
        match self {
            RadiusClass::Normal => (7.0, 0.40, 4.0, 15.0),
            RadiusClass::Type3 => (8.5, 0.48, 4.0, 20.0),
            RadiusClass::Exception => (14.0, 0.38, 10.0, 30.0),
        }
    }

    /// 区间上界（测试与报告用）。
    pub fn bounds(self) -> (f64, f64) {
        let (_, _, lo, hi) = self.params();
        (lo, hi)
    }
}

/// 标准正态分位 → 截断对数正态的 `radius`（米，两位小数）。
///
/// 道路模式原本就抽一次 `gauss`，这里只换映射：抽样消耗不变，下游字段不动。
pub fn radius_from_gauss(z: f64, class: RadiusClass) -> f64 {
    let (median, sigma, lo, hi) = class.params();
    radius_quantile(probit_of_z_tail(z), median, sigma, lo, hi)
}

/// 单个 `[0,1)` 均匀量 → 截断对数正态的 `radius`（米，两位小数）。
///
/// 经典模式原本就抽一次 `uniform`，这里同样只换映射。
pub fn radius_from_unit(u: f64, class: RadiusClass) -> f64 {
    let (median, sigma, lo, hi) = class.params();
    radius_quantile(probit(u), median, sigma, lo, hi)
}

fn probit_of_z_tail(z: f64) -> f64 {
    // 与 uniform 入口共用同一条映射：gauss 抽到的 z 直接当分位用。
    z
}

fn radius_quantile(z: f64, median: f64, sigma: f64, lo: f64, hi: f64) -> f64 {
    let z = if z.is_finite() {
        z.clamp(-6.0, 6.0)
    } else {
        0.0
    };
    // F2：不用硬 clamp。硬截断会在 4.00 / 15.00 / 20.00 这些边界上堆原子值
    // （两位小数四舍五入之后变成一串完全相同的光秃值，实测 4.00 占 8%、10.00 占 18%），
    // 本身就是「分布边界突变」这种机器特征。改成对数空间的软膝压缩：
    // 远离边界时近似恒等（中位数一位不动），越靠近边界导数越小、永远碰不到边界。
    round_to(soft_knee(median * (sigma * z).exp(), lo, hi), 2)
}

/// 对数空间软膝映射：把 `x` 单调压进开区间 `(lo, hi)`。
///
/// 令 `u = ln x`、`c = (ln lo + ln hi)/2`、`h = (ln hi − ln lo)/2`、
/// `t = (u − c)/h`，映射为 `t' = KNEE·tanh(t / KNEE)`（`KNEE = 0.97 < 1`）：
/// - `|t| ≲ 0.5` 时 `tanh(t/KNEE) ≈ t/KNEE`，`t' ≈ t` → 主体分布（含中位数）不变；
/// - `|t| ≥ 3` 时 `t' → ±0.97`，`u' → c ± 0.97h` → 值停在边界内侧，**不落在边界上**；
/// - 严格单调 → 不会产生重复取值。
///
/// 与「硬 clamp」的差别只在尾巴：原来 8.1% 的点被压成 `4.00`，现在是 4.1–4.6 的一段连续值。
pub fn soft_knee(x: f64, lo: f64, hi: f64) -> f64 {
    const KNEE: f64 = 0.97;
    if !(lo > 0.0 && hi > lo) || !x.is_finite() {
        return x;
    }
    let c = 0.5 * (lo.ln() + hi.ln());
    let h = 0.5 * (hi.ln() - lo.ln());
    if h <= 0.0 {
        return x;
    }
    let t = (x.max(f64::MIN_POSITIVE).ln() - c) / h;
    (c + h * KNEE * (t / KNEE).tanh()).exp()
}

/// 确定性「伪噪声」标准正态分位（**不消耗任何 RNG**）。
///
/// 用途：spike 分支（`u<0.20` 时旧代码不抽 gauss）需要给点位一个真实的测量残差，
/// 但又不能再抽一次随机 —— 那会推位主序列，破坏「`Realism::legacy()` 逐点等于旧版」
/// 的等价证明。这里用 `splitmix64(seed ⊕ index·φ ⊕ salt)` → 均匀 → [`probit`]。
///
/// 结果对同一条记录是确定的、对不同的 seed 是不同的（不是跨记录的固定指纹）。
pub fn deterministic_z(seed: u64, index: u64, salt: u64) -> f64 {
    let mut x = seed ^ index.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ salt ^ 0xD1B5_4A32_D192_ED03;
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    let u = ((x >> 11) as f64) * (1.0 / (1u64 << 53) as f64);
    probit(u)
}

/// 标准正态分位函数（Acklam 有理逼近，绝对误差 < 1.2e-9）。
///
/// 只用它把「一次均匀抽样」变成正态分位，不额外消耗随机数。
pub fn probit(p: f64) -> f64 {
    const A: [f64; 6] = [
        -3.969_683_028_665_376e1,
        2.209_460_984_245_205e2,
        -2.759_285_104_469_687e2,
        1.383_577_518_672_690e2,
        -3.066_479_806_614_716e1,
        2.506_628_277_459_239e0,
    ];
    const B: [f64; 5] = [
        -5.447_609_879_822_406e1,
        1.615_858_368_580_409e2,
        -1.556_989_798_598_866e2,
        6.680_131_188_771_972e1,
        -1.328_068_155_288_572e1,
    ];
    const C: [f64; 6] = [
        -7.784_894_002_430_293e-3,
        -3.223_964_580_411_365e-1,
        -2.400_758_277_161_838e0,
        -2.549_732_539_343_734e0,
        4.374_664_141_464_968e0,
        2.938_163_982_698_783e0,
    ];
    const D: [f64; 4] = [
        7.784_695_709_041_462e-3,
        3.224_671_290_700_398e-1,
        2.445_134_137_142_996e0,
        3.754_408_661_907_416e0,
    ];
    const P_LOW: f64 = 0.024_25;
    let p = if p.is_finite() {
        p.clamp(1e-12, 1.0 - 1e-12)
    } else {
        0.5
    };
    if p < P_LOW {
        let q = (-2.0 * p.ln()).sqrt();
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    } else if p <= 1.0 - P_LOW {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    } else {
        let q = (-2.0 * (1.0 - p).ln()).sqrt();
        -(((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    }
}

// ── 短停走窗口 ────────────────────────────────────────────────────

/// 短停走窗口集合（半开区间 `[start, end)`，每段 1–3 点）。
///
/// 覆盖目标 1–3% 的点；70% 的窗口落在轨迹前/后 15%（真人常在出发整理装备、
/// 到点收操时停一下），其余随机。索引 0/1/2 与末点不动：那是哨兵与首个真实点，
/// `postfix` 会把索引 2 的 `avgSpeed` 改写成非零值，落窗口就会自相矛盾。
#[derive(Clone, Debug, Default)]
pub struct StopPlan {
    windows: Vec<(usize, usize)>,
}

impl StopPlan {
    /// 空集合（`Realism::legacy` 与轨迹过短时用）。
    pub fn none() -> Self {
        Self {
            windows: Vec::new(),
        }
    }

    /// 抽一批停走窗口。`blocked` 返回 `true` 的索引不可落窗口（异常点等）。
    pub fn new(n: usize, rng: &mut Rng, blocked: &dyn Fn(usize) -> bool) -> Self {
        let lo = 3usize;
        let hi = n.saturating_sub(1);
        if n < 16 || hi <= lo + 6 {
            return Self::none();
        }
        let span = hi - lo;
        let frac = rng.uniform(0.010, 0.030);
        let target = ((frac * n as f64).round() as usize).clamp(1, 6);
        let mut windows: Vec<(usize, usize)> = Vec::new();
        for _ in 0..96 {
            let covered: usize = windows.iter().map(|w| w.1 - w.0).sum();
            if covered >= target {
                break;
            }
            let centre = if rng.random() < 0.70 {
                // 起末段优先
                let edge = (rng.uniform(0.0, 0.15) * span as f64) as usize;
                if rng.random() < 0.5 {
                    lo + edge
                } else {
                    hi - 1 - edge
                }
            } else {
                lo + (rng.random() * span as f64) as usize
            };
            // 段长 1–3，但不超过剩余目标点数（保证总覆盖率不越过 1–3%）
            let room = target.saturating_sub(covered);
            if room == 0 {
                break;
            }
            let len = (rng.randint(1, 3) as usize).min(room);
            let start = centre.saturating_sub(len / 2).max(lo);
            let end = (start + len).min(hi);
            if end <= start {
                continue;
            }
            // 窗口之间留 1 点间隔，避免两段拼成 4 点长停
            if windows.iter().any(|w| start <= w.1 && w.0 <= end) {
                continue;
            }
            if (start..end).any(blocked) {
                continue;
            }
            windows.push((start, end));
        }
        windows.sort_unstable();
        Self { windows }
    }

    /// 该索引是否在停走窗口内。
    pub fn contains(&self, i: usize) -> bool {
        self.windows.iter().any(|w| i >= w.0 && i < w.1)
    }

    /// 覆盖的点数。
    pub fn covered(&self) -> usize {
        self.windows.iter().map(|w| w.1 - w.0).sum()
    }

    /// 窗口列表（测试与报告用）。
    pub fn windows(&self) -> &[(usize, usize)] {
        &self.windows
    }

    /// 覆盖率（覆盖点数 / n）。
    pub fn ratio(&self, n: usize) -> f64 {
        if n == 0 {
            0.0
        } else {
            self.covered() as f64 / n as f64
        }
    }
}

// ── GPS 野点 ──────────────────────────────────────────────────────

/// 一次短时跳变：`len` 个连续点共用同一段位移（真机的野点就是这样「跳出去再跳回来」）。
#[derive(Clone, Copy, Debug)]
pub struct OutlierEvent {
    pub start: usize,
    pub len: usize,
    pub dx: f64,
    pub dy: f64,
}

impl OutlierEvent {
    /// 位移幅度（米）。
    pub fn magnitude(&self) -> f64 {
        self.dx.hypot(self.dy)
    }
}

/// 野点集合：1–2 次、每次 1–2 个连续点、10–30 m。
#[derive(Clone, Debug, Default)]
pub struct Outliers {
    events: Vec<OutlierEvent>,
}

impl Outliers {
    /// 空集合（`Realism::legacy` 用）。
    pub fn none() -> Self {
        Self { events: Vec::new() }
    }

    /// 抽野点。`blocked` 返回 `true` 的索引不得注入（起点/终点/哨兵/必经点/停走点）。
    pub fn new(n: usize, rng: &mut Rng, blocked: &dyn Fn(usize) -> bool) -> Self {
        let lo = 3usize;
        let hi = n.saturating_sub(2);
        if n < 24 || hi <= lo + 4 {
            return Self::none();
        }
        let count = 1 + usize::from(rng.random() < 0.5);
        let mut events: Vec<OutlierEvent> = Vec::new();
        for _ in 0..count {
            let len = 1 + usize::from(rng.random() < 0.45);
            // 10–30 m，右偏：中位 ≈13 m，长尾到 30 m
            let mag = (13.0 * (0.28 * rng.gauss(0.0, 1.0)).exp()).clamp(10.0, 30.0);
            let ang = rng.uniform(0.0, TAU);
            let mut placed = None;
            for _ in 0..32 {
                if hi <= lo + len {
                    break;
                }
                let start = lo + (rng.random() * (hi - lo - len) as f64) as usize;
                let end = start + len;
                if (start..end).any(blocked) {
                    continue;
                }
                // 与已有事件至少隔 2 点，避免拼成 4 点长跳
                if events
                    .iter()
                    .any(|e| start < e.start + e.len + 2 && e.start < end + 2)
                {
                    continue;
                }
                placed = Some(start);
                break;
            }
            if let Some(start) = placed {
                events.push(OutlierEvent {
                    start,
                    len,
                    dx: mag * ang.cos(),
                    dy: mag * ang.sin(),
                });
            }
        }
        Self { events }
    }

    /// 该索引的额外位移（米，平面系；不在事件内为 `(0,0)`）。
    pub fn offset_at(&self, i: usize) -> (f64, f64) {
        for e in &self.events {
            if i >= e.start && i < e.start + e.len {
                return (e.dx, e.dy);
            }
        }
        (0.0, 0.0)
    }

    /// 该索引是否落在野点事件内（`radius` 要按异常点取值）。
    pub fn covers(&self, i: usize) -> bool {
        self.events
            .iter()
            .any(|e| i >= e.start && i < e.start + e.len)
    }

    /// 事件列表（测试与报告用）。
    pub fn events(&self) -> &[OutlierEvent] {
        &self.events
    }

    /// 被注入的点数。
    pub fn covered(&self) -> usize {
        self.events.iter().map(|e| e.len).sum()
    }
}

// ── 逐圈车道偏移 ──────────────────────────────────────────────────

/// 沿弧长的低频横向偏移场。
///
/// `offset(s) = a1·sin(2πs/2L+p1) + a2·sin(2πs/3L+p2) + a3·sin(2πs/5L+p3)`。
///
/// - 三个周期（2L/3L/5L）互质，前 30 圈两两不同 → 绕圈不再重合；
/// - 是 `s` 的连续函数（没有圈边界跳变），对航向的额外改变 ≤ 约 5° →
///   不会造出 <30° 的针状折返；
/// - 振幅合计 ≤ 5.6 m，远小于 40 m 打卡半径。
#[derive(Clone, Copy, Debug)]
pub struct WarpField {
    a1: f64,
    p1: f64,
    a2: f64,
    p2: f64,
    a3: f64,
    p3: f64,
    total: f64,
}

impl Default for WarpField {
    fn default() -> Self {
        Self::flat()
    }
}

impl WarpField {
    /// 零偏移（`Realism::legacy` 与退化环线用）。
    pub fn flat() -> Self {
        Self {
            a1: 0.0,
            p1: 0.0,
            a2: 0.0,
            p2: 0.0,
            a3: 0.0,
            p3: 0.0,
            total: 0.0,
        }
    }

    /// 按一圈的长度 `total_m` 抽样（必须传派生 RNG）。
    pub fn new(rng: &mut Rng, total_m: f64) -> Self {
        if !total_m.is_finite() || total_m <= 1.0 {
            return Self::flat();
        }
        Self {
            a1: rng.uniform(2.5, 4.0),
            p1: rng.uniform(0.0, TAU),
            a2: rng.uniform(1.0, 1.8),
            p2: rng.uniform(0.0, TAU),
            a3: rng.uniform(0.4, 0.9),
            p3: rng.uniform(0.0, TAU),
            total: total_m,
        }
    }

    /// 是否退化为零偏移。
    pub fn is_flat(&self) -> bool {
        self.total <= 0.0 || (self.a1 == 0.0 && self.a2 == 0.0 && self.a3 == 0.0)
    }

    /// 弧长 `s` 处的横向偏移（米）。
    pub fn offset(&self, s: f64) -> f64 {
        if self.is_flat() || !s.is_finite() {
            return 0.0;
        }
        let w = TAU * s / self.total;
        self.a1 * (0.5 * w + self.p1).sin()
            + self.a2 * (w / 3.0 + self.p2).sin()
            + self.a3 * (w / 5.0 + self.p3).sin()
    }
}

/// 环线上弧长 `s` 处的采样点（含车道偏移）。
///
/// 偏移方向取当地切线的法向：先按 `s±0.5 m` 求切线，再乘 [`WarpField::offset`]。
pub fn sample_course(dense: &[(f64, f64)], arcs: &[f64], s: f64, warp: &WarpField) -> (f64, f64) {
    let p = ring_point_at(dense, arcs, s);
    if warp.is_flat() {
        return p;
    }
    let a = ring_point_at(dense, arcs, s - 0.5);
    let b = ring_point_at(dense, arcs, s + 0.5);
    let (tx, ty) = (b.0 - a.0, b.1 - a.1);
    let len = tx.hypot(ty);
    if len <= 1e-9 {
        return p;
    }
    let off = warp.offset(s);
    (p.0 - ty / len * off, p.1 + tx / len * off)
}

// ── 形态度量（生成与测试、报告共用同一套口径） ────────────────────

/// 重合段占比：段中点落在**沿程相隔 > `min_gap_m`** 的另一段上
/// （点到线段距离 ≤ `tol_m`）的比例。
///
/// 为什么用沿程间隔而不是「隔几段」：一条长直路的相邻若干段本来就共线，按序号跳过
/// 3 段仍会把这条直线自身算成重合。`min_gap_m` 要取得比路网最长直段更大（测试里取
/// 300 m），这样只有「整圈重跑」这种真正的重复才会被算进去。
pub fn overlap_ratio(pts: &[(f64, f64)], tol_m: f64, min_gap_m: f64) -> f64 {
    if pts.len() < 4 {
        return 0.0;
    }
    // (起点, 终点, 中点, 中点沿程弧长, 段长)
    let mut segs: Vec<((f64, f64), (f64, f64), (f64, f64), f64, f64)> =
        Vec::with_capacity(pts.len());
    let mut acc = 0.0f64;
    for w in pts.windows(2) {
        let (a, b) = (w[0], w[1]);
        let len = (b.0 - a.0).hypot(b.1 - a.1);
        let mid = ((a.0 + b.0) * 0.5, (a.1 + b.1) * 0.5);
        segs.push((a, b, mid, acc + len * 0.5, len));
        acc += len;
    }
    let tol2 = tol_m * tol_m;
    let mut hit = 0usize;
    for i in 0..segs.len() {
        let (_, _, mid_i, s_i, len_i) = segs[i];
        if len_i <= 1e-9 {
            continue;
        }
        let dup = segs.iter().any(|&(a_j, b_j, _, s_j, len_j)| {
            len_j > 1e-9
                && (s_i - s_j).abs() > min_gap_m
                && point_segment_dist2(mid_i, a_j, b_j) <= tol2
        });
        if dup {
            hit += 1;
        }
    }
    hit as f64 / segs.len() as f64
}

/// 圈间分离度：第二圈相对第一圈的平均横向间隔（米）。
///
/// 这是「两张重合线」最直接的度量：0 = 第二圈完全压在第一圈上；真人绕圈每圈都
/// 会差个一两米，所以 ≥1.5 m 就不再是同一条线的复制。
pub fn lap_separation(pts: &[(f64, f64)], lap_m: f64) -> f64 {
    if pts.len() < 4 || !(lap_m > 1.0) {
        return 0.0;
    }
    let mut s_at = Vec::with_capacity(pts.len());
    let mut acc = 0.0f64;
    s_at.push(0.0);
    for w in pts.windows(2) {
        acc += (w[1].0 - w[0].0).hypot(w[1].1 - w[0].1);
        s_at.push(acc);
    }
    let first: Vec<(usize, usize)> = (0..pts.len() - 1)
        .filter(|&i| s_at[i + 1] <= lap_m + 1e-9)
        .map(|i| (i, i + 1))
        .collect();
    if first.is_empty() {
        return 0.0;
    }
    let mut sum = 0.0;
    let mut cnt = 0;
    for i in 0..pts.len() {
        if s_at[i] >= lap_m && s_at[i] < 2.0 * lap_m {
            let d2 = first
                .iter()
                .map(|&(a, b)| point_segment_dist2(pts[i], pts[a], pts[b]))
                .fold(f64::INFINITY, f64::min);
            sum += d2.sqrt();
            cnt += 1;
        }
    }
    if cnt == 0 {
        0.0
    } else {
        sum / cnt as f64
    }
}

/// 点到线段距离的平方（平面米）。
fn point_segment_dist2(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (vx, vy) = (b.0 - a.0, b.1 - a.1);
    let len2 = vx * vx + vy * vy;
    let t = if len2 <= 1e-12 {
        0.0
    } else {
        (((p.0 - a.0) * vx + (p.1 - a.1) * vy) / len2).clamp(0.0, 1.0)
    };
    let (dx, dy) = (p.0 - (a.0 + vx * t), p.1 - (a.1 + vy * t));
    dx * dx + dy * dy
}

/// 最小折返角（度）：`180° − 转向角`，即「原路折返程度」。
///
/// 0° = 精确掉头（重合往返），90° = 直角转弯，180° = 直行。
/// 三角形/针状折返的顶点就是折返角很小的位置。
pub fn min_fold_angle_deg(pts: &[(f64, f64)], min_seg_m: f64) -> Option<f64> {
    let mut min = f64::INFINITY;
    for w in pts.windows(3) {
        let u = (w[1].0 - w[0].0, w[1].1 - w[0].1);
        let v = (w[2].0 - w[1].0, w[2].1 - w[1].1);
        let (lu, lv) = (u.0.hypot(u.1), v.0.hypot(v.1));
        if lu < min_seg_m || lv < min_seg_m {
            continue;
        }
        let cos = (-(u.0 * v.0 + u.1 * v.1) / (lu * lv)).clamp(-1.0, 1.0);
        min = min.min(cos.acos().to_degrees());
    }
    if min.is_finite() {
        Some(min)
    } else {
        None
    }
}

/// 往返段（同一段路反方向重跑）长度列表，米。
///
/// 判据：段中点落在某个**更早且非相邻**段中点 `tol_m` 内，且两段方向相反。
/// 返回每一段连续「反着重跑」的长度。
pub fn retrace_runs(pts: &[(f64, f64)], tol_m: f64) -> Vec<f64> {
    if pts.len() < 5 {
        return Vec::new();
    }
    let segs: Vec<((f64, f64), (f64, f64), f64)> = pts
        .windows(2)
        .map(|w| {
            let d = (w[1].0 - w[0].0, w[1].1 - w[0].1);
            let mid = ((w[0].0 + w[1].0) * 0.5, (w[0].1 + w[1].1) * 0.5);
            (mid, d, d.0.hypot(d.1))
        })
        .collect();
    let tol2 = tol_m * tol_m;
    let mut runs = Vec::new();
    let mut cur = 0.0f64;
    for (k, (mid, dir, len)) in segs.iter().enumerate() {
        let mut back = false;
        if *len > 1e-9 {
            for (mid_j, dir_j, len_j) in segs.iter().take(k.saturating_sub(2)) {
                let lj = *len_j;
                if lj <= 1e-9 {
                    continue;
                }
                let near = (mid.0 - mid_j.0).powi(2) + (mid.1 - mid_j.1).powi(2) <= tol2;
                if near && (dir.0 * dir_j.0 + dir.1 * dir_j.1) / (len * lj) < -0.5 {
                    back = true;
                    break;
                }
            }
        }
        if back {
            cur += len;
        } else if cur > 0.0 {
            runs.push(cur);
            cur = 0.0;
        }
    }
    if cur > 0.0 {
        runs.push(cur);
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seq_stats(v: &[f64]) -> (f64, f64, f64) {
        let n = v.len() as f64;
        let mean = v.iter().sum::<f64>() / n;
        let var = v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
        let sd = var.sqrt();
        let skew = if sd <= 1e-12 {
            0.0
        } else {
            v.iter().map(|x| ((x - mean) / sd).powi(3)).sum::<f64>() / n
        };
        let mut sorted = v.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        (mean, sorted[sorted.len() / 2], skew)
    }

    #[test]
    fn probit_matches_known_quantiles() {
        for (p, want) in [
            (0.025, -1.959_964),
            (0.5, 0.0),
            (0.975, 1.959_964),
            (0.9, 1.281_552),
            (0.1, -1.281_552),
        ] {
            assert!(
                (probit(p) - want).abs() < 1e-5,
                "probit({p}) = {}",
                probit(p)
            );
        }
        assert!(probit(f64::NAN).is_finite());
        assert!(probit(0.0).is_finite() && probit(1.0).is_finite());
    }

    #[test]
    fn radius_is_right_skewed_and_inside_class_bounds() {
        let mut rng = Rng::new(0x51ED);
        for class in [
            RadiusClass::Normal,
            RadiusClass::Type3,
            RadiusClass::Exception,
        ] {
            let (lo, hi) = class.bounds();
            let samples: Vec<f64> = (0..4000)
                .map(|_| radius_from_unit(rng.random(), class))
                .collect();
            for v in &samples {
                assert!(
                    (lo - 1e-9..=hi + 1e-9).contains(v),
                    "{class:?} 越界：{v}（应在 {lo}–{hi}）"
                );
            }
            let (mean, median, skew) = seq_stats(&samples);
            assert!(
                mean > median,
                "{class:?} 不是右偏：mean={mean:.2} median={median:.2}"
            );
            assert!(skew > 0.5, "{class:?} 偏度只有 {skew:.2}");
            // 不得出现固定值：两位小数取值几乎全都不同
            let mut uniq = samples.clone();
            uniq.sort_by(|a, b| a.partial_cmp(b).unwrap());
            uniq.dedup();
            assert!(
                uniq.len() as f64 / samples.len() as f64 > 0.2,
                "{class:?} 取值过于集中：{} 个不同值 / {} 个样本",
                uniq.len(),
                samples.len()
            );
        }
    }

    /// F2：软膝替代硬 clamp —— 边界值不再是原子堆，但分布范围/中位/右偏不变。
    #[test]
    fn soft_knee_removes_boundary_atoms_without_narrowing_the_distribution() {
        let mut rng = Rng::new(0x2B1D);
        for class in [
            RadiusClass::Normal,
            RadiusClass::Type3,
            RadiusClass::Exception,
        ] {
            let (lo, hi) = class.bounds();
            let n = 4000;
            let samples: Vec<f64> = (0..n)
                .map(|_| radius_from_unit(rng.random(), class))
                .collect();
            // ① 边界原子：任一「恰等于边界」的取值占比必须 < 1%（旧实现 4–19%）
            for bound in [lo, hi] {
                let hits = samples
                    .iter()
                    .filter(|v| (**v - bound).abs() < 5e-3)
                    .count();
                let share = hits as f64 / n as f64;
                assert!(
                    share < 0.01,
                    "{class:?} 边界值 {bound:.2} 占比 {:.1}% ≥ 1%（软膝没生效）",
                    share * 100.0
                );
            }
            // ② 仍然严格在区间内部，且没有把分布压窄
            let min = samples.iter().cloned().fold(f64::INFINITY, f64::min);
            let max = samples.iter().cloned().fold(0.0f64, f64::max);
            assert!(min >= lo && max <= hi, "{class:?} 越界：{min}–{max}");
            assert!(
                min < lo + 0.25 * (hi - lo) && max > hi - 0.25 * (hi - lo),
                "{class:?} 分布被压窄了：{min:.2}–{max:.2}（区间 {lo}–{hi}）"
            );
            // ③ 中位数不动、仍右偏、唯一值仍然充足
            let (mean, median, skew) = seq_stats(&samples);
            let expect_median = class_median(class);
            assert!(
                (median - expect_median).abs() < 0.06 * expect_median,
                "{class:?} 中位被软膝改动太多：{median:.2} vs {expect_median:.2}"
            );
            assert!(
                mean > median,
                "{class:?} 不是右偏：mean={mean:.2} median={median:.2}"
            );
            assert!(skew > 0.5, "{class:?} 偏度只有 {skew:.2}");
            let mut uniq = samples.clone();
            uniq.sort_by(|a, b| a.partial_cmp(b).unwrap());
            uniq.dedup();
            assert!(
                uniq.len() as f64 / n as f64 > 0.2,
                "{class:?} 取值过于集中：{} 个不同值 / {n}",
                uniq.len()
            );
        }
        // 软膝是恒等映射的良性版本：单点取值看形态
        assert!(
            (soft_knee(7.0, 4.0, 15.0) - 7.0).abs() < 0.05,
            "中位附近应近似恒等"
        );
        assert!(soft_knee(1e9, 4.0, 15.0) < 15.0, "上尾必须停在边界内侧");
        assert!(soft_knee(1e-9, 4.0, 15.0) > 4.0, "下尾必须停在边界内侧");
        assert!(soft_knee(15.0, 4.0, 15.0) < 15.0);
        // 严格单调：不会产生重复取值
        let mut prev = f64::NEG_INFINITY;
        for k in 0..500 {
            let x = 4.0 + 11.0 * k as f64 / 499.0;
            let y = soft_knee(x, 4.0, 15.0);
            assert!(y > prev, "软膝不是严格单调：x={x}");
            prev = y;
        }
    }

    fn class_median(class: RadiusClass) -> f64 {
        match class {
            RadiusClass::Normal => 7.0,
            RadiusClass::Type3 => 8.5,
            RadiusClass::Exception => 14.0,
        }
    }

    /// F1 的取样器自检：`deterministic_z` 必须是「标准正态样」的确定性序列。
    #[test]
    fn deterministic_z_behaves_like_a_normal_sample_but_is_pure() {
        let n = 4000u64;
        let z: Vec<f64> = (0..n)
            .map(|i| deterministic_z(7, i, SALT_SPIKE_RESIDUAL))
            .collect();
        let (mean, _median, _skew) = seq_stats(&z);
        assert!(mean.abs() < 0.06, "均值 {mean:.3} 偏离 0");
        let var = z.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n as f64;
        assert!((var - 1.0).abs() < 0.15, "方差 {var:.3} 偏离 1");
        let mut uniq = z.clone();
        uniq.sort_by(|a, b| a.partial_cmp(b).unwrap());
        uniq.dedup();
        assert!(uniq.len() as f64 / n as f64 > 0.95, "取值重复过多");
        // 纯函数：同样输入恒定；不同 seed 给出不同序列（不是跨记录指纹）
        assert_eq!(
            deterministic_z(7, 42, SALT_SPIKE_RESIDUAL).to_bits(),
            deterministic_z(7, 42, SALT_SPIKE_RESIDUAL).to_bits()
        );
        let other: Vec<f64> = (0..64)
            .map(|i| deterministic_z(8, i, SALT_SPIKE_RESIDUAL))
            .collect();
        let same: Vec<f64> = (0..64)
            .map(|i| deterministic_z(7, i, SALT_SPIKE_RESIDUAL))
            .collect();
        assert_ne!(other, same, "换 seed 必须换序列");
    }

    #[test]
    fn radius_classes_follow_point_type_and_outlier() {
        assert_eq!(RadiusClass::of(3, false), RadiusClass::Type3);
        assert_eq!(RadiusClass::of(0, false), RadiusClass::Normal);
        assert_eq!(RadiusClass::of(-1, false), RadiusClass::Normal);
        assert_eq!(RadiusClass::of(3, true), RadiusClass::Exception);
        assert_eq!(RadiusClass::of(-1, true), RadiusClass::Exception);
    }

    #[test]
    fn stop_windows_cover_one_to_three_percent_and_avoid_sentinels() {
        let (mut edge_hits, mut total_windows) = (0usize, 0usize);
        for n in [64usize, 164, 320] {
            for seed in 0..32u64 {
                let mut rng = Rng::new(seed);
                let blocked = |i: usize| i == 7; // 模拟异常点
                let plan = StopPlan::new(n, &mut rng, &blocked);
                let ratio = plan.ratio(n);
                assert!(
                    (0.008..=0.035).contains(&ratio),
                    "n={n} seed={seed} 覆盖率 {ratio:.3} 不在 1–3% 附近"
                );
                for &(s, e) in plan.windows() {
                    assert!((1..=3).contains(&(e - s)), "窗口长度 {} 越界", e - s);
                    assert!(s >= 3 && e <= n - 1, "窗口 {s}..{e} 碰到哨兵");
                    assert!(!plan.contains(7), "窗口落在被排除的异常点上");
                }
                assert!(!plan.contains(0) && !plan.contains(1) && !plan.contains(2));
                assert!(!plan.contains(n - 1));
                // 起末段优先：窗口中心落在前后 20%（跨 seed 汇总统计）
                for &(s, e) in plan.windows() {
                    let c = (s + e) as f64 / 2.0;
                    total_windows += 1;
                    if c < n as f64 * 0.20 || c > n as f64 * 0.80 {
                        edge_hits += 1;
                    }
                }
            }
        }
        assert!(
            edge_hits * 2 >= total_windows,
            "停走没有优先落在起末段：{edge_hits}/{total_windows}"
        );
    }

    #[test]
    fn outliers_are_short_ten_to_thirty_metre_excursions() {
        for seed in 0..32u64 {
            let mut rng = Rng::new(seed);
            let blocked = |i: usize| (10..14).contains(&i);
            let out = Outliers::new(200, &mut rng, &blocked);
            assert!((1..=2).contains(&out.events().len()), "事件数不对");
            for e in out.events() {
                assert!((1..=2).contains(&e.len), "事件长度 {} 越界", e.len);
                let magnitude = e.magnitude();
                assert!(
                    (9.99..=30.01).contains(&magnitude),
                    "幅度 {magnitude:.3} m 不在 10–30 m"
                );
                for i in e.start..e.start + e.len {
                    assert!(!blocked(i), "野点落在受保护索引 {i} 上");
                    assert_eq!(
                        out.offset_at(i),
                        (e.dx, e.dy),
                        "同一次事件内位移必须一致（否则相邻点跳变过大）"
                    );
                }
            }
        }
        // 轨迹过短时不注入
        let mut rng = Rng::new(1);
        assert!(Outliers::new(10, &mut rng, &|_| false).events().is_empty());
    }

    #[test]
    fn warp_field_is_continuous_and_separates_laps() {
        let mut rng = Rng::new(7);
        let total = 400.0;
        let warp = WarpField::new(&mut rng, total);
        assert!(!warp.is_flat());
        // 连续性：相邻 0.5 m 的偏移差远小于 1 m（否则圈边界会出现折角）
        let mut s = 0.0;
        while s < 6.0 * total {
            let d = (warp.offset(s + 0.5) - warp.offset(s)).abs();
            assert!(d < 0.2, "s={s:.1} 偏移跳变 {d:.3} m");
            s += 0.5;
        }
        // 圈间分离：第 k 圈与第 k+1 圈在同一弧长位置的偏移平均差 ≥ 1.0 m
        for lap in 0..5 {
            let mut sum = 0.0;
            let mut cnt = 0.0;
            let mut s = 0.0;
            while s < total {
                sum += (warp.offset(s + (lap + 1) as f64 * total)
                    - warp.offset(s + lap as f64 * total))
                .abs();
                cnt += 1.0;
                s += 4.0;
            }
            assert!(
                sum / cnt >= 1.0,
                "第 {lap} 圈与第 {} 圈平均只差 {:.2} m（会画成重合线）",
                lap + 1,
                sum / cnt
            );
        }
        // 偏移幅度不超过 6 m（远小于 40 m 打卡半径）
        let mut s = 0.0;
        while s < 10.0 * total {
            assert!(
                warp.offset(s).abs() <= 5.7,
                "偏移 {:.2} m 过大",
                warp.offset(s)
            );
            s += 1.0;
        }
        assert_eq!(WarpField::flat().offset(123.0), 0.0);
    }

    #[test]
    fn course_sampling_moves_points_perpendicular_to_travel() {
        // 直线环：沿 +x 走，法向应为 ±y
        let dense: Vec<(f64, f64)> = (0..=200)
            .map(|i| (i as f64 * 5.0, 0.0))
            .chain((0..=200).rev().map(|i| (i as f64 * 5.0, 8.0)))
            .collect();
        let mut arcs = vec![0.0f64];
        for i in 1..=dense.len() {
            let a = dense[i - 1];
            let b = dense[i % dense.len()];
            arcs.push(arcs[i - 1] + (b.0 - a.0).hypot(b.1 - a.1));
        }
        let warp = WarpField {
            a1: 3.0,
            p1: std::f64::consts::FRAC_PI_2,
            a2: 0.0,
            p2: 0.0,
            a3: 0.0,
            p3: 0.0,
            total: *arcs.last().unwrap(),
        };
        let p0 = sample_course(&dense, &arcs, 100.0, &WarpField::flat());
        let p1 = sample_course(&dense, &arcs, 100.0, &warp);
        assert!((p1.0 - p0.0).abs() < 1e-9, "偏移不应沿前进方向");
        assert!((p1.1 - p0.1).abs() > 1.0, "法向偏移没生效");
    }

    #[test]
    fn shape_metrics_flag_exact_repeats_and_out_and_back() {
        // 同一段路原样重跑两遍（旧行为的合成样本）
        let mut square: Vec<(f64, f64)> = Vec::new();
        for lap in 0..2 {
            let _ = lap;
            square.extend_from_slice(&[
                (0.0, 0.0),
                (100.0, 0.0),
                (100.0, 100.0),
                (0.0, 100.0),
                (0.0, 0.0),
            ]);
        }
        let dup = overlap_ratio(&square, 1.0, 100.0);
        assert!(dup > 0.5, "重合段占比 {dup:.2} 应该很高");

        // 原路折返 300 m
        let mut back: Vec<(f64, f64)> = Vec::new();
        for i in 0..=60 {
            back.push((i as f64 * 5.0, 0.0));
        }
        for i in (0..60).rev() {
            back.push((i as f64 * 5.0, 0.0));
        }
        let runs = retrace_runs(&back, 1.5);
        assert!(!runs.is_empty(), "没识别出往返段");
        assert!(
            runs.iter().cloned().fold(0.0, f64::max) >= 250.0,
            "往返段长度 {:?} 识别偏小",
            runs
        );
        let fold = back
            .windows(3)
            .map(|w| {
                let u = (w[1].0 - w[0].0, w[1].1 - w[0].1);
                let v = (w[2].0 - w[1].0, w[2].1 - w[1].1);
                let (lu, lv) = (u.0.hypot(u.1), v.0.hypot(v.1));
                if lu < 1e-9 || lv < 1e-9 {
                    f64::INFINITY
                } else {
                    (-(u.0 * v.0 + u.1 * v.1) / (lu * lv))
                        .clamp(-1.0, 1.0)
                        .acos()
                        .to_degrees()
                }
            })
            .fold(f64::INFINITY, f64::min);
        assert!(fold < 1.0, "掉头点的折返角 {fold:.1}° 应该接近 0");

        // 正方形一圈：重合 0、折返角 = 90°
        let one = &square[..5];
        assert_eq!(overlap_ratio(one, 1.0, 100.0), 0.0);
        let angle = min_fold_angle_deg(one, 1.0).unwrap();
        assert!((angle - 90.0).abs() < 1e-6, "直角环最小折返角 {angle:.2}");
        assert!(retrace_runs(one, 1.5).is_empty());
    }
}
