//! 真实道路路由轨迹生成器（模式 B）。
//!
//! 与经典模式（`generator::build`）的分工：
//! - 几何源为路网闭环路线（而非 Catmull-Rom 环）；
//! - 配速为运动学速度剖面：OU 均值回归 + 弯道预判刹车（`kinematics::pace_profile`）；
//! - 步频/步幅为生物力学 OU 耦合（`biomech::ou_cadence_series` + `gait`）；
//! - GPS 抖动为 AR(1) 相关漂移 + 高斯测量噪声 + 建筑 SDF 衰减（`noise::GpsJitter`）。
//!
//! 协议字段/哨兵/断崖/吸附/10s 窗与经典模式完全一致。

use super::generator::{SPEED_CEIL, SPEED_FLOOR};
use super::geom::{
    fmt_gain_time, ring_point_at, round_to, to_bd, wgs84_to_bd09, Rng, MET_PER_DEG_LAT,
    MET_PER_DEG_LNG,
};
use super::model::{GenPoint, Track};
use super::postfix::apply_post_fixes;
use route_planner::{
    fatigue_from_km, gait, ou_cadence_series, pace_profile, plan_route_split, Coord, GpsJitter,
    KinParams, RoadGraph, Route, RouteOptions, Sdf,
};

/// 路线算法模式（三选一）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum RouteMode {
    /// 经典打卡点环（Catmull-Rom）。
    #[default]
    Legacy,
    /// 真实道路拓扑路由：强制经过**全部**策略必经点与实时点位。
    Road,
    /// 自定义点位：只按实时点位顺序串联，不追加策略必经点。
    ///
    /// 有可用路网时沿道路走；没有路网（未配置 / 读取失败 / 围栏裁剪为空 / 不连通）
    /// 时**退回按点位连线**，而不是报错 —— 这是它与 `Road` 的关键区别。
    /// 适合「点位接口只给了 3 个点，但策略又要求 5 个必经点」这类必然对不上的场景。
    Manual,
    /// 瓦片矢量化路网（实验）：用百度栅格瓦片自己识别道路，见 `tile_road`。
    ///
    /// 与 `Road` 的区别只在**路网从哪来**：Road 读 OSM/人工导入，Tiles 读瓦片缓存；
    /// 后续路由（`build_road`）完全复用，所以这是「第 4 种取图方式」而不是新算法。
    Tiles,
}

impl RouteMode {
    pub fn from_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "road" => RouteMode::Road,
            "manual" | "custom" => RouteMode::Manual,
            "tiles" | "tile" | "瓦片" => RouteMode::Tiles,
            _ => RouteMode::Legacy,
        }
    }

    /// 配置持久化用的稳定字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            RouteMode::Legacy => "legacy",
            RouteMode::Road => "road",
            RouteMode::Manual => "manual",
            RouteMode::Tiles => "tiles",
        }
    }

    /// 是否**必须**拿到路网才能工作。`Manual` 不必须（拿不到就退回连线）。
    ///
    /// `Tiles` 必须：没有瓦片路网就没有「沿道路走」这回事，拿不到就该明确报错，
    /// 而不是悄悄退回连线（那样用户以为在走瓦片路网，实际是直线）。
    pub fn requires_network(self) -> bool {
        matches!(self, RouteMode::Road | RouteMode::Tiles)
    }

    /// 是否需要拉取学校点位/围栏/路网。经典模式不触发这些端点。
    ///
    /// `Tiles` 要围栏：它用围栏定抓图范围（没有围栏时回落「锚点 + 半径」）。
    pub fn needs_school_route(self) -> bool {
        matches!(self, RouteMode::Road | RouteMode::Manual | RouteMode::Tiles)
    }
}

/// 从本地文件加载 OSM 路网。
pub fn load_network_path(path: &str) -> Result<RoadGraph, String> {
    if path.trim().is_empty() {
        return Err("未配置 OSM 路网，请先导入路网或主动切换经典模式".into());
    }
    // 手机上的路网是从系统文件选择器挑的，可能是任意大小；先按体积挡住，
    // 否则一个几百 MB 的 .osm 会在解析阶段把内存吃光（手机上就是闪退）。
    // 上限直接引用下载路径的常量：两条入口给不同额度，只会让用户遇到
    // 「自动下载能跑、手动导入却闪退」这种自相矛盾的失败（文案也必须跟着它走，
    // 否则用户按提示把文件裁到 24–64 MiB 仍会被拒）。
    const MAX_OSM_BYTES: u64 = crate::api::school_roads::MAX_BYTES;
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > MAX_OSM_BYTES {
            return Err(format!(
                "路网文件过大（{:.1} MiB，上限 {} MiB）；请导出更小的范围",
                meta.len() as f64 / (1024.0 * 1024.0),
                MAX_OSM_BYTES / (1024 * 1024)
            ));
        }
    }
    let bytes = std::fs::read(path).map_err(|e| format!("读取 {path} 失败: {e}"))?;
    let xml = std::str::from_utf8(&bytes)
        .map_err(|_| "文件不是 OSM XML；PBF 等二进制格式请先转换为 .osm XML")?;
    if !xml.contains("<osm") || !xml.contains("</osm>") || xml.contains("<remark") {
        return Err("文件不是完整的 OSM XML 或包含地图服务错误；请重新导出 .osm/.xml".into());
    }
    let net = route_planner::load_osm(&bytes)?;
    if net.graph.edge_count() == 0
        || net.graph.node_weights().any(|n| {
            !n.coord.lat.is_finite()
                || !n.coord.lon.is_finite()
                || n.coord.lat.abs() > 90.
                || n.coord.lon.abs() > 180.
        })
    {
        return Err("路网没有可用道路或包含无效坐标，请检查 OSM 文件".into());
    }
    Ok(net)
}

/// 路网预览计划（供地图显示）：对齐后的路网/建筑 + 生成的路线（BD 系）。
pub struct RoadPlan {
    pub arcs: Vec<f64>,
    pub laps: Vec<f64>,
    pub route: Vec<(f64, f64)>,
    pub length_m: f64,
    pub edges: Vec<Vec<(f64, f64)>>,
    pub buildings: Vec<Vec<(f64, f64)>>,
    pub checkpoints: Vec<(f64, f64)>,
    pub fences: Vec<Vec<(f64, f64)>>,
}

/// 用围栏（BD 系，与打卡点同系）裁剪路网：仅保留围栏内道路；空路网交由路由器明确报错。
pub fn apply_fences(net: &RoadGraph, fences: &[Vec<(f64, f64)>]) -> RoadGraph {
    if fences.is_empty() {
        return net.clone();
    }
    let mut g = net.clone();
    let polys: Vec<Vec<Coord>> = fences
        .iter()
        .map(|r| r.iter().map(|p| Coord::new(p.1, p.0)).collect())
        .collect();
    g.boundaries = polys.clone();
    g.graph
        .retain_nodes(|graph, n| route_planner::boundary::contains(&polys, graph[n].coord));
    // 进一步裁剪：移除「穿越围栏边界」的边（端点在内但中段跑出围栏，如跨校区道路）。
    g.graph.retain_edges(|graph, e| {
        let (a, b) = graph.edge_endpoints(e).unwrap();
        edge_inside(&polys, graph[a].coord, graph[b].coord)
    });
    g.rebuild_index();
    g
}

fn edge_inside(polys: &[Vec<Coord>], a: Coord, b: Coord) -> bool {
    route_planner::boundary::segment_inside(polys, a, b)
}

/// 生成路网预览（对齐 + 围栏裁剪 + 闭环路由），不产生协议字段。
///
/// 调用方提供合并后的 policy/实时点位，预览与生成使用同一个路由函数。
pub fn plan_road_view(
    net: &RoadGraph,
    points_bd: &[(f64, f64)],
    dist: f64,
    dur: i64,
    seed: u64,
    fences: &[Vec<(f64, f64)>],
) -> Result<RoadPlan, String> {
    if !fences.is_empty() {
        crate::api::school_roads::validate_fences(fences)?;
    }
    let mut g = net.clone();
    align_network(&mut g);
    g = apply_fences(&g, fences);
    let ordered = points_bd.to_vec();
    let route = required_route(&g, &ordered, dist, dur, seed)?;
    let route_pts: Vec<(f64, f64)> = route.points.iter().map(|p| (p.lat, p.lon)).collect();
    let edges: Vec<Vec<(f64, f64)>> = g
        .edges_deg()
        .iter()
        .map(|e| e.iter().map(|c| (c.lat, c.lon)).collect())
        .collect();
    let buildings: Vec<Vec<(f64, f64)>> = g
        .buildings
        .iter()
        .map(|r| r.iter().map(|c| (c.lat, c.lon)).collect())
        .collect();
    Ok(RoadPlan {
        arcs: route.points.iter().map(|p| p.s).collect(),
        laps: super::route_view::laps(&route, dist),
        route: route_pts,
        length_m: route.length_m,
        edges,
        buildings,
        checkpoints: ordered,
        fences: fences.to_vec(),
    })
}

/// 将 WGS84 路网逐节点转换为 BD-09（与打卡点同系）。
///
/// OSM 为 WGS84，打卡点为 BD-09。两者不是常数平移关系（境内 GCJ/BD 偏移随位置
/// 变化），故用标准 WGS84→GCJ-02→BD-09 变换逐节点转换，校园尺度下与真实道路
/// 对齐（优于旧版质心常数平移的米级~数十米残差）。
pub fn align_network(net: &mut RoadGraph) {
    // 幂等护栏：WGS84→BD-09 不是常数平移，对已对齐的图再跑一次会**二次偏移数百米
    // 而且不报错**（轨迹整体挪到隔壁街区，看起来仍然"像一条路"）。
    // 现在所有调用点都只调一次；这个门是给将来新增调用方的，`apply_fences` 走 clone
    // 会带着标记一起复制，所以裁剪后不会丢掉"已对齐"的事实。
    if net.align_applied {
        return;
    }
    if net.graph.node_count() == 0 {
        // 空图不标记：等真正有节点时再对齐，否则会永久跳过。
        return;
    }
    for node in net.graph.node_weights_mut() {
        let (lat, lon) = wgs84_to_bd09(node.coord.lat, node.coord.lon);
        node.coord.lat = lat;
        node.coord.lon = lon;
    }
    for ring in net.buildings.iter_mut() {
        for c in ring.iter_mut() {
            let (lat, lon) = wgs84_to_bd09(c.lat, c.lon);
            c.lat = lat;
            c.lon = lon;
        }
    }
    net.align_applied = true;
    net.compute_anchor();
    net.rebuild_index();
}

/// 按质心角度排序打卡点，形成自然环序。
pub fn radial_order(points_bd: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let n = points_bd.len();
    if n == 0 {
        return Vec::new();
    }
    let (cx, cy) = (
        points_bd.iter().map(|q| q.0).sum::<f64>() / n as f64,
        points_bd.iter().map(|q| q.1).sum::<f64>() / n as f64,
    );
    let mut ordered = points_bd.to_vec();
    ordered.sort_by(|a, b| {
        (a.0 - cx)
            .atan2(a.1 - cy)
            .partial_cmp(&(b.0 - cx).atan2(b.1 - cy))
            .unwrap()
    });
    ordered
}

/// 路线 → 平面折线 + 弧长表。
///
/// `arcs[k]` 必须与 `dense[k]` 一一对应（`ring_point_at` 按同下标取弧长），
/// 因此直接取每个采样点的累计弧长 `p.s`（`dense` 末点已闭合回起点）。
fn route_ring(route: &Route, c_lat: f64, c_lng: f64) -> (Vec<(f64, f64)>, Vec<f64>) {
    let dense: Vec<(f64, f64)> = route
        .points
        .iter()
        .map(|p| {
            (
                (p.lon - c_lng) * MET_PER_DEG_LNG,
                (p.lat - c_lat) * MET_PER_DEG_LAT,
            )
        })
        .collect();
    let arcs: Vec<f64> = route.points.iter().map(|p| p.s).collect();
    (dense, arcs)
}

pub fn effective_duration(dist: f64, dur: i64) -> i64 {
    let speed = (dist / dur.max(1) as f64).clamp(SPEED_FLOOR + 0.1, SPEED_CEIL - 0.1);
    (dist / speed).round().max(1.0) as i64
}

fn required_route(
    net: &RoadGraph,
    points: &[(f64, f64)],
    dist: f64,
    dur: i64,
    seed: u64,
) -> Result<Route, String> {
    if !dist.is_finite() || dist <= 0.0 || dur <= 0 {
        return Err("距离和时长必须为正数".into());
    }
    let avg = dist / effective_duration(dist, dur) as f64;
    let opts = RouteOptions {
        a_c_max: 2.5,
        min_radius_m: (avg * avg / 2.5).max(6.0),
        repeat_checkpoint_loop: true,
        ..Default::default()
    };
    let waypoints: Vec<Coord> = points.iter().map(|p| Coord::new(p.1, p.0)).collect();
    if !net.boundaries.is_empty()
        && waypoints
            .iter()
            .any(|p| !route_planner::boundary::contains(&net.boundaries, *p))
    {
        return Err("点位在学校围栏外，无法生成校内路线".into());
    }
    let check = route_planner::check::inspect(net, &waypoints, dist);
    if let Some(error) = check.error() {
        return Err(error);
    }
    let route = plan_route_split(net, &waypoints, &waypoints, dist, seed, &opts)?;
    if route.points.len() < 2 || !route.length_m.is_finite() || route.length_m <= 0.0 {
        return Err("路网无法形成有效闭环".into());
    }
    for (i, &(lat, lon)) in points.iter().enumerate() {
        let nearest = route
            .points
            .iter()
            .map(|p| {
                let dy = (p.lat - lat) * MET_PER_DEG_LAT;
                let dx = (p.lon - lon) * 111_320.0 * lat.to_radians().cos();
                dx.hypot(dy)
            })
            .fold(f64::INFINITY, f64::min);
        if nearest > 40.0 {
            return Err(format!(
                "路网无法到达第 {} 个点位（最近 {:.1} 米，超过 40 米）",
                i + 1,
                nearest
            ));
        }
    }
    Ok(route)
}

/// 模式 B：真实道路拓扑路由轨迹生成。
///
/// `route_bd[0]` 为起点；`route_bd` 和 `must_bd` 合并为强制必经点。
/// 不使用离路吸附；平滑路线和最终采样均须经过全部点位的 40 米范围。
///
/// GPS 抖动幅度沿用旧行为（每个 seed 在 2.8–4.2 m 之间自动抽样）。
/// 需要指定幅度（配置项 `gps_spread_m`）用 [`build_road_tuned`]。
///
/// 保留旧签名是有意的：调用方（测试、以及只想用默认幅度的入口）不必关心
/// 新参数；非测试构建里只有 `build_road_tuned` 被主流程调用，所以这里
/// 显式允许 dead_code，而不是为了消警告去改一堆调用点。
#[allow(dead_code)]
pub fn build_road(
    dist: f64,
    dur: i64,
    seed: u64,
    start_ms: i64,
    route_bd: &[(f64, f64)],
    must_bd: &[(f64, f64)],
    net: &RoadGraph,
) -> Result<Track, String> {
    build_road_tuned(dist, dur, seed, start_ms, route_bd, must_bd, net, 0.0)
}

/// 同 [`build_road`]，但可指定 GPS 抖动幅度。
///
/// `gps_spread_m` 是**单点稳态标准差**（米）：
/// - `0.0` 沿用旧行为：每个 seed 在 2.8–4.2 m 之间自动抽样（与改动前逐点一致）；
/// - `>0` 用指定值（钳在 0–20 m）替代自动抽样。
///
/// 代价要写明：非 0 时是**同一幅度贯穿所有计划**（不再每 seed 抽样），
/// 对「所有人幅度都一样」这件事没有缓解作用 —— 这一项是给「显式要求更小/更大
/// 抖动」的人用的，不是隐私增强开关（配置默认值因此是 0）。
///
/// 实现上**无论是否指定都会先抽一次自动值**，所以 RNG 序列完全不变：
/// 改幅度只影响每个点的经纬度，点位类型/时间轴/海拔/步频步幅一个都不变。
#[allow(clippy::too_many_arguments)]
pub fn build_road_tuned(
    dist: f64,
    dur: i64,
    seed: u64,
    start_ms: i64,
    route_bd: &[(f64, f64)],
    must_bd: &[(f64, f64)],
    net: &RoadGraph,
    gps_spread_m: f64,
) -> Result<Track, String> {
    if dur <= 0 || !dist.is_finite() || dist <= 0.0 {
        return Err("距离和时长必须为正数".into());
    }
    let mut rng = Rng::new(seed);
    let dur_f = dur as f64;
    let avg_v = dist / dur_f;
    let a_c_max = 2.5;
    let mut required = route_bd.to_vec();
    for p in must_bd {
        if !required.contains(p) {
            required.push(*p);
        }
    }
    let route = required_route(net, &required, dist, dur, seed)?;

    let n_pts = route_bd.len().max(1);
    let (c_lat, c_lng) = (
        route_bd.iter().map(|q| q.0).sum::<f64>() / n_pts as f64,
        route_bd.iter().map(|q| q.1).sum::<f64>() / n_pts as f64,
    );
    let (dense, arcs) = route_ring(&route, c_lat, c_lng);

    // 时间网格（同经典模式）
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
    let mut dts: Vec<f64> = (0..n - 1).map(|i| times[i + 1] - times[i]).collect();
    dts.push(1.0f64.max(dur_f - times[n - 1]));

    // 运动学速度剖面：OU 配速 + 弯道预判刹车 + 精确命中目标距离
    let kin = KinParams {
        a_c_max,
        ..Default::default()
    };
    let speed_seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let w = pace_profile(
        &route,
        avg_v,
        &dts,
        dist,
        speed_seed,
        &kin,
        SPEED_FLOOR,
        SPEED_CEIL,
    );
    let speeds: Vec<f64> = w.clone();
    let seg_dist: Vec<f64> = (0..n).map(|i| w[i] * dts[i]).collect();

    // 点位类型（同经典模式）
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

    // 建筑 SDF（自适应放大漂移）
    let sdf = Sdf::from_buildings(
        &net.buildings,
        |c| {
            [
                (c.lon - c_lng) * MET_PER_DEG_LNG,
                (c.lat - c_lat) * MET_PER_DEG_LAT,
            ]
        },
        2.0,
        30.0,
    );

    // 生物力学：步频在目标值附近 OU 波动，步幅反解严格满足 v=(cad/60)·stride
    let targets: Vec<f64> = (0..n)
        .map(|i| {
            let km_done = (times[i] / dur_f) * dist / 1000.0;
            gait(speeds[i], fatigue_from_km(km_done)).cadence
        })
        .collect();
    let cadence_seed = seed.wrapping_add(0xC6A4_A793_5BD1_E995);
    let cadences = ou_cadence_series(&targets, &dts, cadence_seed, 0.25, 0.5, 110.0, 220.0);

    // GPS 抖动：AR(1) 相关漂移 + 高斯测量噪声（SDF 逐点放大）。
    //
    // 参数**每个 seed 独立随机**：固定 α/σ 会让所有用户、所有圈次的漂移形态完全一致，
    // 等于给每个人发同一枚指纹（同一套抖动序列是可聚类的）。单点稳态标准差按手机实测
    // 落在 2.8–4.2 m；旧值 0.72/0.75 的稳态 std 仅 ≈1.08 m，轨迹贴线贴得像尺子画的。
    // 稳态 std = σ_step/√(1−α²)，所以这里由目标稳态值反解 σ_step。
    let jitter_alpha = rng.uniform(0.86, 0.95);
    // 先无条件抽一次自动值：即使调用方指定了幅度，RNG 消耗也与旧版完全一致，
    // 于是「只改幅度」不会连带改掉点位类型/时间轴/海拔等其它抽样结果。
    let jitter_auto = rng.uniform(2.8, 4.2);
    let jitter_steady = if gps_spread_m.is_finite() && gps_spread_m > 0.0 {
        gps_spread_m.clamp(0.0, crate::api::model::GPS_SPREAD_MAX_M)
    } else {
        jitter_auto
    };
    let jitter_sigma = jitter_steady * (1.0 - jitter_alpha * jitter_alpha).sqrt();
    let mut jitter = GpsJitter::new(jitter_alpha, jitter_sigma, rng.uniform(0.25, 0.65));

    let mut locs: Vec<GenPoint> = Vec::with_capacity(n);
    let mut positions = Vec::with_capacity(n);
    let mut s = 0.0f64;
    let mut t_acc = 0.0f64;
    let mut dist_acc = 0.0f64;
    let mut steps_acc = 0.0f64;
    // A5（审计 medium）：Road 模式原先写死 `82.0`（初值与 OU 回归目标都是它）。
    // Legacy 早已改成按区间抽样（`generator.rs`：`base_alt = rng.uniform(20.0, 260.0)`），
    // 只有 Road 漏改 —— 于是选「真实道路」的所有账号、所有记录 bdA 全落在 82±几米，
    // 一个校区的人不可能同一海拔。这里与 Legacy 同款：每次运行抽一个基准，
    // 整条轨迹围绕它做 OU 回归（连续、不逐点跳）。只影响 bdA / totalAscent。
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
            s += d_step;
            let (bx, by) = pos(s);
            x = bx;
            y = by;
            // AR(1) 相关漂移 + 高斯测量噪声，SDF 自适应放大
            let sc = sdf.sigma_scale([bx, by]);
            let z = [
                rng.gauss(0.0, 1.0),
                rng.gauss(0.0, 1.0),
                rng.gauss(0.0, 1.0),
                rng.gauss(0.0, 1.0),
            ];
            let (jxd, jyd) = jitter.step(sc, z);
            px = bx + jxd;
            py = by + jyd;
            // accuracy 按对数正态取：真机上报值多数在 4–8 m，长尾到 15 m 以上。
            // 旧实现是均匀 1.4–2.4 m —— 每条记录、每个用户都困在这个窄区间，
            // 本身就是可聚类特征（真实设备不会这么整齐）。
            let acc_z = rng.gauss(0.0, 1.0);
            let acc_median = if typ == 3 { 11.0 } else { 7.0 };
            rad = round_to((acc_median * (0.42 * acc_z).exp()).clamp(3.0, 15.0), 2);
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
                py = (last.gLat - c_lat) * MET_PER_DEG_LAT;
                px = (last.gLng - c_lng) * MET_PER_DEG_LNG;
            } else {
                let (jx, jy) = jitter.state();
                px = x + jx;
                py = y + jy;
            }
            // 异常点保留原有上限（radius ≤ 10 m 的契约由 `track::tests` 钉住），
            // 只把分布从均匀换成对数正态。
            let acc_z = rng.gauss(0.0, 1.0);
            rad = round_to((6.0 * (0.42 * acc_z).exp()).clamp(3.0, 10.0), 2);
            state = rng.weighted(&[(1, 102), (2, 124), (3, 136)]);
        }
        if typ != -1 {
            dist_acc += d_step;
        }
        positions.push(s);
        // A6（审计 medium）：有围栏时原先是「整条丢掉 AR(1) 抖动、直接贴路网中心线」。
        // 真机 GPS 永远有 3–5m 误差，整条轨迹精确落在中心线上比噪声太大更容易被识别
        // （注释说 boundary mode 跟「实际道路」走，但中心线不是实际道路）。
        //
        // 现在保留抖动，但逐个夹进围栏：候选点必须自己落在围栏内，**而且**它到上一个
        // 已发点的线段也在围栏内（凹多边形下两点在内、连线仍可能出界）；不满足就把偏移
        // 折半再试，实在不行回退到中心线点 —— 宁可贴中心线，也不出界。
        let (lat, lng) = if net.boundaries.is_empty() {
            to_bd(px, py, c_lat, c_lng)
        } else {
            let prev = locs.last().map(|p| Coord::new(p.gLng, p.gLat));
            let acceptable = |cand: (f64, f64)| {
                let c = Coord::new(cand.1, cand.0);
                route_planner::boundary::contains(&net.boundaries, c)
                    && prev.map_or(true, |p| {
                        route_planner::boundary::segment_inside(&net.boundaries, p, c)
                    })
            };
            let mut chosen = to_bd(x, y, c_lat, c_lng);
            for shrink in [1.0_f64, 0.5, 0.25] {
                let cand = to_bd(x + (px - x) * shrink, y + (py - y) * shrink, c_lat, c_lng);
                if acceptable(cand) {
                    chosen = cand;
                    break;
                }
            }
            chosen
        };
        alt += 0.04 * (base_alt - alt) + rng.gauss(0.0, alt_sigma);
        // 生物力学联动：v=(cad/60)·stride
        let v_now = speeds[i];
        let mut stride = v_now * 60.0 / cadences[i];
        if stride > 1.7 {
            stride = 1.7;
        } else if stride < 0.5 {
            stride = 0.5;
        }
        let cad = v_now * 60.0 / stride;
        steps_acc += cad / 60.0 * dt;
        let nxt = pos(s + 2.0);
        let brg =
            ((nxt.0 - x).atan2(nxt.1 - y).to_degrees() + rng.gauss(0.0, 35.0)).rem_euclid(360.0);
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
            gLat: if net.boundaries.is_empty() {
                round_to(lat, 7)
            } else {
                lat
            },
            gLng: if net.boundaries.is_empty() {
                round_to(lng, 7)
            } else {
                lng
            },
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
    if !net.boundaries.is_empty() {
        locs = super::school_track::preserve_corners(
            locs,
            &positions,
            &route,
            &net.boundaries,
            start_ms,
        )?;
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
    track.validate_consistency()?;
    for point in route_bd.iter().chain(must_bd.iter()) {
        if !super::wire::passes_point(&track, *point) {
            return Err("最终道路轨迹未经过全部点位，请调整距离或路网".into());
        }
    }
    Ok(track)
}

#[cfg(test)]
mod preview_tests {
    use super::*;

    #[test]
    fn route_mode_strings_and_capabilities_round_trip() {
        assert_eq!(RouteMode::from_str("road"), RouteMode::Road);
        assert_eq!(RouteMode::from_str("manual"), RouteMode::Manual);
        // 兼容参考实现的别名
        assert_eq!(RouteMode::from_str("custom"), RouteMode::Manual);
        // 容忍大小写与前后空白（配置里手写的值）
        assert_eq!(RouteMode::from_str("  MANUAL "), RouteMode::Manual);
        assert_eq!(RouteMode::from_str("legacy"), RouteMode::Legacy);
        assert_eq!(RouteMode::from_str("随便写点什么"), RouteMode::Legacy);

        // 第 4 种模式：三个别名都认（配置/命令行手写都行）
        for alias in ["tiles", "tile", "瓦片", " TILES "] {
            assert_eq!(RouteMode::from_str(alias), RouteMode::Tiles, "{alias}");
        }

        for m in [
            RouteMode::Legacy,
            RouteMode::Road,
            RouteMode::Manual,
            RouteMode::Tiles,
        ] {
            assert_eq!(RouteMode::from_str(m.as_str()), m, "配置序列化必须可往返");
        }

        // 只有 Road / Tiles 必须有路网；Manual 拿不到会退回按点位连线
        assert!(RouteMode::Road.requires_network());
        assert!(RouteMode::Tiles.requires_network());
        assert!(!RouteMode::Manual.requires_network());
        assert!(!RouteMode::Legacy.requires_network());

        // Manual 仍然要拉学校点位/围栏（只是不追加策略必经点）
        assert!(RouteMode::Road.needs_school_route());
        assert!(RouteMode::Manual.needs_school_route());
        // Tiles 要围栏定抓图范围（没有围栏回落锚点 + 半径）
        assert!(RouteMode::Tiles.needs_school_route());
        assert_eq!(RouteMode::Tiles.as_str(), "tiles");
    }

    #[test]
    fn align_network_shifts_once_and_never_twice() {
        // WGS84→BD-09 在境内会产生数百米偏移；重复应用会再偏一次且不报错，
        // 所以这里钉死「第一次生效、第二次必须无效」。
        let mut net = RoadGraph::new();
        let n = net.add_node(Coord::new(116.4, 39.9));
        net.compute_anchor();
        let before = net.node_coord(n);
        assert!(!net.align_applied);

        align_network(&mut net);
        let after = net.node_coord(n);
        assert!(net.align_applied);
        assert!(
            (after.lat - before.lat).abs() > 1e-4 || (after.lon - before.lon).abs() > 1e-4,
            "首次对齐应产生可见偏移"
        );

        align_network(&mut net);
        let again = net.node_coord(n);
        assert_eq!(
            (after.lat, after.lon),
            (again.lat, again.lon),
            "第二次 align_network 必须不生效"
        );
    }

    #[test]
    fn empty_network_is_not_marked_aligned() {
        // 空图不标记，否则以后加进节点也永远不会对齐。
        let mut net = RoadGraph::new();
        align_network(&mut net);
        assert!(!net.align_applied);
    }

    #[test]
    fn preview_and_generation_share_required_points_options_and_seed() {
        let mut net = RoadGraph::new();
        let wgs = [
            (39.9, 116.4),
            (39.9, 116.402),
            (39.902, 116.402),
            (39.902, 116.4),
        ];
        let nodes: Vec<_> = wgs
            .iter()
            .map(|&(lat, lon)| net.add_node(Coord::new(lon, lat)))
            .collect();
        for i in 0..4 {
            let edge = route_planner::graph::EdgeData {
                way_id: i as u64,
                oneway: false,
                highway: "footway".into(),
                maxspeed_kmh: None,
                length_m: 200.0,
            };
            net.add_edge(nodes[i], nodes[(i + 1) % 4], edge.clone());
            net.add_edge(nodes[(i + 1) % 4], nodes[i], edge);
        }
        net.compute_anchor();
        net.rebuild_index();
        let points: Vec<_> = wgs.iter().map(|&(a, b)| wgs84_to_bd09(a, b)).collect();
        let fences = vec![vec![
            (39.89, 116.39),
            (39.89, 116.42),
            (39.92, 116.42),
            (39.92, 116.39),
        ]
        .into_iter()
        .map(|(a, b)| wgs84_to_bd09(a, b))
        .collect()];
        let preview = plan_road_view(&net, &points, 2000.0, 800, 7, &fences).unwrap();
        let mut aligned = net.clone();
        align_network(&mut aligned);
        let aligned = apply_fences(&aligned, &fences);
        let route = required_route(&aligned, &points, 2000.0, 800, 7).unwrap();
        assert_eq!(preview.checkpoints, points);
        assert_eq!(
            preview.route,
            route
                .points
                .iter()
                .map(|p| (p.lat, p.lon))
                .collect::<Vec<_>>()
        );
        let track = build_road(2000.0, 800, 7, 0, &points, &points, &aligned).unwrap();
        assert!(points
            .iter()
            .all(|p| super::super::wire::passes_point(&track, *p)));
        for p in &track.locations {
            let nearest = preview
                .route
                .windows(2)
                .map(|w| {
                    let xy = |(lat, lon): (f64, f64)| {
                        [
                            (lon - p.gLng) * MET_PER_DEG_LNG,
                            (lat - p.gLat) * MET_PER_DEG_LAT,
                        ]
                    };
                    let (q, _) =
                        route_planner::graph::project_to_segment([0.0, 0.0], xy(w[0]), xy(w[1]));
                    q[0].hypot(q[1])
                })
                .fold(f64::INFINITY, f64::min);
            assert!(nearest < 40.0, "preview/generation mismatch: {nearest}");
        }
    }

    // A concave campus with a road at its re-entrant corner: connecting timed
    // observations without preserving this vertex would leave campus.
    fn concave_campus() -> (RoadGraph, Vec<Vec<(f64, f64)>>, Vec<(f64, f64)>) {
        let coord = |(x, y): (f64, f64)| (39.9 + y / 111132.0, 116.4 + x / 85400.0);
        let fence = vec![
            (0., 0.),
            (240., 0.),
            (240., 100.),
            (100., 100.),
            (100., 240.),
            (0., 240.),
        ]
        .into_iter()
        .map(coord)
        .collect();
        let path: Vec<_> = vec![
            (10., 10.),
            (230., 10.),
            (230., 90.),
            (90., 90.),
            (90., 230.),
            (10., 230.),
        ]
        .into_iter()
        .map(coord)
        .collect();
        let mut net = RoadGraph::new();
        let nodes: Vec<_> = path
            .iter()
            .map(|&(lat, lon)| net.add_node(Coord::new(lon, lat)))
            .collect();
        net.compute_anchor();
        for i in 0..nodes.len() {
            let j = (i + 1) % nodes.len();
            let a = net.to_m(net.node_coord(nodes[i]));
            let b = net.to_m(net.node_coord(nodes[j]));
            let edge = route_planner::graph::EdgeData {
                way_id: i as u64,
                oneway: false,
                highway: "footway".into(),
                maxspeed_kmh: None,
                length_m: (a[0] - b[0]).hypot(a[1] - b[1]),
            };
            net.add_edge(nodes[i], nodes[j], edge.clone());
            net.add_edge(nodes[j], nodes[i], edge);
        }
        net.rebuild_index();
        (net, vec![fence], path)
    }

    #[test]
    fn long_distance_repeats_inside_concave_campus_and_keeps_statistics() {
        let (net, fences, points) = concave_campus();
        let net = apply_fences(&net, &fences);
        for seed in 0..16 {
            // Campus perimeter is 960 m; 5 km must reuse school roads.
            let track = build_road(5000.0, 2000, seed, 123000, &points, &points, &net).unwrap();
            assert!((track.totalDistance - 5000.0).abs() < 0.001);
            assert_eq!(track.totalTime, 2000);
            track.validate_consistency().unwrap();
            assert!(track
                .locations
                .windows(2)
                .all(|w| route_planner::boundary::segment_inside(
                    &net.boundaries,
                    Coord::new(w[0].gLng, w[0].gLat),
                    Coord::new(w[1].gLng, w[1].gLat)
                )));
            assert!(points
                .iter()
                .all(|p| super::super::wire::passes_point(&track, *p)));
            assert_eq!(track.locations.last().unwrap().steps, track.totalSteps);
        }
    }

    /// A5：Road 模式的海拔基准必须每次抽样，不能是写死的 82m。
    ///
    /// 判据三条：① 两条不同 seed 的轨迹海拔（顶点）不同；② 都落在 20–260 区间里；
    /// ③ 同一条轨迹里相邻点不跳（OU 回归的连续性），别把「换了基准」变成「逐点乱跳」。
    #[test]
    fn road_altitude_is_sampled_per_run_not_pinned_to_82m() {
        let (net, fences, points) = concave_campus();
        let net = apply_fences(&net, &fences);
        let mut peaks = Vec::new();
        for seed in [3_u64, 11] {
            let track = build_road(2000.0, 900, seed, 123_000, &points, &points, &net).unwrap();
            let alts: Vec<f64> = track.locations.iter().map(|p| p.bdA).collect();
            assert!(!alts.is_empty());
            let peak = alts.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            assert!(
                (20.0..=260.0).contains(&peak),
                "seed={seed} 顶点海拔 {peak:.1} 越界（应在 20–260m）"
            );
            // OU 回归：相邻点不会跳（σ 已被点数摊薄，单步应远小于整个区间）。
            for pair in alts.windows(2) {
                assert!(
                    (pair[1] - pair[0]).abs() < 30.0,
                    "seed={seed} 相邻点海拔跳变过大：{:.1} -> {:.1}",
                    pair[0],
                    pair[1]
                );
            }
            peaks.push(peak);
        }
        assert!(
            (peaks[0] - peaks[1]).abs() > 0.01,
            "两条不同 seed 的 Road 轨迹海拔基准相同（{:?}），基准仍是常量",
            peaks
        );
    }

    /// A6：有围栏时不能再把 GPS 抖动整条丢掉（旧实现贴死中心线）。
    ///
    /// 用一条**笔直走廊**当跑道：中心线只有经度变化、纬度恒定。旧实现（丢弃抖动）
    /// 会让所有点的纬度几乎完全一致；保留抖动则纬度有 1–5m 量级的横向散布 —— 这个
    /// 判据不依赖任何内部结构，且与「宁可贴中心线也不出界」的约束同时成立。
    #[test]
    fn fenced_road_keeps_lateral_gps_jitter() {
        let mut net = RoadGraph::new();
        let a = net.add_node(Coord::new(116.400, 39.900));
        let b = net.add_node(Coord::new(116.410, 39.900));
        let edge = route_planner::graph::EdgeData {
            way_id: 1,
            oneway: false,
            highway: "footway".into(),
            maxspeed_kmh: None,
            length_m: 850.,
        };
        net.add_edge(a, b, edge.clone());
        net.add_edge(b, a, edge);
        net.rebuild_index();
        let fence = [
            (39.8995, 116.3995),
            (39.8995, 116.4105),
            (39.9005, 116.4105),
            (39.9005, 116.3995),
        ];
        let polys: Vec<Vec<Coord>> = vec![fence
            .iter()
            .map(|&(lat, lon)| Coord::new(lon, lat))
            .collect()];
        let mut fenced = net.clone();
        fenced.boundaries = polys.clone();
        let points = vec![(39.900, 116.400), (39.900, 116.410)];
        let track = build_road(3000.0, 1200, 9, 123_000, &points, &points, &fenced).unwrap();

        let lats: Vec<f64> = track.locations.iter().map(|p| p.gLat).collect();
        let mean = lats.iter().sum::<f64>() / lats.len() as f64;
        let variance = lats.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / lats.len() as f64;
        let lateral_m = variance.sqrt() * 111_320.0;
        assert!(
            lateral_m > 0.3,
            "有围栏时横向抖动被丢弃：纬度标准差只有 {lateral_m:.2} m（几乎贴中心线）"
        );
        assert!(lateral_m < 15.0, "横向抖动过大：{lateral_m:.1} m");
        for point in &track.locations {
            assert!(
                route_planner::boundary::contains(&polys, Coord::new(point.gLng, point.gLat)),
                "抖动把采样点甩出围栏"
            );
        }
    }

    #[test]
    fn no_boundary_extra_mileage_reuses_the_same_checkpoint_circuit() {
        let (mut net, _, points) = concave_campus();
        // A long, returnable side road must not be explored to fill the mileage.
        let start = net.graph.node_indices().next().unwrap();
        let far = net.add_node(Coord::new(116.42, 39.9));
        let edge = route_planner::graph::EdgeData {
            way_id: 99,
            oneway: false,
            highway: "footway".into(),
            maxspeed_kmh: None,
            length_m: 1700.,
        };
        net.add_edge(start, far, edge.clone());
        net.add_edge(far, start, edge);
        net.rebuild_index();
        assert!(net.boundaries.is_empty());
        let short = required_route(&net, &points, 2000., 800, 7).unwrap();
        let long = required_route(&net, &points, 5000., 2000, 19).unwrap();
        assert_eq!(short.length_m, long.length_m);
        assert!(long.length_m < 1100.);
        assert_eq!(
            short
                .points
                .iter()
                .map(|p| (p.lat, p.lon, p.s))
                .collect::<Vec<_>>(),
            long.points
                .iter()
                .map(|p| (p.lat, p.lon, p.s))
                .collect::<Vec<_>>()
        );
        let laps = super::super::route_view::laps(&long, 5000.);
        assert!(laps.len() >= 5);
        assert!((laps.iter().sum::<f64>() - 5000.).abs() < 0.001);
        for seed in 0..4 {
            let track = build_road(5000., 2000, seed, 123000, &points, &points, &net).unwrap();
            track.validate_consistency().unwrap();
            assert!((track.totalDistance - 5000.).abs() < 0.001);
            assert_eq!(track.totalTime, 2000);
            assert!(track.locations.iter().all(|p| p.gLng < 116.405));
            assert!(points
                .iter()
                .all(|p| super::super::wire::passes_point(&track, *p)));
        }
    }

    #[test]
    fn supplied_boundary_still_rejects_outside_points() {
        let (net, fences, mut points) = concave_campus();
        let clipped = apply_fences(&net, &fences);
        points.push((40.0, 117.0));
        assert!(build_road(5000., 2000, 1, 0, &points, &points, &clipped).is_err());
    }

    #[test]
    fn preflight_reports_short_distance_and_marks_offroad_point() {
        let (net, fences, points) = concave_campus();
        let net = apply_fences(&net, &fences);
        let cp: Vec<_> = points.iter().map(|&(a, b)| Coord::new(b, a)).collect();
        let check = route_planner::check::inspect(&net, &cp, 100.);
        assert_eq!(check.components, 1);
        assert!(check.minimum_m.unwrap() > 800.);
        assert!(check.error().unwrap().contains("不足"));
        let bad = route_planner::check::inspect(&net, &[Coord::new(117., 40.)], 5000.);
        assert_eq!(bad.issues[0].point, Some(0));
        assert!(bad.issues[0].position.is_some());
    }

    // ---------------------------------------------------------------
    // GPS 抖动幅度（配置 gps_spread_m）：0 = 与改动前逐点一致
    // ---------------------------------------------------------------

    /// 改动前（HEAD）的 `build_road(2000, 800, seed=7)` 逐点坐标哈希。
    /// 用 `git archive HEAD` 的干净副本跑同一段代码得到，不是改完之后自己算的。
    const PRE_CHANGE_COORD_HASH: u64 = 0x0636_2cc3_b04e_c766;

    /// 固定几何：4 节点方形路网（WGS84）+ 其 BD-09 角点。
    fn snapshot_fixture() -> (RoadGraph, Vec<(f64, f64)>) {
        let mut net = RoadGraph::new();
        let wgs = [
            (39.9, 116.4),
            (39.9, 116.402),
            (39.902, 116.402),
            (39.902, 116.4),
        ];
        let nodes: Vec<_> = wgs
            .iter()
            .map(|&(lat, lon)| net.add_node(Coord::new(lon, lat)))
            .collect();
        for i in 0..4 {
            let edge = route_planner::graph::EdgeData {
                way_id: i as u64,
                oneway: false,
                highway: "footway".into(),
                maxspeed_kmh: None,
                length_m: 200.0,
            };
            net.add_edge(nodes[i], nodes[(i + 1) % 4], edge.clone());
            net.add_edge(nodes[(i + 1) % 4], nodes[i], edge);
        }
        net.compute_anchor();
        net.rebuild_index();
        align_network(&mut net);
        let points = wgs.iter().map(|&(a, b)| wgs84_to_bd09(a, b)).collect();
        (net, points)
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
    fn gps_spread_zero_reproduces_the_pre_change_track_point_by_point() {
        let (net, points) = snapshot_fixture();
        // 旧入口（默认幅度 = 自动区间）与新入口显式传 0 必须都等于改动前的轨迹。
        let old_api = build_road(2000.0, 800, 7, 0, &points, &points, &net).unwrap();
        let zero = build_road_tuned(2000.0, 800, 7, 0, &points, &points, &net, 0.0).unwrap();
        // 负值/NaN 也按「关闭」处理，落到同一条旧路径。
        let nan = build_road_tuned(2000.0, 800, 7, 0, &points, &points, &net, f64::NAN).unwrap();

        for (name, track) in [("old_api", &old_api), ("zero", &zero), ("nan", &nan)] {
            assert_eq!(track.locations.len(), 164, "{name}: 点数变了");
            assert_eq!(track.totalSteps, 2352, "{name}: 步数变了");
            assert_eq!(track.totalDistance.to_bits(), 2000.0f64.to_bits());
            assert_eq!(
                coord_hash(track),
                PRE_CHANGE_COORD_HASH,
                "{name}: 坐标与改动前不一致（gps_spread_m=0 必须逐点相同）"
            );
        }
        // 可读证据：首点与改动前一致（不只是哈希相等）。
        assert!((zero.locations[0].gLat - 39.9077201).abs() < 1e-9);
        assert!((zero.locations[0].gLng - 116.4126209).abs() < 1e-9);
    }

    #[test]
    fn gps_spread_changes_only_coordinates_not_any_other_field() {
        let (net, points) = snapshot_fixture();
        let zero = build_road_tuned(2000.0, 800, 7, 0, &points, &points, &net, 0.0).unwrap();
        let wide = build_road_tuned(2000.0, 800, 7, 0, &points, &points, &net, 8.0).unwrap();
        assert_ne!(
            coord_hash(&zero),
            coord_hash(&wide),
            "指定幅度必须真的改变抖动"
        );
        assert_eq!(zero.locations.len(), wide.locations.len());
        assert_eq!(zero.totalSteps, wide.totalSteps);
        assert_eq!(zero.totalDistance, wide.totalDistance);
        assert_eq!(zero.totalTime, wide.totalTime);
        // 除了经纬度，每个点的报文取值都必须一致：幅度只作用于位置抖动。
        for (a, b) in zero.locations.iter().zip(&wide.locations) {
            assert_eq!(a.totalTime, b.totalTime);
            assert_eq!(a.ptype, b.ptype);
            assert_eq!(a.flag, b.flag);
            assert_eq!(a.steps, b.steps);
            assert_eq!(a.speed.to_bits(), b.speed.to_bits());
            assert_eq!(a.avgSpeed.to_bits(), b.avgSpeed.to_bits());
            assert_eq!(a.bdA.to_bits(), b.bdA.to_bits());
            assert_eq!(a.radius.to_bits(), b.radius.to_bits());
            assert_eq!(a.accuracy.to_bits(), b.accuracy.to_bits());
        }
        // 幅度确实更大：两条独立抖动序列之间的平均坐标差应在数米量级。
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
            "8 m 幅度与自动幅度的坐标差只有 {mean_delta:.2} m，幅度没生效"
        );
    }

    #[test]
    fn new_generation_knobs_never_enter_the_wire_payload() {
        // 举证：`gps_spread_m` / `anchor_offset_m` 只影响本地生成参数，
        // 轨迹序列化出来的键集与幅度取值无关，也不含这两个名字。
        let (net, points) = snapshot_fixture();
        let zero = build_road_tuned(2000.0, 800, 7, 0, &points, &points, &net, 0.0).unwrap();
        let wide = build_road_tuned(2000.0, 800, 7, 0, &points, &points, &net, 20.0).unwrap();
        let a = serde_json::to_value(&zero).unwrap();
        let b = serde_json::to_value(&wide).unwrap();
        let keys = |v: &serde_json::Value| -> Vec<String> {
            v.as_object()
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default()
        };
        assert_eq!(keys(&a), keys(&b), "幅度不该改变轨迹的报文形状");
        let text = a.to_string();
        for forbidden in ["gps_spread", "anchor_offset", "anchorOffset", "gpsSpread"] {
            assert!(
                !text.contains(forbidden),
                "轨迹报文里出现了本地参数 {forbidden}"
            );
        }
        let point_keys = a["locations"][0]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let point_keys_b = b["locations"][0]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(point_keys, point_keys_b);
    }

    #[test]
    fn required_points_in_disconnected_school_roads_fail() {
        let (mut net, fences, points) = concave_campus();
        net.graph.clear_edges();
        let nodes: Vec<_> = net.graph.node_indices().collect();
        for (a, b) in [(0, 1), (3, 4)] {
            let edge = route_planner::graph::EdgeData {
                way_id: a as u64,
                oneway: false,
                highway: "footway".into(),
                maxspeed_kmh: None,
                length_m: 200.0,
            };
            net.add_edge(nodes[a], nodes[b], edge.clone());
            net.add_edge(nodes[b], nodes[a], edge);
        }
        net.rebuild_index();
        let net = apply_fences(&net, &fences);
        assert!(build_road(5000., 2000, 1, 0, &[points[0], points[3]], &[], &net).is_err());
    }
}
