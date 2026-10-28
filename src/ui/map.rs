//! 地图画布绘制原语：等距投影 + 拖拽平移 + 滚轮缩放。
//!
//! 供「路网」页与「跑步」页复用，纯离线绘制折线/多边形/点。

use eframe::egui;
use egui::{Color32, Pos2, Rect, Stroke, Vec2};

use crate::track::geom::{MET_PER_DEG_LAT, MET_PER_DEG_LNG};

/// 地图视图状态（世界坐标为度）。
pub struct MapState {
    /// 参考中心（lat, lon）。
    pub center: (f64, f64),
    /// 屏幕每米像素（缩放）。
    pub scale: f32,
    /// 平移偏移（像素）。
    pub offset: Vec2,
    /// 最近是否被拖拽过（供外部判断）。
    pub dragged: bool,
    /// 触摸/鼠标手势状态机（双指缩放、拖拽、点按判别）。
    pub gesture: Gestures,
}

/// 最小缩放（每米像素）：0.002 ≈ 全国视野。
pub const MIN_SCALE: f32 = 0.002;
/// 最大缩放（每米像素）：200 ≈ 5 mm/像素，够贴近看楼。
pub const MAX_SCALE: f32 = 200.0;

impl Default for MapState {
    fn default() -> Self {
        MapState {
            center: (0.0, 0.0),
            scale: 0.3,
            offset: Vec2::ZERO,
            dragged: false,
            gesture: Gestures::default(),
        }
    }
}

// ── 手势状态机 ───────────────────────────────────────────────────────────
//
// 只读 `raw_scroll_delta` 时，Android 的双指捏合是**完全失效**的：egui 把多点触摸
// 的缩放放进 `zoom_delta()`（触屏捏合会合成 `Event::Zoom`，滚轮槽位保持 0），
// 所以滚轮和捏合必须两条都读。优先级是「捏合 > 拖拽」：边捏边拖时指针位移是两指
// 中点的漂移，跟着走会让地图乱跳。tap 判据单独记（按下→抬起之间没移动才算点按），
// 不然手指按下去那几帧的 0.5 px 抖动就会被当成拖拽，点位点不中。

/// 一帧的原始输入（与 egui 解耦，便于单测）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GestureInput {
    /// 指针是否在画布内。
    pub inside: bool,
    /// 主指针是否按下（触屏=至少一根手指在屏幕上）。
    pub down: bool,
    /// egui 是否已判定为拖拽（`is_decidedly_dragging`）。
    pub dragging: bool,
    /// 本帧指针位移（像素）。
    pub delta: Vec2,
    /// 本帧缩放倍率（1.0 = 无变化；双指 = `zoom_delta()`）。
    pub zoom: f32,
}

impl Default for GestureInput {
    /// 关键：`zoom` 的零值是 0.0，但它的**中性值**是 1.0。用 derive 会让
    /// 「没有缩放的帧」被当成缩放因子 0（= 缩到最小），所以手写。
    fn default() -> Self {
        Self {
            inside: false,
            down: false,
            dragging: false,
            delta: Vec2::ZERO,
            zoom: 1.0,
        }
    }
}

/// 状态机的输出：这一帧该怎么动。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GestureOutput {
    /// 平移量（像素）。
    pub pan: Vec2,
    /// 缩放倍率（1.0 = 不缩放）。
    pub zoom: f32,
    /// 本帧是否在拖拽（写回 `MapState::dragged`）。
    pub dragged: bool,
    /// 本帧是否完成一次点按（按下到抬起全程没移动）。
    pub tapped: bool,
    /// 当前状态（调试/测试用）。
    pub state: GestureState,
}

impl Default for GestureOutput {
    fn default() -> Self {
        Self {
            pan: Vec2::ZERO,
            zoom: 1.0,
            dragged: false,
            tapped: false,
            state: GestureState::Idle,
        }
    }
}

/// 手势状态。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GestureState {
    /// 没有手势（或只是按着没动）。
    #[default]
    Idle,
    /// 单指/左键拖拽。
    Dragging,
    /// 双指捏合缩放。
    Pinching,
}

/// 触摸/鼠标手势状态机。
#[derive(Clone, Copy, Debug, Default)]
pub struct Gestures {
    state: GestureState,
    pressed: bool,
    moved: bool,
}

impl Gestures {
    /// 当前手势状态（诊断/测试用）。
    #[allow(dead_code)]
    pub fn state(&self) -> GestureState {
        self.state
    }

    /// 吃一帧输入，给出平移/缩放/点按。
    pub fn update(&mut self, input: GestureInput) -> GestureOutput {
        let mut out = GestureOutput {
            pan: Vec2::ZERO,
            zoom: 1.0,
            dragged: false,
            tapped: false,
            state: self.state,
        };
        if input.down && !self.pressed {
            self.pressed = true;
            self.moved = false;
        }
        let pinching = input.inside && (input.zoom - 1.0).abs() > 1e-3;
        if pinching {
            self.state = GestureState::Pinching;
            self.moved = true;
            out.zoom = input.zoom;
        } else if input.inside && input.down && input.dragging {
            self.state = GestureState::Dragging;
            self.moved = true;
            out.pan = input.delta;
            out.dragged = true;
        } else {
            self.state = GestureState::Idle;
        }
        if !input.down && self.pressed {
            self.pressed = false;
            out.tapped = !self.moved;
            self.moved = false;
            self.state = GestureState::Idle;
        }
        out.state = self.state;
        out
    }
}

impl MapState {
    pub fn world_bounds(&self, rect: Rect) -> (route_planner::Coord, route_planner::Coord) {
        let x = |screen: f32| {
            self.center.1
                + (screen - rect.center().x - self.offset.x) as f64
                    / (self.scale as f64 * MET_PER_DEG_LNG)
        };
        let y = |screen: f32| {
            self.center.0
                - (screen - rect.center().y - self.offset.y) as f64
                    / (self.scale as f64 * MET_PER_DEG_LAT)
        };
        (
            route_planner::Coord::new(x(rect.left()), y(rect.bottom())),
            route_planner::Coord::new(x(rect.right()), y(rect.top())),
        )
    }
    /// 依据一组点（lat, lon）自动适配视野。
    pub fn fit(&mut self, rect: Rect, pts: &[(f64, f64)]) {
        if pts.len() < 2 {
            return;
        }
        let (mut slat, mut slon) = (0.0f64, 0.0f64);
        for (la, lo) in pts {
            slat += *la;
            slon += *lo;
        }
        let n = pts.len() as f64;
        self.center = (slat / n, slon / n);
        let (mut minx, mut maxx, mut miny, mut maxy) = (
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
        );
        for (la, lo) in pts {
            let x = (lo - self.center.1) * MET_PER_DEG_LNG;
            let y = (la - self.center.0) * MET_PER_DEG_LAT;
            minx = minx.min(x);
            maxx = maxx.max(x);
            miny = miny.min(y);
            maxy = maxy.max(y);
        }
        let w = (maxx - minx).max(40.0);
        let h = (maxy - miny).max(40.0);
        let sx = rect.width() as f64 / w;
        let sy = rect.height() as f64 / h;
        self.scale = ((sx.min(sy)) * 0.92) as f32;
        self.offset = Vec2::ZERO;
    }

    /// 度坐标 → 屏幕坐标。
    pub fn to_screen(&self, rect: Rect, lat: f64, lon: f64) -> Pos2 {
        let x = (lon - self.center.1) * MET_PER_DEG_LNG * self.scale as f64;
        let y = -(lat - self.center.0) * MET_PER_DEG_LAT * self.scale as f64;
        let c = rect.center();
        Pos2::new(
            (c.x as f64 + x + self.offset.x as f64) as f32,
            (c.y as f64 + y + self.offset.y as f64) as f32,
        )
    }

    /// 处理拖拽/缩放交互（需在分配好 rect 后调用）。
    ///
    /// 双指捏合（`zoom_delta()`）与滚轮（`raw_scroll_delta`）都能缩放；两者互斥，
    /// 捏合时 egui 不给滚轮量，所以不会双算。缩放以指针为不动点。
    pub fn interact(&mut self, ui: &egui::Ui, rect: Rect) {
        let (input, pointer) = ui.input(|i| {
            let inside = i
                .pointer
                .interact_pos()
                .or_else(|| i.pointer.hover_pos())
                .map(|p| rect.contains(p))
                .unwrap_or(false);
            (
                GestureInput {
                    inside,
                    down: i.pointer.primary_down(),
                    dragging: i.pointer.is_decidedly_dragging(),
                    delta: i.pointer.delta(),
                    zoom: i.zoom_delta(),
                },
                i.pointer.interact_pos(),
            )
        });
        let out = self.gesture.update(input);
        if out.dragged {
            self.offset += out.pan;
        }
        self.dragged = out.dragged;
        let scroll = ui.input(|i| {
            if input.inside {
                i.raw_scroll_delta.y
            } else {
                0.0
            }
        });
        let mut factor = if (out.zoom - 1.0).abs() > 1e-4 {
            out.zoom
        } else {
            1.0
        };
        if scroll != 0.0 {
            factor *= (scroll * 0.0015).exp();
        }
        if (factor - 1.0).abs() > 1e-4 {
            let anchor = pointer
                .filter(|p| rect.contains(*p))
                .unwrap_or_else(|| rect.center());
            self.zoom_about(rect, anchor, factor);
        }
    }

    /// 以 `anchor`（屏幕坐标）为不动点缩放：双指与滚轮共用。
    ///
    /// 推导：屏幕位置 = 中心 + (世界差 × 系数 × scale) + offset，要让 `anchor` 下的
    /// 世界点不动，缩放 k 倍后 offset' = rel − (rel − offset)·k（rel = anchor − 中心）。
    pub fn zoom_about(&mut self, rect: Rect, anchor: Pos2, factor: f32) {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        let old = self.scale;
        let new = (old * factor).clamp(MIN_SCALE, MAX_SCALE);
        if (new - old).abs() <= f32::EPSILON {
            return;
        }
        let k = new / old;
        let rel = anchor - rect.center();
        self.offset = rel - (rel - self.offset) * k;
        self.scale = new;
    }

    pub fn draw_polyline(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        pts: &[(f64, f64)],
        color: Color32,
        width: f32,
    ) {
        if pts.len() < 2 {
            return;
        }
        let (a, b) = self.world_bounds(rect);
        let (mut minlat, mut maxlat, mut minlon, mut maxlon) = (
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
        );
        for &(lat, lon) in pts {
            minlat = minlat.min(lat);
            maxlat = maxlat.max(lat);
            minlon = minlon.min(lon);
            maxlon = maxlon.max(lon);
        }
        if maxlon < a.lon || minlon > b.lon || maxlat < a.lat || minlat > b.lat {
            return;
        }
        let ps: Vec<Pos2> = pts
            .iter()
            .map(|(la, lo)| self.to_screen(rect, *la, *lo))
            .collect();
        painter.add(egui::Shape::line(ps, Stroke::new(width, color)));
    }

    pub fn draw_polygon(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        pts: &[(f64, f64)],
        color: Color32,
        width: f32,
    ) {
        if pts.len() < 3 {
            return;
        }
        let mut ps: Vec<Pos2> = pts
            .iter()
            .map(|(la, lo)| self.to_screen(rect, *la, *lo))
            .collect();
        ps.push(ps[0]);
        painter.add(egui::Shape::closed_line(ps, Stroke::new(width, color)));
    }

    pub fn draw_point(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        lat: f64,
        lon: f64,
        color: Color32,
        radius: f32,
    ) {
        let c = self.to_screen(rect, lat, lon);
        painter.circle_filled(c, radius, color);
    }

    /// Draw the unmodified WGS84 OSM graph, without school API data or route planning.
    pub fn draw_network(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        net: &route_planner::RoadGraph,
    ) {
        let road_color = Color32::from_rgb(190, 200, 196);
        let (min, max) = self.world_bounds(rect);
        let visible = net.visible_edges(min, max);
        let stride = if self.scale < 0.1 {
            (visible.len() / 4000).max(1)
        } else {
            1
        };
        for edge in visible.iter().step_by(stride) {
            self.draw_polyline(
                painter,
                rect,
                &[(edge[0].lat, edge[0].lon), (edge[1].lat, edge[1].lon)],
                road_color,
                1.0,
            );
        }
        if stride > 1 {
            painter.text(
                rect.left_top() + egui::vec2(6., 6.),
                egui::Align2::LEFT_TOP,
                "缩小视野：道路显示已简化",
                egui::FontId::proportional(12.),
                road_color,
            );
        }
        for building in &net.buildings {
            let points: Vec<_> = building.iter().map(|c| (c.lat, c.lon)).collect();
            self.draw_polygon(
                painter,
                rect,
                &points,
                Color32::from_rgb(214, 182, 158),
                1.0,
            );
        }
    }
}

/// 收集一组折线/环的所有点，用于 fit。
pub fn collect_bounds(edges: &[Vec<(f64, f64)>]) -> Vec<(f64, f64)> {
    let mut pts: Vec<(f64, f64)> = Vec::new();
    for e in edges.iter().take(400) {
        for p in e {
            pts.push(*p);
        }
    }
    pts
}

/// 地图图例：颜色方块 + 说明文字。
pub fn legend(ui: &mut egui::Ui, items: &[(&str, Color32)]) {
    ui.horizontal_wrapped(|ui| {
        ui.label("图例：");
        for (label, color) in items {
            let (rect, _) = ui.allocate_exact_size(Vec2::new(10.0, 10.0), egui::Sense::hover());
            ui.painter()
                .rect_filled(rect, egui::Rounding::same(2.0), *color);
            ui.label(*label);
        }
    });
}
