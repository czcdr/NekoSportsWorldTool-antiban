//! 瓦片底图（**显示层**）：高德 / OSM 可切换、本地磁盘缓存、z≤18 超限放大。
//!
//! 分工（不越界）：
//! - **投影不外扩**：`MapState::to_screen` 仍是唯一的世界→屏幕映射。瓦片四角穿过它
//!   取轴对齐矩形，底图与矢量图因此共用同一条线性近似 —— 不存在「底图一套、矢量
//!   另一套」的漂移。偏差举证见测试 `linear_projection_deviation_is_subpixel`。
//! - **坐标**：项目内部统一 BD-09。瓦片网格按源分两套：高德 = GCJ-02，OSM = WGS84。
//!   BD-09→GCJ-02 复用 `track::wire::bd09_to_gcj02`；BD-09→WGS84 复用
//!   `api::school_roads::to_wgs` —— 全仓**唯一**一份 BD→WGS 反算（手绘路网
//!   `ui/draw_track.rs` 写盘前用的也是它），底图不再起第三份，免得坐标口径漂移。
//! - **抓取全部复用 `track::tile_road`**：`validate_tile_url`（https + 主机白名单 +
//!   拒私网/回环）、单张 2 MiB 上限、4096² 解码限额、`is_placeholder_tile` 判据、
//!   失败重试 1 次。本模块不另写 HTTP 栈与限额，免得两处口径漂移。
//! - **默认关闭**：`basemap=off` 时 `plan_requests` 恒为空 → 一个请求都不发。
//!
//! 线程模型：后台线程（每层一个）负责「磁盘缓存 → 网络抓取 → 解码」，主线程只做
//! 纹理上传（每帧上限 `MAX_UPLOADS_PER_FRAME`）与绘制。底图任何失败都只写状态栏
//! 提示，**不影响矢量图**绘制。

use eframe::egui;
use egui::{Color32, Pos2, Rect, TextureHandle, TextureOptions};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::track::geom::{gcj02_to_bd09, wgs84_to_bd09};
use crate::track::tile_road::{self, TileConfig, TileProjection};
use crate::track::wire::bd09_to_gcj02;

use super::map::MapState;

// ── 瓦片源 ──────────────────────────────────────────────────────────────

/// 瓦片源最大缩放级别。再往上不抓 z19/z20（多数源没有，且空白瓦片更费流量）：
/// 超限时把 z18 瓦片按 `tile_scale_factor` 放大。
pub const MAX_ZOOM: u8 = 18;
/// 单张瓦片边长（像素）。slippy 网格固定 256。
pub const TILE_PX: i64 = 256;
/// 每帧最多上传几张纹理（Android 上一次性上传几十张会卡住一帧）。
pub const MAX_UPLOADS_PER_FRAME: usize = 4;
/// 每帧最多派发几个抓取任务（平移时不要一次灌爆后台线程）。
pub const MAX_JOBS_PER_FRAME: usize = 8;
/// 内存里最多保留多少张已上传纹理（超出后丢掉视野外的）。
pub const MAX_TEXTURES: usize = 192;
/// 单张瓦片纹理的边长上限（256 正常，512 用于 @2x 视网膜源；再大按异常丢弃）。
pub const MAX_TILE_SIDE: u32 = 512;
/// 失败集合的上限：超了只淘汰**最旧的一条**（绝不整体清空，否则已失败的一批
/// 瓦片会一次性重新入队、对同一批坏瓦片反复抓）。
pub const MAX_FAILED_TILES: usize = 512;
/// `.tmp` 的“过期”阈值：正常写入是毫秒级完成的，超过 1 小时还在的只能是中断残留。
pub const TEMP_MAX_AGE: Duration = Duration::from_secs(3600);
/// 磁盘缓存默认上限（MiB）；配置键 `map_cache_limit_mb`。
pub const DEFAULT_CACHE_LIMIT_MB: u32 = 100;
/// 缓存目录名（`data_dir()/map_cache`）。
pub const CACHE_DIR: &str = "map_cache";

/// 高德栅格瓦片（`{host}` 轮换 1..4，分散单域限流）。
pub const AMAP_URL_TEMPLATE: &str = "https://webrd0{host}.is.autonavi.com/appmaptile?lang=zh_cn&size=1&scale=1&style=8&x={x}&y={y}&z={z}";
/// `{host}` 的取值（拼在 `webrd0` 后面 → webrd01..webrd04）。
pub const AMAP_HOSTS: [&str; 4] = ["1", "2", "3", "4"];
/// 高德主机白名单（`validate_tile_url` 不认识未显式声明的主机）。
pub const AMAP_ALLOWED_HOST: &str = "autonavi.com";

// ── 瓦片源 ──────────────────────────────────────────────────────────────

/// 底图源。`Off` 是默认值：**首次启动绝不联网**。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BasemapSource {
    /// 关闭（只画矢量图，不发任何瓦片请求）。
    #[default]
    Off,
    /// 高德栅格（GCJ-02 网格）。
    Amap,
    /// OSM 标准 XYZ（WGS84 网格）。
    Osm,
}

impl BasemapSource {
    /// 认不出的值（空串、老配置没这个键、手改成乱码）一律回落 `Off`：
    /// 宁可没有底图，也不要在用户没表态时联网。
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "amap" | "gaode" | "高德" => BasemapSource::Amap,
            "osm" | "openstreetmap" => BasemapSource::Osm,
            _ => BasemapSource::Off,
        }
    }

    /// 落盘用的值（`off` / `amap` / `osm`）。
    pub fn as_str(self) -> &'static str {
        match self {
            BasemapSource::Off => "off",
            BasemapSource::Amap => "amap",
            BasemapSource::Osm => "osm",
        }
    }

    /// 工具条上的中文标签。
    pub fn label(self) -> &'static str {
        match self {
            BasemapSource::Off => "关闭",
            BasemapSource::Amap => "高德",
            BasemapSource::Osm => "OSM",
        }
    }

    /// 缓存子目录名（`Off` 不落盘）。
    pub fn cache_key(self) -> Option<&'static str> {
        match self {
            BasemapSource::Off => None,
            BasemapSource::Amap => Some("amap"),
            BasemapSource::Osm => Some("osm"),
        }
    }

    /// 是否真的会去抓瓦片。
    pub fn is_online(self) -> bool {
        self != BasemapSource::Off
    }

    pub fn url_template(self) -> Option<&'static str> {
        match self {
            BasemapSource::Off => None,
            BasemapSource::Amap => Some(AMAP_URL_TEMPLATE),
            BasemapSource::Osm => Some(tile_road::DEFAULT_URL_TEMPLATE),
        }
    }

    /// 该源对应的 `TileConfig`：模板 + **显式**主机白名单（高德要加 autonavi.com）。
    pub fn tile_config(self) -> TileConfig {
        let mut cfg = TileConfig::default();
        match self {
            BasemapSource::Off => {}
            BasemapSource::Amap => {
                cfg.url_template = AMAP_URL_TEMPLATE.to_string();
                cfg.allowed_hosts = vec![AMAP_ALLOWED_HOST.to_string()];
                cfg.projection = TileProjection::WebMercator;
            }
            BasemapSource::Osm => {
                cfg.url_template = tile_road::DEFAULT_URL_TEMPLATE.to_string();
                cfg.projection = TileProjection::WebMercator;
            }
        }
        cfg
    }

    /// BD-09 → 该源的瓦片网格坐标（高德 GCJ-02 / OSM WGS84）。
    ///
    /// BD→WGS 用 `api::school_roads::to_wgs`（`wgs84_to_bd09` 的迭代反解，与手绘
    /// 路网写 `.osm` 同一个逆变换）：底图不另写第三份，坐标口径只留一处。
    pub fn from_bd09(self, lat: f64, lon: f64) -> (f64, f64) {
        match self {
            BasemapSource::Off | BasemapSource::Osm => crate::api::school_roads::to_wgs(lat, lon),
            BasemapSource::Amap => bd09_to_gcj02(lat, lon),
        }
    }

    /// 该源网格坐标 → BD-09。
    pub fn to_bd09(self, lat: f64, lon: f64) -> (f64, f64) {
        match self {
            BasemapSource::Off | BasemapSource::Osm => wgs84_to_bd09(lat, lon),
            BasemapSource::Amap => gcj02_to_bd09(lat, lon),
        }
    }
}

// ── 瓦片编号与几何 ──────────────────────────────────────────────────────

/// 一张瓦片的身份。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileKey {
    pub z: u8,
    pub x: i64,
    pub y: i64,
}

/// 覆盖视口的瓦片包围盒（闭区间）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TileSpan {
    pub z: u8,
    pub x0: i64,
    pub x1: i64,
    pub y0: i64,
    pub y1: i64,
}

impl TileSpan {
    pub fn cols(&self) -> i64 {
        (self.x1 - self.x0 + 1).max(0)
    }

    pub fn rows(&self) -> i64 {
        (self.y1 - self.y0 + 1).max(0)
    }

    pub fn count(&self) -> usize {
        (self.cols() * self.rows()) as usize
    }

    pub fn contains(&self, key: TileKey) -> bool {
        key.z == self.z
            && (self.x0..=self.x1).contains(&key.x)
            && (self.y0..=self.y1).contains(&key.y)
    }

    /// 逐行遍历（北 → 南、西 → 东）。
    pub fn keys(&self) -> impl Iterator<Item = TileKey> + '_ {
        (self.y0..=self.y1)
            .flat_map(move |y| (self.x0..=self.x1).map(move |x| TileKey { z: self.z, x, y }))
    }
}

/// 瓦片格网的经度边界（西边经度，单位度）。
pub fn tile_lon(x: i64, z: u8) -> f64 {
    x as f64 / (1i64 << z) as f64 * 360.0 - 180.0
}

/// 瓦片格网的纬度边界（北边纬度，单位度，Web 墨卡托）。
pub fn tile_lat(y: i64, z: u8) -> f64 {
    let n = (1i64 << z) as f64;
    let t = std::f64::consts::PI * (1.0 - 2.0 * y as f64 / n);
    t.sinh().atan().to_degrees()
}

/// 瓦片的经纬度包围盒 `(min_lat, min_lon, max_lat, max_lon)`（该源网格坐标）。
pub fn tile_bounds(x: i64, y: i64, z: u8) -> (f64, f64, f64, f64) {
    (
        tile_lat(y + 1, z),
        tile_lon(x, z),
        tile_lat(y, z),
        tile_lon(x + 1, z),
    )
}

/// 视口 → 瓦片包围盒。
///
/// 视口四角用 `MapState::world_bounds`（BD-09），换到该源网格后按 slippy 公式取
/// 索引（复用 `tile_road::tile_xy`）。`margin` 是外扩圈数（默认 1：预取一圈，
/// 拖动时不至于露白）。索引钳在 `[0, 2^z−1]`，跨国际日期线不会 panic。
pub fn viewport_span(
    map: &MapState,
    rect: Rect,
    src: BasemapSource,
    z: u8,
    margin: i64,
) -> TileSpan {
    let (a, b) = map.world_bounds(rect);
    let (lat_a, lon_a) = src.from_bd09(a.lat, a.lon);
    let (lat_b, lon_b) = src.from_bd09(b.lat, b.lon);
    let (xa, ya) = tile_road::tile_xy(lat_a, lon_a, z, TileProjection::WebMercator);
    let (xb, yb) = tile_road::tile_xy(lat_b, lon_b, z, TileProjection::WebMercator);
    let max = (1i64 << z) - 1;
    TileSpan {
        z,
        x0: (xa.min(xb) - margin).clamp(0, max),
        x1: (xa.max(xb) + margin).clamp(0, max),
        y0: (ya.min(yb) - margin).clamp(0, max),
        y1: (ya.max(yb) + margin).clamp(0, max),
    }
}

/// 一张瓦片在屏幕上的轴对齐矩形：**四角过 `MapState::to_screen`**。
///
/// 这是「不改投影」的落点：四个角各自换算，再取 min/max。因为 `to_screen` 对经纬度
/// 是线性的，四角包围盒与逐像素插值完全一致，底图和矢量图不会错位。
pub fn tile_screen_rect(map: &MapState, rect: Rect, src: BasemapSource, key: TileKey) -> Rect {
    let (lat_n, lon_w, lat_s, lon_e) = tile_bounds(key.x, key.y, key.z);
    let corners = [
        (lat_n, lon_w),
        (lat_n, lon_e),
        (lat_s, lon_e),
        (lat_s, lon_w),
    ];
    let mut min = Pos2::new(f32::INFINITY, f32::INFINITY);
    let mut max = Pos2::new(f32::NEG_INFINITY, f32::NEG_INFINITY);
    for (lat, lon) in corners {
        let (bd_lat, bd_lon) = src.to_bd09(lat, lon);
        let p = map.to_screen(rect, bd_lat, bd_lon);
        if !p.x.is_finite() || !p.y.is_finite() {
            return Rect::NOTHING;
        }
        min.x = min.x.min(p.x);
        min.y = min.y.min(p.y);
        max.x = max.x.max(p.x);
        max.y = max.y.max(p.y);
    }
    if max.x <= min.x || max.y <= min.y {
        return Rect::NOTHING;
    }
    Rect::from_min_max(min, max)
}

/// 每米像素 → 小数缩放级别（未截断）。
///
/// 令「一张瓦片像素 = 一个屏幕像素」的 z 为理想值：
/// `scale × 该 z 的地面米/像素 = 1`，其中地面米/像素 = `2·SHIFT·cos φ / (256·2^z)`，
/// 于是 `z = log2(scale × 2·SHIFT·cos φ / 256)`。`tile_road::meters_per_pixel`
/// 是同一个式子的另一头，测试 `zoom_matches_meters_per_pixel` 逐级核对。
pub fn zoom_float(scale: f32, lat: f64) -> f32 {
    let cos = lat.to_radians().cos().abs().clamp(0.05, 1.0);
    let m_per_px_z0 = 2.0 * tile_road::WEB_MERCATOR_SHIFT / TILE_PX as f64;
    let ideal = (scale.max(1e-6) as f64) * m_per_px_z0 * cos;
    if !ideal.is_finite() || ideal <= 0.0 {
        return 0.0;
    }
    ideal.log2() as f32
}

/// 当前缩放应抓哪一级瓦片（四舍五入，上限 `MAX_ZOOM`）。
pub fn zoom_level(scale: f32, lat: f64) -> u8 {
    let z = zoom_float(scale, lat).round();
    if !z.is_finite() {
        return 0;
    }
    z.clamp(0.0, MAX_ZOOM as f32) as u8
}

/// 该级瓦片要放大/缩小多少倍才与当前缩放吻合。
///
/// z 被 `MAX_ZOOM` 截断后这个值会 >1：**超限放大已有 z18 瓦片**，而不是去抓
/// z19/z20（那两级多数源没有，抓回来也是空白，白费流量还更模糊）。
pub fn tile_scale_factor(scale: f32, lat: f64, z: u8) -> f32 {
    let f = (zoom_float(scale, lat) - z as f32).exp2();
    if f.is_finite() {
        f.clamp(0.25, 8.0)
    } else {
        1.0
    }
}

/// 拼一条瓦片 URL（`{host}` 按 `index` 在 1..4 轮换）。
///
/// `Off` 返回 `Err`：没有源就没有 URL，调用方也就无从发请求。
pub fn tile_url(src: BasemapSource, key: TileKey, index: usize) -> Result<String, String> {
    let template = src
        .url_template()
        .ok_or_else(|| "底图已关闭：不会发起瓦片请求".to_string())?;
    let host = AMAP_HOSTS[index % AMAP_HOSTS.len()];
    Ok(template
        .replace("{host}", host)
        .replace("{x}", &key.x.to_string())
        .replace("{y}", &key.y.to_string())
        .replace("{z}", &key.z.to_string()))
}

/// 本帧要抓的瓦片。
///
/// **`Off` 恒为空**——这是「默认关闭时零网络请求」的第一道闸（第二道在 `draw`：
/// 源是 `Off` 直接 return）。
pub fn plan_requests(
    src: BasemapSource,
    span: &TileSpan,
    known: &dyn Fn(TileKey) -> bool,
    limit: usize,
) -> Vec<TileKey> {
    if !src.is_online() || limit == 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(limit.min(span.count()));
    for key in span.keys() {
        if known(key) {
            continue;
        }
        out.push(key);
        if out.len() >= limit {
            break;
        }
    }
    out
}

// ── 磁盘缓存 ────────────────────────────────────────────────────────────

/// `data_dir()/map_cache/{proj}/{z}/{x}/{y}.png`。
///
/// 只缓存「成功且非占位图」的瓦片（判据在后台线程里用
/// `tile_road::is_placeholder_tile`）；上限按 mtime 做 LRU 淘汰。
#[derive(Clone, Debug)]
pub struct TileCache {
    root: PathBuf,
}

impl TileCache {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// 默认根目录：`data_dir()/map_cache`。
    pub fn default_root() -> PathBuf {
        crate::platform::data_dir().join(CACHE_DIR)
    }

    /// 某张瓦片的缓存路径；`Off` 没有缓存目录。
    ///
    /// 路径只由枚举（proj）+ 三个整数（z/x/y）拼成，**不接受外部字符串**，
    /// 所以不存在 `../` 之类的路径注入面。
    pub fn path_for(&self, src: BasemapSource, key: TileKey) -> Option<PathBuf> {
        let proj = src.cache_key()?;
        Some(
            self.root
                .join(proj)
                .join(key.z.to_string())
                .join(key.x.to_string())
                .join(format!("{}.png", key.y)),
        )
    }

    pub fn read(&self, src: BasemapSource, key: TileKey) -> Option<Vec<u8>> {
        let path = self.path_for(src, key)?;
        let bytes = std::fs::read(&path).ok()?;
        if bytes.is_empty() {
            return None;
        }
        Some(bytes)
    }

    /// 原子写：先写唯一临时名再 rename，避免两个线程（跑步页/路网页各一层）撞车。
    ///
    /// **任何一步失败都要删掉临时文件**：Windows 上目标被占用、杀软扫描都会让
    /// `rename` 失败；残留的 `.tmp` 既不计入 `map_cache_limit_mb`、也不会被
    /// `clear()` 删掉，能一直堆（最终盲审 L1）。
    pub fn write(
        &self,
        src: BasemapSource,
        key: TileKey,
        bytes: &[u8],
    ) -> std::io::Result<PathBuf> {
        let path = self.path_for(src, key).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "底图已关闭，不写缓存")
        })?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let tmp = path.with_file_name(format!(
            "{}.{}.{:?}.{}.tmp",
            key.y,
            std::process::id(),
            std::thread::current().id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        if let Err(error) = std::fs::write(&tmp, bytes) {
            let _ = std::fs::remove_file(&tmp);
            return Err(error);
        }
        if let Err(error) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(error);
        }
        Ok(path)
    }

    /// 递归列出所有**已落盘**缓存文件：`(路径, 字节数, mtime 毫秒)`。
    pub fn entries(&self) -> Vec<(PathBuf, u64, u128)> {
        let mut out = Vec::new();
        collect_entries(&self.root, 0, &mut out);
        out
    }

    /// 缓存目录里的临时文件（`*.tmp`）：`(路径, mtime 毫秒)`。
    pub fn temp_files(&self) -> Vec<(PathBuf, u128)> {
        let mut out = Vec::new();
        collect_temps(&self.root, 0, &mut out);
        out
    }

    /// 临时文件个数（诊断/测试）。
    #[allow(dead_code)]
    pub fn temp_count(&self) -> usize {
        self.temp_files().len()
    }

    /// 删掉**过期**临时文件（最后修改早于 `max_age` 之前），返回删除数。
    ///
    /// 口径：正常写入是毫秒级完成的，超过 `TEMP_MAX_AGE`（1 小时）还在的 `.tmp`
    /// 只能是中断的写入残留。正在写的那个 mtime 就是“现在”，不会被误删。
    /// mtime 拿不到时按“过期”处理（宁可多清一个残留，也不要让泄漏无界）。
    pub fn sweep_temps_older_than(&self, max_age: Duration) -> usize {
        let now = now_ms();
        let max_age = max_age.as_millis();
        let mut removed = 0;
        for (path, mtime) in self.temp_files() {
            if now.saturating_sub(mtime) >= max_age && std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
        removed
    }

    /// `(总字节数, 张数)`。顺手清掉过期 `.tmp`，否则它们绕过上限一直堆着。
    pub fn stats(&self) -> (u64, usize) {
        self.sweep_temps_older_than(TEMP_MAX_AGE);
        let entries = self.entries();
        (entries.iter().map(|e| e.1).sum(), entries.len())
    }

    /// 清空缓存目录：`.png` 与 `.tmp` 一并删，返回删除的文件数。
    ///
    /// 用户显式点“清理缓存”时不区分新鲜/过期——残留的 tmp 都该走。
    pub fn clear(&self) -> usize {
        let mut removed = 0;
        for (path, _, _) in self.entries() {
            if std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
        for (path, _) in self.temp_files() {
            if std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
        removed
    }

    /// 超过 `limit` 就按 mtime 从旧到新删，直到总量 ≤ limit。返回删除数。
    pub fn evict_to(&self, limit: u64) -> usize {
        // 先把过期 tmp 清掉，再按 png 做 LRU：否则总量口径里“看不见”的 tmp
        // 会把真实占用顶到上限之上。
        self.sweep_temps_older_than(TEMP_MAX_AGE);
        let victims = plan_eviction(self.entries(), limit);
        let mut removed = 0;
        for path in victims {
            if std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
        removed
    }
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn collect_entries(dir: &Path, depth: usize, out: &mut Vec<(PathBuf, u64, u128)>) {
    if depth > 4 {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            collect_entries(&path, depth + 1, out);
            continue;
        }
        if !kind.is_file() || path.extension().and_then(|e| e.to_str()) != Some("png") {
            continue;
        }
        let meta = entry.metadata().ok();
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let mtime = meta
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis())
            .unwrap_or(0);
        out.push((path, size, mtime));
    }
}

fn collect_temps(dir: &Path, depth: usize, out: &mut Vec<(PathBuf, u128)>) {
    if depth > 4 {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            collect_temps(&path, depth + 1, out);
            continue;
        }
        if !kind.is_file() || path.extension().and_then(|e| e.to_str()) != Some("tmp") {
            continue;
        }
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis())
            .unwrap_or(0);
        out.push((path, mtime));
    }
}

/// 纯函数版的 LRU 淘汰计划：给定 `(路径, 字节, mtime)`，返回要删的路径。
///
/// 单独成函数是为了能离线覆盖策略（真删文件只在 `evict_to` 里做）：
/// 按 mtime 从旧到新、同 mtime 按路径排序，删到总量 ≤ `limit` 为止。
pub fn plan_eviction(mut entries: Vec<(PathBuf, u64, u128)>, limit: u64) -> Vec<PathBuf> {
    let mut total: u64 = entries.iter().map(|e| e.1).sum();
    if total <= limit {
        return Vec::new();
    }
    entries.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.0.cmp(&b.0)));
    let mut victims = Vec::new();
    for (path, size, _) in entries {
        if total <= limit {
            break;
        }
        total = total.saturating_sub(size);
        victims.push(path);
    }
    victims
}

// ── 后台抓取 ────────────────────────────────────────────────────────────

/// 后台线程回传的消息。
enum TileMsg {
    Ready {
        src: BasemapSource,
        key: TileKey,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    Failed {
        src: BasemapSource,
        key: TileKey,
        reason: String,
    },
}

struct Job {
    src: BasemapSource,
    key: TileKey,
    url: String,
    cfg: TileConfig,
    cache: TileCache,
    limit: Arc<AtomicU64>,
}

/// 「这批瓦片抓过、失败了，别再重试」的集合，带 FIFO 上限。
///
/// 为什么要上限：用户长时间拖动会踩到成百上千张坏瓦片，记不住全部；
/// 但**不能整体清空**（最终盲审 L2）—— 清空的下一帧那些瓦片会一次性重新入队
/// （每帧 8 张），对同一批坏瓦片反复抓，比少记几条糟得多。所以超限只淘汰最旧的一条。
#[derive(Default)]
struct FailedSet {
    set: HashSet<TileKey>,
    /// 插入顺序（只用来淘汰最旧的一条）。
    order: VecDeque<TileKey>,
}

impl FailedSet {
    #[allow(dead_code)]
    fn len(&self) -> usize {
        self.set.len()
    }

    #[allow(dead_code)]
    fn contains(&self, key: &TileKey) -> bool {
        self.set.contains(key)
    }

    fn iter(&self) -> impl Iterator<Item = TileKey> + '_ {
        self.set.iter().copied()
    }

    /// 记录一个失败键；超上限时淘汰最旧的一条（先插入、后收敛，长度恒 ≤ 上限）。
    fn insert(&mut self, key: TileKey) {
        if !self.set.insert(key) {
            return;
        }
        self.order.push_back(key);
        while self.order.len() > MAX_FAILED_TILES {
            if let Some(oldest) = self.order.pop_front() {
                self.set.remove(&oldest);
            }
        }
    }

    fn clear(&mut self) {
        self.set.clear();
        self.order.clear();
    }
}

/// 一层底图的状态（跑步页、路网页各一个）。
pub struct TileLayer {
    source: BasemapSource,
    cache: TileCache,
    cache_limit: Arc<AtomicU64>,
    textures: HashMap<TileKey, TextureHandle>,
    /// 已派发、等回包的瓦片。
    pending: HashSet<TileKey>,
    /// 已失败的瓦片：不再重试（`fetch_tile_bytes` 内部已经重试过 1 次）。
    /// FIFO 上限，超了只淘汰最旧的一条。
    failed: FailedSet,
    /// 已解码、等主线程上传的纹理。
    ready: VecDeque<(TileKey, egui::ColorImage)>,
    job_tx: Option<Sender<Job>>,
    tx: Sender<TileMsg>,
    rx: Receiver<TileMsg>,
    max_uploads: usize,
    notice: Option<String>,
    jobs_sent: usize,
    uploads: usize,
    cache_bytes: u64,
    cache_files: usize,
    cache_stat_at: Option<Instant>,
}

impl Default for TileLayer {
    fn default() -> Self {
        Self::new(DEFAULT_CACHE_LIMIT_MB)
    }
}

impl TileLayer {
    pub fn new(limit_mb: u32) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            source: BasemapSource::Off,
            cache: TileCache::new(TileCache::default_root()),
            cache_limit: Arc::new(AtomicU64::new(Self::limit_bytes(limit_mb))),
            textures: HashMap::new(),
            pending: HashSet::new(),
            failed: FailedSet::default(),
            ready: VecDeque::new(),
            job_tx: None,
            tx,
            rx,
            max_uploads: MAX_UPLOADS_PER_FRAME,
            notice: None,
            jobs_sent: 0,
            uploads: 0,
            cache_bytes: 0,
            cache_files: 0,
            cache_stat_at: None,
        }
    }

    pub fn limit_bytes(mb: u32) -> u64 {
        (mb.max(1) as u64) * 1024 * 1024
    }

    /// 当前源（诊断用）。
    #[allow(dead_code)]
    pub fn source(&self) -> BasemapSource {
        self.source
    }

    pub fn set_cache_limit_mb(&mut self, mb: u32) {
        self.cache_limit
            .store(Self::limit_bytes(mb), Ordering::Relaxed);
    }

    /// 累计派发的抓取任务数（举证「关闭时零请求」用）。
    #[allow(dead_code)]
    pub fn jobs_sent(&self) -> usize {
        self.jobs_sent
    }

    #[allow(dead_code)]
    pub fn uploads(&self) -> usize {
        self.uploads
    }

    /// 已上传纹理数（内存占用代理指标）。
    #[allow(dead_code)]
    pub fn texture_count(&self) -> usize {
        self.textures.len()
    }

    pub fn cache_stats(&self) -> (u64, usize) {
        (self.cache_bytes, self.cache_files)
    }

    /// 重新统计缓存占用（走文件系统；界面上限频调用）。
    pub fn refresh_cache_stats(&mut self) {
        self.cache_stat_at = Some(Instant::now());
        let (bytes, files) = self.cache.stats();
        self.cache_bytes = bytes;
        self.cache_files = files;
        let limit = self.cache_limit.load(Ordering::Relaxed);
        if bytes > limit {
            self.cache.evict_to(limit);
            let (bytes, files) = self.cache.stats();
            self.cache_bytes = bytes;
            self.cache_files = files;
        }
    }

    pub fn clear_cache(&mut self) {
        let removed = self.cache.clear();
        self.cache_bytes = 0;
        self.cache_files = 0;
        self.cache_stat_at = Some(Instant::now());
        self.notice = Some(format!("已清理底图缓存：{removed} 张瓦片"));
    }

    /// 取走一条待显示的状态栏提示（失败/清理结果）。
    pub fn take_notice(&mut self) -> Option<String> {
        self.notice.take()
    }

    /// 换源：丢掉上一套纹理与状态（缓存目录不同，不能复用）。
    pub fn reset(&mut self, src: BasemapSource) {
        self.source = src;
        self.textures.clear();
        self.pending.clear();
        self.failed.clear();
        self.ready.clear();
        self.jobs_sent = 0;
        self.uploads = 0;
        self.notice = None;
    }

    /// 绘制底图。返回本帧画了几张瓦片。
    ///
    /// 绝不 `?`/panic：底图失败只影响自己（写 `notice` 给状态栏），矢量图照画。
    pub fn draw(
        &mut self,
        painter: &egui::Painter,
        ctx: &egui::Context,
        map: &MapState,
        rect: Rect,
        src: BasemapSource,
    ) -> usize {
        if src != self.source {
            self.reset(src);
        }
        if !src.is_online() || rect.width() < 1.0 || rect.height() < 1.0 {
            return 0;
        }
        let lat = map.center.0;
        let z = zoom_level(map.scale, lat);
        let span = viewport_span(map, rect, src, z, 1);

        // 1) 收后台结果 → 待上传队列（主线程不做解码/网络）。
        self.drain();

        // 2) 上传纹理，每帧限量。超限放大（z 被截断）时用最近邻，像素块比模糊更清楚。
        let factor = tile_scale_factor(map.scale, lat, z);
        let filter = if factor > 1.5 {
            TextureOptions::NEAREST
        } else {
            TextureOptions::LINEAR
        };
        let mut uploaded = 0usize;
        while uploaded < self.max_uploads {
            let Some((key, image)) = self.ready.pop_front() else {
                break;
            };
            let name = format!("tile-{}-{}-{}-{}", src.as_str(), key.z, key.x, key.y);
            let texture = ctx.load_texture(name, image, filter);
            self.textures.insert(key, texture);
            self.uploads += 1;
            uploaded += 1;
        }

        // 3) 派发缺失的瓦片。
        let known: HashSet<TileKey> = self
            .textures
            .keys()
            .copied()
            .chain(self.pending.iter().copied())
            .chain(self.failed.iter())
            .chain(self.ready.iter().map(|(k, _)| *k))
            .collect();
        let asks = plan_requests(src, &span, &|k| known.contains(&k), MAX_JOBS_PER_FRAME);
        if !asks.is_empty() {
            self.ensure_worker();
        }
        for key in asks {
            let url = match tile_url(src, key, self.jobs_sent) {
                Ok(url) => url,
                Err(reason) => {
                    self.failed.insert(key);
                    self.notice = Some(reason);
                    continue;
                }
            };
            let Some(tx) = self.job_tx.as_ref() else {
                break;
            };
            let job = Job {
                src,
                key,
                url,
                cfg: src.tile_config(),
                cache: self.cache.clone(),
                limit: self.cache_limit.clone(),
            };
            if tx.send(job).is_ok() {
                self.pending.insert(key);
                self.jobs_sent += 1;
            } else {
                // 后台线程没了：这一轮不再尝试，避免每帧空转。
                self.job_tx = None;
                break;
            }
        }

        // 4) 画：只画有纹理且在画布内的。
        let uv = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
        let mut drawn = 0usize;
        for key in span.keys() {
            let Some(texture) = self.textures.get(&key) else {
                continue;
            };
            let target = tile_screen_rect(map, rect, src, key);
            if target == Rect::NOTHING || !rect.intersects(target) {
                continue;
            }
            painter.image(texture.id(), target, uv, Color32::WHITE);
            drawn += 1;
        }

        // 5) 视野外的纹理丢掉（超出上限才清，避免来回拖时反复重传）。
        if self.textures.len() > MAX_TEXTURES {
            self.textures.retain(|k, _| span.contains(*k));
        }
        if !self.pending.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(80));
        }
        drawn
    }

    fn drain(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(TileMsg::Ready {
                    src,
                    key,
                    width,
                    height,
                    rgba,
                }) => {
                    self.pending.remove(&key);
                    // 正常瓦片 256²（@2x 视网膜源 512²）；再大的按异常丢弃，
                    // 免得一张伪造大图在 Android 上顶爆纹理内存。
                    if src != self.source
                        || width == 0
                        || height == 0
                        || width > MAX_TILE_SIDE
                        || height > MAX_TILE_SIDE
                    {
                        continue;
                    }
                    let image = egui::ColorImage::from_rgba_unmultiplied(
                        [width as usize, height as usize],
                        &rgba,
                    );
                    self.ready.push_back((key, image));
                    // 上传跟不上时丢最旧的，防止内存无上限。
                    while self.ready.len() > MAX_TEXTURES {
                        self.ready.pop_front();
                    }
                }
                Ok(TileMsg::Failed { src, key, reason }) => {
                    self.pending.remove(&key);
                    if src == self.source {
                        // 只记「别再重试这一张」；上限由 `FailedSet::insert` 里逐条淘汰，
                        // 绝不整体清空（整体清空会让失败瓦片一次性重新入队、反复抓）。
                        self.failed.insert(key);
                        self.notice = Some(format!("底图加载失败（已重试 1 次）：{reason}"));
                    }
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    fn ensure_worker(&mut self) {
        if self.job_tx.is_some() {
            return;
        }
        let (job_tx, job_rx) = std::sync::mpsc::channel::<Job>();
        let out = self.tx.clone();
        let spawned = std::thread::Builder::new()
            .name("map-tiles".into())
            .spawn(move || worker_loop(job_rx, out))
            .is_ok();
        if spawned {
            self.job_tx = Some(job_tx);
        } else {
            self.notice = Some("底图后台线程启动失败：本轮不加载底图".into());
        }
    }
}

/// 后台线程：磁盘缓存 → 网络抓取（复用 `tile_road` 的全套校验/限额）→ 解码。
fn worker_loop(jobs: Receiver<Job>, out: Sender<TileMsg>) {
    // 每个线程一个 agent（建 TLS 客户端贵，不能每张瓦片新建）。
    let agent = crate::http::Agent::new(Duration::from_secs(15), None);
    let mut writes = 0usize;
    while let Ok(job) = jobs.recv() {
        let key = job.key;
        // 1) 磁盘命中：零网络。
        if let Some(bytes) = job.cache.read(job.src, key) {
            match tile_road::decode_tile_rgba(&bytes) {
                Ok((width, height, rgba)) => {
                    let _ = out.send(TileMsg::Ready {
                        src: job.src,
                        key,
                        width,
                        height,
                        rgba,
                    });
                    continue;
                }
                Err(_) => {
                    // 坏文件：删掉，走网络重下。
                    if let Some(path) = job.cache.path_for(job.src, key) {
                        let _ = std::fs::remove_file(path);
                    }
                }
            }
        }
        let agent = match &agent {
            Ok(agent) => agent,
            Err(reason) => {
                let _ = out.send(TileMsg::Failed {
                    src: job.src,
                    key,
                    reason: format!("无法创建 HTTP 客户端：{reason}"),
                });
                continue;
            }
        };
        // 2) 网络：白名单校验 / 2 MiB 上限 / 占位图判定 / 失败重试 1 次，全在 tile_road 里。
        let bytes = match tile_road::fetch_tile_bytes(agent, &job.cfg, &job.url) {
            Ok(bytes) => bytes,
            Err(reason) => {
                let _ = out.send(TileMsg::Failed {
                    src: job.src,
                    key,
                    reason,
                });
                continue;
            }
        };
        // 3) 只缓存「成功且非占位图」。
        if !tile_road::is_placeholder_tile(&bytes) {
            let _ = job.cache.write(job.src, key, &bytes);
            writes += 1;
            if writes % 16 == 0 {
                job.cache.evict_to(job.limit.load(Ordering::Relaxed));
            }
        }
        match tile_road::decode_tile_rgba(&bytes) {
            Ok((width, height, rgba)) => {
                let _ = out.send(TileMsg::Ready {
                    src: job.src,
                    key,
                    width,
                    height,
                    rgba,
                });
            }
            Err(reason) => {
                let _ = out.send(TileMsg::Failed {
                    src: job.src,
                    key,
                    reason,
                });
            }
        }
    }
}

// ── 工具条 ──────────────────────────────────────────────────────────────

/// 地图上方的底图工具条：切换源 + 缓存占用 + 一键清理。
///
/// 返回 `true` 表示用户改了选择，**调用方负责落盘**（`ui::save_config`）。
pub fn toolbar(
    ui: &mut egui::Ui,
    basemap: &mut String,
    layer: &mut TileLayer,
    limit_mb: u32,
) -> bool {
    layer.set_cache_limit_mb(limit_mb);
    if layer
        .cache_stat_at
        .map_or(true, |t| t.elapsed() > Duration::from_millis(2500))
    {
        layer.refresh_cache_stats();
    }
    let mut changed = false;
    let current = BasemapSource::parse(basemap);
    ui.horizontal_wrapped(|ui| {
        ui.label("底图：");
        for src in [BasemapSource::Off, BasemapSource::Amap, BasemapSource::Osm] {
            if ui.selectable_label(current == src, src.label()).clicked() && current != src {
                *basemap = src.as_str().to_string();
                layer.reset(src);
                changed = true;
            }
        }
        let (bytes, files) = layer.cache_stats();
        ui.label(format!(
            "缓存 {:.1} MB / {} MB，{} 张",
            bytes as f64 / (1024.0 * 1024.0),
            limit_mb,
            files
        ));
        if ui.small_button("清理缓存").clicked() {
            layer.clear_cache();
        }
    });
    if current.is_online() {
        ui.colored_label(
            super::theme::text_dim(),
            "高德=GCJ-02 网格，OSM=WGS84 网格；内部数据 BD-09，贴图前按源换算。缩放超过 z18 用 z18 瓦片放大。",
        );
    } else {
        ui.colored_label(
            super::theme::text_dim(),
            "底图关闭：只画矢量路网，不发起任何瓦片请求（默认）。",
        );
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::super::map::{GestureInput, GestureState, Gestures, MAX_SCALE, MIN_SCALE};
    use super::*;
    use crate::track::geom::{MET_PER_DEG_LAT, MET_PER_DEG_LNG};
    use egui::Vec2;

    fn map_at(lat: f64, lon: f64, scale: f32) -> MapState {
        MapState {
            center: (lat, lon),
            scale,
            offset: Vec2::ZERO,
            dragged: false,
            gesture: Gestures::default(),
        }
    }

    fn canvas(w: f32, h: f32) -> Rect {
        Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(w, h))
    }

    fn temp_dir(tag: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "nsw-tiles-{}-{}-{}",
            tag,
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    // ── 坐标 ────────────────────────────────────────────────────────────

    /// 显示层的 BD-09 → WGS84 必须与全仓唯一的反解同口径，且往返误差 < 1e-6 度。
    ///
    /// 这里刻意断言「同一函数」：底图不再自带一份 BD→WGS（那会是第三份实现，
    /// 与 `draw_track` 写 `.osm` 的口径迟早漂移）。
    #[test]
    fn bd09_wgs84_display_path_is_the_shared_inverse() {
        for &(lat, lon) in &[
            (39.9, 116.4),
            (22.54, 114.05),
            (31.2304, 121.4737),
            (38.901678, 121.540241),
        ] {
            // 底图显示路径（OSM 网格）。
            let (w_lat, w_lon) = BasemapSource::Osm.from_bd09(lat, lon);
            let (s_lat, s_lon) = crate::api::school_roads::to_wgs(lat, lon);
            assert_eq!(
                (w_lat, w_lon),
                (s_lat, s_lon),
                "底图 BD→WGS 必须复用 school_roads::to_wgs"
            );
            // 正向变换（wgs84_to_bd09）往返 < 1e-6 度。
            let (back_lat, back_lon) = crate::track::geom::wgs84_to_bd09(w_lat, w_lon);
            assert!(
                (back_lat - lat).abs() < 1e-6,
                "lat 往返误差 {} deg",
                (back_lat - lat).abs()
            );
            assert!(
                (back_lon - lon).abs() < 1e-6,
                "lon 往返误差 {} deg",
                (back_lon - lon).abs()
            );
            // 两套网格确实不同源：GCJ-02 与 WGS84 的差在数百米量级，不能是 0。
            let (g_lat, g_lon) = BasemapSource::Amap.from_bd09(lat, lon);
            assert!(
                (g_lat - w_lat).abs() > 1e-5 || (g_lon - w_lon).abs() > 1e-5,
                "高德/OSM 网格差为 0，坐标系没分开"
            );
        }
    }

    #[test]
    fn bd09_to_gcj02_reuses_wire_implementation() {
        // 复用而不是抄一份：数值必须与 wire.rs 的既有向量逐位一致
        // （fixture 与 track/mod.rs 的 `test_bd09_to_gcj02_vector` 同一组）。
        let (g_lat, g_lon) = bd09_to_gcj02(38.901678, 121.540241);
        assert!((g_lat - 38.8956025774013).abs() < 1e-9, "lat={g_lat}");
        assert!((g_lon - 121.5337497718317).abs() < 1e-9, "lon={g_lon}");
        assert_eq!(
            BasemapSource::Amap.from_bd09(38.901678, 121.540241),
            (g_lat, g_lon)
        );
        // 往返（GCJ → BD）也要闭合到 1e-6 度：BD↔GCJ 是**近似**对，本身精度就到
        // 1e-7 度量级（见 track/mod.rs 的 `test_coord_datum_roundtrip`）。
        let (bd_lat, bd_lon) = BasemapSource::Amap.to_bd09(g_lat, g_lon);
        assert!((bd_lat - 38.901678).abs() < 1e-6 && (bd_lon - 121.540241).abs() < 1e-6);
    }

    #[test]
    fn tile_bounds_are_contiguous_and_one_tile_tall() {
        // z18 一张瓦片约 117 m（北纬 40°），格网相邻瓦片首尾相接。
        let (min_lat, min_lon, max_lat, max_lon) = tile_bounds(215_830, 99_328, 18);
        assert!(max_lat > min_lat && max_lon > min_lon);
        assert!((tile_lon(215_831, 18) - max_lon).abs() < 1e-12);
        assert!((tile_lat(99_329, 18) - min_lat).abs() < 1e-12);
        let height_m = (max_lat - min_lat) * MET_PER_DEG_LAT;
        let expect =
            TILE_PX as f64 * tile_road::meters_per_pixel(max_lat, 18, TileProjection::WebMercator);
        assert!(
            (height_m - expect).abs() < 0.5,
            "瓦片高度 {height_m} m 与 meters_per_pixel {expect} m 不一致"
        );
    }

    // ── z 反算 ──────────────────────────────────────────────────────────

    #[test]
    fn zoom_matches_meters_per_pixel() {
        // 让「1 瓦片像素 = 1 屏幕像素」的 scale 反算回来必须正好是那一级。
        for z in 1..=18u8 {
            for lat in [22.5_f64, 31.2, 39.9, 45.0] {
                let mpp = tile_road::meters_per_pixel(lat, z, TileProjection::WebMercator);
                let scale = (1.0 / mpp) as f32;
                assert_eq!(zoom_level(scale, lat), z, "z={z} lat={lat}");
                assert!(
                    (zoom_float(scale, lat) - z as f32).abs() < 1e-3,
                    "z={z} lat={lat} 反算 {}",
                    zoom_float(scale, lat)
                );
                // 正好 1:1 时不需要放大。
                assert!((tile_scale_factor(scale, lat, z) - 1.0).abs() < 1e-3);
            }
        }
        // 上限 18：再放大也停在 z18，并给出 >1 的放大倍率（继续用 z18 瓦片）。
        let huge = 50.0f32;
        assert_eq!(zoom_level(huge, 39.9), MAX_ZOOM);
        let factor = tile_scale_factor(huge, 39.9, MAX_ZOOM);
        assert!(factor > 1.0, "超限放大倍率 {factor}");
        // 单调：scale 越大 z 越大。
        let mut prev = 0;
        for k in 0..12 {
            let z = zoom_level(0.05 * (1.7f32).powi(k), 39.9);
            assert!(z >= prev, "z 不单调：{z} < {prev}");
            prev = z;
        }
    }

    // ── 视口 → 瓦片包围盒 ───────────────────────────────────────────────

    #[test]
    fn viewport_span_covers_center_tile() {
        let map = map_at(39.9, 116.4, 2.0);
        let rect = canvas(400.0, 300.0);
        let z = zoom_level(map.scale, map.center.0);
        let span = viewport_span(&map, rect, BasemapSource::Osm, z, 0);
        assert_eq!(span.z, z);
        assert!((1..=16).contains(&span.count()), "count={}", span.count());

        // 视口中心点所在瓦片必须被覆盖。
        let (w_lat, w_lon) = BasemapSource::Osm.from_bd09(39.9, 116.4);
        let (cx, cy) = tile_road::tile_xy(w_lat, w_lon, z, TileProjection::WebMercator);
        assert!(
            span.contains(TileKey { z, x: cx, y: cy }),
            "中心瓦片 ({cx},{cy}) 不在 span {span:?}"
        );

        // 外扩一圈后只多一圈，并且不会越界。
        let wide = viewport_span(&map, rect, BasemapSource::Osm, z, 1);
        assert!(wide.x0 <= span.x0 && wide.x1 >= span.x1);
        assert_eq!(wide.x1 - wide.x0, span.x1 - span.x0 + 2);
        assert!(wide.x0 >= 0 && wide.y0 >= 0);
        assert!(wide.x1 < (1i64 << z) && wide.y1 < (1i64 << z));

        // z=0 只有一张瓦片，钳位不能越界。
        let z0 = viewport_span(&map, rect, BasemapSource::Osm, 0, 3);
        assert_eq!((z0.x0, z0.x1, z0.y0, z0.y1), (0, 0, 0, 0));

        // 视口越大，覆盖越多。
        let bigger = viewport_span(&map, canvas(1600.0, 1200.0), BasemapSource::Osm, z, 0);
        assert!(bigger.count() >= span.count());

        // 高德源同一 BD-09 视口下也覆盖它自己的中心瓦片（GCJ-02 偏移约 500 m）。
        let amap = viewport_span(&map, rect, BasemapSource::Amap, z, 0);
        let (g_lat, g_lon) = BasemapSource::Amap.from_bd09(39.9, 116.4);
        let (ax, ay) = tile_road::tile_xy(g_lat, g_lon, z, TileProjection::WebMercator);
        assert!(amap.contains(TileKey { z, x: ax, y: ay }));
    }

    // ── 贴图对齐 ────────────────────────────────────────────────────────

    #[test]
    fn tile_rect_is_built_from_projected_corners() {
        let map = map_at(39.9, 116.4, 3.0);
        let rect = canvas(400.0, 400.0);
        let z = zoom_level(map.scale, map.center.0);
        let span = viewport_span(&map, rect, BasemapSource::Osm, z, 0);
        let key = TileKey {
            z,
            x: span.x0,
            y: span.y0,
        };
        let r = tile_screen_rect(&map, rect, BasemapSource::Osm, key);
        assert_ne!(r, Rect::NOTHING);
        // 四角：瓦片经纬度角点过 `to_screen`，矩形必须是这四个点的外接框。
        let (lat_n, lon_w, lat_s, lon_e) = tile_bounds(key.x, key.y, key.z);
        let project = |lat: f64, lon: f64| {
            let (bd_lat, bd_lon) = BasemapSource::Osm.to_bd09(lat, lon);
            map.to_screen(rect, bd_lat, bd_lon)
        };
        let corners = [
            project(lat_n, lon_w),
            project(lat_n, lon_e),
            project(lat_s, lon_e),
            project(lat_s, lon_w),
        ];
        let bbox = Rect::from_min_max(
            Pos2::new(
                corners.iter().map(|p| p.x).fold(f32::INFINITY, f32::min),
                corners.iter().map(|p| p.y).fold(f32::INFINITY, f32::min),
            ),
            Pos2::new(
                corners
                    .iter()
                    .map(|p| p.x)
                    .fold(f32::NEG_INFINITY, f32::max),
                corners
                    .iter()
                    .map(|p| p.y)
                    .fold(f32::NEG_INFINITY, f32::max),
            ),
        );
        assert!(
            (r.min - bbox.min).length() < 1e-3 && (r.max - bbox.max).length() < 1e-3,
            "矩形 {r:?} 与四角外接框 {bbox:?} 不一致"
        );
        // 每个角都贴在这个矩形的边上（BD↔WGS 的交叉项带来的偏差 < 0.05 px）。
        for p in corners {
            assert!(
                p.x >= r.min.x - 0.05 && p.x <= r.max.x + 0.05,
                "角点 x={} 越出 [{}, {}]",
                p.x,
                r.min.x,
                r.max.x
            );
            assert!(
                p.y >= r.min.y - 0.05 && p.y <= r.max.y + 0.05,
                "角点 y={} 越出 [{}, {}]",
                p.y,
                r.min.y,
                r.max.y
            );
        }
        // 纵向尺寸 ≈ 256 px × 放大倍率。残差（本例 ~0.9%）不是投影问题，而是
        // BD↔WGS/GCJ 换算的经纬耦合：四个角点的经纬度各自带一点交叉项，包围盒因此
        // 比理想值略大。后果只是相邻瓦片重叠 ≤3 px（上下边取的是同一组角点，
        // 只会重叠、不会留白），肉眼不可见。
        let factor = tile_scale_factor(map.scale, map.center.0, z);
        let expect_h = TILE_PX as f32 * factor;
        assert!(
            (r.height() - expect_h).abs() < expect_h * 0.02,
            "高 {} 期望 ≈ {expect_h}",
            r.height()
        );
        // 宽度同理（横向每度米数是既有的 MET_PER_DEG_LNG）。
        let expect_w = (lon_e - lon_w) as f32 * MET_PER_DEG_LNG as f32 * map.scale;
        assert!(
            (r.width() - expect_w).abs() < expect_w * 0.02,
            "宽 {} 期望 ≈ {expect_w}",
            r.width()
        );
    }

    /// 线性近似（`map.rs` 的线性投影）与真 Mercator 的偏差 —— 只量**曲率**。
    ///
    /// 为什么只量曲率：底图与矢量图共用同一条线性映射，瓦片逐块按自身经纬度边界
    /// 贴图（`tile_rect_is_built_from_projected_corners`），所以「全局比例常数」
    /// 不产生任何相对错位；真正会露出来的是「曲面 vs 弦」的矢高。
    ///
    /// 结论：矢高 ∝ 跨度²。手机竖屏 z18 一侧 1920 px（地面 ~880 m）仅 0.03 px；
    /// 把「3 km 校园」硬塞进 z18 需要 ~6500 px 宽的屏幕，此时 0.33 px —— 仍亚像素。
    #[test]
    fn linear_projection_deviation_is_subpixel() {
        const R: f64 = 6_378_137.0;
        let lat0 = 39.9_f64;
        let merc =
            |lat: f64| R * ((std::f64::consts::FRAC_PI_4 + lat.to_radians() / 2.0).tan()).ln();
        // 给定 z 与「屏幕像素边长」→ 该屏幕覆盖的地面米数。
        let span_m = |z: u8, screen_px: f64| {
            screen_px
                * tile_road::mercator_m_per_px(z, TileProjection::WebMercator)
                * lat0.to_radians().cos()
        };
        // 真 Mercator 曲线与「两端点连线」之间的最大偏差（px，按该 z 的瓦片像素）。
        let deviation_px = |z: u8, span: f64| -> f64 {
            let mpp = tile_road::mercator_m_per_px(z, TileProjection::WebMercator);
            let dlat = span / MET_PER_DEG_LAT;
            let (a, b) = (lat0 - dlat / 2.0, lat0 + dlat / 2.0);
            let (ya, yb) = (merc(a) - merc(lat0), merc(b) - merc(lat0));
            let mut worst: f64 = 0.0;
            for i in 0..=400 {
                let t = i as f64 / 400.0;
                let lat = a + (b - a) * t;
                let exact = (merc(lat) - merc(lat0)) / mpp;
                let chord = (ya + (yb - ya) * t) / mpp;
                worst = worst.max((exact - chord).abs());
            }
            worst
        };
        let one_tile = deviation_px(18, span_m(18, 256.0));
        let phone = deviation_px(18, span_m(18, 1920.0));
        let tablet = deviation_px(18, span_m(18, 3000.0));
        let campus_z15 = deviation_px(15, span_m(15, 800.0));
        let campus_at_z18 = deviation_px(18, 3000.0);
        println!(
            "对齐偏差(px)：单张瓦片 z18 256px→{one_tile:.4}；手机 z18 1920px→{phone:.4}；\
             平板 z18 3000px→{tablet:.4}；z15 3km(800px 屏)→{campus_z15:.4}；\
             3km 硬塞进 z18(≈6566px 屏)→{campus_at_z18:.4}"
        );
        assert!(one_tile < 0.01, "单瓦片偏差 {one_tile} px");
        assert!(phone < 0.2, "手机 z18 偏差 {phone} px");
        assert!(tablet < 0.2, "平板 z18 偏差 {tablet} px");
        assert!(campus_z15 < 0.2, "z15 3km 偏差 {campus_z15} px");
        // 极端组合（3 km 在 z18 上是 6500+ px 宽，真实屏幕放不下）仍是亚像素。
        assert!(campus_at_z18 < 0.5, "3km@z18 偏差 {campus_at_z18} px");
    }

    // ── URL / 白名单 ────────────────────────────────────────────────────

    #[test]
    fn amap_url_rotates_hosts_and_passes_whitelist() {
        let cfg = BasemapSource::Amap.tile_config();
        let key = TileKey {
            z: 18,
            x: 215_830,
            y: 99_328,
        };
        for index in 0..8usize {
            let url = tile_url(BasemapSource::Amap, key, index).unwrap();
            let n = index % 4 + 1;
            assert!(url.contains(&format!("webrd0{n}.is.autonavi.com")), "{url}");
            assert!(url.contains("appmaptile?lang=zh_cn"), "{url}");
            assert!(url.contains("x=215830") && url.contains("y=99328") && url.contains("z=18"));
            tile_road::validate_tile_url(&url, &cfg).unwrap();
        }
        // 白名单是显式的：把 autonavi.com 拿掉就必须被拒。
        let mut strict = BasemapSource::Amap.tile_config();
        strict.allowed_hosts.clear();
        assert!(tile_road::validate_tile_url(
            &tile_url(BasemapSource::Amap, key, 0).unwrap(),
            &strict
        )
        .is_err());
        // OSM 走既有默认模板。
        let osm = tile_url(BasemapSource::Osm, key, 0).unwrap();
        assert_eq!(osm, "https://tile.openstreetmap.org/18/215830/99328.png");
        tile_road::validate_tile_url(&osm, &BasemapSource::Osm.tile_config()).unwrap();
        // http / 私网仍被拒（底图不多开一个 SSRF 口子）。
        for bad in [
            "http://127.0.0.1:8080/1/2/3.png",
            "https://192.168.1.9/18/1/2.png",
            "https://localhost/18/1/2.png",
        ] {
            assert!(
                tile_road::validate_tile_url(bad, &cfg).is_err(),
                "{bad} 应被拒"
            );
        }
    }

    // ── 默认 off 零请求 ─────────────────────────────────────────────────

    #[test]
    fn default_off_never_requests_network() {
        use crate::api::model::Config;
        assert_eq!(Config::default().basemap, "off");
        assert_eq!(
            BasemapSource::parse(&Config::default().basemap),
            BasemapSource::Off
        );
        assert_eq!(BasemapSource::parse(""), BasemapSource::Off);
        assert_eq!(BasemapSource::parse("bogus"), BasemapSource::Off);
        assert!(BasemapSource::Off.url_template().is_none());
        assert!(BasemapSource::Off.cache_key().is_none());
        assert!(!BasemapSource::Off.is_online());

        // 计划恒空 → 没有 URL、没有 job、没有请求。
        let span = TileSpan {
            z: 18,
            x0: 0,
            x1: 9,
            y0: 0,
            y1: 9,
        };
        let nothing: &dyn Fn(TileKey) -> bool = &|_| false;
        assert!(plan_requests(BasemapSource::Off, &span, nothing, 64).is_empty());
        assert!(BasemapSource::Off
            .tile_config()
            .url_template
            .contains("{x}"));
        assert!(tile_url(BasemapSource::Off, TileKey { z: 18, x: 1, y: 1 }, 0).is_err());
        // 对照：开源的同一 span 有请求计划（证明上面的空不是因为 span 空）。
        assert_eq!(
            plan_requests(BasemapSource::Osm, &span, nothing, 64).len(),
            64
        );

        // 新建的层在 off 下不派发任务、不上传纹理。
        let layer = TileLayer::new(DEFAULT_CACHE_LIMIT_MB);
        assert_eq!(layer.source(), BasemapSource::Off);
        assert_eq!(layer.jobs_sent(), 0);
        assert_eq!(layer.texture_count(), 0);
    }

    // ── 缓存 ────────────────────────────────────────────────────────────

    #[test]
    fn cache_layout_is_data_dir_map_cache() {
        let dir = temp_dir("layout");
        let cache = TileCache::new(dir.clone());
        let path = cache
            .path_for(BasemapSource::Amap, TileKey { z: 18, x: 3, y: 4 })
            .unwrap();
        assert_eq!(path, dir.join("amap").join("18").join("3").join("4.png"));
        assert!(cache
            .path_for(BasemapSource::Off, TileKey { z: 1, x: 1, y: 1 })
            .is_none());
        // 写 → 读 → 统计 → 清空。
        cache
            .write(
                BasemapSource::Osm,
                TileKey { z: 18, x: 1, y: 2 },
                &[7u8; 700],
            )
            .unwrap();
        assert_eq!(
            cache
                .read(BasemapSource::Osm, TileKey { z: 18, x: 1, y: 2 })
                .unwrap()
                .len(),
            700
        );
        assert_eq!(cache.stats(), (700, 1));
        assert_eq!(cache.clear(), 1);
        assert_eq!(cache.stats(), (0, 0));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lru_plan_deletes_oldest_first() {
        let entries = vec![
            (PathBuf::from("a.png"), 1000u64, 10u128),
            (PathBuf::from("b.png"), 1000, 20),
            (PathBuf::from("c.png"), 1000, 30),
            (PathBuf::from("d.png"), 1000, 5),
        ];
        // 总量 4000，限 2500：删最旧的两张（d=5、a=10）。
        let victims = plan_eviction(entries, 2500);
        assert_eq!(
            victims,
            vec![PathBuf::from("d.png"), PathBuf::from("a.png")]
        );
        // 不超限就一个都不删。
        let entries = vec![(PathBuf::from("a.png"), 100u64, 1u128)];
        assert!(plan_eviction(entries, 100).is_empty());
        // 同一 mtime 时按路径稳定排序（不会随机挑一张）。
        let entries = vec![
            (PathBuf::from("z.png"), 10u64, 7u128),
            (PathBuf::from("a.png"), 10, 7),
        ];
        assert_eq!(plan_eviction(entries, 10), vec![PathBuf::from("a.png")]);
    }

    #[test]
    fn cache_evicts_files_down_to_limit() {
        let dir = temp_dir("lru-files");
        let cache = TileCache::new(dir.clone());
        for i in 0..5i64 {
            cache
                .write(
                    BasemapSource::Osm,
                    TileKey { z: 18, x: i, y: 0 },
                    &vec![1u8; 1000],
                )
                .unwrap();
            std::thread::sleep(Duration::from_millis(25));
        }
        let (total, files) = cache.stats();
        assert_eq!(files, 5, "total={total}");
        assert!(total >= 5000, "total={total}");
        // 上限 3000：必须真的降到 ≤ 3000，且只留最新的 3 张。
        let removed = cache.evict_to(3000);
        let (total, files) = cache.stats();
        assert!(total <= 3000, "淘汰后仍超限：{total}");
        assert_eq!(files, 3, "removed={removed}");
        // 最新那张一定还在，最旧那张一定没了（同 mtime 也按路径稳定排序）。
        assert!(cache
            .path_for(BasemapSource::Osm, TileKey { z: 18, x: 4, y: 0 })
            .unwrap()
            .exists());
        assert!(!cache
            .path_for(BasemapSource::Osm, TileKey { z: 18, x: 0, y: 0 })
            .unwrap()
            .exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn placeholder_tiles_are_not_cached() {
        // 占位图判据复用 tile_road：几十/几百字节的单色 PNG 不算成功。
        assert!(tile_road::is_placeholder_tile(&vec![0u8; 197]));
        assert!(!tile_road::is_placeholder_tile(&vec![0u8; 4096]));
    }

    /// 最终盲审 L1：`.tmp` 不能成为绕过缓存上限的泄漏面。
    ///
    /// 三条都要成立：写入失败当场清 tmp；过期残留会被扫掉；用户点「清理缓存」连
    /// 正在写的 tmp 也一起清。同时不能把「正在写的」误删（stats 只看过期）。
    #[test]
    fn temp_files_are_cleaned_and_never_leak() {
        let dir = temp_dir("tmp-leak");
        let cache = TileCache::new(dir.clone());
        let key = TileKey { z: 18, x: 7, y: 9 };
        // 成功路径不留 tmp。
        cache.write(BasemapSource::Osm, key, &[1u8; 900]).unwrap();
        assert_eq!(cache.temp_count(), 0, "成功写入不该留下 tmp");

        // ① rename 失败分支：目标位置被占住（Windows 上目标被打开/杀软扫描就是这种），
        //    必须顺手把 tmp 删掉，不能留成「看不见的残留」。
        let blocked = TileKey { z: 18, x: 8, y: 0 };
        let blocked_path = cache.path_for(BasemapSource::Osm, blocked).unwrap();
        std::fs::create_dir_all(&blocked_path).unwrap();
        assert!(
            cache
                .write(BasemapSource::Osm, blocked, &[3u8; 500])
                .is_err(),
            "目标位置是目录时 rename 必须失败"
        );
        assert_eq!(cache.temp_count(), 0, "rename 失败后必须清掉 tmp");

        // ② 中断残留（进程被杀等）：按「过期」扫掉。
        let path = cache.path_for(BasemapSource::Osm, key).unwrap();
        let stale = path.with_file_name("9.999.1.0.tmp");
        std::fs::write(&stale, vec![2u8; 4096]).unwrap();
        assert_eq!(cache.sweep_temps_older_than(Duration::ZERO), 1);
        assert!(!stale.exists());

        // 正在写的 tmp（mtime 就是现在）不能被自动路径误删。
        let fresh = path.with_file_name("9.999.1.1.tmp");
        std::fs::write(&fresh, vec![2u8; 4096]).unwrap();
        let _ = cache.stats();
        // 上限给足：只验证「淘汰路径也会顺带扫 tmp」，别把 png 清掉。
        let _ = cache.evict_to(u64::MAX);
        assert!(fresh.exists(), "stats/evict 不该删正在写的 tmp");

        // ③ 用户显式「清理缓存」：png 与 tmp 一起删干净。
        let removed = cache.clear();
        assert!(!fresh.exists(), "clear 之后 tmp 必须不存在");
        assert_eq!(cache.temp_count(), 0);
        assert_eq!(cache.stats(), (0, 0));
        assert!(removed >= 2, "removed={removed}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 最终盲审 L2：失败集溢出只淘汰最旧的一条，不许整体清空。
    #[test]
    fn failed_set_evicts_one_key_instead_of_clearing() {
        let key = |x: i64| TileKey { z: 18, x, y: 0 };
        let mut failed = FailedSet::default();
        for x in 0..MAX_FAILED_TILES as i64 {
            failed.insert(key(x));
        }
        assert_eq!(failed.len(), MAX_FAILED_TILES);
        // 再塞 3 条：只淘汰最旧的 3 条，其余全部还在（没有整体清空）。
        for x in 1000..1003i64 {
            failed.insert(key(x));
        }
        assert!(
            failed.len() <= MAX_FAILED_TILES,
            "上限被突破：{}",
            failed.len()
        );
        for x in 0..3i64 {
            assert!(!failed.contains(&key(x)), "最旧的 {x} 应被淘汰");
        }
        assert!(failed.contains(&key(3)), "第 4 条仍应记住");
        assert!(failed.contains(&key(1002)), "最新的仍应记住");
        assert_eq!(failed.len(), MAX_FAILED_TILES, "不能整体清空");
        // 重复插入同一键不改变长度，也不挤掉别人。
        failed.insert(key(1002));
        assert_eq!(failed.len(), MAX_FAILED_TILES);
        assert!(failed.contains(&key(3)));
    }

    // ── 手势状态机 ──────────────────────────────────────────────────────

    #[test]
    fn pinch_gesture_state_machine() {
        let mut g = Gestures::default();
        // 双指：zoom_delta 1.1，同时指针位移非零 → 只缩放、不平移（中点漂移不跟）。
        let out = g.update(GestureInput {
            inside: true,
            down: true,
            dragging: true,
            delta: Vec2::new(9.0, -4.0),
            zoom: 1.1,
        });
        assert_eq!(out.state, GestureState::Pinching);
        assert!((out.zoom - 1.1).abs() < 1e-6);
        assert_eq!(out.pan, Vec2::ZERO);
        assert!(!out.dragged);
        // 捏合过的这一手势不算 tap。
        let out = g.update(GestureInput {
            inside: true,
            down: false,
            ..Default::default()
        });
        assert!(!out.tapped);
        assert_eq!(out.state, GestureState::Idle);

        // 单指轻点：按下 → 抬起，中间没有拖拽判定 → tap，全程不平移。
        let mut g = Gestures::default();
        let out = g.update(GestureInput {
            inside: true,
            down: true,
            ..Default::default()
        });
        assert!(!out.dragged && !out.tapped);
        let out = g.update(GestureInput {
            inside: true,
            down: true,
            delta: Vec2::new(0.5, 0.0), // 手指按下时的抖动
            ..Default::default()
        });
        assert!(!out.dragged, "未达拖拽阈值时不许平移");
        let out = g.update(GestureInput {
            inside: true,
            down: false,
            ..Default::default()
        });
        assert!(out.tapped, "轻点必须被识别为 tap");

        // 真拖拽：明显位移 → 平移；松手不是 tap。
        let mut g = Gestures::default();
        g.update(GestureInput {
            inside: true,
            down: true,
            ..Default::default()
        });
        let out = g.update(GestureInput {
            inside: true,
            down: true,
            dragging: true,
            delta: Vec2::new(30.0, 0.0),
            zoom: 1.0,
        });
        assert_eq!(out.state, GestureState::Dragging);
        assert!(out.dragged);
        assert_eq!(out.pan, Vec2::new(30.0, 0.0));
        let out = g.update(GestureInput {
            inside: true,
            down: false,
            ..Default::default()
        });
        assert!(!out.tapped, "拖动后松手不是 tap");

        // 画布外的手势一律忽略（不会平移、不缩放）。
        let mut g = Gestures::default();
        let out = g.update(GestureInput {
            inside: false,
            down: true,
            dragging: true,
            delta: Vec2::new(50.0, 50.0),
            zoom: 1.4,
        });
        assert_eq!(out.pan, Vec2::ZERO);
        assert!((out.zoom - 1.0).abs() < 1e-6);
        assert!(!out.dragged);
    }

    #[test]
    fn zoom_about_keeps_anchor_fixed_and_clamps() {
        let mut map = map_at(39.9, 116.4, 1.0);
        map.offset = Vec2::new(12.0, -7.0);
        let rect = Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(400.0, 300.0));
        let (lat, lon) = (39.9012, 116.4023);
        let before = map.to_screen(rect, lat, lon);
        map.zoom_about(rect, before, 1.7);
        let after = map.to_screen(rect, lat, lon);
        assert!(
            (after - before).length() < 0.01,
            "缩放后锚点漂移 {} px",
            (after - before).length()
        );
        assert!((map.scale - 1.7).abs() < 1e-6);
        // 边界钳位。
        map.zoom_about(rect, rect.center(), 1e9);
        assert!(map.scale <= MAX_SCALE);
        map.zoom_about(rect, rect.center(), 1e-9);
        assert!(map.scale >= MIN_SCALE);
    }
}
