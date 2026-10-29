//! 「异常记录诊断（实验）」的离线测试。
//!
//! 覆盖三类，全部离线（无外网）：
//! 1. 闸门 / 状态机：禁跑置灰、冻结未知拦停、一次一条、提交后必须重新诊断；
//! 2. 真后台任务：诊断跑在 worker 线程（UI 帧不被阻塞），结果走消息回填；
//! 3. 手机尺寸下的可达性：折叠区入口、试修按钮、二次确认弹窗都是普通按钮
//!    （Android 没有右键/长按，确认不能藏进上下文菜单）。

use super::*;
use crate::ui::recover_panel::{
    DiagSnapshot, FixGate, FixReport, RecheckReport, RecoverPage, RowSnapshot, CONFIRM_RISK,
    COVER_ASSUMPTION,
};
use serde_json::json;
use std::time::{Duration, Instant};

fn row(rrid: i64, dis: f64) -> RowSnapshot {
    RowSnapshot {
        rrid,
        start_ms: 1_761_100_000_000,
        total_dis: dis,
        total_time: 420,
        reasons: vec!["点位未经过".into()],
        reason_text: "点位未经过".into(),
    }
}

fn clean_snapshot(rows: Vec<RowSnapshot>) -> DiagSnapshot {
    DiagSnapshot {
        freeze_seconds: Some(0),
        freeze_message: String::new(),
        freeze_text: "未冻结".into(),
        verdict: "2 条记录未达标，违规榜暂无本人条目。这是最可修的一种：覆盖提交后应能转绿。"
            .into(),
        flagged: false,
        cheat_label: "本次查询暂未发现违规记录".into(),
        cheat_reason: "服务端未提供原因".into(),
        cheat_time: "服务端未提供时间".into(),
        total_records: rows.len() + 1,
        rows,
        checked_at_ms: crate::crypto::envelope::now_ms(),
    }
}

fn frozen_snapshot() -> DiagSnapshot {
    let mut snapshot = clean_snapshot(vec![row(111, 1200.0), row(222, 1300.0)]);
    snapshot.freeze_seconds = Some(172_800);
    snapshot.freeze_text = "禁跑中，还剩 2 天 00:00:00".into();
    snapshot.freeze_message = "数据异常".into();
    snapshot
}

fn session() -> Session {
    Session {
        uid: 42,
        unid: "77".into(),
        token: "local-fixture-only".into(),
        ..Default::default()
    }
}

fn app_with(ctx: &egui::Context, logged_in: bool, snapshot: Option<DiagSnapshot>) -> App {
    let mut app = App::from_state(
        ctx,
        HeaderIdentity::default(),
        if logged_in {
            session()
        } else {
            Session::default()
        },
        Config::default(),
    );
    app.tab = 3;
    app.data_page.recover.snapshot = snapshot;
    app
}

/// 与 `App::update` 同序：shell + 二次确认弹窗（弹窗在 shell 之上）。
fn shell_frame(
    app: &mut App,
    ctx: &egui::Context,
    width: f32,
    height: f32,
    events: Vec<egui::Event>,
) -> egui::FullOutput {
    ctx.run(
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(width, height))),
            events,
            ..Default::default()
        },
        |ctx| {
            app.draw_shell(ctx);
            app.draw_recover_confirm(ctx);
        },
    )
}

fn click_shell(app: &mut App, ctx: &egui::Context, w: f32, h: f32, at: Pos2) {
    for pressed in [true, false] {
        shell_frame(
            app,
            ctx,
            w,
            h,
            vec![
                egui::Event::PointerMoved(at),
                egui::Event::PointerButton {
                    pos: at,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: Default::default(),
                },
            ],
        );
    }
}

/// 在可见区域里找一个**包含** needle 的文本（`label_position` 是精确匹配，
/// 而这里很多行是拼接出来的，例如「禁跑状态：禁跑中，还剩 …」）。
fn visible_text_pos(output: &egui::FullOutput, needle: &str) -> Option<Pos2> {
    output.shapes.iter().find_map(|clipped| {
        if let egui::Shape::Text(text) = &clipped.shape {
            let rect = Rect::from_min_size(text.pos, text.galley.size());
            if text.galley.text().contains(needle) && clipped.clip_rect.contains(rect.center()) {
                return Some(rect.center());
            }
        }
        None
    })
}

/// 滚动到某段文本可见为止。
fn scroll_to_contains(app: &mut App, ctx: &egui::Context, w: f32, h: f32, needle: &str) -> Pos2 {
    for _ in 0..60 {
        let output = frame(app, ctx, w, h, vec![]);
        if let Some(pos) = visible_text_pos(&output, needle) {
            return pos;
        }
        frame(
            app,
            ctx,
            w,
            h,
            vec![
                egui::Event::PointerMoved(Pos2::new(w / 2.0, h - 70.0)),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: Vec2::new(0.0, -60.0),
                    modifiers: Default::default(),
                },
            ],
        );
        for _ in 0..12 {
            frame(app, ctx, w, h, vec![]);
        }
    }
    panic!("outer scroll cannot reach {needle} at {w}x{h}");
}

/// 打开数据页里的折叠区（默认折叠 = 默认只读，进页面不会自动发请求）。
fn open_section(app: &mut App, ctx: &egui::Context, w: f32, h: f32) {
    let at = scroll_to_contains(app, ctx, w, h, crate::ui::recover_panel::RECOVER_TITLE);
    click(app, ctx, w, h, at);
}

fn frame_texts(output: &egui::FullOutput) -> Vec<String> {
    output
        .shapes
        .iter()
        .filter_map(|clipped| match &clipped.shape {
            egui::Shape::Text(text) => Some(text.galley.text().to_string()),
            _ => None,
        })
        .collect()
}

fn has_text(output: &egui::FullOutput, needle: &str) -> bool {
    frame_texts(output).iter().any(|text| text.contains(needle))
}

// ───────────────────────── 1. 闸门 / 状态机（无网络、无 egui）─────────────────────────

#[test]
fn frozen_account_greys_the_single_fix_button_with_the_cli_reason() {
    let page = RecoverPage {
        snapshot: Some(frozen_snapshot()),
        ..Default::default()
    };
    for rrid in [111, 222] {
        let gate = page.decide_fix(rrid, true);
        assert!(!gate.is_ready(), "禁跑期不该放行 rrid={rrid}");
        let reason = gate.reason().unwrap();
        assert!(
            reason.contains("禁跑中，还剩 2 天"),
            "必须带上与 CLI 同源的倒计时: {reason}"
        );
        assert!(
            reason.contains("服务端会拒绝提交"),
            "必须说明为什么不能点: {reason}"
        );
    }
}

#[test]
fn unknown_freeze_blocks_submission_until_rediagnosed() {
    let mut snapshot = clean_snapshot(vec![row(111, 1200.0)]);
    snapshot.freeze_seconds = None;
    snapshot.freeze_text = "未取到策略（冻结状态未知）".into();
    let page = RecoverPage {
        snapshot: Some(snapshot),
        ..Default::default()
    };
    let gate = page.decide_fix(111, true);
    assert!(!gate.is_ready(), "冻结状态未知等于不能提交");
    assert!(gate.reason().unwrap().contains("冻结状态未知"));
}

#[test]
fn stale_diagnosis_blocks_submission_until_refreshed() {
    let mut snapshot = clean_snapshot(vec![row(111, 1200.0)]);
    // 「几分钟前没冻结」不能当作现在也没冻结：CLI 每次都是现读策略。
    snapshot.checked_at_ms = crate::crypto::envelope::now_ms() - crate::api::cheat::FRESH_MS - 1;
    let mut page = RecoverPage {
        snapshot: Some(snapshot),
        ..Default::default()
    };
    let gate = page.decide_fix(111, true);
    assert!(!gate.is_ready(), "过期快照不许直接提交");
    assert!(gate.reason().unwrap().contains("已过期"));

    // 重新诊断后恢复（快照时间重新取现在）。
    page.apply_diag(clean_snapshot(vec![row(111, 1200.0)]));
    assert_eq!(page.decide_fix(111, true), FixGate::Ready);
}

#[test]
fn only_one_record_per_confirmation_and_one_submission_at_a_time() {
    let mut page = RecoverPage {
        snapshot: Some(clean_snapshot(vec![row(111, 1200.0), row(222, 1300.0)])),
        ..Default::default()
    };
    assert_eq!(page.decide_fix(111, true), FixGate::Ready);
    assert!(page.open_confirm(111, true).is_ok());
    assert_eq!(page.confirm, Some(111), "一次只能挂一条确认");
    // 换一条会替换掉当前确认，而不是排队 —— 不存在"多选"。
    assert!(page.open_confirm(222, true).is_ok());
    assert_eq!(page.confirm, Some(222));

    page.begin_fix(222);
    assert_eq!(page.confirm, None, "确认后弹窗必须收起");
    assert!(page.fix_busy);
    let gate = page.decide_fix(111, true);
    assert!(!gate.is_ready(), "提交在飞时不许再选第二条");
    assert!(gate.reason().unwrap().contains("每次只能提交一条"));
}

#[test]
fn after_a_submission_a_fresh_diagnosis_is_required_before_the_next_one() {
    let mut page = RecoverPage {
        snapshot: Some(clean_snapshot(vec![row(111, 1200.0), row(222, 1300.0)])),
        ..Default::default()
    };
    page.begin_fix(111);
    page.apply_fix(FixReport {
        rrid: 111,
        new_rrid: 900,
        verdict: "达标".into(),
        complete: Some(true),
        recheck: Some(RecheckReport {
            complete: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    });
    assert!(!page.fix_busy);
    assert_eq!(page.fix_rrid, None);
    assert_eq!(page.fix_result.as_ref().unwrap().turned_green(), Some(true));
    let gate = page.decide_fix(222, true);
    assert!(
        !gate.is_ready(),
        "提交过一条后连点下一条 = 事实上的批量，必须拦住"
    );
    assert!(gate.reason().unwrap().contains("重新诊断"));

    // 重新诊断后（且新快照里仍列着这条）才恢复单条入口。
    page.apply_diag(clean_snapshot(vec![row(222, 1300.0)]));
    assert!(!page.needs_rediagnose);
    assert_eq!(page.decide_fix(222, true), FixGate::Ready);
    assert!(
        !page.decide_fix(111, true).is_ready(),
        "新快照里没有 rrid=111 就不许再对它提交"
    );
}

#[test]
fn failure_also_requires_rediagnosis_because_the_server_may_have_kept_it() {
    let mut page = RecoverPage {
        snapshot: Some(clean_snapshot(vec![row(111, 1200.0)])),
        ..Default::default()
    };
    page.begin_fix(111);
    page.apply_fix(FixReport {
        rrid: 111,
        error: Some("连接中断".into()),
        ..Default::default()
    });
    assert!(
        page.needs_rediagnose,
        "失败也可能是「已提交未确认」，不能直接重试"
    );
    assert!(page
        .fix_result
        .as_ref()
        .unwrap()
        .conclusion()
        .contains("重新诊断"));
}

#[test]
fn diagnosis_states_clear_busy_flags_and_busy_diagnosis_blocks_fixing() {
    let mut page = RecoverPage::default();
    page.begin_diag();
    assert!(page.diag_busy);
    assert!(!page.decide_fix(1, true).is_ready(), "诊断在飞时不许动记录");
    page.apply_diag(clean_snapshot(vec![row(1, 1200.0)]));
    assert!(!page.diag_busy);
    assert_eq!(page.decide_fix(1, true), FixGate::Ready);

    page.begin_diag();
    page.apply_diag_error("网络不可用".into());
    assert!(!page.diag_busy);
    assert!(page.snapshot.is_none(), "诊断失败不能留着旧快照当现状");
    assert!(!page.decide_fix(1, true).is_ready());
}

#[test]
fn expired_session_releases_every_in_flight_flag() {
    let mut page = RecoverPage {
        snapshot: Some(clean_snapshot(vec![row(111, 1200.0)])),
        confirm: Some(111),
        needs_rediagnose: true,
        ..Default::default()
    };
    page.begin_fix(111);
    page.session_lost();
    assert!(!page.fix_busy);
    assert!(!page.diag_busy);
    assert_eq!(page.fix_rrid, None);
    assert_eq!(page.confirm, None);
    assert!(page.snapshot.is_none());
    assert!(page.diag_error.unwrap().contains("登录已失效"));
}

// ───────────────────────── 2. 手机尺寸可达性（无网络）─────────────────────────

#[test]
fn recover_entry_and_read_only_button_are_reachable_on_phone_screens() {
    for (w, h) in [
        (320.0, 360.0),
        (360.0, 640.0),
        (640.0, 360.0),
        (880.0, 720.0),
    ] {
        let ctx = egui::Context::default();
        let mut app = app_with(&ctx, false, None);
        open_section(&mut app, &ctx, w, h);
        // 固定警示（覆盖假设）必须在折叠区里看得到，不能只写在代码注释里。
        scroll_to_contains(&mut app, &ctx, w, h, COVER_ASSUMPTION);
        // 未登录：按钮存在但不可点，且不会悄悄起一个后台任务。
        let button = scroll_to_contains(&mut app, &ctx, w, h, "只读诊断");
        click(&mut app, &ctx, w, h, button);
        assert!(!app.data_page.recover.diag_busy, "未登录不该发起诊断");
        assert!(app.data_page.recover.snapshot.is_none());
        let output = shell_frame(&mut app, &ctx, w, h, vec![]);
        assert!(
            has_text(&output, "未登录：诊断与试修都不可用"),
            "width={w}: 必须说明为什么点不动"
        );
    }
}

#[test]
fn frozen_snapshot_shows_the_reason_and_never_opens_the_confirm_dialog() {
    for (w, h) in [(360.0, 640.0), (640.0, 360.0)] {
        let ctx = egui::Context::default();
        let mut app = app_with(&ctx, true, Some(frozen_snapshot()));
        open_section(&mut app, &ctx, w, h);
        scroll_to_contains(&mut app, &ctx, w, h, "禁跑中，还剩 2 天");
        let button = scroll_to_contains(&mut app, &ctx, w, h, "试修 rrid=111");
        click(&mut app, &ctx, w, h, button);
        assert_eq!(
            app.data_page.recover.confirm, None,
            "禁跑期点试修必须没有任何反应（按钮置灰）"
        );
        assert!(!app.data_page.recover.fix_busy);
        let output = shell_frame(&mut app, &ctx, w, h, vec![]);
        assert!(
            has_text(&output, "禁跑"),
            "width={w}: 置灰原因必须写在页面上"
        );
    }
}

#[test]
fn confirm_dialog_is_a_plain_two_button_dialog_with_the_mandated_warning() {
    for (w, h) in [(360.0, 640.0), (640.0, 360.0)] {
        let ctx = egui::Context::default();
        let mut app = app_with(&ctx, true, Some(clean_snapshot(vec![row(42, 1200.0)])));
        open_section(&mut app, &ctx, w, h);
        let button = scroll_to_contains(&mut app, &ctx, w, h, "试修 rrid=42");
        click_shell(&mut app, &ctx, w, h, button);
        assert_eq!(app.data_page.recover.confirm, Some(42));

        let output = shell_frame(&mut app, &ctx, w, h, vec![]);
        assert!(
            has_text(&output, CONFIRM_RISK),
            "width={w}: 确认弹窗必须写明「{CONFIRM_RISK}」"
        );
        assert!(has_text(&output, "未抓包验证"));
        // 普通按钮，不依赖右键/长按。
        let cancel = label_position(&output, "取消").expect("取消按钮必须是普通按钮");
        let ok = label_position(&output, "确认覆盖提交").expect("确认按钮必须是普通按钮");
        assert!(cancel.x >= 0.0 && cancel.x <= w, "width={w}: 取消越界");
        assert!(ok.x >= 0.0 && ok.x <= w, "width={w}: 确认越界");

        click_shell(&mut app, &ctx, w, h, cancel);
        assert_eq!(
            app.data_page.recover.confirm, None,
            "取消必须关掉弹窗，且不改任何提交状态"
        );
        assert!(!app.data_page.recover.fix_busy);
        assert_eq!(app.data_page.recover.fix_rrid, None);
    }
}

#[test]
fn confirming_a_fix_runs_off_the_ui_thread_and_is_gated_afterwards() {
    // 死回环主机：请求必然失败，绝不会碰到真实服务端。
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let host = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let fixture = FixtureDir::new();
    let ctx = egui::Context::default();
    let mut app = app_with(&ctx, true, Some(clean_snapshot(vec![row(42, 1200.0)])));
    app.route_fixture = Some((host, fixture.path()));
    open_section(&mut app, &ctx, 360.0, 640.0);
    let button = scroll_to_contains(&mut app, &ctx, 360.0, 640.0, "试修 rrid=42");
    click_shell(&mut app, &ctx, 360.0, 640.0, button);
    assert_eq!(app.data_page.recover.confirm, Some(42));

    let started = Instant::now();
    let output = shell_frame(&mut app, &ctx, 360.0, 640.0, vec![]);
    let ok = label_position(&output, "确认覆盖提交").expect("确认按钮");
    click_shell(&mut app, &ctx, 360.0, 640.0, ok);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "确认点击不能等待提交完成（必须后台跑）"
    );
    assert!(app.data_page.recover.fix_busy);
    assert_eq!(app.data_page.recover.fix_rrid, Some(42));
    assert_eq!(app.data_page.recover.confirm, None);

    let deadline = Instant::now() + Duration::from_secs(10);
    while app.data_page.recover.fix_busy {
        app.poll_messages();
        assert!(Instant::now() < deadline, "试修回执超时");
        std::thread::sleep(Duration::from_millis(5));
    }
    let report = app
        .data_page
        .recover
        .fix_result
        .as_ref()
        .expect("失败也要留下结论");
    assert_eq!(report.rrid, 42);
    assert!(report.error.is_some(), "离线主机必须报错而不是假成功");
    assert!(
        app.data_page.recover.needs_rediagnose,
        "动过一次就要重新诊断"
    );

    // 再点一次：按钮置灰，不会再起第二个任务。
    let before = app.data_page.recover.fix_busy;
    let at = scroll_to_contains(&mut app, &ctx, 360.0, 640.0, "试修 rrid=42");
    click_shell(&mut app, &ctx, 360.0, 640.0, at);
    assert!(!before && !app.data_page.recover.fix_busy);
    assert_eq!(app.data_page.recover.confirm, None);
}

// ───────────────────────── 3. 真后台任务（回环 mock，无外网）─────────────────────────

/// 一个 fixture 目录：本测试独占，避免并行用例互相看到对方的落盘。
struct FixtureDir(std::path::PathBuf);

impl FixtureDir {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("recover-ui-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("create fixture dir");
        Self(root)
    }

    fn path(&self) -> std::path::PathBuf {
        self.0.clone()
    }
}

impl Drop for FixtureDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn diagnose_fixture(freeze_seconds: i64) -> (String, std::thread::JoinHandle<usize>) {
    let start_ms = 1_761_100_000_000_i64;
    let policy = json!({"error":10000,"data": json!({
        "timestamp": 1_761_100_000_i64,
        "policy": 1,
        "runRuleModel": {"minDistance": 2000, "validTime": 1800},
        "freezeRunTime": freeze_seconds,
        "message": if freeze_seconds > 0 { "数据异常" } else { "" },
    }).to_string()});
    let cheat = json!({"error":10000,"data": json!({"self": null, "list": []}).to_string()});
    let records = json!({"error":10000,"data": {"list": [
        {"rrid": 111, "totalDis": 1200.0, "totalTime": 420, "startTime": start_ms, "complete": false},
        {"rrid": 222, "totalDis": 2000.0, "totalTime": 700, "startTime": start_ms + 86_400_000, "complete": true},
    ]}});
    let detail = json!({"error":10000,"data": {
        "rrid": 111,
        "complete": false,
        "reasonList": [
            {"reason": "点位未经过", "complete": false},
            {"reason": "里程达标", "complete": true},
        ],
    }});
    super::route_refresh_tests::mock_school(vec![
        (crate::api::policy::POLICY_PATH, policy),
        (crate::api::cheat::CHEATLIST_PATH, cheat),
        (crate::api::records::RECORDS_PATH, records),
        (crate::api::records::GET_ONE_PATH, detail),
    ])
}

#[test]
fn read_only_diagnosis_runs_on_a_worker_thread_and_fills_the_panel() {
    let (host, server) = diagnose_fixture(0);
    let fixture = FixtureDir::new();
    let ctx = egui::Context::default();
    let mut app = app_with(&ctx, true, None);
    app.route_fixture = Some((host, fixture.path()));

    let started = Instant::now();
    app.start_recover_diag();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "诊断必须立刻返回，网络留给后台线程"
    );
    assert!(app.data_page.recover.diag_busy);
    // 后台还在跑的时候界面照样能画（不卡 UI 线程）。
    let frame_started = Instant::now();
    shell_frame(&mut app, &ctx, 360.0, 640.0, vec![]);
    assert!(
        frame_started.elapsed() < Duration::from_secs(1),
        "诊断进行中，UI 帧不能被阻塞"
    );

    let deadline = Instant::now() + Duration::from_secs(15);
    while app.data_page.recover.diag_busy {
        app.poll_messages();
        assert!(Instant::now() < deadline, "诊断回执超时");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(server.join().unwrap(), 4, "诊断只发这 4 个请求");
    let snapshot = app
        .data_page
        .recover
        .snapshot
        .as_ref()
        .expect("诊断成功必须有快照");
    assert_eq!(snapshot.freeze_seconds, Some(0));
    assert_eq!(snapshot.freeze_text, "未冻结");
    assert_eq!(snapshot.total_records, 2);
    assert_eq!(snapshot.rows.len(), 1);
    assert_eq!(snapshot.rows[0].rrid, 111);
    assert_eq!(snapshot.rows[0].reason_text, "点位未经过");
    assert!(snapshot.verdict.contains("最可修"), "{}", snapshot.verdict);
    assert!(!app.data_page.recover.diag_busy);
    assert!(
        !app.data_page.recover.needs_rediagnose,
        "只读诊断不是提交，不该锁住单条入口"
    );

    // 手机上要能看到记录行与可点的单条按钮。
    open_section(&mut app, &ctx, 360.0, 640.0);
    scroll_to_contains(&mut app, &ctx, 360.0, 640.0, "试修 rrid=111");
    let output = frame(&mut app, &ctx, 360.0, 640.0, vec![]);
    assert!(
        has_text(&output, "rrid=111"),
        "记录行（rrid / 时间）必须在手机上可见"
    );
    assert!(has_text(&output, "点位未经过"), "原因列必须显示");
}

#[test]
fn frozen_diagnosis_uses_the_cli_wording_and_disables_the_button() {
    let (host, server) = diagnose_fixture(172_800);
    let fixture = FixtureDir::new();
    let ctx = egui::Context::default();
    let mut app = app_with(&ctx, true, None);
    app.route_fixture = Some((host, fixture.path()));
    app.start_recover_diag();
    let deadline = Instant::now() + Duration::from_secs(15);
    while app.data_page.recover.diag_busy {
        app.poll_messages();
        assert!(Instant::now() < deadline, "诊断回执超时");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(server.join().unwrap(), 4);

    let snapshot = app.data_page.recover.snapshot.clone().unwrap();
    // 与 CLI 同一方法产出，不是界面另写一套。
    assert_eq!(snapshot.freeze_text, "禁跑中，还剩 2 天 00:00:00");
    assert!(snapshot.verdict.contains("禁跑期"), "{}", snapshot.verdict);
    let gate = app.data_page.recover.decide_fix(111, true);
    assert!(!gate.is_ready());
    assert!(gate.reason().unwrap().contains("禁跑中，还剩 2 天"));

    open_section(&mut app, &ctx, 360.0, 640.0);
    let button = scroll_to_contains(&mut app, &ctx, 360.0, 640.0, "试修 rrid=111");
    click_shell(&mut app, &ctx, 360.0, 640.0, button);
    assert_eq!(app.data_page.recover.confirm, None);
    assert!(!app.data_page.recover.fix_busy);
}

#[test]
fn stale_fix_receipts_cannot_rewrite_the_current_result() {
    let ctx = egui::Context::default();
    let mut app = app_with(&ctx, true, Some(clean_snapshot(vec![row(111, 1200.0)])));
    app.data_page.recover.begin_fix(111);
    // 别的 rrid 的回执：忽略（迟到的旧回复不能把当前结论改掉）。
    app.tx
        .send(format!(
            "{RECOVER_FIX}{}",
            json!({"ok": true, "rrid": 999, "new_rrid": 1000, "verdict": "达标"})
        ))
        .unwrap();
    app.poll_messages();
    assert!(app.data_page.recover.fix_busy, "没等到自己那条就不能收工");
    assert!(app.data_page.recover.fix_result.is_none());

    app.tx
        .send(format!(
            "{RECOVER_FIX}{}",
            json!({
                "ok": true, "rrid": 111, "new_rrid": 900, "verdict": "达标",
                "complete": true, "failure_reasons": [],
                "recheck": {"complete": true, "reasons": []},
            })
        ))
        .unwrap();
    app.poll_messages();
    assert!(!app.data_page.recover.fix_busy);
    let report = app.data_page.recover.fix_result.clone().unwrap();
    assert_eq!(report.new_rrid, 900);
    assert_eq!(report.turned_green(), Some(true));
    assert!(app.data_page.recover.needs_rediagnose);
}

#[test]
fn late_diagnosis_reply_for_an_old_session_is_dropped() {
    let ctx = egui::Context::default();
    let mut app = app_with(&ctx, true, None);
    app.data_page.recover.begin_diag();
    let snapshot = clean_snapshot(vec![row(111, 1200.0)]);
    app.tx
        .send(format!(
            "__SESSION_JOB__{}|{RECOVER}{}",
            app.session_epoch + 1,
            json!({"ok": true, "diag": snapshot})
        ))
        .unwrap();
    app.poll_messages();
    assert!(
        app.data_page.recover.diag_busy,
        "旧会话的诊断回执不能落到新会话上"
    );
    assert!(app.data_page.recover.snapshot.is_none());
}

// ─────────────────── 4. 复查单条 / 结论映射 / 原 startTime 保留 ───────────────────

fn recheck_with(
    responses: Vec<(&'static str, serde_json::Value)>,
) -> (crate::ui::recover_panel::RecheckReport, usize) {
    let (host, server) = super::route_refresh_tests::mock_school(responses);
    let mut client = crate::api::client::ApiClient::new(HeaderIdentity::default(), Some(session()));
    client.set_test_host(host);
    let mut log = |_: &str| {};
    let report = crate::ui::recover_panel::recheck_one(&mut client, 42, &mut log);
    (report, server.join().unwrap())
}

#[test]
fn recheck_reads_back_the_same_record_and_reports_its_state() {
    // 覆盖生效：原 rrid 变达标。
    let (green, hits) = recheck_with(vec![(
        crate::api::records::GET_ONE_PATH,
        json!({"error":10000,"data":{"rrid":42,"complete":true,"reasonList":[
            {"reason":"里程达标","complete":true}
        ]}}),
    )]);
    assert_eq!(hits, 1);
    assert_eq!(green.complete, Some(true));
    assert!(green.reasons.is_empty());
    assert!(green.error.is_none());

    // 覆盖没生效：仍然未达标，原因要原样带回来。
    let (red, _) = recheck_with(vec![(
        crate::api::records::GET_ONE_PATH,
        json!({"error":10000,"data":{"rrid":42,"complete":false,"reasonList":[
            {"reason":"点位未经过","complete":false}
        ]}}),
    )]);
    assert_eq!(red.complete, Some(false));
    assert_eq!(red.reasons, vec!["点位未经过".to_string()]);

    // 顶层 complete 自相矛盾时按 reasonList 的显式失败算（与 Verification 同口径）。
    let (contradiction, _) = recheck_with(vec![(
        crate::api::records::GET_ONE_PATH,
        json!({"error":10000,"data":{"rrid":42,"complete":true,"reasonList":[
            {"reason":"步频异常","complete":false}
        ]}}),
    )]);
    assert_eq!(contradiction.complete, Some(false));

    // 原 rrid 读不到（可能已被覆盖成新记录）：如实报错，不猜结果。
    let (missing, _) = recheck_with(vec![(
        crate::api::records::GET_ONE_PATH,
        json!({"error":10000,"data":{"rrid":99,"complete":true,"reasonList":[]}}),
    )]);
    assert_eq!(missing.complete, None);
    assert!(missing.error.unwrap().contains("不一致"));
}

#[test]
fn fix_conclusion_distinguishes_green_red_and_unreadable() {
    let green = FixReport {
        rrid: 42,
        new_rrid: 900,
        verdict: "达标".into(),
        complete: Some(true),
        recheck: Some(RecheckReport {
            complete: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(green.turned_green(), Some(true));
    assert!(green.conclusion().contains("复查已达标"));

    let red = FixReport {
        rrid: 42,
        new_rrid: 900,
        recheck: Some(RecheckReport {
            complete: Some(false),
            reasons: vec!["点位未经过".into()],
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(red.turned_green(), Some(false));
    assert!(red.conclusion().contains("覆盖可能没生效"));

    let unreadable = FixReport {
        rrid: 42,
        new_rrid: 900,
        recheck: Some(RecheckReport {
            error: Some("记录不存在".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(unreadable.turned_green(), None);
    assert!(unreadable.conclusion().contains("重新诊断"));
}

#[test]
fn fix_reuses_the_original_start_time_so_the_submission_targets_the_same_day() {
    // 这是「覆盖」成立的前提：提交必须沿用原记录的 startTime，不能用现在。
    let snapshot = clean_snapshot(vec![row(111, 1234.0)]);
    let abnormal = snapshot.to_abnormal(111).expect("快照里应该有这条");
    assert_eq!(abnormal.start_ms, snapshot.rows[0].start_ms);
    assert!(abnormal.start_ms > 0);
    assert!((abnormal.total_dis - 1234.0).abs() < 1e-9);
    // 距离沿用合规区间内的原值（不改写、不放宽常量）。
    assert!((abnormal.fix_dist() - 1234.0).abs() < 1e-9);
    assert!(snapshot.to_abnormal(222).is_none());
}
