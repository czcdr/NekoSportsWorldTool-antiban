//! 假跑（dry run）：整条跑步链跑在 loopback mock 上。
//!
//! 目的：在**不碰真实账号、不碰真实服务**的前提下，把 `/policy → /fence → /points →
//! 生成轨迹 → /save/record → /obs → /get_one_record` 全链跑一遍，并对客户端真正发出的
//! 报文做硬断言。
//!
//! 断言口径（这是本文件存在的理由）：
//! 1. 请求集合完整 —— 少一条（例如 OBS 整体掉线）就红；
//! 2. UA 必须**等于** `ua_for(identity)`，不是"去重后只有一种"；
//! 3. 外层 `appVersion` 与信封同源；
//! 4. `obs_ok` 与详情验证结果必须成立（这两处 `flow` 只写日志、不改返回值，
//!    `outcome.is_ok()` 盖不住）。
//!
//! 未知路径一律返回业务错误，避免"mock 什么都说成功"掩盖掉意外的请求。

use super::{run_full_flow, RunOutcome, RunParams};
use crate::api::client::ApiClient;
use crate::api::model::Session;
use crate::crypto::header::{
    ua_for, HeaderIdentity, ANDROID_APP_VERSION, IOS_APP_VERSION, UA_ANDROID, UA_IOS,
};
use crate::track::generate_road::RouteMode;
use chrono::TimeZone;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 一次被 mock 接到的请求。
#[derive(Clone)]
struct Hit {
    method: String,
    path: String,
    headers_raw: String,
    body: String,
}

impl Hit {
    fn header(&self, name: &str) -> Option<String> {
        self.headers_raw.lines().find_map(|line| {
            let (k, v) = line.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    }

    fn ua(&self) -> Option<String> {
        self.header("user-agent")
    }
}

fn wire_point(lat: f64, lon: f64, name: &str) -> Value {
    let (lat, lon) = crate::track::geom::wgs84_to_bd09(lat, lon);
    json!({
        "lat": lat,
        "lon": lon,
        "pointName": name,
        "isMustPoint": false,
    })
}

/// 按路径给出响应。业务 data 用 JSON 字符串包裹，与真实服务端一致。
fn reply_for(path: &str, now_s: i64, host: &str) -> Value {
    if path.starts_with(crate::api::policy::POLICY_PATH) {
        let data = json!({
            "timestamp": now_s,
            "policy": 1,
            "runRuleModel": {"minDistance": 1000, "validTime": 1800},
            "mustPoints": [],
        });
        return json!({"error": 10000, "data": data.to_string()});
    }
    if path.starts_with(crate::api::fence::FENCE_PATH) {
        let data = json!({
            "runAreaId": "12",
            "geoFences": [{
                "campusId": "east",
                "campusName": "测试东校区",
                "runAreaId": "73",
                "points": [
                    wire_point(39.8998, 116.3998, "西北"),
                    wire_point(39.8998, 116.4012, "东北"),
                    wire_point(39.9012, 116.4012, "东南"),
                    wire_point(39.9012, 116.3998, "西南"),
                ],
            }],
        });
        return json!({"error": 10000, "data": data.to_string()});
    }
    if path.starts_with(crate::api::points::POINTS_PATH_PREFIX) {
        let data = json!({
            "pointsResModels": [
                wire_point(39.9000, 116.4000, "打卡点1"),
                wire_point(39.9010, 116.4010, "打卡点2"),
            ],
        });
        return json!({"error": 10000, "data": data.to_string()});
    }
    if path.starts_with(crate::api::submit::RECORD_PATH) {
        return json!({"error": 10000, "data": {"rrid": 12345, "uuid": "FAKE-UUID", "complete": true}});
    }
    if path.starts_with(crate::api::obs::OBS_SIGN_PATH) {
        // OBS 签名地址指回 mock 自己：PUT 必须被记录下来，否则"OBS 掉线"无从断言。
        return json!({"error": 10000, "data": {
            "signedUrl": format!("{host}/obs-put"),
            "url": format!("{host}/obs-put"),
        }});
    }
    if path.starts_with(crate::api::records::GET_ONE_PATH) {
        return json!({"error": 10000, "data": {"complete": true, "reasonList": []}});
    }
    if path.starts_with(crate::api::records::RECORDS_PATH) {
        return json!({"error": 10000, "data": {"list": []}});
    }
    // 未知路径：返回业务错误而不是成功，让"多出来的请求"立刻暴露。
    json!({"error": 10999, "message": format!("mock 未建模的路径: {path}")})
}

fn handle(mut stream: std::net::TcpStream, hits: &Arc<Mutex<Vec<Hit>>>, now_s: i64, host: &str) {
    stream.set_nonblocking(false).ok();
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut buf = [0_u8; 4096];
        let n = match stream.read(&mut buf) {
            // 读错误/空读：请求没有成形，记一条"残缺请求"，不让它静默消失。
            Ok(0) | Err(_) => {
                hits.lock().unwrap().push(Hit {
                    method: "?".into(),
                    path: "<incomplete>".into(),
                    headers_raw: String::new(),
                    body: String::new(),
                });
                return;
            }
            Ok(n) => n,
        };
        bytes.extend_from_slice(&buf[..n]);
        if bytes.len() > 8 * 1024 * 1024 {
            return;
        }
        if let Some(i) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let headers_raw = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
    let first = headers_raw.lines().next().unwrap_or("").to_string();
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();

    let length: usize = headers_raw
        .lines()
        .find_map(|line| {
            let (k, v) = line.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse().ok())?
        })
        .unwrap_or(0);
    while bytes.len() < header_end + length {
        let mut buf = [0_u8; 4096];
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => bytes.extend_from_slice(&buf[..n]),
        }
    }
    let body = String::from_utf8_lossy(&bytes[header_end..(header_end + length).min(bytes.len())])
        .into_owned();

    hits.lock().unwrap().push(Hit {
        method: method.clone(),
        path: path.clone(),
        headers_raw: headers_raw.clone(),
        body: body.clone(),
    });

    let response = reply_for(&path, now_s, host).to_string();
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response.len(),
        response
    );
    let _ = stream.flush();
}

/// 起 mock，返回 (host, 命中记录, 停止信号)。
fn start_mock() -> (String, Arc<Mutex<Vec<Hit>>>, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let host = format!("http://{}", listener.local_addr().unwrap());
    let hits = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let hits_thread = hits.clone();
    let stop_thread = stop.clone();
    let now_s = chrono::Local::now().timestamp();
    let host_owned = host.clone();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(120);
        while !stop_thread.load(Ordering::SeqCst) && Instant::now() < deadline {
            match listener.accept() {
                Ok((stream, _)) => handle(stream, &hits_thread, now_s, &host_owned),
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });
    (host, hits, stop)
}

/// 一次假跑的结果。
struct Chain {
    identity: HeaderIdentity,
    hits: Vec<Hit>,
    outcome: Result<RunOutcome, String>,
    log: Vec<String>,
}

/// 用指定平台身份把整条链跑一遍。
///
/// `start_ms` 固定取**北京时间当天 12:00**：护栏的深夜判定按北京时间算，
/// 用本地时区的话，UTC-5 的 CI 上"中午"会落到北京时间凌晨，整条链被护栏拦掉。
///
/// `safety.enabled=false` 是**必须**的，不只是省事：深夜护栏判的是「**现在**几点」
/// （`safety::check_night`），而本文件把提交时刻钉在今天 12:00 —— 于是在
/// 23:00–06:00 之间跑这组用例必然被拦，`请求集合不完整；实际：[]`
/// （2026-10-28 23:0x 实测复现，与协议行为无关）。护栏本身由 `safety::tests`
/// 覆盖（含 `night_window_blocks_late_and_early_submissions`），这里要的是一条
/// 与墙钟无关的干净全链。
fn drive(platform: &str) -> Chain {
    drive_with_config(
        platform,
        r#"{"safety":{"enabled":false,"allow_timezone_mismatch":true}}"#,
    )
}

/// 同 [`drive`]，但可以指定 config.json 内容（用于 `gps_spread_m` / `anchor_offset_m`）。
fn drive_with_config(platform: &str, config_json: &str) -> Chain {
    let root = std::env::temp_dir().join(format!("fake-run-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = Some(root.clone()));
    // 全链会检查设备时区必须是 UTC+8。CI 跑在 UTC 上，若这里不放行，
    // 整条链在「时区」就停了 —— 与要验证的协议行为无关。
    std::fs::write(root.join("config.json"), config_json).unwrap();

    let (host, hits, stop) = start_mock();
    let (lat, lon) = crate::track::geom::wgs84_to_bd09(39.9, 116.4);
    let identity = HeaderIdentity {
        city: "测试城市".into(),
        anchor_lat: lat,
        anchor_lon: lon,
        platform: platform.to_string(),
        device_id: "ABCDEF01-2345-4678-9ABC-DEF012345678".into(),
        ..Default::default()
    };
    let session = Session {
        uid: 42,
        unid: "77".into(),
        token: "fake-run-token".into(),
        ..Default::default()
    };
    let mut client = ApiClient::new(identity.clone(), Some(session));
    client.set_test_host(host);

    let bj = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
    let today = chrono::Utc::now().with_timezone(&bj).date_naive();
    let noon = bj
        .from_local_datetime(&today.and_hms_opt(12, 0, 0).unwrap())
        .unwrap()
        .timestamp_millis();
    let params = RunParams {
        dist: 1200.0,
        dur: 420,
        start_ms: noon,
        face_check: 0,
        manual_altitude: None,
        manual_altitude_range: None,
        seed: 7,
        route_mode: RouteMode::Legacy,
    };

    let mut log: Vec<String> = Vec::new();
    let outcome = {
        let mut sink = |line: &str| log.push(line.to_string());
        run_full_flow(&mut client, &params, &mut sink)
    };
    stop.store(true, Ordering::SeqCst);
    let hits = hits.lock().unwrap().clone();

    // 落盘是可选的诊断产物：默认写进本次测试的临时目录，不依赖盘符、不污染仓库。
    // 需要固定位置时用 NSWT_FAKE_RUN_DUMP 指定（例如 D:\moni\captures）。
    if let Some(dir) = std::env::var_os("NSWT_FAKE_RUN_DUMP") {
        let dir = std::path::PathBuf::from(dir);
        let _ = std::fs::create_dir_all(&dir);
        let dump = serde_json::to_string_pretty(
            &hits
                .iter()
                .map(|h| {
                    json!({
                        "method": h.method,
                        "path": h.path,
                        "headers": h.headers_raw.lines().collect::<Vec<_>>(),
                        "body": h.body,
                    })
                })
                .collect::<Vec<_>>(),
        )
        .unwrap_or_default();
        let _ = std::fs::write(dir.join(format!("fake-run-{platform}.json")), dump);
        let _ = std::fs::write(dir.join(format!("fake-run-{platform}.log")), log.join("\n"));
    }

    crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = None);
    let _ = std::fs::remove_dir_all(&root);

    Chain {
        identity,
        hits,
        outcome,
        log,
    }
}

fn seen(hits: &[Hit]) -> Vec<String> {
    hits.iter()
        .map(|h| format!("{} {}", h.method, h.path))
        .collect()
}

fn multiset(patterns: &[(&str, usize)], hits: &[Hit]) -> BTreeMap<String, usize> {
    patterns
        .iter()
        .map(|(p, _)| {
            let count = hits.iter().filter(|h| h.path.starts_with(p)).count();
            (p.to_string(), count)
        })
        .collect()
}

/// Android 身份：全链必须是一台设备 —— 一套 UA、一个 appVersion。
#[test]
fn android_chain_reports_exactly_one_identity() {
    let chain = drive("android");
    let requests = seen(&chain.hits);

    // ① 请求集合完整（OBS 掉线会少两条 PUT，这里直接红）。
    let expected = BTreeMap::from([
        (crate::api::policy::POLICY_PATH.to_string(), 1usize),
        (crate::api::fence::FENCE_PATH.to_string(), 1),
        (crate::api::points::POINTS_PATH_PREFIX.to_string(), 1),
        (crate::api::submit::RECORD_PATH.to_string(), 1),
        (crate::api::obs::OBS_SIGN_PATH.to_string(), 2),
        ("/obs-put".to_string(), 2),
        (crate::api::records::GET_ONE_PATH.to_string(), 1),
    ]);
    let actual = multiset(
        &expected
            .keys()
            .map(|k| (k.as_str(), 1usize))
            .collect::<Vec<_>>(),
        &chain.hits,
    );
    assert_eq!(
        actual, expected,
        "请求集合不完整；实际：{requests:#?}\n日志：{:#?}",
        chain.log
    );

    // ② 链路本身与两处软失败必须都是成功的。
    let outcome = chain
        .outcome
        .as_ref()
        .unwrap_or_else(|e| panic!("全链失败：{e}\n请求：{requests:#?}\n日志：{:#?}", chain.log));
    assert_eq!(
        outcome.obs_ok, 2,
        "OBS 双 key 上传没有全部成功；日志：{:#?}",
        chain.log
    );
    assert_eq!(
        outcome.verification.complete,
        Some(true),
        "详情验证没有拿到达标结果；日志：{:#?}",
        chain.log
    );

    // ③ UA 必须是 ua_for(identity) 的那一个，而不是"凑巧只有一种"。
    let want_ua = ua_for(&chain.identity);
    assert_eq!(want_ua, UA_ANDROID, "Android 身份的 UA 常量被改动过");
    for hit in &chain.hits {
        assert_eq!(
            hit.ua().as_deref(),
            Some(want_ua.as_str()),
            "{} {} 的 UA 不是 ua_for(identity)",
            hit.method,
            hit.path
        );
    }

    // ④ appVersion：业务请求一律 ANDROID_APP_VERSION；OBS 直传不属于信封链路，不该带。
    for hit in &chain.hits {
        if hit.path == "/obs-put" {
            assert!(
                hit.header("appversion").is_none(),
                "OBS 直传不该带 appVersion"
            );
            continue;
        }
        assert_eq!(
            hit.header("appversion").as_deref(),
            Some(ANDROID_APP_VERSION),
            "{} {} 的 appVersion 与信封不同源",
            hit.method,
            hit.path
        );
    }
}

/// iOS 身份：提交在**发请求之前**就被拒绝（审计 A1）。
///
/// 旧行为是「登录 iOS、提交 Android」的双身份分裂 —— 同一条 token 上两个平台，
/// 审计判为 high 的跨来源矛盾。现在改成准入拒绝：链路上一条提交请求都不该出现，
/// 也不该出现任何 Android 的 UA/appVersion。这个用例就是那条结论的看门人。
#[test]
fn ios_identity_is_refused_before_the_submit_request() {
    let chain = drive("ios");
    let requests = seen(&chain.hits);
    let error = match &chain.outcome {
        Ok(_) => {
            assert!(false, "iOS 身份不该跑通提交；请求：{requests:#?}");
            return;
        }
        Err(error) => error.clone(),
    };
    assert!(
        error.contains("iOS") && error.contains("Android"),
        "拒绝文案要说明平台并给出出口：{error}"
    );

    // ① 提交请求一条都不该发出。
    assert!(
        !chain
            .hits
            .iter()
            .any(|h| h.path.starts_with(crate::api::submit::RECORD_PATH)),
        "提交请求不该发出；请求：{requests:#?}"
    );
    // ② 整条链只能是 iOS 那一套身份，不出现 Android 的 UA/appVersion。
    for hit in &chain.hits {
        assert_eq!(
            hit.ua().as_deref(),
            Some(UA_IOS),
            "{} {} 不是 iOS UA（iOS 身份不该出现 Android 报文）",
            hit.method,
            hit.path
        );
        if hit.path == "/obs-put" {
            continue;
        }
        assert_eq!(
            hit.header("appversion").as_deref(),
            Some(IOS_APP_VERSION),
            "{} {} 的 appVersion 与身份不同源",
            hit.method,
            hit.path
        );
    }
}

// ---------------------------------------------------------------
// 上传锚点偏移（配置 anchor_offset_m）
//
// 点位请求的**明文**被信封加密（`envelope_request`），wire 上只有密文，所以坐标
// 数值本身在 `api::points::anchor_offset_tests` 里对 `request_body` 断言（那就是
// 进信封前的原文）。这里能断言的是另外两件同样重要的事：
// ① 偏移不新增任何请求字段（0 偏移与 50 m 的外层信封键集必须一致）；
// ② 偏移不改变设备锚点本身，整条链在两种配置下都跑得通。
// ---------------------------------------------------------------

/// 抓到的那一条 `/points` 请求的**外层信封** body。
fn points_envelope(chain: &Chain) -> Value {
    let hit = chain
        .hits
        .iter()
        .find(|h| h.path.starts_with(crate::api::points::POINTS_PATH_PREFIX))
        .unwrap_or_else(|| panic!("没抓到点位请求；请求：{:#?}", seen(&chain.hits)));
    serde_json::from_str(&hit.body).unwrap_or_else(|e| {
        panic!("点位请求 body 不是 JSON（{e}）：{}", hit.body);
    })
}

fn sorted_keys(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    keys.sort();
    keys
}

#[test]
fn anchor_offset_adds_no_request_field_and_keeps_the_device_anchor() {
    let baseline = drive_with_config(
        "android",
        r#"{"safety":{"enabled":false,"allow_timezone_mismatch":true},"anchor_offset_m":0.0}"#,
    );
    let offset = drive_with_config(
        "android",
        r#"{"safety":{"enabled":false,"allow_timezone_mismatch":true},"anchor_offset_m":50.0}"#,
    );
    for chain in [&baseline, &offset] {
        assert!(
            chain.outcome.is_ok(),
            "全链失败：{:?}\n日志：{:#?}",
            chain.outcome.as_ref().err(),
            chain.log
        );
    }
    // ① 请求集合不变（偏移不该让链路变成另一条），字段集合也不变。
    assert_eq!(seen(&baseline.hits), seen(&offset.hits), "请求集合变了");
    let base_body = points_envelope(&baseline);
    let moved_body = points_envelope(&offset);
    assert_eq!(
        sorted_keys(&base_body),
        sorted_keys(&moved_body),
        "偏移不该改变点位请求的字段集合"
    );
    let text = moved_body.to_string();
    for forbidden in ["anchor_offset", "anchorOffset", "gps_spread", "offset"] {
        assert!(
            !text.contains(forbidden),
            "点位请求里出现了本地参数 {forbidden}"
        );
    }
    // ② 设备锚点本身没被改：身份里的经纬度仍是精确值（偏移只用于请求）。
    let exact = offset.identity.anchor_coordinate().unwrap();
    assert_eq!(
        offset.identity.anchor_lat.to_bits(),
        exact.latitude.to_bits()
    );
    assert_eq!(
        offset.identity.anchor_lon.to_bits(),
        exact.longitude.to_bits()
    );
    // ③ 计划级偏移：同一计划两次相同，新计划重抽（与 flow 无关的纯函数保证，
    //    预览与提交都用 plan.seed 派生，所以必然一致）。
    let expected = crate::location::plan_anchor(exact, 50.0, 7);
    assert_eq!(crate::location::plan_anchor(exact, 50.0, 7), expected);
    assert_ne!(crate::location::plan_anchor(exact, 50.0, 8), expected);
    assert_eq!(
        crate::location::plan_anchor(exact, 0.0, 7)
            .latitude
            .to_bits(),
        exact.latitude.to_bits(),
        "0 偏移必须逐位等于精确锚点"
    );
}
