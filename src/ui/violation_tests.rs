use super::*;
use crate::api::cheat::{CheatReport, Status};
use serde_json::json;

/// 每个测试一份独立的落盘目录。
///
/// 原来两处都把 `route_fixture` 指向 `std::env::temp_dir()`（全系统同一个 %TEMP%）：
/// 并行跑测试时几个用例会看到对方写下的文件，症状是"偶发失败、单跑不复现"。
struct FixtureDir(std::path::PathBuf);

impl FixtureDir {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("violation-fixture-{}", uuid::Uuid::new_v4()));
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

#[test]
fn query_failure_missing_fields_and_stale_result_are_visible() {
    for width in [320., 360., 412., 880.] {
        let ctx = egui::Context::default();
        let mut app = App::from_state(
            &ctx,
            HeaderIdentity::default(),
            Session::default(),
            Config::default(),
        );
        app.tab = 3;
        // A6：违规自查默认收起，先点开标题，否则里面的状态文案不会被渲染。
        let header = scroll_to_label(&mut app, &ctx, width, 480., "违规自查");
        click(&mut app, &ctx, width, 480., header);
        let now = crate::crypto::envelope::now_ms();
        for (data, at, expected) in [
            (json!({"self":null}), now, Status::NoRecord),
            (json!({}), now, Status::Unknown),
            (json!({"self":false}), now, Status::Unknown),
            (
                json!({"self":null}),
                now - crate::api::cheat::FRESH_MS,
                Status::Stale,
            ),
        ] {
            let report = CheatReport::from_data(&data, at).unwrap();
            app.tx
                .send(format!(
                    "{CHEAT}{}",
                    serde_json::to_string(&report).unwrap()
                ))
                .unwrap();
            app.poll_messages();
            assert_eq!(app.data_page.cheat.as_ref().unwrap().status(), expected);
            let label = app.data_page.cheat.as_ref().unwrap().label();
            scroll_to_label(&mut app, &ctx, width, 480., label);
        }
        app.data_page.cheat_checking = true;
        app.tx
            .send(format!("{CHEAT}{}", json!({"fetchError":"模拟查询失败"})))
            .unwrap();
        app.poll_messages();
        assert!(!app.data_page.cheat_checking);
        assert!(app.data_page.cheat.is_none());
        scroll_to_label(&mut app, &ctx, width, 480., "检查失败：模拟查询失败");
    }
}

#[test]
fn related_record_button_scrolls_and_opens_only_the_explicit_id() {
    for (width, height) in [(320., 360.), (360., 400.), (412., 480.), (880., 720.)] {
        let (host, server) = super::route_refresh_tests::mock_school(vec![(
            crate::api::records::GET_ONE_PATH,
            json!({"error":10000,"data":{"rrid":42,"statusInfo":"fixture detail"}}),
        )]);
        let ctx = egui::Context::default();
        let mut app = App::from_state(
            &ctx,
            HeaderIdentity::default(),
            Session {
                uid: 42,
                unid: "77".into(),
                token: "offline-only".into(),
                ..Default::default()
            },
            Config::default(),
        );
        let fixture = FixtureDir::new();
        app.route_fixture = Some((host, fixture.path()));
        app.tab = 3;
        app.data_page.cheat = Some(CheatReport::from_data(&json!({"self":{
            "reason":"服务端原始原因".repeat(10), "rrid":"42", "id":999, "time":"2026-10-20 07:30:00"
        }}), crate::crypto::envelope::now_ms()).unwrap());
        // A6：违规自查默认收起，先点开标题。
        let header = scroll_to_label(&mut app, &ctx, width, height, "违规自查");
        click(&mut app, &ctx, width, height, header);
        let at = scroll_to_label(&mut app, &ctx, width, height, "查看关联跑步记录 #42");
        click(&mut app, &ctx, width, height, at);
        assert_eq!(app.tab, 2);
        assert_eq!(app.records_page.detail_rrid, Some(42));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while app.records_page.detail_loading {
            app.poll_messages();
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(server.join().unwrap(), 1);
        assert_eq!(app.records_page.detail_raw.as_ref().unwrap()["rrid"], 42);
    }
}

#[test]
fn late_record_detail_cannot_replace_current_violation_record() {
    let ctx = egui::Context::default();
    let mut app = App::from_state(
        &ctx,
        HeaderIdentity::default(),
        Session::default(),
        Config::default(),
    );
    app.records_page.detail_rrid = Some(42);
    app.records_page.detail_loading = true;
    app.tx
        .send(format!(
            "{RUN_DETAIL}{}",
            json!({"rrid":9,"statusInfo":"old"})
        ))
        .unwrap();
    app.poll_messages();
    assert!(app.records_page.detail_raw.is_none());
    assert!(app.records_page.detail_loading);
    app.tx
        .send(format!(
            "{RUN_DETAIL}{}",
            json!({"rrid":42,"statusInfo":"current"})
        ))
        .unwrap();
    app.tx
        .send(format!(
            "{RUN_DETAIL}{}",
            json!({"rrid":9,"statusInfo":"late old reply"})
        ))
        .unwrap();
    app.poll_messages();
    assert_eq!(
        app.records_page.detail_raw.unwrap()["statusInfo"],
        "current"
    );
}

#[test]
fn mismatched_detail_finishes_with_an_error_for_the_requested_record() {
    let (host, server) = super::route_refresh_tests::mock_school(vec![(
        crate::api::records::GET_ONE_PATH,
        json!({"error":10000,"data":{"rrid":99}}),
    )]);
    let ctx = egui::Context::default();
    let mut app = App::from_state(
        &ctx,
        HeaderIdentity::default(),
        Session {
            uid: 42,
            unid: "77".into(),
            token: "offline-only".into(),
            ..Default::default()
        },
        Config::default(),
    );
    let fixture = FixtureDir::new();
    app.route_fixture = Some((host, fixture.path()));
    app.records_page.detail_rrid = Some(42);
    app.fetch_run_detail(42);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while app.records_page.detail_loading {
        app.poll_messages();
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(server.join().unwrap(), 1);
    let raw = app.records_page.detail_raw.unwrap();
    assert_eq!(raw["rrid"], 42);
    assert!(raw["statusInfo"]
        .as_str()
        .unwrap()
        .contains("编号与请求不一致"));
}

#[test]
fn active_submission_button_cannot_start_a_second_task() {
    let ctx = egui::Context::default();
    let mut app = App::from_state(
        &ctx,
        HeaderIdentity::default(),
        Session::default(),
        Config::default(),
    );
    app.tab = 0;
    app.run_busy = true;
    app.status = "existing submission".into();
    // Tall viewport isolates button gating from the separately tested short-screen scrolling.
    for _ in 0..2 {
        for _ in 0..3 {
            frame(&mut app, &ctx, 880., 900., vec![]);
        }
        let output = frame(&mut app, &ctx, 880., 900., vec![]);
        let at = label_position(&output, "提交中…").unwrap();
        click(&mut app, &ctx, 880., 900., at);
    }
    assert!(app.run_busy);
    assert_eq!(app.status, "existing submission");
}

// ── 记录详情页三态（0.9.1 / A 档）──────────────────────────────────────────

/// 详情页 fixture：一条真实形态的 `RunDetail`（`fetch_detail` 序列化后的样子）。
fn detail_fixture(complete: bool, reasons: serde_json::Value) -> serde_json::Value {
    json!({
        "rrid": 42,
        "uuid": "fixture-uuid",
        "sportType": 1,
        "totalTime": 840,
        "totalDis": 2000.0,
        "validDis": 1980.0,
        "avgStepFreq": 160,
        "totalSteps": 2240,
        "calorie": 120,
        "avgPower": 90,
        "totalAscent": 12,
        "totalDescent": 9,
        "address": "fixture 校区",
        "statusInfo": if complete { "成绩达标" } else { "成绩未达标" },
        "complete": complete,
        "reasonList": reasons,
        "startTime": 1_700_000_000_000_i64,
    })
}

/// 服务端实测的规则项（`review/SERVER-EVIDENCE.md` 截图原文）。
fn passing_rule_list() -> serde_json::Value {
    json!([
        {"reason": "按顺序通过所有点位", "complete": true},
        {"reason": "最少要跑 2.00 公里", "complete": true},
        {"reason": "跑 14 分钟以上", "complete": true},
        {"reason": "步频 40–300", "complete": true},
        {"reason": "配速 3'00\"–10'00\"", "complete": true},
        {"reason": "人脸验证", "complete": true}
    ])
}

/// 取某个文字标签实际用的颜色（`RichText::color` → LayoutJob 的 section 颜色）。
fn text_color(output: &egui::FullOutput, label: &str) -> Option<egui::Color32> {
    output.shapes.iter().find_map(|clipped| {
        if let egui::Shape::Text(text) = &clipped.shape {
            if text.galley.text() == label {
                return Some(
                    text.galley
                        .job
                        .sections
                        .first()
                        .map(|section| section.format.color)
                        .unwrap_or(text.fallback_color),
                );
            }
        }
        None
    })
}

/// 打开详情页并渲染两帧（第二帧才有稳定的 hit-test 与裁剪信息）。
fn open_detail(
    app: &mut App,
    ctx: &egui::Context,
    detail: serde_json::Value,
    w: f32,
    h: f32,
) -> egui::FullOutput {
    app.tab = 2;
    app.records_page.detail_rrid = Some(42);
    app.records_page.detail_loading = false;
    app.records_page.detail_raw = Some(detail);
    frame(app, ctx, w, h, vec![]);
    frame(app, ctx, w, h, vec![])
}

/// A 档核心断言（360×640 手机）：规则全过但有非规则项时，
/// 顶部结论是红色的「通过但异常」，且那一行异常项不滚动就在可见区。
#[test]
fn passed_but_anomalous_detail_is_red_and_visible_on_a_phone() {
    for (w, h) in [(360.0, 640.0), (412.0, 600.0)] {
        let ctx = egui::Context::default();
        let mut app = App::from_state(
            &ctx,
            HeaderIdentity::default(),
            Session::default(),
            Config::default(),
        );
        let mut reasons = passing_rule_list().as_array().unwrap().clone();
        // 被判可疑的那条记录：规则项全部通过，只有这一行 ✗「运动数据有问题」。
        reasons.push(json!({"reason": "运动数据有问题", "complete": false}));
        let detail = detail_fixture(true, serde_json::Value::Array(reasons));

        let output = open_detail(&mut app, &ctx, detail, w, h);
        let status_pos = label_position(&output, "通过但异常")
            .unwrap_or_else(|| panic!("顶部三态结论不可见（{w}x{h}）"));
        assert!(
            ctx.available_rect().contains(status_pos),
            "「通过但异常」落在首屏外：{status_pos:?}（{w}x{h}）"
        );
        assert_eq!(
            text_color(&output, "通过但异常"),
            Some(theme::err()),
            "「通过但异常」必须标红（风控信号）"
        );
        // 异常项置顶且可见：它就是用户要看见的那一行。
        let anomaly_pos = label_position(&output, "运动数据有问题")
            .unwrap_or_else(|| panic!("异常项不在可见区（{w}x{h}）"));
        assert!(
            ctx.available_rect().contains(anomaly_pos),
            "异常项落在首屏外：{anomaly_pos:?}（{w}x{h}）"
        );
        assert_eq!(
            text_color(&output, "运动数据有问题"),
            Some(theme::err()),
            "非规则项必须标红"
        );
    }
}

/// 「未达标」是规则层结论：普通字色，**不标红**；规则项照样逐条列出。
#[test]
fn failed_detail_stays_neutral_colored() {
    let ctx = egui::Context::default();
    let mut app = App::from_state(
        &ctx,
        HeaderIdentity::default(),
        Session::default(),
        Config::default(),
    );
    let reasons = json!([
        {"reason": "按顺序通过所有点位", "complete": true},
        {"reason": "最少要跑 2.00 公里", "complete": false},
        {"reason": "步频 40–300", "complete": true}
    ]);
    let output = open_detail(&mut app, &ctx, detail_fixture(false, reasons), 360.0, 640.0);

    let pos = label_position(&output, "未达标").expect("未达标结论必须可见");
    assert!(ctx.available_rect().contains(pos));
    let color = text_color(&output, "未达标").expect("未达标结论必须有颜色");
    assert_ne!(color, theme::err(), "「未达标」不是风控信号，不能标红");
    assert_eq!(color, theme::text());
    // 失败的那条规则项仍要能看见（它是「为什么没过」）。
    label_position(&output, "最少要跑 2.00 公里").expect("未达标的规则项必须可见");
}

/// 拉详情失败的空壳（只有 statusInfo）没有判定数据：不给「达标/未达标/通过但异常」结论。
#[test]
fn detail_without_verdict_data_does_not_guess_a_status() {
    let ctx = egui::Context::default();
    let mut app = App::from_state(
        &ctx,
        HeaderIdentity::default(),
        Session::default(),
        Config::default(),
    );
    let output = open_detail(
        &mut app,
        &ctx,
        json!({"rrid": 42, "statusInfo": "详情拉取失败：超时"}),
        360.0,
        640.0,
    );
    for label in ["达标", "未达标", "通过但异常"] {
        assert!(
            label_position(&output, label).is_none(),
            "没有判定数据时不能给出「{label}」结论"
        );
    }
    // 原始错误仍然要如实显示（字段区在下面，允许滚动到达）。
    let err_pos = scroll_to_label(&mut app, &ctx, 360.0, 640.0, "详情拉取失败：超时");
    assert!(ctx.available_rect().contains(err_pos));
}

// ── 列表徽标回填（0.9.1 / B 档）────────────────────────────────────────────

fn record_row(rrid: i64, start_time: i64, complete: bool) -> crate::api::records::RecordRow {
    crate::api::records::RecordRow {
        rrid,
        total_dis: 2000.0,
        total_time: 840,
        start_time,
        complete,
        avg_step_freq: 160,
        calorie: 120,
        avg_power: 90,
        total_steps: 2240,
        uuid: format!("uuid-{rrid}"),
    }
}

/// 有没有一条以 needle 开头的文字（长提示会被折行，用前缀找更稳）。
fn has_text_starting_with(output: &egui::FullOutput, needle: &str) -> bool {
    output.shapes.iter().any(|clipped| {
        matches!(&clipped.shape, egui::Shape::Text(text) if text.galley.text().starts_with(needle))
    })
}

/// 后台回填只改徽标：异常行标红 + 有解释，未达标保持中性色，失败不清列表。
#[test]
fn badge_backfill_marks_anomalous_row_red_without_touching_the_list() {
    let ctx = egui::Context::default();
    let mut app = App::from_state(
        &ctx,
        HeaderIdentity::default(),
        Session::default(),
        Config::default(),
    );
    app.tab = 2;
    app.records_page.rows = vec![
        record_row(12, 1_700_000_012_000, true),
        record_row(11, 1_700_000_011_000, false),
    ];
    // 还没有任何详情结果：只能用列表自带的 complete，不许出现「异常」占位。
    let before = frame(&mut app, &ctx, 360.0, 640.0, vec![]);
    let before = {
        frame(&mut app, &ctx, 360.0, 640.0, vec![]);
        before
    };
    assert!(
        label_position(&before, "异常").is_none(),
        "还没核对出结论时不许显示异常徽标（避免闪烁式误报）"
    );

    // 模拟后台线程的回包：12 号「通过但异常」，11 号这条没取到。
    let control = std::sync::Arc::new(crate::task_control::Control::default());
    let generation = app.records_page.badges.begin(control);
    for payload in [
        json!({"generation": generation, "rrid": 12, "status": "anomalous", "anomaly": "运动数据有问题"}),
        json!({"generation": generation, "rrid": 11, "error": "超时"}),
        json!({"generation": generation, "done": true}),
    ] {
        app.tx.send(format!("{RECORD_BADGES}{payload}")).unwrap();
    }
    app.poll_messages();
    assert_eq!(app.records_page.rows.len(), 2, "回填失败不许清空列表");
    assert!(!app.records_page.badges.pending());

    let output = {
        frame(&mut app, &ctx, 360.0, 640.0, vec![]);
        frame(&mut app, &ctx, 360.0, 640.0, vec![])
    };
    let anomaly = label_position(&output, "异常").expect("通过但异常必须出现在列表上");
    assert!(ctx.available_rect().contains(anomaly));
    assert_eq!(
        text_color(&output, "异常"),
        Some(theme::err()),
        "「通过但异常」的徽标必须标红"
    );
    assert!(
        has_text_starting_with(&output, "规则已达标，但运动数据有问题"),
        "红色徽标必须自解释，而不是只写「异常」两个字"
    );
    // 没取到的那条（11 号超时）：必须显示「待核」，**不得**用列表的 complete 兜底。
    //
    // BLOCKER-2（盲审）：旧实现回退到 `row.complete`，于是「没核对」被显示成「未达标」；
    // 更糟的是 `complete=true` 时会被显示成绿色「达标」——一条其实「通过但异常」的记录，
    // 会因为详情没取到而看起来完美。没核对到就只能是「待核」。
    assert_eq!(
        text_color(&output, "待核"),
        Some(theme::text_dim()),
        "没核对到的行必须显示中性的「待核」"
    );
    assert!(
        !has_text_starting_with(&output, "未达标"),
        "没核对到 ≠ 未达标：不得替服务端下结论"
    );
    assert!(
        has_text_starting_with(&output, "判定没全部取到"),
        "取不到判定时要有弱提示（只影响徽标）"
    );
    // 两条记录都还在列表上（失败不影响展示）。
    label_position(&output, "步频 160 spm · rrid 12").expect("12 号记录必须还在列表里");
    assert!(
        has_text_starting_with(&output, "步频 160 spm · rrid 11"),
        "11 号那条判定没取到，但它本身必须还在列表里"
    );
}

/// 没登录时不起回填（不新增请求），也不会留下「在飞」状态。
#[test]
fn badge_backfill_does_not_start_without_a_session() {
    let ctx = egui::Context::default();
    let mut app = App::from_state(
        &ctx,
        HeaderIdentity::default(),
        Session::default(),
        Config::default(),
    );
    app.records_page.rows = vec![record_row(12, 1_700_000_012_000, true)];
    app.ensure_record_badges();
    assert!(!app.records_page.badges.in_flight());
    assert_eq!(app.records_page.badges.error(), None);
}

/// 离开记录页会取消在飞回填（取消打到 worker 检查的同一个开关上）。
#[test]
fn leaving_the_records_tab_cancels_the_badge_backfill() {
    let ctx = egui::Context::default();
    let mut app = App::from_state(
        &ctx,
        HeaderIdentity::default(),
        Session::default(),
        Config::default(),
    );
    app.tab = 2;
    app.records_page.rows = vec![record_row(12, 1_700_000_012_000, true)];
    let control = std::sync::Arc::new(crate::task_control::Control::default());
    app.records_page.badges.begin(control.clone());
    frame(&mut app, &ctx, 360.0, 640.0, vec![]);
    assert!(control.cancellable(), "还在记录页时不许取消");

    app.tab = 3;
    frame(&mut app, &ctx, 360.0, 640.0, vec![]);
    assert!(
        control.check().is_err(),
        "切走后必须让后台线程的下一次 check() 失败"
    );
    assert!(!app.records_page.badges.in_flight());
}
