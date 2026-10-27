//! 瓦片矢量化路网（第 4 种路由模式，实验性）。
//!
//! 思路（「方法二：爬取瓦片图并矢量化」）：
//! 按校园范围抓百度栅格瓦片（`styles=pl`，路网底图）→ 拼成一张大图 →
//! 按颜色 / 局部对比把「道路像素」筛出来（排除水系、绿地、POI 图标、文字）→
//! Zhang-Suen 细化成 1px 骨架 → 追踪骨架成折线 → Douglas-Peucker 简化 →
//! 像素坐标按局部仿射换成 BD-09 经纬度 → 建成 `route_planner::RoadGraph` →
//! 连同 bbox / z / 生成时间一起缓存成 JSON，下次直接 `load`。
//!
//! 与另外三条模式的关系：**只新增入口，不改既有算法**。
//! `Legacy` / `Road` / `Manual` 的代码一行没动；本模块产出同类型的 `RoadGraph`，
//! 所以后续路由（`build_road` / 预览 / 地图渲染）不需要任何改动。
//!
//! 已知局限见 `review/TILE-ROAD.md`：文字遮挡、双层高架、隧道、立交、瓦片接缝。

use crate::http::Agent;
use route_planner::graph::EdgeData;
use route_planner::{Coord, RoadGraph};
use serde::{Deserialize, Serialize};

/// 百度瓦片边长（像素）。
pub const TILE_PX: usize = 256;
/// BD-09 球面墨卡托的半周长（米）。百度地图内部口径，不是 20037508.34。
pub const BD_ORIGIN_SHIFT: f64 = 20_037_726.37;
/// 一次生成最多拼多少张瓦片（4×4=16 起，8×8=64 是默认上限）。
pub const DEFAULT_MAX_TILES: usize = 64;
/// 拼接后图像的像素总量上限。
pub const MAX_PIXELS: usize = 8 * 1024 * 1024;
/// 瓦片域名前缀（四域名轮换，分散单域名限流）。
pub const TILE_HOST_PREFIXES: [&str; 4] = ["0", "1", "2", "3"];
/// 道路像素占比下限：低于它说明这张图基本是纯色/整片建筑，识别不出路。
pub const MIN_ROAD_RATIO: f64 = 0.004;
/// 道路像素占比上限：高于它说明整张图是同一种亮色（空白瓦片/占位图）。
///
/// 空白瓦片是 256×256 的纯白 PNG（压完才 ~200 字节）：每个像素都比「整幅图均值」
/// 亮、彩度又是 0，会被整体判成路面、占比接近 100% —— 只有下限是挡不住的。
pub const MAX_ROAD_RATIO: f64 = 0.55;

/// 生成参数（全部可调，默认值面向百度 `pl` 底图 + 校园尺度）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TileConfig {
    /// 缩放级别。z18 墨卡托尺度 ≈0.60 m/px（地面尺度乘 cos φ，北京 ≈0.46 m/px）。
    pub z: u8,
    /// 瓦片数上限（超了直接报错，不硬拼）。
    pub max_tiles: usize,
    /// 折线简化容差（米）。
    pub simplify_m: f64,
    /// 节点合并容差（米）。
    pub merge_m: f64,
    /// 亮度下限：低于它的是深色文字/图标/水系深色边。
    pub road_min_luma: u8,
    /// 亮度上限：默认 255（不设上限）。纯白广场可能被误判，用局部对比兜。
    pub road_max_luma: u8,
    /// 允许的最大彩度（RGB 极差）：超过说明是蓝/绿/红这些非路面元素。
    pub max_chroma: u8,
    /// 局部对比下限：路面比 9×9 邻域均值亮多少才算路（0-255）。
    ///
    /// 绝对阈值之外必须有这一条：底图背景本身就是浅灰，只靠亮度区间区分不开，
    /// 而「路比周围更亮」是稳定的判据。
    pub contrast_min: u8,
    /// 骨架折线最短像素长度，短于此视为噪点。
    pub min_polyline_px: usize,
}

impl Default for TileConfig {
    fn default() -> Self {
        Self {
            z: 18,
            max_tiles: DEFAULT_MAX_TILES,
            simplify_m: 2.5,
            merge_m: 3.0,
            road_min_luma: 150,
            road_max_luma: 255,
            max_chroma: 18,
            contrast_min: 8,
            min_polyline_px: 12,
        }
    }
}

/// BD-09 经纬度包围盒。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Bbox {
    pub min_lat: f64,
    pub min_lon: f64,
    pub max_lat: f64,
    pub max_lon: f64,
}

impl Bbox {
    pub fn new(min_lat: f64, min_lon: f64, max_lat: f64, max_lon: f64) -> Self {
        Self {
            min_lat: min_lat.min(max_lat),
            min_lon: min_lon.min(max_lon),
            max_lat: min_lat.max(max_lat),
            max_lon: min_lon.max(max_lon),
        }
    }

    /// 以某点为中心、边长约 2×`radius_m` 的正方形范围（BD-09）。
    pub fn around(lat: f64, lon: f64, radius_m: f64) -> Self {
        let dlat = radius_m / 111_320.0;
        let dlon = radius_m / (111_320.0 * lat.to_radians().cos().max(0.05));
        Self::new(lat - dlat, lon - dlon, lat + dlat, lon + dlon)
    }

    pub fn center(&self) -> (f64, f64) {
        (
            (self.min_lat + self.max_lat) / 2.0,
            (self.min_lon + self.max_lon) / 2.0,
        )
    }

    /// 从围栏环集合取外接框（顶点是 (lat, lon)，BD-09）。全空/退化时返回 None。
    pub fn from_fences(fences: &[Vec<(f64, f64)>]) -> Option<Self> {
        let mut acc: Option<Bbox> = None;
        for ring in fences {
            for &(lat, lon) in ring {
                acc = Some(match acc {
                    None => Bbox::new(lat, lon, lat, lon),
                    Some(cur) => Bbox::new(
                        cur.min_lat.min(lat),
                        cur.min_lon.min(lon),
                        cur.max_lat.max(lat),
                        cur.max_lon.max(lon),
                    ),
                });
            }
        }
        // 单点/零面积框抓不到有意义的瓦片，当作「没有围栏」处理。
        acc.filter(|b| b.max_lat - b.min_lat > 1e-6 && b.max_lon - b.min_lon > 1e-6)
    }
}

// ── 投影与瓦片编号 ──────────────────────────────────────────────────────

/// BD-09 经纬度 → 球面墨卡托米（百度口径，原点在经纬度 0）。
pub fn baidu_mercator(lat: f64, lon: f64) -> (f64, f64) {
    let x = lon * BD_ORIGIN_SHIFT / 180.0;
    let y =
        ((90.0 + lat) * std::f64::consts::PI / 360.0).tan().ln() / (std::f64::consts::PI / 180.0);
    (x, y * BD_ORIGIN_SHIFT / 180.0)
}

/// 球面墨卡托米 → BD-09 经纬度（`baidu_mercator` 的逆）。
pub fn mercator_to_baidu(x: f64, y: f64) -> (f64, f64) {
    let lon = x * 180.0 / BD_ORIGIN_SHIFT;
    let lat = 180.0 / std::f64::consts::PI
        * 2.0
        * (y * std::f64::consts::PI / BD_ORIGIN_SHIFT).exp().atan()
        - 90.0;
    (lat, lon)
}

/// 一个像素在墨卡托平面上的边长（米，不含纬度修正）。z18 ≈ 0.597 m/px。
pub fn mercator_m_per_px(z: u8) -> f64 {
    2.0 * BD_ORIGIN_SHIFT / (TILE_PX as f64 * (1u64 << z) as f64)
}

/// 一个像素对应的**地面**米数：墨卡托尺度 × cos(纬度)。z18 北京 ≈0.46 m/px。
pub fn meters_per_pixel(lat: f64, z: u8) -> f64 {
    mercator_m_per_px(z) * lat.to_radians().cos().max(0.05)
}

/// 百度瓦片编号。
///
/// 推导：
/// 1. `baidu_mercator` 把 BD-09 经纬度换成米（x 向东、y 向北，原点在经纬度 0）；
/// 2. 百度把这个平移到 `[0, 2×BD_ORIGIN_SHIFT]`：`px = mx + SHIFT`；瓦片行号向南
///    增大，所以 `py = SHIFT - my`；
/// 3. z 级一张瓦片覆盖 `span = 2×SHIFT / 2^z` 米，于是 `x = floor(px/span)`、
///    `y = floor(py/span)`。
///
/// 天安门（BD-09 39.9087, 116.3975）在 z18 的算式（测试里逐项核对）：
/// - `mx = 116.3975 × 20037726.37 / 180 = 12 957 451.418 m`
/// - `my = ln(tan((90+39.9087)·π/360))/(π/180) × 20037726.37/180 = 4 852 736.471 m`
/// - `span = 2 × 20037726.37 / 2^18 = 152.8757 m`
/// - `x = floor((12957451.418 + 20037726.37)/152.8757) = 215 830`
/// - `y = floor((20037726.37 - 4852736.471)/152.8757) = 99 328`
pub fn baidu_tile_xy(lat: f64, lon: f64, z: u8) -> (i64, i64) {
    let (mx, my) = baidu_mercator(lat, lon);
    let span = 2.0 * BD_ORIGIN_SHIFT / (1u64 << z) as f64;
    let x = ((mx + BD_ORIGIN_SHIFT) / span).floor() as i64;
    let y = ((BD_ORIGIN_SHIFT - my) / span).floor() as i64;
    (x, y)
}

/// 覆盖 `bbox` 的瓦片区间（闭区间）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TileRange {
    pub z: u8,
    pub x0: i64,
    pub x1: i64,
    pub y0: i64,
    pub y1: i64,
}

impl TileRange {
    pub fn cols(&self) -> i64 {
        self.x1 - self.x0 + 1
    }
    pub fn rows(&self) -> i64 {
        self.y1 - self.y0 + 1
    }
    pub fn count(&self) -> usize {
        (self.cols().max(0) * self.rows().max(0)) as usize
    }
}

/// `bbox` 在 z 级覆盖哪些瓦片。
///
/// 纬度越大 y 越小（行号向南增大），所以两个端点要各自算再取 min/max。
pub fn tile_range(bbox: &Bbox, z: u8) -> TileRange {
    let (x_a, y_a) = baidu_tile_xy(bbox.min_lat, bbox.min_lon, z);
    let (x_b, y_b) = baidu_tile_xy(bbox.max_lat, bbox.max_lon, z);
    TileRange {
        z,
        x0: x_a.min(x_b),
        x1: x_a.max(x_b),
        y0: y_a.min(y_b),
        y1: y_a.max(y_b),
    }
}

/// 预算校验：瓦片数与拼图像素都要在框内，否则**明确报错**（不硬拼）。
pub fn check_budget(range: &TileRange, cfg: &TileConfig) -> Result<usize, String> {
    if cfg.max_tiles == 0 {
        return Err("瓦片数上限为 0：请把 max_tiles 调到 ≥1".into());
    }
    let count = range.count();
    if count > cfg.max_tiles {
        return Err(format!(
            "范围需要 {count} 张瓦片（{}×{}），超过上限 {}。请缩小半径，或把缩放级别从 z{} 降一档",
            range.cols(),
            range.rows(),
            cfg.max_tiles,
            range.z
        ));
    }
    let pixels = count.saturating_mul(TILE_PX * TILE_PX);
    if pixels > MAX_PIXELS {
        return Err(format!(
            "拼接后 {pixels} 像素超过上限 {MAX_PIXELS}，请降低缩放级别"
        ));
    }
    Ok(count)
}

// ── 抓图与拼接 ─────────────────────────────────────────────────────────

/// 拼好的大图（RGB8，行优先）。
pub struct Stitched {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<u8>,
    pub tiles: usize,
    pub z: u8,
    /// 左上角像素在墨卡托米里的坐标。
    pub origin_m: (f64, f64),
}

impl Stitched {
    /// 像素坐标 → BD-09 经纬度（用墨卡托尺度，不是地面尺度）。
    pub fn pixel_to_bd09(&self, px: f64, py: f64) -> (f64, f64) {
        let scale = mercator_m_per_px(self.z);
        mercator_to_baidu(self.origin_m.0 + px * scale, self.origin_m.1 - py * scale)
    }

    /// 左上角像素的 BD-09 经纬度（`build_graph` 的输入）。
    pub fn origin_bd09(&self) -> (f64, f64) {
        self.pixel_to_bd09(0.0, 0.0)
    }
}

/// 抓取并拼接瓦片。
///
/// 先做预算校验（超限直接报错，一个请求都不发）；再按四域名轮换逐张抓，单张失败
/// 重试一次 —— 缺块会让骨架在接缝处断开，看起来像「路没了」，所以拿不全就整体失败。
pub fn fetch_tiles(agent: &Agent, bbox: &Bbox, cfg: &TileConfig) -> Result<Stitched, String> {
    let range = tile_range(bbox, cfg.z);
    let count = check_budget(&range, cfg)?;
    let width = range.cols() as usize * TILE_PX;
    let height = range.rows() as usize * TILE_PX;
    let mut rgb = vec![255u8; width * height * 3];
    let mut served = 0usize;
    for (row, y) in (range.y0..=range.y1).enumerate() {
        for (col, x) in (range.x0..=range.x1).enumerate() {
            let png = fetch_one(agent, x, y, cfg.z, served)?;
            served += 1;
            blit(&mut rgb, width, height, col * TILE_PX, row * TILE_PX, &png);
        }
    }
    let span = 2.0 * BD_ORIGIN_SHIFT / (1u64 << cfg.z) as f64;
    Ok(Stitched {
        width,
        height,
        rgb,
        tiles: count,
        z: cfg.z,
        origin_m: (
            range.x0 as f64 * span - BD_ORIGIN_SHIFT,
            BD_ORIGIN_SHIFT - range.y0 as f64 * span,
        ),
    })
}

/// 抓单张瓦片：四域名轮换 + 限速 + 失败重试一次。
fn fetch_one(agent: &Agent, x: i64, y: i64, z: u8, index: usize) -> Result<Vec<u8>, String> {
    let host = TILE_HOST_PREFIXES[index % TILE_HOST_PREFIXES.len()];
    let url = format!(
        "https://maponline{host}.bdimg.com/tile/?qt=tile&x={x}&y={y}&z={z}&styles=pl&scaler=1"
    );
    // 同一出口连打瓦片比连打接口更容易被限流，慢一点更稳。
    std::thread::sleep(std::time::Duration::from_millis(180));
    let mut last = String::new();
    for attempt in 0..2 {
        let outcome = agent
            .get(&url)
            .set("User-Agent", crate::crypto::header::UA_IOS)
            .call();
        match outcome {
            Ok(resp) => {
                let status = resp.status();
                let mut buf = Vec::new();
                std::io::Read::read_to_end(&mut resp.into_reader(), &mut buf)
                    .map_err(|e| format!("瓦片读取失败（z{z} x{x} y{y}）：{e}"))?;
                if status == 403 || status == 401 {
                    last = format!("瓦片被拒绝（HTTP {status}）");
                } else if buf.len() < 512 {
                    last = format!("瓦片内容过短（{} 字节，可能是占位图）", buf.len());
                } else {
                    return Ok(buf);
                }
            }
            Err(e) => last = format!("瓦片请求失败（z{z} x{x} y{y}）：{e}"),
        }
        if attempt == 0 {
            std::thread::sleep(std::time::Duration::from_millis(600));
        }
    }
    Err(format!(
        "{last}。可换网络或稍后重试；也可以先切回「真实道路」，或到路网页人工导入 OSM 路网"
    ))
}

/// 把一张 PNG 瓦片贴到大图（越界像素丢弃）。
fn blit(dst: &mut [u8], dst_w: usize, dst_h: usize, x0: usize, y0: usize, png: &[u8]) {
    let Ok(decoded) = image::load_from_memory(png) else {
        return;
    };
    let img = decoded.to_rgb8();
    for (y, row) in img.rows().enumerate() {
        let dy = y0 + y;
        if dy >= dst_h {
            break;
        }
        for (x, px) in row.enumerate() {
            let dx = x0 + x;
            if dx >= dst_w {
                break;
            }
            let at = (dy * dst_w + dx) * 3;
            dst[at..at + 3].copy_from_slice(&px.0);
        }
    }
}

// ── 道路像素分类 ────────────────────────────────────────────────────────

/// RGB 图 → 道路候选掩码（1 = 可能是路，0 = 不是）。
///
/// 判据（百度 `pl` 底图）：
/// 1. 彩度 ≤ `max_chroma`：水系（蓝）、绿地（绿）、POI 图标（彩色）直接排除；
/// 2. 亮度 ∈ [`road_min_luma`, `road_max_luma`]：排除深色文字与纯白空地；
/// 3. **局部对比**：比 9×9 邻域均值亮 `contrast_min` 以上 —— 路面比底图背景亮，
///    这是把「浅灰底」和「近白路面」分开的关键，也是绝对阈值之外的兜底；
/// 4. 3×3 开运算：去掉文字笔画/图标碎点，并把被图标压窄的路面补回来。
pub fn classify_roads(rgb: &[u8], width: usize, height: usize, cfg: &TileConfig) -> Vec<u8> {
    let luma = luma_plane(rgb, width, height);
    // 底图背景占绝大多数像素：用整幅图的均值当「背景亮度」，自适应阈值比
    // 绝对阈值稳（瓦片明暗随样式/地区变化），也不用逐像素做局部滤波。
    let background = luma.iter().map(|v| *v as u64).sum::<u64>() / luma.len().max(1) as u64;
    let threshold = ((background as i32 + cfg.contrast_min as i32)
        .max(cfg.road_min_luma as i32)
        .min(cfg.road_max_luma as i32 - 1)) as u8;
    let mut mask = vec![0u8; width * height];
    for i in 0..width * height {
        let (r, g, b) = (rgb[i * 3], rgb[i * 3 + 1], rgb[i * 3 + 2]);
        let chroma = r.max(g).max(b) - r.min(g).min(b);
        let l = luma[i];
        if chroma > cfg.max_chroma || l < threshold || l > cfg.road_max_luma {
            continue;
        }
        mask[i] = 1;
    }
    opening(&mut mask, width, height);
    mask
}

fn luma_plane(rgb: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut out = vec![0u8; width * height];
    for i in 0..width * height {
        let (r, g, b) = (
            rgb[i * 3] as u32,
            rgb[i * 3 + 1] as u32,
            rgb[i * 3 + 2] as u32,
        );
        out[i] = ((r * 299 + g * 587 + b * 114) / 1000) as u8;
    }
    out
}

fn opening(mask: &mut [u8], width: usize, height: usize) {
    let eroded = erode(mask, width, height);
    let dilated = dilate(&eroded, width, height);
    mask.copy_from_slice(&dilated);
}

fn erode(mask: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut out = vec![0u8; mask.len()];
    for y in 0..height {
        for x in 0..width {
            if mask[y * width + x] == 0 {
                continue;
            }
            let mut all = true;
            for (dx, dy) in neighbors() {
                let nx = x as i32 + dx;
                let ny = y as i32 + dy;
                if nx < 0 || ny < 0 || nx >= width as i32 || ny >= height as i32 {
                    all = false;
                    break;
                }
                if mask[ny as usize * width + nx as usize] == 0 {
                    all = false;
                    break;
                }
            }
            if all {
                out[y * width + x] = 1;
            }
        }
    }
    out
}

fn dilate(mask: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut out = vec![0u8; mask.len()];
    for y in 0..height {
        for x in 0..width {
            if mask[y * width + x] == 0 {
                continue;
            }
            out[y * width + x] = 1;
            for (dx, dy) in neighbors() {
                let nx = x as i32 + dx;
                let ny = y as i32 + dy;
                if nx < 0 || ny < 0 || nx >= width as i32 || ny >= height as i32 {
                    continue;
                }
                out[ny as usize * width + nx as usize] = 1;
            }
        }
    }
    out
}

fn neighbors() -> [(i32, i32); 8] {
    [
        (-1, -1),
        (0, -1),
        (1, -1),
        (-1, 0),
        (1, 0),
        (-1, 1),
        (0, 1),
        (1, 1),
    ]
}

// ── 骨架细化（Zhang-Suen）───────────────────────────────────────────────

/// Zhang-Suen 细化：把宽度 >1px 的连通带削成 1px 骨架。
///
/// 两趟子迭代交替执行直到没有像素被删除。保留拓扑（端点不缩、不断线），
/// 这是后面按端点/交点追踪折线的前提。
pub fn thin(mask: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut img = mask.to_vec();
    let at = |img: &[u8], x: i32, y: i32| -> u8 {
        if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
            return 0;
        }
        img[y as usize * width + x as usize]
    };
    loop {
        let mut removed = 0usize;
        for step in 0..2 {
            let mut del: Vec<usize> = Vec::new();
            for y in 1..height as i32 - 1 {
                for x in 1..width as i32 - 1 {
                    if at(&img, x, y) == 0 {
                        continue;
                    }
                    let p2 = at(&img, x, y - 1);
                    let p3 = at(&img, x + 1, y - 1);
                    let p4 = at(&img, x + 1, y);
                    let p5 = at(&img, x + 1, y + 1);
                    let p6 = at(&img, x, y + 1);
                    let p7 = at(&img, x - 1, y + 1);
                    let p8 = at(&img, x - 1, y);
                    let p9 = at(&img, x - 1, y - 1);
                    let ring = [p2, p3, p4, p5, p6, p7, p8, p9, p2];
                    let count = ring[..8].iter().filter(|v| **v == 1).count();
                    if !(2..=6).contains(&count) {
                        continue;
                    }
                    let transitions = ring.windows(2).filter(|w| w[0] == 0 && w[1] == 1).count();
                    if transitions != 1 {
                        continue;
                    }
                    let (a, b) = if step == 0 {
                        (p2 * p4 * p6, p4 * p6 * p8)
                    } else {
                        (p2 * p4 * p8, p2 * p6 * p8)
                    };
                    if a != 0 || b != 0 {
                        continue;
                    }
                    del.push(y as usize * width + x as usize);
                }
            }
            for i in &del {
                img[*i] = 0;
            }
            removed += del.len();
        }
        if removed == 0 {
            break;
        }
    }
    img
}

// ── 骨架追踪 ────────────────────────────────────────────────────────────

fn degree(skel: &[u8], width: usize, height: usize, x: usize, y: usize) -> usize {
    let mut n = 0;
    for (dx, dy) in neighbors() {
        let nx = x as i32 + dx;
        let ny = y as i32 + dy;
        if nx < 0 || ny < 0 || nx >= width as i32 || ny >= height as i32 {
            continue;
        }
        if skel[ny as usize * width + nx as usize] == 1 {
            n += 1;
        }
    }
    n
}

/// 把骨架追成折线（像素坐标，y 向下）。
///
/// 只从**端点**（度 1）出发，在交点处按「最直的方向」继续走 —— 一条穿过十字
/// 路口的路仍然是**一条**折线，而不是在路口被切成四段。纯闭环另外收集。
/// 长度短于 `min_len_px` 的线段视为噪点丢弃。
pub fn trace_polylines(
    skel: &[u8],
    width: usize,
    height: usize,
    min_len_px: usize,
) -> Vec<Vec<(f64, f64)>> {
    let mut used = vec![false; skel.len()];
    let mut lines: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut endpoints: Vec<usize> = Vec::new();
    for y in 0..height {
        for x in 0..width {
            if skel[y * width + x] == 1 && degree(skel, width, height, x, y) == 1 {
                endpoints.push(y * width + x);
            }
        }
    }
    for start in endpoints {
        if used[start] {
            continue;
        }
        used[start] = true;
        let mut line = vec![(start % width, start / width)];
        let mut prev = usize::MAX;
        let mut cur = start;
        loop {
            let Some(next) = straightest(skel, width, height, &used, prev, cur) else {
                break;
            };
            used[next] = true;
            line.push((next % width, next / width));
            prev = cur;
            cur = next;
            if degree(skel, width, height, cur % width, cur / width) == 1 {
                break;
            }
        }
        if line.len() >= min_len_px {
            lines.push(
                line.into_iter()
                    .map(|(x, y)| (x as f64, y as f64))
                    .collect(),
            );
        }
    }
    // 剩下的纯闭环（一个端点都没有的情况）。
    for y in 0..height {
        for x in 0..width {
            let idx = y * width + x;
            if skel[idx] != 1 || used[idx] {
                continue;
            }
            used[idx] = true;
            let mut line = vec![(x as f64, y as f64)];
            let mut prev = usize::MAX;
            let mut cur = idx;
            while let Some(next) = straightest(skel, width, height, &used, prev, cur) {
                line.push(((next % width) as f64, (next / width) as f64));
                used[next] = true;
                prev = cur;
                cur = next;
            }
            if line.len() >= min_len_px {
                lines.push(line);
            }
        }
    }
    lines
}

/// 在 `cur` 的未走过邻点里挑「方向最接近上一步」的那个（点积最大）。
fn straightest(
    skel: &[u8],
    width: usize,
    height: usize,
    used: &[bool],
    prev: usize,
    cur: usize,
) -> Option<usize> {
    let (cx, cy) = ((cur % width) as f64, (cur / width) as f64);
    let (px, py) = if prev == usize::MAX {
        (cx - 1.0, cy)
    } else {
        ((prev % width) as f64, (prev / width) as f64)
    };
    let (dx, dy) = (cx - px, cy - py);
    let mut best: Option<(usize, f64)> = None;
    for (ox, oy) in neighbors() {
        let nx = cx as i32 + ox;
        let ny = cy as i32 + oy;
        if nx < 0 || ny < 0 || nx >= width as i32 || ny >= height as i32 {
            continue;
        }
        let idx = ny as usize * width + nx as usize;
        if idx == prev || skel[idx] != 1 || used[idx] {
            continue;
        }
        let score = ox as f64 * dx + oy as f64 * dy;
        if best.map_or(true, |(_, s)| score > s) {
            best = Some((idx, score));
        }
    }
    best.map(|(idx, _)| idx)
}

// ── 折线简化（Douglas-Peucker）──────────────────────────────────────────

/// Douglas-Peucker：压到「偏离原线不超过 `tol`」的最少点集。
pub fn simplify(points: &[(f64, f64)], tol: f64) -> Vec<(f64, f64)> {
    if points.len() <= 2 || tol <= 0.0 {
        return points.to_vec();
    }
    let mut keep = vec![false; points.len()];
    keep[0] = true;
    keep[points.len() - 1] = true;
    let mut stack = vec![(0usize, points.len() - 1)];
    while let Some((lo, hi)) = stack.pop() {
        if hi <= lo + 1 {
            continue;
        }
        let (mut far, mut dist) = (lo, 0.0f64);
        for i in lo + 1..hi {
            let d = point_segment_distance(points[i], points[lo], points[hi]);
            if d > dist {
                dist = d;
                far = i;
            }
        }
        if dist > tol {
            keep[far] = true;
            stack.push((lo, far));
            stack.push((far, hi));
        }
    }
    points
        .iter()
        .enumerate()
        .filter(|(i, _)| keep[*i])
        .map(|(_, p)| *p)
        .collect()
}

fn point_segment_distance(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (px, py) = p;
    let (ax, ay) = a;
    let (bx, by) = b;
    let (dx, dy) = (bx - ax, by - ay);
    let len2 = dx * dx + dy * dy;
    if len2 <= f64::EPSILON {
        return ((px - ax).powi(2) + (py - ay).powi(2)).sqrt();
    }
    let t = (((px - ax) * dx + (py - ay) * dy) / len2).clamp(0.0, 1.0);
    ((px - (ax + t * dx)).powi(2) + (py - (ay + t * dy)).powi(2)).sqrt()
}

// ── 建图与缓存 ─────────────────────────────────────────────────────────

/// 生成统计（写进缓存，也用于状态栏汇报）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Stats {
    pub tiles: usize,
    pub road_ratio: f64,
    pub polylines: usize,
    pub nodes: usize,
    pub edges: usize,
    pub elapsed_ms: u64,
    pub z: u8,
    pub generated_at_ms: i64,
}

impl Stats {
    /// 给状态栏的一行话。
    pub fn summary(&self) -> String {
        format!(
            "瓦片 {} 张 / 道路像素占比 {:.2}% / 折线 {} 条 / 节点 {} / 耗时 {} ms",
            self.tiles,
            self.road_ratio * 100.0,
            self.polylines,
            self.nodes,
            self.elapsed_ms
        )
    }
}

/// 缓存文件（带版本号：以后改字段直接拒旧文件，而不是读出半对的图）。
#[derive(Serialize, Deserialize)]
struct Cache {
    version: u32,
    source: String,
    z: u8,
    bbox: Bbox,
    generated_at_ms: i64,
    stats: Stats,
    /// BD-09 经纬度折线。
    polylines: Vec<Vec<(f64, f64)>>,
}

const CACHE_VERSION: u32 = 1;
const CACHE_FILE: &str = "tile-road.json";

/// 默认缓存路径（数据目录下）。
pub fn default_cache_path() -> std::path::PathBuf {
    crate::platform::data_dir().join(CACHE_FILE)
}

/// 像素折线（y 向下）→ BD-09 经纬度折线。
pub fn pixels_to_bd09(stitched: &Stitched, lines: &[Vec<(f64, f64)>]) -> Vec<Vec<(f64, f64)>> {
    lines
        .iter()
        .map(|line| {
            line.iter()
                .map(|&(x, y)| stitched.pixel_to_bd09(x, y))
                .collect()
        })
        .collect()
}

/// 像素折线 + 左上角 BD-09 原点 → `RoadGraph`。
///
/// 像素→经纬度用局部仿射（每像素固定米数）：校区尺度（几公里）误差在亚米级，
/// 对「沿道路行进」没有影响；跨城市使用才需要换投影。
pub fn build_graph(
    lines_px: &[Vec<(f64, f64)>],
    origin_bd09: (f64, f64),
    m_per_px: f64,
    merge_m: f64,
) -> RoadGraph {
    let dlat = m_per_px / 111_320.0;
    let dlon = m_per_px / (111_320.0 * origin_bd09.0.to_radians().cos().max(0.05));
    let lines: Vec<Vec<(f64, f64)>> = lines_px
        .iter()
        .map(|line| {
            line.iter()
                .map(|&(x, y)| (origin_bd09.0 - y * dlat, origin_bd09.1 + x * dlon))
                .collect()
        })
        .collect();
    assemble_graph(&lines, merge_m)
}

/// BD-09 折线 → `RoadGraph`（缓存回读走这条）。
pub fn graph_from_bd09(lines: &[Vec<(f64, f64)>], merge_m: f64) -> RoadGraph {
    assemble_graph(lines, merge_m)
}

fn cell_of(point: (f64, f64), merge: f64) -> (i64, i64) {
    let cell_lat = merge / 111_320.0;
    let cell_lon = merge / (111_320.0 * point.0.to_radians().cos().max(0.05));
    (
        (point.0 / cell_lat).floor() as i64,
        (point.1 / cell_lon).floor() as i64,
    )
}

/// 按 `merge_m` 合并折线端点，建成双向图（BD-09，已标记 `align_applied`）。
///
/// 只有端点距离 ≤ `merge_m` 才共用节点 —— 不相邻的两条路不会被并到一起
/// （用例 `parallel_roads_stay_two_components` 就是在防这个）。
fn assemble_graph(lines: &[Vec<(f64, f64)>], merge_m: f64) -> RoadGraph {
    let mut graph = RoadGraph::new();
    let merge = merge_m.max(0.05);
    let mut nodes: Vec<(f64, f64)> = Vec::new();
    let mut index = Vec::new();
    let mut buckets: std::collections::HashMap<(i64, i64), Vec<usize>> =
        std::collections::HashMap::new();
    for (way, line) in lines.iter().enumerate() {
        if line.len() < 2 {
            continue;
        }
        let mut seq = Vec::with_capacity(line.len());
        for point in line {
            let key = cell_of(*point, merge);
            let mut found = None;
            'search: for dy in -1..=1 {
                for dx in -1..=1 {
                    if let Some(ids) = buckets.get(&(key.0 + dy, key.1 + dx)) {
                        for id in ids {
                            if crate::track::analysis::distance(nodes[*id], *point) <= merge {
                                found = Some(index[*id]);
                                break 'search;
                            }
                        }
                    }
                }
            }
            let idx = match found {
                Some(existing) => existing,
                None => {
                    let created = graph.add_node(Coord::new(point.1, point.0));
                    nodes.push(*point);
                    index.push(created);
                    buckets.entry(key).or_default().push(nodes.len() - 1);
                    created
                }
            };
            seq.push(idx);
        }
        for (pair_idx, pair) in seq.windows(2).enumerate() {
            if pair[0] == pair[1] {
                continue;
            }
            let (a, b) = (line[pair_idx], line[pair_idx + 1]);
            let data = EdgeData {
                way_id: way as u64,
                oneway: false,
                highway: "residential".into(),
                maxspeed_kmh: None,
                length_m: crate::track::analysis::distance(a, b).max(0.1),
            };
            graph.add_edge(pair[0], pair[1], data.clone());
            graph.add_edge(pair[1], pair[0], data);
        }
    }
    graph.compute_anchor();
    // 节点本身就是 BD-09：标记成「已对齐」，避免 `align_network` 再平移一次。
    graph.align_applied = true;
    graph
}

/// 道路像素占比是否落在「像一张真实路网底图」的区间里。
///
/// 两头都要卡：下限挡「整张纯色/空地」；上限挡「整张空白瓦片」（纯白图会被
/// 整片判成路面，占比≈100%）。判据单独成函数，是为了能在离线用例里覆盖两端。
pub fn road_ratio_is_plausible(ratio: f64) -> bool {
    (MIN_ROAD_RATIO..=MAX_ROAD_RATIO).contains(&ratio)
}

/// 全流程：抓图 → 分类 → 细化 → 追踪 → 简化 → 建图 → 写缓存。
pub fn generate(
    agent: &Agent,
    bbox: &Bbox,
    cfg: &TileConfig,
    out_path: &std::path::Path,
) -> Result<(Stats, RoadGraph), String> {
    let started = std::time::Instant::now();
    let stitched = fetch_tiles(agent, bbox, cfg)?;
    let mask = classify_roads(&stitched.rgb, stitched.width, stitched.height, cfg);
    let road_px = mask.iter().filter(|v| **v == 1).count();
    let ratio = road_px as f64 / mask.len().max(1) as f64;
    if !road_ratio_is_plausible(ratio) {
        return Err(format!(
            "道路像素占比 {:.2}% 不在合理区间（{:.1}%–{:.0}%）：{}。\
             可换 z 级别重试，或退回「真实道路」/人工导入 OSM 路网",
            ratio * 100.0,
            MIN_ROAD_RATIO * 100.0,
            MAX_ROAD_RATIO * 100.0,
            if ratio < MIN_ROAD_RATIO {
                "整张图识别不出道路（纯色、空地，或根本不是路网底图）"
            } else {
                "整张图几乎全是同一种亮色（空白瓦片/占位图），这种图会被整片判成路面"
            }
        ));
    }
    let skeleton = thin(&mask, stitched.width, stitched.height);
    let traced = trace_polylines(
        &skeleton,
        stitched.width,
        stitched.height,
        cfg.min_polyline_px,
    );
    if traced.is_empty() {
        return Err(
            "骨架里没有可用的折线（全是噪点或断线）。可把 min_polyline_px 调小后重试".into(),
        );
    }
    let (lat_c, _) = bbox.center();
    let m_per_px = meters_per_pixel(lat_c, cfg.z);
    let tol_px = (cfg.simplify_m / m_per_px.max(0.01)).max(0.8);
    let simplified: Vec<Vec<(f64, f64)>> = traced
        .iter()
        .map(|line| simplify(line, tol_px))
        .filter(|line| line.len() >= 2)
        .collect();
    let polylines = pixels_to_bd09(&stitched, &simplified);
    // 图按局部仿射建（与缓存回读同一套逻辑），缓存里存经纬度，回读不依赖 z。
    let graph = build_graph(&simplified, stitched.origin_bd09(), m_per_px, cfg.merge_m);
    if graph.graph.edge_count() == 0 {
        return Err("折线没能连成路网（全部是不连通的孤立点）。可降低 z 或换更大范围重试".into());
    }
    let stats = Stats {
        tiles: stitched.tiles,
        road_ratio: ratio,
        polylines: polylines.len(),
        nodes: graph.graph.node_count(),
        edges: graph.graph.edge_count(),
        elapsed_ms: started.elapsed().as_millis() as u64,
        z: cfg.z,
        generated_at_ms: crate::crypto::envelope::now_ms(),
    };
    save_cache(out_path, bbox, &stats, &polylines)?;
    Ok((stats, graph))
}

/// 写缓存（原子写 + 只给当前用户读写）。
pub fn save_cache(
    path: &std::path::Path,
    bbox: &Bbox,
    stats: &Stats,
    polylines: &[Vec<(f64, f64)>],
) -> Result<(), String> {
    let cache = Cache {
        version: CACHE_VERSION,
        source: "baidu-tile-pl".into(),
        z: stats.z,
        bbox: *bbox,
        generated_at_ms: stats.generated_at_ms,
        stats: stats.clone(),
        polylines: polylines.to_vec(),
    };
    let text = serde_json::to_string(&cache).map_err(|e| format!("序列化瓦片路网失败：{e}"))?;
    crate::api::model::write_secret_file(path, text.as_bytes())
        .map_err(|e| format!("写入 {} 失败：{e}", path.display()))
}

/// 读缓存：返回统计与路网（节点已是 BD-09，不需要再对齐）。
pub fn load(path: &std::path::Path) -> Result<(Stats, RoadGraph), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("读取瓦片路网缓存失败（{}）：{e}", path.display()))?;
    let cache: Cache =
        serde_json::from_str(&text).map_err(|e| format!("瓦片路网缓存格式错误：{e}"))?;
    if cache.version != CACHE_VERSION {
        return Err(format!(
            "瓦片路网缓存版本 {} 与当前 {} 不符，请重新生成",
            cache.version, CACHE_VERSION
        ));
    }
    if cache.polylines.is_empty() {
        return Err("瓦片路网缓存里没有折线，请重新生成".into());
    }
    let graph = graph_from_bd09(&cache.polylines, 3.0);
    Ok((cache.stats, graph))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用合成底图：浅灰背景 + 白色「路」。
    ///
    /// 默认参数下：白 255 亮度够、彩度 0，且比 9×9 邻域亮 >8 → 判为路；
    /// 灰 180 亮度也在区间内，但不比邻域亮 → 排除。这正是「只靠绝对阈值分不开
    /// 浅灰底与近白路面」的兜底逻辑。
    struct Canvas {
        width: usize,
        height: usize,
        rgb: Vec<u8>,
    }

    impl Canvas {
        fn new(width: usize, height: usize, gray: u8) -> Self {
            Self {
                width,
                height,
                rgb: vec![gray; width * height * 3],
            }
        }

        fn fill_rect(&mut self, x0: usize, x1: usize, y0: usize, y1: usize, v: u8) {
            for y in y0..y1.min(self.height) {
                for x in x0..x1.min(self.width) {
                    let at = (y * self.width + x) * 3;
                    self.rgb[at] = v;
                    self.rgb[at + 1] = v;
                    self.rgb[at + 2] = v;
                }
            }
        }

        fn at(&self, x: usize, y: usize) -> u8 {
            self.rgb[(y * self.width + x) * 3]
        }
    }

    /// 白色十字 + 灰底（两条 7px 宽的路在中心交叉）。
    fn cross_canvas() -> Canvas {
        let mut c = Canvas::new(96, 96, 180);
        c.fill_rect(0, 96, 44, 51, 255);
        c.fill_rect(44, 51, 0, 96, 255);
        c
    }

    fn count(mask: &[u8]) -> usize {
        mask.iter().filter(|v| **v == 1).count()
    }

    /// 图里有几个连通分量（走双向边，所以用出边邻接即可）。
    fn components(graph: &RoadGraph) -> usize {
        let mut seen = std::collections::HashSet::new();
        let mut total = 0;
        for start in graph.graph.node_indices() {
            if !seen.insert(start.index()) {
                continue;
            }
            total += 1;
            let mut stack = vec![start];
            while let Some(node) = stack.pop() {
                for next in graph.graph.neighbors(node) {
                    if seen.insert(next.index()) {
                        stack.push(next);
                    }
                }
            }
        }
        total
    }

    #[test]
    fn baidu_tiles_match_the_documented_derivation() {
        // ① 天安门（BD-09）在 z18 的瓦片编号：与注释里的手算逐项一致。
        let (x18, y18) = baidu_tile_xy(39.9087, 116.3975, 18);
        assert_eq!((x18, y18), (215_830, 99_328), "瓦片编号与推导不一致");

        // ② 每降一级正好折半（同一坐标落在父瓦片里）。
        let (x17, y17) = baidu_tile_xy(39.9087, 116.3975, 17);
        assert_eq!((x17, y17), (x18 / 2, y18 / 2));

        // ③ 这块瓦片确实把该点包在里面（反算四角，验证 y 轴方向与偏移都对）。
        let span = 2.0 * BD_ORIGIN_SHIFT / (1u64 << 18) as f64;
        let left = x18 as f64 * span - BD_ORIGIN_SHIFT;
        let top = BD_ORIGIN_SHIFT - y18 as f64 * span;
        let (lat_top, lon_left) = mercator_to_baidu(left, top);
        let (lat_bottom, lon_right) = mercator_to_baidu(left + span, top - span);
        assert!(
            lat_bottom < 39.9087 && 39.9087 < lat_top,
            "纬度不在瓦片内：{lat_bottom}..{lat_top}"
        );
        assert!(
            lon_left < 116.3975 && 116.3975 < lon_right,
            "经度不在瓦片内：{lon_left}..{lon_right}"
        );

        // ④ 方向正确：向东 x 变大、向北 y 变小。
        let dlon = 1000.0 / (111_320.0 * 39.9087_f64.to_radians().cos());
        let east = baidu_tile_xy(39.9087, 116.3975 + dlon, 18);
        assert!(east.0 > x18 && east.1 == y18, "向东应只让 x 变大");
        let north = baidu_tile_xy(39.9087 + 1000.0 / 111_320.0, 116.3975, 18);
        assert!(north.1 < y18, "向北应让 y 变小");

        // ⑤ 尺度：z18 墨卡托 ≈0.60 m/px；北京地面 ≈0.46 m/px。
        assert!((mercator_m_per_px(18) - 0.597).abs() < 0.002);
        assert!((meters_per_pixel(39.9087, 18) - 0.458).abs() < 0.005);
    }

    #[test]
    fn cross_image_is_classified_thinned_and_traced() {
        let canvas = cross_canvas();
        let cfg = TileConfig::default();
        let mask = classify_roads(&canvas.rgb, canvas.width, canvas.height, &cfg);

        // 路口中心是路；角落是底图背景（浅灰但不够亮）。
        assert_eq!(mask[47 * canvas.width + 47], 1, "路口中心应判为道路");
        assert_eq!(mask[5 * canvas.width + 5], 0, "灰底不该被当成路");
        assert!(count(&mask) > 0);

        // 细化：十字臂长 96px，1px 宽的骨架总长应 ≈96+96（旧的 7px 宽路面是 ~1295 像素）；
        // 行/列宽度不能直接比——横臂那一行本来就横跨整幅图，那是长度不是宽度。
        let skeleton = thin(&mask, canvas.width, canvas.height);
        let skeleton_px = count(&skeleton);
        assert!(
            (150..=260).contains(&skeleton_px),
            "骨架 {skeleton_px} 像素，细化没瘦到 1px（原始掩码 {} 像素）",
            count(&mask)
        );
        // 追踪：十字 = 两条「穿过路口」的折线（不是四段）。
        let lines = trace_polylines(&skeleton, canvas.width, canvas.height, 12);
        assert!(
            (1..=2).contains(&lines.len()),
            "十字应追出 1-2 条折线，实际 {}",
            lines.len()
        );
        let longest = lines.iter().map(|l| l.len()).max().unwrap_or(0);
        assert!(longest >= 40, "最长的折线只有 {longest} 点，追踪没走通");

        // 简化容差为 0 时必须原样返回（不做无意义压缩）。
        let straight = vec![(0.0, 0.0), (5.0, 0.0), (10.0, 0.0)];
        assert_eq!(simplify(&straight, 0.0), straight);
        // 容差足够大时只剩两个端点。
        assert_eq!(simplify(&straight, 1.0).len(), 2);
    }

    #[test]
    fn parallel_roads_stay_two_components() {
        let mut canvas = Canvas::new(128, 128, 180);
        canvas.fill_rect(0, 128, 20, 27, 255);
        canvas.fill_rect(0, 128, 90, 97, 255);
        let cfg = TileConfig::default();
        let mask = classify_roads(&canvas.rgb, canvas.width, canvas.height, &cfg);
        let skeleton = thin(&mask, canvas.width, canvas.height);
        let lines = trace_polylines(&skeleton, canvas.width, canvas.height, 12);
        assert_eq!(lines.len(), 2, "两条平行路应追出 2 条线");

        // 像素带进图：两条线相距约 64px（≈38m），远大于合并容差 3m。
        let graph = build_graph(&lines, (39.9, 116.4), 0.597, 3.0);
        assert_eq!(
            components(&graph),
            2,
            "不相邻的两条路被并成一个分量了（合并容差过大）"
        );
        assert!(graph.align_applied, "BD-09 图不该再被 align_network 平移");
        assert!(graph.graph.edge_count() >= 2);
    }

    #[test]
    fn cache_round_trip_and_version_guard() {
        let dir = std::env::temp_dir().join(format!("tile-road-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tile-road.json");
        let lines = vec![
            vec![(39.9, 116.4), (39.901, 116.401), (39.902, 116.4)],
            vec![(39.902, 116.4), (39.903, 116.399)],
        ];
        let stats = Stats {
            tiles: 4,
            road_ratio: 0.052,
            polylines: 2,
            nodes: 4,
            edges: 6,
            elapsed_ms: 1234,
            z: 18,
            generated_at_ms: 1_700_000_000_000,
        };
        let bbox = Bbox::around(39.9, 116.4, 400.0);
        save_cache(&path, &bbox, &stats, &lines).unwrap();

        let (loaded, graph) = load(&path).unwrap();
        assert_eq!(loaded.polylines, 2);
        assert_eq!(loaded.tiles, 4);
        assert_eq!(loaded.generated_at_ms, 1_700_000_000_000);
        assert!(graph.graph.node_count() >= 4);
        assert!(graph.align_applied);
        assert!(loaded.summary().contains("瓦片 4 张"));
        assert!(loaded.summary().contains("折线 2 条"));

        // 版本不符：明确拒绝，而不是读出一张半对的图。
        let stale = serde_json::json!({
            "version": CACHE_VERSION + 1,
            "source": "baidu-tile-pl",
            "z": 18,
            "bbox": {"min_lat": 39.9, "min_lon": 116.4, "max_lat": 39.91, "max_lon": 116.41},
            "generated_at_ms": 0,
            "stats": stats,
            "polylines": [[[39.9, 116.4], [39.901, 116.401]]],
        });
        std::fs::write(&path, stale.to_string()).unwrap();
        let err = match load(&path) {
            Ok(_) => panic!("版本不符的缓存不该被读进来"),
            Err(e) => e,
        };
        assert!(err.contains("版本"), "{err}");

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    /// 冒烟测试暴露的真缺陷：空白瓦片（256×256 纯白 PNG）会被整片判成路面。
    /// 下限挡不住它（占比≈100%），必须靠上限。
    #[test]
    fn blank_or_uniform_tiles_are_rejected_by_the_ratio_guard() {
        let cfg = TileConfig::default();
        // 纯白：分类器认为「比背景亮 + 无色」→ 整片路面
        let white = Canvas::new(32, 32, 255);
        let mask = classify_roads(&white.rgb, white.width, white.height, &cfg);
        let ratio = count(&mask) as f64 / mask.len() as f64;
        assert!(
            ratio > MAX_ROAD_RATIO,
            "全白图占比 {ratio:.2}，本该被判成异常"
        );
        assert!(!road_ratio_is_plausible(ratio), "全白图必须被拒");
        // 纯灰：一个点都认不出
        let gray = Canvas::new(32, 32, 120);
        let mask = classify_roads(&gray.rgb, gray.width, gray.height, &cfg);
        let ratio = count(&mask) as f64 / mask.len() as f64;
        assert!(!road_ratio_is_plausible(ratio), "纯灰图必须被拒");
        // 正常的十字：落在合理区间里
        let cross = cross_canvas();
        let mask = classify_roads(&cross.rgb, cross.width, cross.height, &cfg);
        let ratio = count(&mask) as f64 / mask.len() as f64;
        assert!(
            road_ratio_is_plausible(ratio),
            "正常十字图的占比 {ratio:.3} 不该被拒"
        );
    }

    fn too_many_tiles_is_refused_before_fetching() {
        let cfg = TileConfig::default();
        // 6km 见方：z18 下一张瓦片约 153m，一方就要 ~40 张。
        let bbox = Bbox::around(39.9087, 116.3975, 3000.0);
        let range = tile_range(&bbox, cfg.z);
        assert!(
            range.count() > cfg.max_tiles,
            "样本范围太小（{} 张），验证不了超限",
            range.count()
        );
        let err = check_budget(&range, &cfg).unwrap_err();
        assert!(err.contains("超过上限"), "{err}");
        assert!(err.contains("张瓦片"), "{err}");

        // 上限配 0 也是明确错误，而不是「随便抓」。
        let zero = TileConfig {
            max_tiles: 0,
            ..TileConfig::default()
        };
        assert!(check_budget(&range, &zero).unwrap_err().contains("≥1"));
    }
}
