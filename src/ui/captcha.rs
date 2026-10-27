//! 手动过码界面：把 GT4 挑战图显示出来，让用户把方块拖进缺口。
//!
//! 线程模型：整条登录链在后台线程里跑。取到挑战图之后，后台线程把「两张图 +
//! 一个回收答案的通道」丢进 UI 队列，然后阻塞等答案；界面这边弹窗，用户
//! 拖完 / 换一张 / 取消，答案回填后后台线程继续 /verify → geevalidate → login。
//! 界面线程一秒都没被网络请求挡住。

use super::App;
use crate::api::gt4::{Gt4Challenge, ManualOutcome, ManualSolver};
use crate::api::login::CaptchaMode;
use eframe::egui;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};

/// 等答案时的轮询间隔：手机前台要一直重绘，才能看见拖动中的方块。
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);
/// 多久没人动就放弃，别让登录线程永远挂着。
const PATIENCE: std::time::Duration = std::time::Duration::from_secs(300);

/// 用户取消验证码时后台线程回报的那句话（与 `Gt4Error::Cancelled` 的文案一致）。
pub const CANCELLED_STATUS: &str = "已取消验证码";

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
        let (responder, answers) = std::sync::mpsc::channel();
        let request = request_for(challenge, responder);
        await_answer(&self.ctx, &self.tx, request, answers, PATIENCE)
    }
}

/// 把一张挑战图打包成一条界面消息。
fn request_for(challenge: &Gt4Challenge, responder: Sender<CaptchaAnswer>) -> CaptchaRequest {
    static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);
    CaptchaRequest {
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
    }
}

/// 把图交给界面，然后阻塞等答案。
///
/// 与 egui 解耦（只要一个 `Context` 来催重绘），所以三条通道 —— 交卷 / 换一张 /
/// 界面消失（通道断开）—— 都能单独测。
fn await_answer(
    ctx: &egui::Context,
    tx: &Sender<CaptchaRequest>,
    request: CaptchaRequest,
    answers: Receiver<CaptchaAnswer>,
    patience: std::time::Duration,
) -> ManualOutcome {
    let max_offset = request.max_offset;
    if tx.send(request).is_err() {
        // 界面已经没了，没人能回答这个问题。
        return ManualOutcome::Cancel;
    }
    ctx.request_repaint();
    let started = std::time::Instant::now();
    loop {
        match answers.recv_timeout(POLL_INTERVAL) {
            Ok(CaptchaAnswer::Slide {
                set_left,
                passtime_ms,
            }) => {
                return ManualOutcome::Slide {
                    // 界面已经夹过，这里再夹一次：位移超出图宽一定是 bug，
                    // 与其把怪值交给服务端，不如按边界交卷。
                    set_left: set_left.min(max_offset),
                    passtime_ms,
                };
            }
            Ok(CaptchaAnswer::Retry) => return ManualOutcome::Retry,
            Ok(CaptchaAnswer::Cancel) => return ManualOutcome::Cancel,
            Err(RecvTimeoutError::Timeout) => {
                // 每 200ms 催一次重绘：手机前台时窗口需要一直画着拖动效果。
                ctx.request_repaint();
                // 超时没人动就当放弃，别让登录线程一直挂着。
                if started.elapsed() > patience {
                    return ManualOutcome::Cancel;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return ManualOutcome::Cancel,
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

impl CaptchaUi {
    /// 登录线程已经结束（成功 / 失败 / 取消 / 超时）：还挂着的挑战图不可能再
    /// 被回答了。留着它只会让界面停在一个永远等不到结果的弹窗上。
    pub(crate) fn abandon(&mut self) {
        self.pending = None;
    }
}

/// 「过码方式」选择器。桌面与手机两套布局共用同一份控件（手机屏窄，放同一行
/// 会挤成一列，但控件行为一致）。返回值只在用户真的改了选择时给出。
pub(crate) fn captcha_mode_selector(
    ui: &mut egui::Ui,
    current: &mut String,
) -> Option<CaptchaMode> {
    let before = CaptchaMode::parse(current);
    let mut mode = before;
    ui.label("过码方式：");
    ui.selectable_value(&mut mode, CaptchaMode::Auto, CaptchaMode::Auto.label())
        .on_hover_text("程序自动识别缺口（默认）");
    ui.selectable_value(&mut mode, CaptchaMode::Manual, CaptchaMode::Manual.label())
        .on_hover_text("登录时弹出挑战图，由你把方块拖进缺口；认错时不会自动当成判错");
    if mode == before {
        return None;
    }
    *current = mode.as_str().to_string();
    Some(mode)
}

/// 切换过码方式后的状态栏文案。
///
/// 保存失败必须明说：`save_config` 在本地加密不可用时会拒绝写盘（不把口令
/// 落成明文），这时显示「已切换」就是在骗人 —— 用户重启后会莫名其妙回到自动。
pub(crate) fn mode_status(mode: CaptchaMode, saved: Result<(), String>) -> String {
    match saved {
        Ok(()) => format!("过码方式：{}（已保存）", mode.label()),
        Err(error) => format!("过码方式已切换为{}，但{error}", mode.label()),
    }
}

/// 登录失败消息是不是「用户自己取消了验证码」。
pub fn is_cancelled(message: &str) -> bool {
    message.trim_start().starts_with(CANCELLED_STATUS)
}

/// `LOGIN_DONE` 失败分支的状态栏文案。
///
/// 取消是用户自己的选择，不是失败：显示「登录失败：已取消验证码」既不准，
/// 也会让人以为账号或网络出了问题。
pub fn failure_status(message: &str) -> String {
    if is_cancelled(message) {
        message.trim().to_string()
    } else {
        format!("登录失败：{message}")
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

    /// 切换过码方式并立刻落盘（重启后保持）。
    pub(crate) fn set_captcha_mode(&mut self, mode: CaptchaMode) {
        self.config.captcha_mode = mode.as_str().to_string();
        self.status = mode_status(mode, super::save_config(&self.config));
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
                egui::Stroke::new(1.0_f32, super::theme::accent()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::model::{Config, HeaderIdentity, Session};
    use egui::{Event, PointerButton, Pos2, Rect, Vec2};

    /// 独立数据目录：切换过码方式会落盘，不能写到开发机真实配置里。
    struct TempDataDir(std::path::PathBuf);

    impl TempDataDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("neko-{tag}-{}", uuid::Uuid::new_v4()));
            let _ = std::fs::create_dir_all(&dir);
            crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = Some(dir.clone()));
            Self(dir)
        }
    }

    impl Drop for TempDataDir {
        fn drop(&mut self) {
            crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = None);
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture_request(responder: Sender<CaptchaAnswer>, max_offset: usize) -> CaptchaRequest {
        CaptchaRequest {
            id: 1,
            bg_png: Vec::new(),
            slice_png: Vec::new(),
            bg_width: 300,
            bg_height: 150,
            slice_width: 50,
            slice_height: 50,
            ypos: 20,
            max_offset,
            responder,
        }
    }

    fn fixture_app(ctx: &egui::Context) -> App {
        App::from_state(
            ctx,
            HeaderIdentity::default(),
            Session::default(),
            Config::default(),
        )
    }

    fn run_frame(app: &mut App, ctx: &egui::Context, w: f32, h: f32) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(w, h))),
                ..Default::default()
            },
            |ctx| app.draw_shell(ctx),
        )
    }

    fn click_at(app: &mut App, ctx: &egui::Context, w: f32, h: f32, at: Pos2) {
        for pressed in [true, false] {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(w, h))),
                    events: vec![
                        Event::PointerMoved(at),
                        Event::PointerButton {
                            pos: at,
                            button: PointerButton::Primary,
                            pressed,
                            modifiers: Default::default(),
                        },
                    ],
                    ..Default::default()
                },
                |ctx| app.draw_shell(ctx),
            );
        }
    }

    fn label_position(output: &egui::FullOutput, label: &str) -> Option<Pos2> {
        output.shapes.iter().find_map(|clipped| {
            if let egui::Shape::Text(text) = &clipped.shape {
                let rect = Rect::from_min_size(text.pos, text.galley.size());
                if text.galley.text() == label && clipped.clip_rect.contains(rect.center()) {
                    return Some(rect.center());
                }
            }
            None
        })
    }

    /// 画几帧（让 egui 建好交互矩形）再找标签。
    fn find_label(app: &mut App, ctx: &egui::Context, w: f32, h: f32, label: &str) -> Option<Pos2> {
        let mut found = None;
        for _ in 0..3 {
            let output = run_frame(app, ctx, w, h);
            if let Some(pos) = label_position(&output, label) {
                found = Some(pos);
            }
        }
        found
    }

    /// UiCaptchaSolver 的三条通道：交卷 / 换一张 / 界面消失。
    #[test]
    fn submit_path_clamps_slide_position_to_the_image_width() {
        let (tx, requests) = std::sync::mpsc::channel::<CaptchaRequest>();
        let (responder, answers) = std::sync::mpsc::channel();
        let request = fixture_request(responder, 250);
        let ui = std::thread::spawn(move || {
            if let Ok(got) = requests.recv_timeout(std::time::Duration::from_secs(2)) {
                // 界面夹过一遍，这里再故意给一个越界值：后台必须再夹一次。
                let _ = got.responder.send(CaptchaAnswer::Slide {
                    set_left: 9_999,
                    passtime_ms: 1_234,
                });
            }
        });
        let outcome = await_answer(
            &egui::Context::default(),
            &tx,
            request,
            answers,
            std::time::Duration::from_secs(2),
        );
        let _ = ui.join();
        match outcome {
            ManualOutcome::Slide {
                set_left,
                passtime_ms,
            } => {
                assert_eq!(set_left, 250, "越界位移必须夹到 max_offset");
                assert_eq!(passtime_ms, 1_234, "拖动耗时原样交给服务端");
            }
            other => assert!(false, "提交通道应得到 Slide，实际 {other:?}"),
        }
    }

    #[test]
    fn retry_path_asks_for_another_challenge() {
        let (tx, requests) = std::sync::mpsc::channel::<CaptchaRequest>();
        let (responder, answers) = std::sync::mpsc::channel();
        let request = fixture_request(responder, 250);
        let ui = std::thread::spawn(move || {
            if let Ok(got) = requests.recv_timeout(std::time::Duration::from_secs(2)) {
                let _ = got.responder.send(CaptchaAnswer::Retry);
            }
        });
        let outcome = await_answer(
            &egui::Context::default(),
            &tx,
            request,
            answers,
            std::time::Duration::from_secs(2),
        );
        let _ = ui.join();
        assert!(matches!(outcome, ManualOutcome::Retry));
    }

    #[test]
    fn cancel_path_stops_the_login_thread() {
        let (tx, requests) = std::sync::mpsc::channel::<CaptchaRequest>();
        let (responder, answers) = std::sync::mpsc::channel();
        let request = fixture_request(responder, 250);
        let ui = std::thread::spawn(move || {
            if let Ok(got) = requests.recv_timeout(std::time::Duration::from_secs(2)) {
                let _ = got.responder.send(CaptchaAnswer::Cancel);
            }
        });
        let outcome = await_answer(
            &egui::Context::default(),
            &tx,
            request,
            answers,
            std::time::Duration::from_secs(2),
        );
        let _ = ui.join();
        assert!(matches!(outcome, ManualOutcome::Cancel));
    }

    /// 界面直接消失（窗口关了 / App 退出 / 登录收尾时丢掉待答题）：
    /// 后台线程必须立刻收手，不能挂到 5 分钟。
    #[test]
    fn disappearing_interface_cancels_instead_of_blocking() {
        let (tx, requests) = std::sync::mpsc::channel::<CaptchaRequest>();
        let (responder, answers) = std::sync::mpsc::channel();
        let request = fixture_request(responder, 250);
        let ui = std::thread::spawn(move || {
            if let Ok(got) = requests.recv_timeout(std::time::Duration::from_secs(2)) {
                // 拿到请求就丢掉：responder 随之断开。
                drop(got);
            }
        });
        let started = std::time::Instant::now();
        let outcome = await_answer(
            &egui::Context::default(),
            &tx,
            request,
            answers,
            std::time::Duration::from_secs(300),
        );
        let _ = ui.join();
        assert!(matches!(outcome, ManualOutcome::Cancel));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "通道断开必须立刻返回，不能等满 5 分钟"
        );
    }

    /// 没人动：等满耐心值就放弃（不让登录线程永远挂着）。
    #[test]
    fn unanswered_challenge_gives_up_after_patience() {
        let (tx, requests) = std::sync::mpsc::channel::<CaptchaRequest>();
        let (responder, answers) = std::sync::mpsc::channel();
        let request = fixture_request(responder, 250);
        let ui = std::thread::spawn(move || {
            if let Ok(got) = requests.recv_timeout(std::time::Duration::from_secs(2)) {
                std::thread::sleep(std::time::Duration::from_millis(400));
                drop(got);
            }
        });
        let outcome = await_answer(
            &egui::Context::default(),
            &tx,
            request,
            answers,
            std::time::Duration::from_millis(10),
        );
        let _ = ui.join();
        assert!(matches!(outcome, ManualOutcome::Cancel));
    }

    /// 登录线程收工后，界面不能留着一个永远等不到答案的弹窗。
    #[test]
    fn pending_challenge_is_dropped_when_the_login_thread_ends() {
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx);
        let (responder, answers) = std::sync::mpsc::channel();
        assert!(app
            .captcha_tx
            .clone()
            .send(fixture_request(responder, 250))
            .is_ok());
        app.poll_captcha();
        assert!(app.captcha.pending.is_some(), "请求要能被界面收下");
        assert!(app.status.contains("滑块验证"));
        app.captcha.abandon();
        assert!(app.captcha.pending.is_none(), "登录结束后弹窗必须消失");
        // 丢掉请求 = 后台线程收到断开，不会一直等。
        assert!(answers
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err());
    }

    /// 「取消」不是失败：状态栏不能出现「登录失败：」。
    #[test]
    fn cancel_status_never_reads_as_a_login_failure() {
        assert_eq!(failure_status(CANCELLED_STATUS), CANCELLED_STATUS);
        assert!(is_cancelled("已取消验证码"));
        assert!(!is_cancelled("验证码校验未通过"));
        assert_eq!(failure_status("账号或密码错误"), "登录失败：账号或密码错误");
        assert!(!failure_status("账号或密码错误").starts_with(CANCELLED_STATUS));
    }

    /// 落盘失败（本地加密不可用 → save_config 拒绝写明文）时不能显示「已保存」。
    #[test]
    fn save_failure_is_reported_instead_of_pretending_it_worked() {
        let saved = mode_status(CaptchaMode::Manual, Ok(()));
        assert!(saved.contains(CaptchaMode::Manual.label()));
        assert!(saved.contains("已保存"));

        let refused = mode_status(
            CaptchaMode::Manual,
            Err("设置保存失败：本地加密不可用（本次改动仅当前会话有效）".into()),
        );
        assert!(refused.contains("设置保存失败"), "实际：{refused}");
        assert!(!refused.contains("已保存"), "实际：{refused}");
        assert!(
            refused.contains(CaptchaMode::Manual.label()),
            "要说明当前选择的是哪一种"
        );
    }

    /// 选择器只在用户真的改了选择时回报一次。
    #[test]
    fn selector_reports_only_real_changes() {
        let ctx = egui::Context::default();
        let mut mode = "auto".to_string();
        let mut changed = None;
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                if let Some(m) = captcha_mode_selector(ui, &mut mode) {
                    changed = Some(m);
                }
            });
        });
        assert_eq!(changed, None);
        assert_eq!(mode, "auto");
    }

    /// 桌面宽屏：顶栏直接能切到手动过码，并且当场落盘。
    #[test]
    fn desktop_layout_switches_captcha_mode_and_saves_it() {
        let _dir = TempDataDir::new("captcha-desktop");
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx);
        let (w, h) = (900.0, 700.0);
        let Some(at) = find_label(&mut app, &ctx, w, h, CaptchaMode::Manual.label()) else {
            assert!(false, "桌面布局必须能看到「手动过码」");
            return;
        };
        click_at(&mut app, &ctx, w, h, at);
        assert_eq!(app.config.captcha_mode, "manual");
        assert!(
            app.status.contains("手动过码") && app.status.contains("已保存"),
            "切换后要给状态提示，实际：{}",
            app.status
        );
        // 落盘：模拟重启后的读回。
        assert_eq!(crate::api::model::load_config().captcha_mode, "manual");
    }

    /// 手机 compact：折叠区里的同一个控件也能切，且同样落盘。
    #[test]
    fn compact_layout_switches_captcha_mode_and_saves_it() {
        let _dir = TempDataDir::new("captcha-compact");
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx);
        // 登录失效时折叠区会自动展开（真实场景：会话过期后要重新过码）。
        app.auth_expired = true;
        let (w, h) = (360.0, 600.0);
        let Some(at) = find_label(&mut app, &ctx, w, h, CaptchaMode::Manual.label()) else {
            assert!(false, "手机布局必须能看到「手动过码」");
            return;
        };
        click_at(&mut app, &ctx, w, h, at);
        assert_eq!(app.config.captcha_mode, "manual");
        assert!(app.status.contains("已保存"), "实际：{}", app.status);
        assert_eq!(crate::api::model::load_config().captcha_mode, "manual");
    }
}
