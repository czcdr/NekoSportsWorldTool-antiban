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
/// 登录线程已经不在时（panic / 退出）再点弹窗按钮要给的提示。
pub const LOGIN_GONE_STATUS: &str = "登录线程已结束，请重新登录";

/// 解码上限：GT4 的图只有几百像素宽，超过这个尺寸一定是坏数据或被换过的图。
/// `max_alloc` 限的是解码期的分配（正常图不到 1 MB），32 MB 足够容错。
const MAX_IMAGE_WIDTH: u32 = 4_096;
const MAX_IMAGE_HEIGHT: u32 = 4_096;
const MAX_IMAGE_ALLOC: u64 = 32 * 1024 * 1024;

/// 弹窗里图片之外要占的高度：标题 + 说明 + 滑杆 + 微调行 + 按钮行 + 间距。
/// 算缩放时要先把它从可用高度里扣掉，否则「图塞得下、按钮在屏外」。
const CONTROLS_RESERVE: f32 = 150.0;
/// 窗口与屏幕边缘之间留的余量（贴边 + 内边距）。
const WINDOW_MARGIN: f32 = 32.0;

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
                    return ManualOutcome::Timeout;
                }
            }
            // 界面那边把请求丢了（App 退出 / 收尾清理）：没有人在看结果，
            // 退回取消语义即可 —— 这条路本来就没有能被读到的文案。
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
    /// 首帧是否已经清过输入焦点（只做一次，之后用户自己点哪儿就归哪儿）。
    focus_cleared: bool,
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
            focus_cleared: false,
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

/// 解码上限。`Limits` 是 `#[non_exhaustive]`，只能从 `default()` 起改。
fn decode_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_WIDTH);
    limits.max_image_height = Some(MAX_IMAGE_HEIGHT);
    limits.max_alloc = Some(MAX_IMAGE_ALLOC);
    limits
}

/// 图片在弹窗里的显示缩放：宽、高都要塞得下，且不放大（放大只会让像素对齐更难）。
fn fit_scale(bg_w: f32, bg_h: f32, avail_w: f32, avail_h: f32) -> f32 {
    if bg_w < 1.0 || bg_h < 1.0 {
        // 尺寸读不出来（理论上不会：解码成功就有尺寸）—— 按原尺寸画，
        // 别在这里除出 NaN 把一个还能看的弹窗整没。
        return 1.0;
    }
    let by_w = avail_w.max(1.0) / bg_w;
    let by_h = avail_h.max(1.0) / bg_h;
    by_w.min(by_h).min(1.0)
}

/// 解码一张挑战图。
///
/// 走带上限的 reader：这张图来自服务端（中间人也够得着），`load_from_memory`
/// 会在 UI 线程上无上限地按声明的尺寸分配。超限/坏图都落到 `decode_error`，
/// 由弹窗给出「换一张 / 取消」两条出口。
fn decode_texture(
    ctx: &egui::Context,
    name: &str,
    png: &[u8],
) -> Result<egui::TextureHandle, String> {
    // 只按 PNG 解：GT4 给的就是 PNG，让解码器自己猜格式等于把不可信字节
    // 交给一串别的解码器。
    let mut reader =
        image::ImageReader::with_format(std::io::Cursor::new(png), image::ImageFormat::Png);
    reader.limits(decode_limits());
    let image = reader
        .decode()
        .map_err(|e| format!("图片解码失败：{e}"))?
        .to_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    let color = egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw());
    Ok(ctx.load_texture(name, color, egui::TextureOptions::LINEAR))
}

impl App {
    /// 收取后台线程送来的挑战图。
    ///
    /// `LOGIN_DONE` 与挑战图可能同时躺在队列里（弱网 + 切后台时最典型：
    /// 后台线程 5 分钟超时后发了 LOGIN_DONE，回来第一帧 poll_messages 先把
    /// 登录判死，紧接着这里又把那张已经没人会回答的图收成 pending）。
    /// 收下就是给用户一个「拖完点提交没反应」的僵尸弹窗，所以只收登录还活着
    /// 的请求；丢掉的请求随 responder 一起断开，后台线程那边立刻收到 Disconnected。
    pub(crate) fn poll_captcha(&mut self) {
        while let Ok(request) = self.captcha_rx.try_recv() {
            if !self.login_busy {
                drop(request);
                continue;
            }
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
                self.send_answer(pending, CaptchaAnswer::Cancel, CANCELLED_STATUS.to_string());
            }
            Some(Action::Retry) => {
                self.send_answer(pending, CaptchaAnswer::Retry, "换一张图…".to_string());
            }
            Some(Action::Submit {
                set_left,
                passtime_ms,
            }) => {
                self.send_answer(
                    pending,
                    CaptchaAnswer::Slide {
                        set_left,
                        passtime_ms,
                    },
                    format!("已提交滑块位置 {set_left}px，正在校验…"),
                );
            }
        }
    }

    /// 把答案送回登录线程。
    ///
    /// 发送失败说明后台线程已经不在了（panic / 被退出）：这时还显示「正在校验…」
    /// 就是在撒谎，而且登录按钮会永远灰着。改成明说登录线程已经结束。
    fn send_answer(&mut self, pending: PendingCaptcha, answer: CaptchaAnswer, status: String) {
        if pending.request.responder.send(answer).is_err() {
            self.login_busy = false;
            self.status = LOGIN_GONE_STATUS.into();
            return;
        }
        self.status = status;
    }
}

fn draw_modal(ctx: &egui::Context, pending: &mut PendingCaptcha) -> Option<Action> {
    if !pending.focus_cleared {
        pending.focus_cleared = true;
        // 弹窗打开时焦点往往还留在密码框里：拖动的第一下会被当成文本框点击，
        // 手机上软键盘也不收（`adjustResize` 会把窗口压矮，把按钮挤出屏幕）。
        // 注意不能 dispatch_focus 到弹窗自己的 id —— `surrender_focus` 只在传入的
        // id 正好是当前焦点时才生效，所以这里清的是「当前焦点」。
        if let Some(focused) = ctx.memory(|m| m.focused()) {
            ctx.memory_mut(|m| m.surrender_focus(focused));
        }
    }
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
            let avail_w = ui.available_width().max(120.0);
            // 高度也必须参与：窗口一矮（分屏、键盘顶起、桌面把窗口拉小）时，
            // 只按宽度缩会把底部那行「提交/取消」裁到屏幕外 —— 那是唯一的逃生口。
            // 不套 ScrollArea：图上是 Sense::drag，滚动会和拖动抢手势。
            let avail_h = (ctx.screen_rect().height() - WINDOW_MARGIN - CONTROLS_RESERVE).max(80.0);
            let scale = fit_scale(bg_w, bg_h, avail_w, avail_h);
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

    /// 一张真 PNG：弹窗要能画出来，才能验「按钮在不在屏内」。
    fn png_fixture(w: u32, h: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(w, h, image::Rgba([200, 120, 40, 255]));
        let mut bytes = Vec::new();
        let written = image::DynamicImage::ImageRgba8(image).write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        );
        assert!(written.is_ok(), "测试用的 PNG 要能编码");
        bytes
    }

    /// 把一张「真图」请求塞进队列并收进界面（调用方负责先置 login_busy）。
    fn pending_with_png(app: &mut App, id: u64) -> Receiver<CaptchaAnswer> {
        let (responder, answers) = std::sync::mpsc::channel();
        let request = CaptchaRequest {
            id,
            bg_png: png_fixture(600, 300),
            slice_png: png_fixture(100, 100),
            bg_width: 600,
            bg_height: 300,
            slice_width: 100,
            slice_height: 100,
            ypos: 120,
            max_offset: 500,
            responder,
        };
        assert!(app.captcha_tx.clone().send(request).is_ok());
        app.poll_captcha();
        answers
    }

    fn run_modal_frame(app: &mut App, ctx: &egui::Context, w: f32, h: f32) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(w, h))),
                ..Default::default()
            },
            |ctx| app.draw_captcha_window(ctx),
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

    /// 没人动：等满耐心值就放弃（不让登录线程永远挂着），并且要能和
    /// 「用户自己点了取消」区分开 —— 否则状态栏会冤枉用户误触。
    #[test]
    fn unanswered_challenge_times_out_distinctly_from_cancel() {
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
        assert!(
            matches!(outcome, ManualOutcome::Timeout),
            "等待超时必须和取消区分开，实际 {outcome:?}"
        );
    }

    /// 登录线程收工后，界面不能留着一个永远等不到答案的弹窗。
    #[test]
    fn pending_challenge_is_dropped_when_the_login_thread_ends() {
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx);
        app.login_busy = true;
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

    /// R1：登录已经判死之后才到的挑战图必须丢弃，不能再弹出一个
    /// 「拖完点提交没反应」的僵尸弹窗。
    #[test]
    fn stale_challenge_is_dropped_after_the_login_ended() {
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx);
        let (responder, answers) = std::sync::mpsc::channel();
        assert!(app
            .captcha_tx
            .clone()
            .send(fixture_request(responder, 250))
            .is_ok());
        // 后台线程已经收工（超时/失败/取消），LOGIN_DONE 已把 login_busy 放下。
        app.login_busy = false;
        app.poll_captcha();
        assert!(
            app.captcha.pending.is_none(),
            "登录已结束，这张图没人会回答，不能收成 pending"
        );
        // 丢掉请求的同时 responder 断开，后台线程不会挂在那儿等。
        assert!(answers
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err());
    }

    /// R6：后台线程已经不在了（panic/退出）时点弹窗按钮，不能装作「正在校验」。
    #[test]
    fn dead_login_thread_is_reported_instead_of_stuck_in_checking() {
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx);
        app.login_busy = true;
        let (responder, answers) = std::sync::mpsc::channel();
        assert!(app
            .captcha_tx
            .clone()
            .send(fixture_request(responder, 250))
            .is_ok());
        app.poll_captcha();
        let Some(pending) = app.captcha.pending.take() else {
            assert!(false, "请求要能被界面收下");
            return;
        };
        // 登录线程没了：接收端随栈解开被丢掉。
        drop(answers);
        app.send_answer(pending, CaptchaAnswer::Cancel, CANCELLED_STATUS.to_string());
        assert!(!app.login_busy, "登录线程已结束就必须放开登录按钮");
        assert_eq!(app.status, LOGIN_GONE_STATUS);
    }

    /// R1 的顺序假设：`LOGIN_DONE` 与那张旧挑战图**同一帧**到达时，
    /// 必须按 `update()` 的真实顺序（先 `poll_messages` 后 `poll_captcha`）
    /// 得出「登录结束、弹窗不出现」。
    ///
    /// 不直接调 `eframe::App::update`：`&mut eframe::Frame` 在单元测试里造不出来。
    /// 这里复刻 update() 的那两行调用顺序，把顺序本身钉死。
    #[test]
    fn r1_same_frame_login_done_wins_over_the_stale_challenge() {
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx);
        app.login_busy = true;
        // 后台线程超时前发出的挑战图，和 LOGIN_DONE 一起排在队列里。
        let (responder, answers) = std::sync::mpsc::channel();
        assert!(app
            .captcha_tx
            .clone()
            .send(fixture_request(responder, 250))
            .is_ok());
        assert!(app
            .tx
            .send(format!(
                "{}{}",
                crate::ui::LOGIN_DONE,
                serde_json::json!({"ok": false, "message": CANCELLED_STATUS})
            ))
            .is_ok());
        // update() 的真实顺序。
        app.poll_messages();
        app.poll_captcha();
        assert!(!app.login_busy);
        assert!(
            app.captcha.pending.is_none(),
            "同一帧里登录已结束，旧挑战图不能再复活成弹窗"
        );
        assert_eq!(app.status, CANCELLED_STATUS);
        // 丢掉请求即断开，后台线程不会继续等。
        assert!(answers
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err());
    }

    /// R8：弹窗首帧要把输入焦点从密码框上拿走（否则手机上软键盘不收，
    /// 拖动的第一下也落在文本框里）。
    #[test]
    fn opening_the_modal_releases_input_focus() {
        let ctx = egui::Context::default();
        let mut app = fixture_app(&ctx);
        app.login_busy = true;
        let field = egui::Id::new("login_password");
        ctx.memory_mut(|m| m.request_focus(field));
        assert_eq!(ctx.memory(|m| m.focused()), Some(field), "先造出焦点");
        let (responder, _answers) = std::sync::mpsc::channel();
        assert!(app
            .captcha_tx
            .clone()
            .send(fixture_request(responder, 250))
            .is_ok());
        app.poll_captcha();
        let _ = run_modal_frame(&mut app, &ctx, 900.0, 700.0);
        assert_eq!(ctx.memory(|m| m.focused()), None, "弹窗打开时必须让出焦点");
    }

    /// R4：窗口再矮也要够得着三个按钮（取消是唯一逃生口）。
    #[test]
    fn challenge_modal_keeps_every_button_on_screen() {
        for (w, h) in [(640.0f32, 360.0f32), (360.0, 640.0)] {
            let ctx = egui::Context::default();
            let mut app = fixture_app(&ctx);
            app.login_busy = true;
            let _answers = pending_with_png(&mut app, 11);
            let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(w, h));
            let mut output = run_modal_frame(&mut app, &ctx, w, h);
            // 窗口要一两帧才摆到居中位置。
            for _ in 0..3 {
                output = run_modal_frame(&mut app, &ctx, w, h);
            }
            for label in ["提交验证", "换一张", "取消"] {
                let Some(at) = label_position(&output, label) else {
                    assert!(false, "{w}x{h} 下「{label}」必须画得出来（不被裁掉）");
                    continue;
                };
                assert!(
                    screen.contains(at),
                    "{w}x{h} 下「{label}」跑到屏幕外了：{at:?}"
                );
            }
        }
    }

    /// R4 的缩放规则本身：宽、高都要满足，且永不放大。
    #[test]
    fn scale_respects_both_axes_and_never_upscales() {
        // 宽受限
        assert!((fit_scale(600.0, 300.0, 300.0, 1_000.0) - 0.5).abs() < 1e-6);
        // 高受限（正是 R4 的回归点：只除宽会让底部按钮出屏）
        assert!((fit_scale(600.0, 300.0, 1_000.0, 150.0) - 0.5).abs() < 1e-6);
        // 都不放大
        assert!((fit_scale(100.0, 100.0, 1_000.0, 1_000.0) - 1.0).abs() < 1e-6);
        // 尺寸坏掉时不产生 NaN（会整块画不出来）
        assert!(fit_scale(0.0, 0.0, 500.0, 500.0).is_finite());
        assert!(fit_scale(600.0, 0.0, 500.0, 500.0).is_finite());
    }

    /// R5：解码有上限；坏图走 decode_error → 弹窗给「换一张/取消」的出口。
    #[test]
    fn decoding_is_bounded_and_failures_land_in_the_retry_exit() {
        let ctx = egui::Context::default();
        let limits = decode_limits();
        assert_eq!(limits.max_image_width, Some(MAX_IMAGE_WIDTH));
        assert_eq!(limits.max_image_height, Some(MAX_IMAGE_HEIGHT));
        assert_eq!(limits.max_alloc, Some(32 * 1024 * 1024));

        // 正常尺寸能解。
        assert!(decode_texture(&ctx, "t-small", &png_fixture(64, 32)).is_ok());
        // 超宽（> MAX_IMAGE_WIDTH）必须拒绝，而不是照尺寸分配。
        assert!(
            decode_texture(&ctx, "t-wide", &png_fixture(MAX_IMAGE_WIDTH + 16, 8)).is_err(),
            "超过解码宽度上限的图必须被拒绝"
        );
        // 不是 PNG 的字节同样拒绝。
        assert!(decode_texture(&ctx, "t-garbage", &[0x89, b'P', b'N', b'G', 0, 1, 2]).is_err());

        // 坏图进弹窗：落到 decode_error，而不是卡在「正在解码图片…」。
        let mut app = fixture_app(&ctx);
        app.login_busy = true;
        let (responder, _answers) = std::sync::mpsc::channel();
        assert!(app
            .captcha_tx
            .clone()
            .send(fixture_request(responder, 250))
            .is_ok());
        app.poll_captcha();
        // 窗口要一两帧才知道自己的尺寸（首帧的位置判定用的是上一帧的矩形）。
        let mut output = run_modal_frame(&mut app, &ctx, 640.0, 480.0);
        for _ in 0..3 {
            output = run_modal_frame(&mut app, &ctx, 640.0, 480.0);
        }
        let error = app
            .captcha
            .pending
            .as_ref()
            .and_then(|p| p.decode_error.clone());
        assert!(error.is_some(), "坏图必须落到 decode_error 分支");
        // 该分支的两个出口都在屏上。
        assert!(label_position(&output, "换一张").is_some());
        assert!(label_position(&output, "取消").is_some());
    }

    /// 「取消」不是失败：状态栏不能出现「登录失败：」，超时也不能被当成取消。
    #[test]
    fn cancel_status_never_reads_as_a_login_failure() {
        assert_eq!(failure_status(CANCELLED_STATUS), CANCELLED_STATUS);
        assert!(is_cancelled("已取消验证码"));
        assert!(!is_cancelled("验证码校验未通过"));
        assert_eq!(failure_status("账号或密码错误"), "登录失败：账号或密码错误");
        assert!(!failure_status("账号或密码错误").starts_with(CANCELLED_STATUS));
        // R3：等待超时不是「用户取消」，必须说真实原因。
        let timeout = "验证码未能完成：验证码等待超时（5 分钟未操作）";
        assert!(!is_cancelled(timeout), "超时不能被当成取消");
        assert!(failure_status(timeout).contains("等待超时"));
        assert!(!failure_status(timeout).contains(CANCELLED_STATUS));
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
