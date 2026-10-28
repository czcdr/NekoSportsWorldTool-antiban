//! 手绘自定义路网：在地图上画一条闭合轨迹 → 存成标准 OSM XML（`.osm`）→
//! 交给既有本地路网通道（`osm.rs::App::load_osm_async`）加载。
//!
//! 为什么要有这一页：自动获取学校路网依赖 Overpass 与学校围栏，两者都拿不到时
//! 「真实道路路由」就完全用不了。手绘是最低成本的兜底——用户按校园里实际跑圈
//! 的形状点几下，生成一份只含 `highway=footway` 的 way，路网加载器照常解析，
//! 跑步页照常沿它走，不需要新增任何 RouteMode。
//!
//! 坐标约定（重要）：
//! - 画布与内部数据一致，全部是 **BD-09**（打卡点、预览、围栏同系）。
//! - `.osm` 必须是 **WGS84**，所以写盘前逐点过
//!   [`crate::api::school_roads::to_wgs`]（就是 `wgs84_to_bd09` 的迭代逆变换，
//!   与瓦片/围栏显示共用**同一份**实现，不在这里再抄一遍）。
//! - 节点 id 从 1 开始**全是正数**：本项目历史上出现过「负数 id 的 way 被整份
//!   丢弃」的坑，手绘文件不给自己埋雷（解析器现在也认负数了，但导出仍守正数）。
//!
//! 状态存在 egui 的临时存储里（按 `Id` 取），不往 `App` 上加字段：这样本模块
//! 与 `run.rs` / `map.rs` 的并发改动互不影响，模块声明只占 `mod.rs` 一行。

use eframe::egui;
use egui::{Pos2, Vec2};

use super::map::MapState;
use super::{theme, App};
use crate::track::geom::{MET_PER_DEG_LAT, MET_PER_DEG_LNG};

/// 少于这个点数既不能闭合也不能保存。
pub const MIN_POINTS: usize = 3;
/// 与 `school_roads::MAX_BYTES` / `generate_road::load_network_path` 同一个额度：
/// 超过它的文件在加载入口就会被拒，写盘阶段先挡住，避免「保存成功但永远加载失败」。
pub const MAX_OSM_BYTES: u64 = 24 * 1024 * 1024;
/// 顶点命中半径（像素）：按手指取 20（直径 40 px，接近 44 px 的触控目标），
/// 又不至于把相邻点全吸过来。
const VERTEX_HIT_PX: f32 = 20.0;
/// 判定「这是点击而不是拖拽」的位移阈值（像素）。
const TAP_SLOP_PX: f32 = 8.0;
/// 长按删点的按住时长。
const LONG_PRESS: std::time::Duration = std::time::Duration::from_millis(600);
/// 右键（长按）删点的地理判定半径，和参考实现一致：120 米内找不到就明确提示。
const DELETE_RADIUS_M: f64 = 120.0;
/// 按住拖动画线时的采样间距：太密会生成几万个节点，手机解析会卡。
const STROKE_SAMPLE_M: f64 = 4.0;
/// 单击加点的最小间距：连点两下同一个位置会产生零长度边，直接忽略。
const TAP_MIN_SPACING_M: f64 = 0.5;
/// 松开后按距离抽稀：保留间距 ≥ 该值的点（首尾必留）。
const STROKE_KEEP_M: f64 = 5.0;

/// 指针手势的当前阶段（跨帧保存）。
#[derive(Clone)]
struct Press {
    pos: Pos2,
    at: std::time::Instant,
    /// 按下时命中的顶点下标；None = 按在空白处。
    hit: Option<usize>,
    moved: bool,
    /// 拖拽画线的采样点（世界坐标 BD-09）。
    stroke: Vec<(f64, f64)>,
}

/// 画布视图（不复用 `MapState` 存状态：它没有 Clone，egui 临时存储要求 Clone）。
#[derive(Clone)]
struct MapView {
    center: (f64, f64),
    scale: f32,
    offset: Vec2,
}

impl Default for MapView {
    fn default() -> Self {
        Self {
            center: (0.0, 0.0),
            scale: 0.35,
            offset: Vec2::ZERO,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DrawMode {
    /// 单击加点 / 按住直接画线；拖动空白处仍是平移视野。
    Tap,
    /// 拖拽只移视野，不加点（想仔细对齐时用）。
    Pan,
}

/// 绘制页的全部内存状态。
#[derive(Clone)]
pub struct DrawTrack {
    /// 标点（BD-09），顺序即行进顺序。
    pub points: Vec<(f64, f64)>,
    /// 是否已闭合（闭合才允许保存：路网要能绕圈）。
    pub closed: bool,
    pub mode: DrawMode,
    /// 保存文件名（不含扩展名）。
    pub name: String,
    pub status: String,
    pub last_saved: Option<String>,
    view: MapView,
    view_ready: bool,
    drag: Option<usize>,
    press: Option<Press>,
    /// 最近一帧的画布矩形（状态机测试与调试用；不影响交互语义）。
    canvas: Option<egui::Rect>,
}

impl Default for DrawTrack {
    fn default() -> Self {
        Self {
            points: Vec::new(),
            closed: false,
            mode: DrawMode::Tap,
            name: "自定义轨迹".into(),
            status: String::new(),
            last_saved: None,
            view: MapView::default(),
            view_ready: false,
            drag: None,
            press: None,
            canvas: None,
        }
    }
}

/// 两点平面距离（米），与 `MapState` 投影使用同一组常量。
fn segment_m(a: (f64, f64), b: (f64, f64)) -> f64 {
    let dy = (a.0 - b.0) * MET_PER_DEG_LAT;
    let dx = (a.1 - b.1) * MET_PER_DEG_LNG;
    dy.hypot(dx)
}

/// 标点轨迹长度（米）。`closed = true` 时补上末点 → 首点的闭合段。
pub fn perimeter_m(points: &[(f64, f64)], closed: bool) -> f64 {
    if points.len() < 2 {
        return 0.0;
    }
    let mut sum: f64 = points.windows(2).map(|w| segment_m(w[0], w[1])).sum();
    if closed && points.len() >= MIN_POINTS {
        sum += segment_m(points[points.len() - 1], points[0]);
    }
    sum
}

/// 找出距 `target` 最近、且不超过 `radius_m` 的标点。右键/长按删点用。
pub fn nearest_vertex_within(
    points: &[(f64, f64)],
    target: (f64, f64),
    radius_m: f64,
) -> Option<usize> {
    let mut best: Option<(f64, usize)> = None;
    for (index, point) in points.iter().enumerate() {
        let d = segment_m(*point, target);
        if d <= radius_m && best.map(|(bd, _)| d < bd).unwrap_or(true) {
            best = Some((d, index));
        }
    }
    best.map(|(_, index)| index)
}

/// 按住拖动画线后的距离抽稀：保留首点、间距 ≥ `min_spacing_m` 的点与末点。
pub fn simplify_stroke(points: &[(f64, f64)], min_spacing_m: f64) -> Vec<(f64, f64)> {
    if points.len() <= 2 {
        return points.to_vec();
    }
    let mut out = vec![points[0]];
    for point in &points[1..points.len() - 1] {
        if segment_m(*out.last().unwrap(), *point) >= min_spacing_m {
            out.push(*point);
        }
    }
    let last = points[points.len() - 1];
    if segment_m(*out.last().unwrap(), last) > 1e-9 {
        out.push(last);
    }
    out
}

/// XML 属性/文本转义（文件名与 way 的 name 标签都会进 XML）。
fn xml_escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

/// 文件名净化：去掉路径分隔符/控制字符，压掉首尾的点与空白，限长。
///
/// 这里同时是**路径穿越**的防线：`../evil` 只会变成 `_evil`，绝不会跑出
/// `custom_tracks/` 目录。
///
/// 还要避开 Windows 设备保留名（`CON`/`PRN`/`AUX`/`NUL`/`COM0-9`/`LPT0-9`）：
/// 把它们当文件名会直接写盘失败，用户只能看到一条底层 IO 错误。命中时统一加
/// `_` 前缀（`CON` → `_CON`），带扩展名的写法（`CON.osm`）同样处理。
pub fn safe_file_stem(raw: &str) -> String {
    let mut cleaned: String = raw
        .trim()
        .trim_end_matches(".osm")
        .trim_end_matches(".OSM")
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
            {
                '_'
            } else {
                ch
            }
        })
        .collect();
    cleaned = cleaned.trim().trim_matches('.').trim().to_string();
    if cleaned.is_empty() {
        cleaned = "自定义轨迹".into();
    }
    let mut stem: String = cleaned.chars().take(60).collect();
    if is_windows_reserved_stem(&stem) {
        stem.insert(0, '_');
    }
    stem
}

/// 是否是 Windows 设备保留名（大小写不敏感，带扩展名也算）。
///
/// `COM0`/`LPT0` 在部分 Windows 版本里同样保留，这里按保守做法一起挡掉；
/// `COM10`、`CONSOLE` 这类只是前缀相同的普通名字不受影响。
fn is_windows_reserved_stem(stem: &str) -> bool {
    let base = stem.split('.').next().unwrap_or("").trim();
    let upper = base.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.as_bytes()[3].is_ascii_digit())
}

/// BD-09 标点 → 标准 OSM XML（WGS84，正数 id，`highway=footway`）。
///
/// `closed = true` 时 way 的最后一个 `nd` 指回首节点（OSM 闭合 way 的写法）；
/// 保存入口要求闭合，这个函数本身允许生成开放折线，便于单测与将来扩展。
pub fn build_osm_xml(name: &str, points: &[(f64, f64)], closed: bool) -> Result<String, String> {
    if points.len() < MIN_POINTS {
        return Err(format!(
            "至少 {MIN_POINTS} 个标点才能保存（当前 {} 个）",
            points.len()
        ));
    }
    let mut nodes = String::new();
    let mut refs = String::new();
    for (index, &(lat, lon)) in points.iter().enumerate() {
        if !lat.is_finite() || !lon.is_finite() {
            return Err(format!("第 {} 个标点坐标无效", index + 1));
        }
        let (wgs_lat, wgs_lon) = crate::api::school_roads::to_wgs(lat, lon);
        if !wgs_lat.is_finite()
            || !wgs_lon.is_finite()
            || wgs_lat.abs() > 85.0
            || wgs_lon.abs() > 180.0
        {
            return Err(format!(
                "第 {} 个标点换算到 WGS84 后超出可用范围（{wgs_lat:.6},{wgs_lon:.6}）",
                index + 1
            ));
        }
        let id = index + 1; // 正数 id：负数 id 在本项目里曾被整份丢弃
        nodes.push_str(&format!(
            "  <node id=\"{id}\" lat=\"{wgs_lat:.7}\" lon=\"{wgs_lon:.7}\" version=\"1\"/>\n"
        ));
        refs.push_str(&format!("    <nd ref=\"{id}\"/>\n"));
    }
    if closed {
        refs.push_str("    <nd ref=\"1\"/>\n");
    }
    let way_id = points.len() + 1;
    let name_tag = if name.trim().is_empty() {
        String::new()
    } else {
        format!("    <tag k=\"name\" v=\"{}\"/>\n", xml_escape(name.trim()))
    };
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <osm version=\"0.6\" generator=\"NekoSportsWorldTool\">\n\
         {nodes}  <way id=\"{way_id}\" version=\"1\">\n\
         {refs}    <tag k=\"highway\" v=\"footway\"/>\n\
         {name_tag}  </way>\n\
         </osm>\n"
    );
    if xml.len() as u64 > MAX_OSM_BYTES {
        return Err(format!(
            "生成的 .osm 超过 {} MiB，请减少标点",
            MAX_OSM_BYTES / (1024 * 1024)
        ));
    }
    Ok(xml)
}

/// 用**加载端同一个解析器**先验一遍：保存成功而加载失败是最难解释的状态。
pub fn validate_osm_xml(xml: &str) -> Result<(usize, usize), String> {
    let graph = route_planner::load_osm(xml.as_bytes())
        .map_err(|error| format!("生成的 .osm 无法被路网加载器解析：{error}"))?;
    if graph.graph.edge_count() == 0 {
        return Err("生成的 .osm 没有任何可通行边，请检查标点是否重合".into());
    }
    Ok((graph.graph.node_count(), graph.graph.edge_count()))
}

/// `data_dir()/custom_tracks`（Android 为应用私有目录）。
pub fn custom_tracks_dir() -> std::path::PathBuf {
    crate::platform::data_dir().join("custom_tracks")
}

fn unique_path(dir: &std::path::Path, stem: &str) -> std::path::PathBuf {
    let first = dir.join(format!("{stem}.osm"));
    if !first.exists() {
        return first;
    }
    for n in 2..1000 {
        let candidate = dir.join(format!("{stem}-{n}.osm"));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{stem}-{}.osm", std::process::id()))
}

/// 保存标点为 `.osm`，返回最终路径。
///
/// 先写临时文件再改名：手机上写一半被杀进程时不会留下「半份 .osm」被下一次
/// 加载当成合法文件。同名文件不覆盖，自动加 `-2`、`-3` 后缀。
pub fn save_custom_track(
    dir: &std::path::Path,
    name: &str,
    points: &[(f64, f64)],
    closed: bool,
) -> Result<std::path::PathBuf, String> {
    if points.len() < MIN_POINTS {
        return Err(format!(
            "至少 {MIN_POINTS} 个标点才能保存（当前 {} 个）",
            points.len()
        ));
    }
    if !closed {
        return Err("轨迹尚未闭合：先点『闭合』再保存（路网要能绕圈）".into());
    }
    let xml = build_osm_xml(name, points, closed)?;
    validate_osm_xml(&xml)?;
    std::fs::create_dir_all(dir)
        .map_err(|error| format!("创建目录 {} 失败：{error}", dir.display()))?;
    let path = unique_path(dir, &safe_file_stem(name));
    let tmp = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "draw".into()),
        std::process::id()
    ));
    std::fs::write(&tmp, xml.as_bytes())
        .map_err(|error| format!("写入 {} 失败：{error}", tmp.display()))?;
    if let Err(error) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("保存 {} 失败：{error}", path.display()));
    }
    Ok(path)
}

fn screen_to_world(map: &MapState, rect: egui::Rect, pos: Pos2) -> (f64, f64) {
    let dx = pos.x as f64 - rect.center().x as f64 - map.offset.x as f64;
    let dy = pos.y as f64 - rect.center().y as f64 - map.offset.y as f64;
    (
        map.center.0 - dy / (map.scale as f64 * MET_PER_DEG_LAT),
        map.center.1 + dx / (map.scale as f64 * MET_PER_DEG_LNG),
    )
}

fn nearest_vertex_screen(
    map: &MapState,
    rect: egui::Rect,
    points: &[(f64, f64)],
    pos: Pos2,
) -> Option<usize> {
    let mut best: Option<(f32, usize)> = None;
    for (index, &(lat, lon)) in points.iter().enumerate() {
        let screen = map.to_screen(rect, lat, lon);
        let d = screen.distance(pos);
        if d <= VERTEX_HIT_PX && best.map(|(bd, _)| d < bd).unwrap_or(true) {
            best = Some((d, index));
        }
    }
    best.map(|(_, index)| index)
}

impl App {
    /// 在「路网」页顶部渲染手绘面板（折叠，默认收起）。
    pub(crate) fn draw_track_editor(&mut self, ui: &mut egui::Ui) {
        let state_id = egui::Id::new("ui/draw_track/state");
        let mut state = ui
            .data(|data| data.get_temp::<DrawTrack>(state_id))
            .unwrap_or_default();
        let mut to_load: Option<String> = None;

        egui::CollapsingHeader::new("手绘自定义路网（画线 → 保存 .osm → 跑步页沿它走）")
            .default_open(false)
            .show(ui, |ui| {
                self.draw_track_body(ui, &mut state, &mut to_load);
            });

        ui.data_mut(|data| {
            data.get_temp_mut_or_default::<DrawTrack>(state_id)
                .clone_from(&state);
        });

        if let Some(path) = to_load {
            // 复用既有本地路网通道：它负责解析、设 network、写 config.osm_path 与落盘。
            // 不新增加载器，也不新增 RouteMode，只是把模式切回既有的「真实道路路由」。
            self.config.route_mode = "road".into();
            self.run_page.route_mode = crate::track::generate_road::RouteMode::Road;
            self.run_page.preview_stale = true;
            self.run_page.map_fitted = false;
            self.osm_page.status = format!("已保存手绘路网，正在加载：{path}");
            self.load_osm_async(path);
        }
    }

    fn draw_track_body(
        &mut self,
        ui: &mut egui::Ui,
        state: &mut DrawTrack,
        to_load: &mut Option<String>,
    ) {
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut state.mode, DrawMode::Tap, "点击标点模式");
            ui.selectable_value(&mut state.mode, DrawMode::Pan, "拖动地图模式");
            match state.mode {
                DrawMode::Tap => {
                    ui.colored_label(
                        theme::text_dim(),
                        "单击加点；按住拖动可直接画线（松开按距离抽稀）",
                    );
                }
                DrawMode::Pan => {
                    ui.colored_label(theme::text_dim(), "拖拽只移视野，不加点；单击不加点");
                }
            }
        });
        ui.label(
            "长按顶点或右键删点（120 米内最近的标点）。标点顺序就是行进顺序；画布坐标 BD-09，保存时换算成 WGS84。",
        );

        ui.horizontal_wrapped(|ui| {
            ui.label(format!("{} 个标点", state.points.len()));
            ui.separator();
            if state.closed {
                ui.colored_label(
                    theme::ok(),
                    format!("已闭合 · 周长 {:.1} 米", perimeter_m(&state.points, true)),
                );
            } else {
                ui.label(format!(
                    "未闭合 · 折线 {:.1} 米",
                    perimeter_m(&state.points, false)
                ));
                if state.points.len() >= MIN_POINTS {
                    ui.colored_label(theme::text_dim(), "可以闭合了");
                } else {
                    ui.colored_label(
                        theme::text_dim(),
                        format!("再加 {} 个点才能闭合/保存", MIN_POINTS - state.points.len()),
                    );
                }
            }
        });

        ui.horizontal_wrapped(|ui| {
            let can_close = state.points.len() >= MIN_POINTS && !state.closed;
            if ui
                .add_enabled(can_close, theme::primary_btn("闭合"))
                .clicked()
            {
                state.closed = true;
                state.status = "已闭合，可以保存".into();
            }
            if state.closed && ui.button("取消闭合").clicked() {
                state.closed = false;
                state.status = "已取消闭合".into();
            }
            if ui
                .add_enabled(!state.points.is_empty(), egui::Button::new("撤销"))
                .clicked()
            {
                state.points.pop();
                state.closed = false;
                state.status = "已撤销最后一个标点".into();
            }
            if ui
                .add_enabled(!state.points.is_empty(), egui::Button::new("清空"))
                .clicked()
            {
                state.points.clear();
                state.closed = false;
                state.drag = None;
                state.press = None;
                state.status = "已清空标点".into();
            }
            if ui.button("从打卡点初始化").clicked() {
                let mut seeded = self
                    .run_page
                    .preview_inputs
                    .as_ref()
                    .map(|inputs| inputs.points.clone())
                    .unwrap_or_default();
                if seeded.len() < MIN_POINTS {
                    seeded = self
                        .run_page
                        .preview
                        .as_ref()
                        .map(|plan| plan.checkpoints.clone())
                        .unwrap_or_default();
                }
                if seeded.len() >= MIN_POINTS {
                    let count = seeded.len();
                    state.points = seeded;
                    state.closed = false;
                    state.view_ready = false;
                    state.status = format!("已从打卡点填入 {count} 个标点（可再手动调整）");
                } else {
                    state.status = "没有可用的打卡点：请先登录并点『刷新路线点位与围栏』".into();
                }
            }
            if ui.button("适配视野").clicked() {
                state.view_ready = false;
            }
        });

        ui.horizontal_wrapped(|ui| {
            ui.label("文件名：");
            ui.add(
                egui::TextEdit::singleline(&mut state.name)
                    .desired_width(170.0)
                    .hint_text("自定义轨迹（自动加 .osm）"),
            );
            let can_save = state.closed && state.points.len() >= MIN_POINTS;
            if ui
                .add_enabled(can_save, theme::primary_btn("保存为 .osm 并加载"))
                .clicked()
            {
                match save_custom_track(&custom_tracks_dir(), &state.name, &state.points, true) {
                    Ok(path) => {
                        state.last_saved = Some(path.display().to_string());
                        state.status = format!(
                            "已保存 {}；已切到真实道路路由，保存后在跑步页即可沿它走",
                            path.display()
                        );
                        *to_load = Some(path.display().to_string());
                    }
                    Err(error) => state.status = error,
                }
            }
            if !can_save {
                ui.colored_label(theme::text_dim(), "需至少 3 个标点并闭合");
            }
        });
        if let Some(path) = &state.last_saved {
            ui.label(format!("上次保存：{path}"));
        }
        if !state.status.is_empty() {
            ui.label(&state.status);
        }

        let anchor = self
            .identity
            .anchor_coordinate()
            .map(|coordinate| (coordinate.latitude, coordinate.longitude))
            .unwrap_or((38.901678, 121.540241));
        let height = if super::mobile::compact_ui(ui) {
            230.0
        } else {
            300.0
        };
        let (response, painter) = ui.allocate_painter(
            egui::Vec2::new(ui.available_width().max(200.0), height),
            egui::Sense::click_and_drag(),
        );
        let canvas = response.rect;
        state.canvas = Some(canvas);

        // 用 `..Default::default()` 兜住并发新增的字段（底图那一路正在扩 MapState）。
        let mut map = MapState {
            center: state.view.center,
            scale: state.view.scale,
            offset: state.view.offset,
            ..Default::default()
        };
        if !state.view_ready {
            let bounds = if state.points.len() >= 2 {
                state.points.clone()
            } else {
                vec![
                    (anchor.0 - 0.0008, anchor.1 - 0.0008),
                    (anchor.0 + 0.0008, anchor.1 + 0.0008),
                ]
            };
            map.fit(canvas, &bounds);
            if state.points.len() < 2 {
                map.center = anchor;
            }
            state.view_ready = true;
        }

        // ── 手势 ────────────────────────────────────────────────
        let (
            pointer,
            primary_down,
            primary_pressed,
            primary_released,
            secondary_pressed,
            delta,
            scroll,
            zoom,
        ) = ui.input(|input| {
            (
                input.pointer.interact_pos(),
                input.pointer.primary_down(),
                input.pointer.primary_pressed(),
                input.pointer.primary_released(),
                input.pointer.secondary_pressed(),
                input.pointer.delta(),
                input.raw_scroll_delta.y,
                input.zoom_delta(),
            )
        });
        let inside = |pos: Pos2| canvas.contains(pos);
        let mut press = state.press.take();
        let mut drag = state.drag;

        if let Some(pos) = pointer.filter(|pos| inside(*pos)) {
            if primary_pressed {
                let hit = nearest_vertex_screen(&map, canvas, &state.points, pos);
                // 模式优先级：拖动地图模式一律平移，即使起手落在顶点上；
                // 只有点击标点模式才允许拖顶点。否则「拖动只移视野」的文案是假的，
                // 而且会静默改掉用户已经画好的轨迹。
                drag = match (state.mode, hit) {
                    (DrawMode::Tap, Some(index)) => Some(index),
                    _ => None,
                };
                press = Some(Press {
                    pos,
                    at: std::time::Instant::now(),
                    hit,
                    moved: false,
                    stroke: if hit.is_none() && state.mode == DrawMode::Tap {
                        vec![screen_to_world(&map, canvas, pos)]
                    } else {
                        Vec::new()
                    },
                });
            }
        }

        if primary_down {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(50));
        }

        if let Some(current) = press.as_mut() {
            if let Some(pos) = pointer {
                if pos.distance(current.pos) > TAP_SLOP_PX {
                    current.moved = true;
                }
                if current.moved {
                    if let Some(index) = drag {
                        if index < state.points.len() {
                            state.points[index] = screen_to_world(&map, canvas, pos);
                            state.closed = false;
                        }
                    } else if state.mode == DrawMode::Pan {
                        if inside(pos) || inside(current.pos) {
                            map.offset += delta;
                        }
                    } else if current.hit.is_none() {
                        let world = screen_to_world(&map, canvas, pos);
                        let far_enough = current
                            .stroke
                            .last()
                            .map(|last| segment_m(*last, world) >= STROKE_SAMPLE_M)
                            .unwrap_or(true);
                        if far_enough && current.stroke.len() < 4000 {
                            current.stroke.push(world);
                        }
                    }
                } else if let Some(index) = current.hit {
                    // 长按删点：手指按住不动 600ms。先判 moved，避免拖顶点时误删。
                    if current.at.elapsed() >= LONG_PRESS && index < state.points.len() {
                        state.points.remove(index);
                        state.closed = false;
                        state.status = format!("已删除第 {} 个标点（长按）", index + 1);
                        press = None;
                        drag = None;
                    }
                }
            }
        }

        if primary_released {
            if let Some(current) = press.take() {
                if current.hit.is_none() && state.mode == DrawMode::Tap {
                    if current.moved && current.stroke.len() >= 2 {
                        let simplified = simplify_stroke(&current.stroke, STROKE_KEEP_M);
                        let added = simplified.len();
                        state.points.extend(simplified);
                        state.closed = false;
                        state.status =
                            format!("按住画线：已按 {STROKE_KEEP_M:.0} 米抽稀成 {added} 个标点");
                    } else if !current.moved && inside(current.pos) {
                        // tap 加点：用按下点而不是松开点，手指轻微滑动不会把点放偏。
                        let world = screen_to_world(&map, canvas, current.pos);
                        let too_close = state
                            .points
                            .last()
                            .map(|last| segment_m(*last, world) < TAP_MIN_SPACING_M)
                            .unwrap_or(false);
                        if too_close {
                            state.status = format!(
                                "离上一个标点不足 {TAP_MIN_SPACING_M:.1} 米，已忽略这次点击"
                            );
                        } else {
                            state.points.push(world);
                            state.closed = false;
                            state.status = format!("已加第 {} 个标点", state.points.len());
                        }
                    }
                }
            }
            drag = None;
        }

        if secondary_pressed {
            if let Some(pos) = pointer.filter(|pos| inside(*pos)) {
                let target = screen_to_world(&map, canvas, pos);
                match nearest_vertex_within(&state.points, target, DELETE_RADIUS_M) {
                    Some(index) => {
                        state.points.remove(index);
                        state.closed = false;
                        drag = None;
                        state.status = format!("已删除第 {} 个标点（右键）", index + 1);
                    }
                    None => {
                        state.status = format!("未找到 {DELETE_RADIUS_M:.0} 米内的标点");
                    }
                }
            }
        }

        if (response.hovered() || pointer.map(inside).unwrap_or(false))
            && (scroll != 0.0 || (zoom - 1.0).abs() > 1e-4)
        {
            // 触摸双指缩放走 zoom_delta，鼠标滚轮走 raw_scroll_delta；两条都支持，
            // 不依赖 map.rs 的交互函数（避免与底图那一路的实现互相牵制）。
            let factor = if (zoom - 1.0).abs() > 1e-4 {
                zoom
            } else {
                (scroll * 0.0015).exp()
            };
            map.scale = (map.scale * factor).clamp(0.002, 200.0);
        }

        state.press = press;
        state.drag = drag;
        state.view = MapView {
            center: map.center,
            scale: map.scale,
            offset: map.offset,
        };

        // ── 绘制 ────────────────────────────────────────────────
        painter.rect_filled(
            canvas,
            egui::Rounding::ZERO,
            egui::Color32::from_rgb(250, 252, 251),
        );
        let line_color = egui::Color32::from_rgb(30, 111, 216);
        if state.closed {
            map.draw_polygon(&painter, canvas, &state.points, line_color, 2.5);
        } else {
            map.draw_polyline(&painter, canvas, &state.points, line_color, 2.5);
        }
        if let Some(current) = &state.press {
            if current.stroke.len() >= 2 {
                map.draw_polyline(
                    &painter,
                    canvas,
                    &current.stroke,
                    egui::Color32::from_rgb(120, 160, 220),
                    1.5,
                );
            }
        }
        for (index, &(lat, lon)) in state.points.iter().enumerate() {
            let active = state.drag == Some(index);
            map.draw_point(
                &painter,
                canvas,
                lat,
                lon,
                if active {
                    egui::Color32::from_rgb(220, 38, 38)
                } else {
                    egui::Color32::from_rgb(22, 160, 90)
                },
                if active { 6.5 } else { 4.5 },
            );
        }
        painter.text(
            canvas.left_top() + egui::vec2(8.0, 8.0),
            egui::Align2::LEFT_TOP,
            if state.points.is_empty() {
                "点击画布加第一个点".to_string()
            } else {
                format!(
                    "{} 个标点{}",
                    state.points.len(),
                    if state.closed {
                        " · 已闭合"
                    } else {
                        " · 未闭合"
                    }
                )
            },
            egui::FontId::proportional(12.0),
            egui::Color32::from_rgb(88, 104, 99),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square() -> Vec<(f64, f64)> {
        vec![
            (30.0, 120.0),
            (30.0, 120.001),
            (30.001, 120.001),
            (30.001, 120.0),
        ]
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nsw-draw-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn xml_ids(xml: &str) -> Vec<i64> {
        let mut ids = Vec::new();
        let mut rest = xml;
        while let Some(at) = rest.find("id=\"") {
            let after = &rest[at + 4..];
            let end = after.find('"').unwrap();
            ids.push(after[..end].parse().unwrap());
            rest = &after[end..];
        }
        ids
    }

    /// 3 点闭合 → 标准 OSM XML：正数 id、含 highway=footway，且加载器真的能解析。
    #[test]
    fn three_point_closed_track_writes_valid_positive_id_osm() {
        let points = vec![(30.0, 120.0), (30.0, 120.001), (30.001, 120.001)];
        let xml = build_osm_xml("测试轨迹", &points, true).unwrap();

        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(xml.contains("<osm"));
        assert!(xml.contains("</osm>"));
        assert!(xml.contains("k=\"highway\" v=\"footway\""));
        let ids = xml_ids(&xml);
        assert!(!ids.is_empty(), "XML 里应当有 id 属性");
        assert!(ids.iter().all(|id| *id > 0), "id 必须全为正数：{ids:?}");

        // 闭合 way 的最后一个 nd 指回首节点。
        let way_refs = xml.matches("<nd ref=\"1\"/>").count();
        assert_eq!(way_refs, 2, "闭合 way 应首尾都引用 1 号节点");

        // 用加载端同一个解析器验证：3 个节点、闭合环共 6 条有向边。
        let (nodes, edges) = validate_osm_xml(&xml).unwrap();
        assert_eq!(nodes, 3, "节点数应等于标点数");
        assert_eq!(edges, 6, "闭合三边形应产生 3 段 × 双向 = 6 条有向边");

        // 坐标是 WGS84：与 BD-09→WGS84 的结果一致（7 位小数，容差 1e-6）。
        let graph = route_planner::load_osm(xml.as_bytes()).unwrap();
        let parsed: Vec<(f64, f64)> = graph
            .graph
            .node_weights()
            .map(|node| (node.coord.lat, node.coord.lon))
            .collect();
        for &(lat, lon) in &points {
            let (wgs_lat, wgs_lon) = crate::api::school_roads::to_wgs(lat, lon);
            assert!(
                parsed
                    .iter()
                    .any(|&(a, b)| (a - wgs_lat).abs() < 1e-6 && (b - wgs_lon).abs() < 1e-6),
                "缺少 WGS84 节点 ({wgs_lat:.7},{wgs_lon:.7})：{parsed:?}"
            );
        }
        // 原始 BD-09 坐标不应直接出现在文件里（否则就是坐标系混用）。
        let bd_lat = format!("lat=\"{:.7}\"", points[1].0);
        let bd_lon = format!("lon=\"{:.7}\"", points[1].1);
        assert!(
            !xml.contains(&bd_lat) && !xml.contains(&bd_lon),
            "不能把 BD-09 坐标原样写进 .osm：{xml}"
        );
    }

    /// 保存出来的 .osm 走既有加载通道能建图，并能在它上面跑出预览路线。
    ///
    /// 这条覆盖「手绘 → 保存 → 加载 → 预览」全链路：`load_osm` 就是
    /// `load_network_path`/`load_osm_async` 用的同一个解析入口，
    /// `plan_road_view` 就是跑步页预览调用的同一个函数。
    #[test]
    fn saved_osm_loads_and_previews_along_the_drawn_ring() {
        let dir = temp_dir("route");
        let ring = square();
        let path = save_custom_track(&dir, "环路", &ring, true).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let net = route_planner::load_osm(&bytes).unwrap();
        assert_eq!(net.graph.node_count(), ring.len(), "节点数应等于标点数");
        assert_eq!(net.graph.edge_count(), ring.len() * 2, "闭合环每段双向");

        // 与跑步页相同的预览入口：路网 WGS84，点位 BD-09，内部自己对齐。
        let preview =
            crate::track::generate_road::plan_road_view(&net, &ring, 1200.0, 600, 7, &[]).unwrap();
        assert_eq!(preview.checkpoints, ring, "预览打卡点应就是手绘标点");
        assert!(
            preview.length_m >= 300.0,
            "沿手绘环的路线长度异常：{}",
            preview.length_m
        );
        assert!(preview.route.len() > 10, "路线采样点太少");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 少于 3 点：构建与保存都拒绝，且不留下文件。
    #[test]
    fn fewer_than_three_points_are_refused() {
        let two = [(30.0, 120.0), (30.0, 120.001)];
        let err = build_osm_xml("两点", &two, true).unwrap_err();
        assert!(err.contains("至少 3 个标点"), "错误文案不对：{err}");

        let dir = temp_dir("two");
        let err = save_custom_track(&dir, "两点", &two, true).unwrap_err();
        assert!(err.contains("至少 3 个标点"), "错误文案不对：{err}");
        let files = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(files, 0, "拒绝保存时不应写任何文件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 未闭合不保存：路网需要能绕圈，否则跑步页无法圈跑。
    #[test]
    fn open_track_is_refused_on_save() {
        let dir = temp_dir("open");
        let err = save_custom_track(&dir, "未闭合", &square(), false).unwrap_err();
        assert!(err.contains("闭合"), "错误文案不对：{err}");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 闭合周长与已知值一致：0.001° 经度 = 86.6 m，0.001° 纬度 = 111.132 m。
    #[test]
    fn perimeter_matches_known_value() {
        let closed = perimeter_m(&square(), true);
        let expected_closed = 2.0 * (0.001 * MET_PER_DEG_LAT + 0.001 * MET_PER_DEG_LNG);
        assert!(
            (closed - expected_closed).abs() < 1e-6,
            "闭合周长 {closed} 应为 {expected_closed}"
        );
        assert!(
            (closed - 395.464).abs() < 1e-6,
            "已知值 395.464 m，实际 {closed}"
        );

        let open = perimeter_m(&square(), false);
        let expected_open =
            2.0 * 0.001 * MET_PER_DEG_LNG + 2.0 * 0.001 * MET_PER_DEG_LAT - 0.001 * MET_PER_DEG_LAT;
        assert!((open - expected_open).abs() < 1e-6);
        assert!(
            (open - 284.332).abs() < 1e-6,
            "折线长度 284.332 m，实际 {open}"
        );

        assert_eq!(perimeter_m(&[], true), 0.0);
        assert_eq!(perimeter_m(&[(30.0, 120.0)], true), 0.0);
        // 两点闭合没有第三条边，不能凭空加一条。
        let two = perimeter_m(&[(30.0, 120.0), (30.0, 120.001)], true);
        assert!(
            (two - 0.001 * MET_PER_DEG_LNG).abs() < 1e-6,
            "两点闭合不应加边：{two}"
        );
    }

    /// 右键/长按删点：120 米内命中最近点，超距离明确不命中。
    #[test]
    fn nearest_vertex_respects_120m_radius() {
        let points = [(30.0, 120.0), (30.0, 120.002)];
        assert_eq!(
            nearest_vertex_within(&points, (30.0, 120.0009), 120.0),
            Some(0)
        );
        assert_eq!(
            nearest_vertex_within(&points, (30.0, 120.0021), 120.0),
            Some(1)
        );
        assert_eq!(nearest_vertex_within(&points, (30.0, 120.01), 120.0), None);
        assert_eq!(nearest_vertex_within(&[], (30.0, 120.0), 120.0), None);
    }

    /// 按住画线后的距离抽稀：首尾保留、间距不小于阈值。
    #[test]
    fn stroke_is_simplified_by_distance() {
        let dense: Vec<(f64, f64)> = (0..=20)
            .map(|i| (30.0, 120.0 + i as f64 * 0.0001)) // 每步约 8.66 m
            .collect();
        let kept = simplify_stroke(&dense, 20.0);
        assert!(kept.len() < dense.len(), "没有抽稀：{}", kept.len());
        assert_eq!(kept.first(), dense.first());
        assert_eq!(kept.last(), dense.last());
        // 末点按约定必留，可能离上一个保留点不足阈值；其余相邻间距必须达标。
        for pair in kept.windows(2).take(kept.len().saturating_sub(2)) {
            assert!(
                segment_m(pair[0], pair[1]) >= 20.0,
                "抽稀后仍有小于阈值的间距：{:?}",
                segment_m(pair[0], pair[1])
            );
        }
        // 两个点原样返回。
        let two = [(30.0, 120.0), (30.0, 120.001)];
        assert_eq!(simplify_stroke(&two, 20.0), two.to_vec());
    }

    /// 文件名净化：路径分隔符不能逃出 custom_tracks；空名有兜底。
    #[test]
    fn file_stem_cannot_escape_directory() {
        for raw in ["../evil", "..\\evil", "a/b:c*?", "  ...  "] {
            let stem = safe_file_stem(raw);
            assert!(!stem.contains('/'), "{raw} → {stem}");
            assert!(!stem.contains('\\'), "{raw} → {stem}");
            assert!(!stem.contains(':'), "{raw} → {stem}");
            assert_ne!(stem, ".", "{raw}");
            assert_ne!(stem, "..", "{raw}");
        }
        assert_eq!(safe_file_stem(""), "自定义轨迹");

        let dir = temp_dir("escape");
        let path = save_custom_track(&dir, "../evil/../../x", &square(), true).unwrap();
        assert_eq!(
            path.parent().unwrap().canonicalize().unwrap(),
            dir.canonicalize().unwrap(),
            "保存路径必须留在目标目录内：{}",
            path.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Windows 设备保留名（含带扩展名写法）必须加前缀，否则写盘直接失败。
    #[test]
    fn windows_device_names_are_prefixed() {
        for raw in [
            "CON", "aux", "COM1", "lpt9", "NUL", "PRN", "com0", "LPT0", "CON.osm", "nul.txt",
        ] {
            let stem = safe_file_stem(raw);
            assert_ne!(stem, raw, "{raw} 未被处理");
            assert!(
                stem.starts_with('_'),
                "{raw} → {stem} 应以 _ 开头，避免命中设备名"
            );
            assert!(
                !is_windows_reserved_stem(&stem),
                "{raw} → {stem} 仍是设备保留名"
            );
            // 大小写不敏感：结果的基名不能等于原保留名。
            let base = stem.split('.').next().unwrap().to_ascii_uppercase();
            assert!(
                !matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL"),
                "{raw} → {stem} 基名仍是保留名"
            );
            assert_ne!(base, "COM1", "{raw} → {stem}");
            assert_ne!(base, "LPT9", "{raw} → {stem}");
        }

        // 只是前缀相同、本身不保留的名字不受影响。
        for raw in ["COM10", "CONSOLE", "console", "lpt", "夜间跑步"] {
            assert_eq!(safe_file_stem(raw), raw, "{raw} 不该被改名");
        }

        // 真的保存一次：文件必须以 _CON 开头（Windows 上 CON.osm 无法创建）。
        let dir = temp_dir("con");
        let path = save_custom_track(&dir, "CON", &square(), true).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.to_ascii_uppercase().starts_with("_CON"),
            "设备名未被改写：{name}"
        );
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 同名保存不覆盖旧文件（加 -2 后缀）；XML 里 name 已转义。
    #[test]
    fn repeated_save_keeps_previous_track_and_escapes_name() {
        let dir = temp_dir("repeat");
        let first = save_custom_track(&dir, "夜跑<&>", &square(), true).unwrap();
        let second = save_custom_track(&dir, "夜跑<&>", &square(), true).unwrap();
        assert_ne!(first, second, "同名保存不应覆盖");
        assert!(second.to_string_lossy().contains("-2"));

        let xml = std::fs::read_to_string(&first).unwrap();
        assert!(
            xml.contains("v=\"夜跑&lt;&amp;&gt;\""),
            "name 未转义：{xml}"
        );
        assert!((std::fs::metadata(&first).unwrap().len() as u64) <= MAX_OSM_BYTES);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 手势状态机（无头 egui，直接跑真实的 draw_track_body） ────────────

    use crate::api::model::{Config, HeaderIdentity, Session};

    fn test_app(ctx: &egui::Context) -> App {
        App::from_state(
            ctx,
            HeaderIdentity::default(),
            Session::default(),
            Config::default(),
        )
    }

    /// 跑一帧真实的 `draw_track_body`（900×900 逻辑屏），事件按 egui 原生方式注入。
    fn frame(app: &mut App, ctx: &egui::Context, state: &mut DrawTrack, events: Vec<egui::Event>) {
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 900.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let mut to_load = None;
                    app.draw_track_body(ui, state, &mut to_load);
                });
            },
        );
    }

    fn primary(pos: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        }
    }

    fn map_of(state: &DrawTrack) -> MapState {
        MapState {
            center: state.view.center,
            scale: state.view.scale,
            offset: state.view.offset,
            ..Default::default()
        }
    }

    /// F5：「拖动地图模式」从顶点起手也必须只平移视野，顶点坐标不能变。
    #[test]
    fn pan_mode_pans_view_even_when_the_gesture_starts_on_a_vertex() {
        let ctx = egui::Context::default();
        let mut app = test_app(&ctx);
        let ring = square();
        let mut state = DrawTrack {
            points: ring.clone(),
            closed: true,
            mode: DrawMode::Pan,
            ..Default::default()
        };
        frame(&mut app, &ctx, &mut state, vec![]);
        let canvas = state.canvas.expect("首帧应记录画布");
        let start = map_of(&state).to_screen(canvas, ring[0].0, ring[0].1);
        assert!(canvas.contains(start), "起手点应在画布内：{start:?}");

        let before_points = state.points.clone();
        let before_offset = state.view.offset;

        frame(
            &mut app,
            &ctx,
            &mut state,
            vec![egui::Event::PointerMoved(start), primary(start, true)],
        );
        let end = start + egui::vec2(36.0, 18.0);
        frame(
            &mut app,
            &ctx,
            &mut state,
            vec![egui::Event::PointerMoved(end)],
        );
        frame(
            &mut app,
            &ctx,
            &mut state,
            vec![egui::Event::PointerMoved(end), primary(end, false)],
        );

        assert_eq!(
            state.points, before_points,
            "拖动地图模式不得改顶点坐标（F5）"
        );
        let moved = state.view.offset - before_offset;
        assert!(
            moved.length() > 10.0,
            "视野应随拖动平移，实际位移 {moved:?}"
        );
        assert!(state.drag.is_none(), "拖动地图模式不应进入拖顶点状态");
    }

    /// 「点击标点模式」：空白处 tap 加点；从顶点起手拖动只移动该顶点，不新增点。
    #[test]
    fn tap_mode_adds_on_tap_and_drags_vertex_without_adding() {
        let ctx = egui::Context::default();
        let mut app = test_app(&ctx);
        let mut state = DrawTrack {
            points: square(),
            closed: false,
            mode: DrawMode::Tap,
            ..Default::default()
        };
        frame(&mut app, &ctx, &mut state, vec![]);
        let canvas = state.canvas.expect("首帧应记录画布");
        let empty = canvas.center();
        let world_at_tap = screen_to_world(&map_of(&state), canvas, empty);

        frame(
            &mut app,
            &ctx,
            &mut state,
            vec![egui::Event::PointerMoved(empty), primary(empty, true)],
        );
        frame(&mut app, &ctx, &mut state, vec![primary(empty, false)]);
        assert_eq!(state.points.len(), 5, "tap 空白处应加一个点");
        let added = *state.points.last().unwrap();
        assert!(
            (added.0 - world_at_tap.0).abs() < 1e-9 && (added.1 - world_at_tap.1).abs() < 1e-9,
            "tap 加点应落在按下位置：{added:?} vs {world_at_tap:?}"
        );

        // 加点后状态栏文本首次出现会把画布下移；先跑一帧让布局稳定，再取顶点屏幕坐标
        // （真实交互里用户看到的顶点与同一帧的命中判定始终一致，这里只是测试要多等一帧）。
        frame(&mut app, &ctx, &mut state, vec![]);
        let canvas = state.canvas.expect("画布");
        let vertex = state.points[0];
        let start = map_of(&state).to_screen(canvas, vertex.0, vertex.1);
        let before_len = state.points.len();
        let before = state.points.clone();

        frame(
            &mut app,
            &ctx,
            &mut state,
            vec![egui::Event::PointerMoved(start), primary(start, true)],
        );
        let end = start + egui::vec2(28.0, 0.0);
        frame(
            &mut app,
            &ctx,
            &mut state,
            vec![egui::Event::PointerMoved(end)],
        );
        frame(
            &mut app,
            &ctx,
            &mut state,
            vec![egui::Event::PointerMoved(end), primary(end, false)],
        );

        assert_eq!(state.points.len(), before_len, "拖顶点不应新增标点");
        assert_ne!(state.points[0], before[0], "顶点应被移动到新位置");
    }
}
