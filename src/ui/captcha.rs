//! 手动过码界面：把 GT4 挑战图显示出来，让用户把方块拖进缺口。
//!
//! 线程模型：整条登录链在后台线程里跑。取到挑战图之后，后台线程把「两张图 +
//! 一个回收答案的通道」丢进 UI 队列，然后阻塞等答案；界面这边弹窗，用户
//! 拖完 / 换一张 / 取消，答案回填后后台线程继续 /verify → geevalidate → login。
//! 界面线程一秒都没被网络请求挡住。

use crate::api::gt4::{Gt4Challenge, ManualOutcome, ManualSolver};
use eframe::egui;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};

/// 后台线程 → 界面：一张待拖的挑战图。
pub struct CaptchaRequest {
    pub id: u64,
    pub bg_png: Vec<u8>,
    pub slice_png: Vec<u8>,
    pub bg_width: usize,
    pub bg_height: usize,
    pub slice_width: usize,
    pub slice_height: usize,
    /// 切片在背景图里的纵向位置（背景图坐标）。
    pub ypos: usize,
    /// 切片可移动的最大横向位移（背景图坐标）。
    pub max_offset: usize,
    /// 答案回填通道；界面放下它时后台线程会收到断开并取消。
    pub responder: Sender<CaptchaAnswer>,
}

/// 界面 → 后台线程：用户的选择。
pub enum CaptchaAnswer {
    /// 拖完了：`set_left` 是切片左缘在背景图上的像素位置。
    Slide { set_left: usize, passtime_ms: u64 },
    /// 这张图看不清，换一张。
    Retry,
    /// 不登了。
    Cancel,
}

/// 手动过码的界面实现：把图交出去，等答案。
pub struct UiCaptchaSolver {
    ctx: egui::Context,
    tx: Sender<CaptchaRequest>,
}

impl UiCaptchaSolver {
    pub fn new(ctx: egui::Context, tx: Sender<CaptchaRequest>) -> Self {
        Self { ctx, tx }
    }
}

impl ManualSolver for UiCaptchaSolver {
    fn solve(&mut self, challenge: &Gt4Challenge) -> ManualOutcome {
        static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);
        let (responder, answers) = std::sync::mpsc::channel();
        let request = CaptchaRequest {
            id: NEXT_REQUEST.fetch_add(1, Ordering::Relaxed),
            bg_png: challenge.bg_png.clone(),
            slice_png: challenge.slice_png.clone(),
            bg_width: challenge.bg_width,
            bg_height: challenge.bg_height,
            slice_width: challenge.slice_width,
            slice_height: challenge.slice_height,
            ypos: challenge.ypos.unwrap_or(0),
            max_offset: challenge.max_offset(),
            responder,
        };
        if self.tx.send(request).is_err() {
            // 界面已经没了，没人能回答这个问题。
            return ManualOutcome::Cancel;
        }
        self.ctx.request_repaint();
        let started = std::time::Instant::now();
        loop {
            match answers.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(CaptchaAnswer::Slide {
                    set_left,
                    passtime_ms,
                }) => {
                    return ManualOutcome::Slide {
                        // 界面已经夹过，这里再夹一次：位移超出图宽一定是 bug，
                        // 与其把怪值交给服务端，不如按边界交卷。
                        set_left: set_left.min(challenge.max_offset()),
                        passtime_ms,
                    };
                }
                Ok(CaptchaAnswer::Retry) => return ManualOutcome::Retry,
                Ok(CaptchaAnswer::Cancel) => return ManualOutcome::Cancel,
                Err(RecvTimeoutError::Timeout) => {
                    // 每 200ms 催一次重绘：手机前台时窗口需要一直画着拖动效果。
                    self.ctx.request_repaint();
                    // 5 分钟没人动就当放弃，别让登录线程一直挂着。
                    if started.elapsed() > std::time::Duration::from_secs(300) {
                        return ManualOutcome::Cancel;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return ManualOutcome::Cancel,
            }
        }
    }
}

/// UI 侧状态（挂在 App 上）。
#[derive(Default)]
pub struct CaptchaUi {
    pending: Option<PendingCaptcha>,
}

pub struct PendingCaptcha {
    request: CaptchaRequest,
    bg: Option<egui::TextureHandle>,
    slice: Option<egui::TextureHandle>,
    /// 当前切片位置（背景图像素）。
    offset: f32,
    /// 第一次按下去的时刻 —— `passtime` 从这一刻起算。
    drag_started: Option<std::time::Instant>,
    decode_error: Option<String>,
}

/// 界面这一帧得出的动作。
enum Action {
    Submit { set_left: usize, passtime_ms: u64 },
    Retry,
    Cancel,
}

impl PendingCaptcha {
    fn new(request: CaptchaRequest) -> Self {
        Self {
            request,
            bg: None,
            slice: None,
            offset: 0.0,
            drag_started: None,
            decode_error: None,
        }
    }

    fn max_offset(&self) -> f32 {
        self.request.max_offset as f32
    }
}

fn decode_texture(
    ctx: &egui::Context,
    name: &str,
    png: &[u8],
) -> Result<egui::TextureHandle, String> {
    let image = image::load_from_memory(png)
        .map_err(|e| format!("图片解码失败：{e}"))?
        .to_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    let color = egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw());
    Ok(ctx.load_texture(name, color, egui::TextureOptions::LINEAR))
}

impl App {
    /// 收取后台线程送来的挑战图。
    pub(crate) fn poll_captcha(&mut self) {
        while let Ok(request) = self.captcha_rx.try_recv() {
            self.captcha.pending = Some(PendingCaptcha::new(request));
            self.status = "滑块验证：把方块拖进缺口，然后点「提交」".into();
        }
    }

    /// 每帧调用：有待拖的图就弹窗。
    pub(crate) fn draw_captcha_window(&mut self, ctx: &egui::Context) {
        let Some(mut pending) = self.captcha.pending.take() else {
            return;
        };
        let action = draw_modal(ctx, &mut pending);
        match action {
            None => self.captcha.pending = Some(pending),
            Some(Action::Cancel) => {
                let _ = pending.request.responder.send(CaptchaAnswer::Cancel);
                self.status = "已取消验证码".into();
            }
            Some(Action::Retry) => {
                let _ = pending.request.responder.send(CaptchaAnswer::Retry);
                self.status = "换一张图…".into();
            }
            Some(Action::Submit {
                set_left,
                passtime_ms,
            }) => {
                let _ = pending.request.responder.send(CaptchaAnswer::Slide {
                    set_left,
                    passtime_ms,
                });
                self.status = format!("已提交滑块位置 {set_left}px，正在校验…");
            }
        }
    }
}

fn draw_modal(ctx: &egui::Context, pending: &mut PendingCaptcha) -> Option<Action> {
    if pending.bg.is_none() && pending.decode_error.is_none() {
        match decode_texture(ctx, "gt4-captcha-bg", &pending.request.bg_png) {
            Ok(texture) => pending.bg = Some(texture),
            Err(error) => pending.decode_error = Some(error),
        }
    }
    if pending.slice.is_none() && pending.decode_error.is_none() {
        match decode_texture(ctx, "gt4-captcha-slice", &pending.request.slice_png) {
            Ok(texture) => pending.slice = Some(texture),
            Err(error) => pending.decode_error = Some(error),
        }
    }

    let mut action = None;
    egui::Window::new(egui::RichText::new("滑块验证").strong())
        .id(egui::Id::new(("captcha_modal", pending.request.id)))
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .collapsible(false)
        .resizable(false)
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            ui.set_max_width((ctx.screen_rect().width() - 24.0).min(460.0));
            ui.label("把方块拖进缺口（可以先用滑块粗调，再点 ◀ ▶ 微调 1 像素）");
            if let Some(error) = &pending.decode_error {
                ui.colored_label(super::theme::err(), error);
                ui.horizontal(|ui| {
                    if ui.button("换一张").clicked() {
                        action = Some(Action::Retry);
                    }
                    if ui.button("取消").clicked() {
                        action = Some(Action::Cancel);
                    }
                });
                return;
            }
            let (Some(bg), Some(slice)) = (pending.bg.as_ref(), pending.slice.as_ref()) else {
                ui.label("正在解码图片…");
                return;
            };

            let bg_w = pending.request.bg_width as f32;
            let bg_h = pending.request.bg_height as f32;
            let avail = ui.available_width().max(120.0);
            // 不放大（放大只会让像素对齐更难），只按窗口宽度缩小。
            let scale = (avail / bg_w).min(1.0);
            let (rect, response) =
                ui.allocate_exact_size(egui::vec2(bg_w * scale, bg_h * scale), egui::Sense::drag());
            let painter = ui.painter_at(rect);
            painter.image(
                bg.id(),
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );

            // 拖动：按指针的相对位移累加，手机触摸与鼠标同一套逻辑。
            if response.dragged() {
                if pending.drag_started.is_none() {
                    pending.drag_started = Some(std::time::Instant::now());
                }
                pending.offset = (pending.offset + response.drag_delta().x / scale)
                    .clamp(0.0, pending.max_offset());
            }
            if response.drag_started() && pending.drag_started.is_none() {
                pending.drag_started = Some(std::time::Instant::now());
            }

            // 切片画在当前位移上 —— 和真机上的滑块一致，能直接看出对不对齐。
            let slice_size = egui::vec2(
                pending.request.slice_width as f32 * scale,
                pending.request.slice_height as f32 * scale,
            );
            let slice_min = egui::pos2(
                rect.left() + pending.offset * scale,
                rect.top() + pending.request.ypos as f32 * scale,
            );
            painter.image(
                slice.id(),
                egui::Rect::from_min_size(slice_min, slice_size),
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
            // 缺口左缘的一条细线，方便对齐。
            painter.line_segment(
                [
                    egui::pos2(slice_min.x, rect.top()),
                    egui::pos2(slice_min.x, rect.bottom()),
                ],
                egui::Stroke::new(1.0, super::theme::accent()),
            );

            ui.add_space(4.0);
            let mut offset = pending.offset;
            if ui
                .add(
                    egui::Slider::new(&mut offset, 0.0..=pending.max_offset())
                        .show_value(false)
                        .text("位置"),
                )
                .changed()
            {
                pending.offset = offset;
                if pending.drag_started.is_none() {
                    pending.drag_started = Some(std::time::Instant::now());
                }
            }
            ui.horizontal(|ui| {
                if ui.button("◀ 1px").clicked() {
                    pending.offset = (pending.offset - 1.0).clamp(0.0, pending.max_offset());
                }
                if ui.button("▶ 1px").clicked() {
                    pending.offset = (pending.offset + 1.0).clamp(0.0, pending.max_offset());
                }
                ui.label(format!("{} px", pending.offset.round() as i64));
            });

            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                if ui.add(super::theme::primary_btn("提交验证")).clicked() {
                    let passtime = pending
                        .drag_started
                        .map(|t| t.elapsed().as_millis() as u64)
                        .unwrap_or(0);
                    action = Some(Action::Submit {
                        set_left: pending.offset.round().max(0.0) as usize,
                        passtime_ms: passtime,
                    });
                }
                if ui.button("换一张").clicked() {
                    action = Some(Action::Retry);
                }
                if ui.button("取消").clicked() {
                    action = Some(Action::Cancel);
                }
            });
        });
    action
}

/// 供 `App::do_login` 使用：按当前过码方式造一个手动过码回调。
pub(crate) fn solver_for(ctx: egui::Context, tx: Sender<CaptchaRequest>) -> UiCaptchaSolver {
    UiCaptchaSolver::new(ctx, tx)
}
