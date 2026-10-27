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
fn drive(platform: &str) -> Chain {
    let root = std::env::temp_dir().join(format!("fake-run-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = Some(root.clone()));
    // 全链会检查设备时区必须是 UTC+8。CI 跑在 UTC 上，若这里不放行，
    // 整条链在「时区」就停了 —— 与要验证的协议行为无关。
    std::fs::write(
        root.join("config.json"),
        r#"{"safety":{"allow_timezone_mismatch":true}}"#,
    )
    .unwrap();

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

/// iOS 身份：**已知的双身份**。
///
/// 提交固定走 Android 信封（`osType=0` + `androidId` 结构），其余链路跟身份走 iOS。
/// 这不是本轮引入的，是提交接口的既有设计；把它写成断言，是为了让这条限制可见 ——
/// 一旦有人真把提交也做成 iOS，这里会红，提醒同步更新结论。
#[test]
fn ios_identity_keeps_the_known_two_identity_split() {
    let chain = drive("ios");
    let requests = seen(&chain.hits);
    let outcome = chain.outcome.as_ref().unwrap_or_else(|e| {
        panic!(
            "iOS 身份全链失败：{e}\n请求：{requests:#?}\n日志：{:#?}",
            chain.log
        )
    });
    assert_eq!(outcome.obs_ok, 2, "OBS 上传失败；日志：{:#?}", chain.log);

    for hit in &chain.hits {
        let is_submit = hit.path.starts_with(crate::api::submit::RECORD_PATH);
        let want_ua = if is_submit { UA_ANDROID } else { UA_IOS };
        assert_eq!(
            hit.ua().as_deref(),
            Some(want_ua),
            "{} {} 的 UA 与预期不符（提交固定 Android，其余跟身份）",
            hit.method,
            hit.path
        );
        let want_version = if is_submit {
            ANDROID_APP_VERSION
        } else {
            IOS_APP_VERSION
        };
        if hit.path == "/obs-put" {
            continue;
        }
        assert_eq!(
            hit.header("appversion").as_deref(),
            Some(want_version),
            "{} {} 的 appVersion 与信封不同源",
            hit.method,
            hit.path
        );
    }

    // 把这条已知分裂显式记下来：非提交链路是 iOS，提交是 Android。
    let non_submit_ios = chain
        .hits
        .iter()
        .filter(|h| !h.path.starts_with(crate::api::submit::RECORD_PATH))
        .all(|h| h.ua().as_deref() == Some(UA_IOS));
    assert!(non_submit_ios, "非提交链路应统一为 iOS UA");
}
