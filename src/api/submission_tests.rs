//! Actual encrypted POSTs are sent only to a loopback server.
use super::*;
use serde_json::json;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!("submit-http-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        crate::platform::TEST_DATA_DIR.with(|v| *v.borrow_mut() = Some(p.clone()));
        Self(p)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        crate::platform::TEST_DATA_DIR.with(|v| *v.borrow_mut() = None);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

enum Reply {
    Lost,
    Record(Value),
    Unauthorized,
    List(bool),
    /// 列表里直接给一条记录（预置回执的 uuid/时刻由用例自己决定）。
    ListRows(Value),
    /// 列表接口不回包、直接断开：复现「对账这一步失败」。
    DropList,
}
fn server(replies: Vec<Reply>) -> (String, std::thread::JoinHandle<Vec<String>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let host = format!("http://{}", listener.local_addr().unwrap());
    let task = std::thread::spawn(move || {
        let mut paths = Vec::new();
        let mut request_uuid = String::new();
        for reply in replies {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((s, _)) => break s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "expected request did not arrive");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            // Windows accepted sockets can inherit the listener's nonblocking mode.
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let end = loop {
                let mut b = [0; 4096];
                let n = stream.read(&mut b).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&b[..n]);
                assert!(bytes.len() < 2 * 1024 * 1024);
                if let Some(i) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    break i + 4;
                }
            };
            let headers = String::from_utf8_lossy(&bytes[..end]).into_owned();
            let path = headers
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap()
                .to_string();
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            while bytes.len() < end + length {
                let mut b = [0; 4096];
                let n = stream.read(&mut b).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&b[..n]);
            }
            assert!(serde_json::from_slice::<Value>(&bytes[end..end + length])
                .unwrap()
                .is_object());
            let (status, body) = match reply {
                Reply::List(found) => {
                    assert_eq!(path, crate::api::records::RECORDS_PATH);
                    (
                        200,
                        json!({"error":10000,"data":{"list":if found {
                        vec![json!({"rrid":"77","uuid":request_uuid})]
                    } else { vec![] }}}),
                    )
                }
                // 列表里返回一条**指定**的记录（测试预置回执时需要自己给 uuid/时刻）。
                Reply::ListRows(row) => {
                    assert_eq!(path, crate::api::records::RECORDS_PATH);
                    (200, json!({"error":10000,"data":{"list":[row]}}))
                }
                // 列表接口连接被掐断：模拟对账这一步本身失败（fail-closed 路径）。
                Reply::DropList => {
                    assert_eq!(path, crate::api::records::RECORDS_PATH);
                    paths.push(path);
                    continue;
                }
                other => {
                    assert_eq!(path, RECORD_PATH);
                    request_uuid = headers
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("runef")
                                // 按字符取前 36 位：header 值目前是 ASCII UUID，但按字节
                                // 切片一旦遇到非 ASCII 就会 panic。
                                .then(|| value.trim().chars().take(36).collect::<String>())
                        })
                        .unwrap();
                    paths.push(path);
                    if matches!(other, Reply::Lost) {
                        continue;
                    }
                    let answer = match other {
                        Reply::Record(v) => (200, v),
                        Reply::Unauthorized => (401, json!({"message":"fixture token rejected"})),
                        _ => unreachable!(),
                    };
                    let text = answer.1.to_string();
                    write!(stream, "HTTP/1.1 {} fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", answer.0, text.len(), text).unwrap();
                    continue;
                }
            };
            paths.push(path);
            let text = body.to_string();
            write!(stream, "HTTP/1.1 {status} fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", text.len(), text).unwrap();
        }
        paths
    });
    (host, task)
}
fn client(host: &str) -> ApiClient {
    let mut c = ApiClient::new(
        // 提交链路要求 Android 身份（A1 的准入校验）；默认 HeaderIdentity 是 iOS。
        crate::crypto::header::HeaderIdentity {
            platform: "android".into(),
            ..Default::default()
        },
        Some(crate::api::model::Session {
            uid: 42,
            unid: "77".into(),
            token: "offline-only".into(),
            ..Default::default()
        }),
    );
    c.set_test_host(host.into());
    c.agent = crate::http::Agent::with_timeout(Duration::from_secs(2));
    c
}
fn params() -> SubmitParams {
    let track = crate::track::generator::build(
        1000.,
        400,
        42,
        (39.9, 116.4),
        1_700_000_000_000,
        &[(39.9, 116.4), (39.901, 116.401), (39.902, 116.4)],
    );
    track.validate_consistency().unwrap();
    SubmitParams {
        five_point_json: json!({"fivePointJson":json!([{"lat":39.9,"lon":116.4,"glat":39.9,"glon":116.4}]).to_string()}).to_string(),
        track, uid:42, selected_unid:77, policy:1, policy_ts:123, min_distance:1000,
        weight:68., face_check:0, address:"offline fixture".into(),
    }
}
fn error(result: Result<SubmitResult, String>) -> String {
    match result {
        Ok(_) => panic!("unexpected success"),
        Err(e) => e,
    }
}
/// A1：iOS 身份必须**在发请求、写回执之前**被拒 —— 提交报文固定按 Android 构造，
/// 用 iOS 身份提交就是同一条 token 上「登录 iOS / 提交 Android」的硬矛盾。
///
/// 判据用「同一台 server 上，iOS 那次不产生任何请求，随后 Android 身份能一次成功」，
/// 顺带证明拒绝没有留下 pending 回执（否则后续提交会被锁死）。
/// N1（0.9.0 复审）：回执里的目标距离必须是**实跑距离**，不能是学校门槛。
///
/// 判据靠"两者明显不同"来区分：夹具实跑约 1000 m，这里把门槛设成 2000 m。
/// 一旦调用点回退成传 `p.min_distance`，这条立刻红。
#[test]
fn receipt_records_the_actual_distance_not_the_school_threshold() {
    let mut p = params();
    p.min_distance = 2000;
    let run_m = p.track.totalDistance;
    assert!(
        (run_m - p.min_distance as f64).abs() > 100.0,
        "夹具必须让实跑与门槛明显不同，否则区分不了两者：run={run_m} min={}",
        p.min_distance
    );
    assert_eq!(
        receipt_target_distance(run_m),
        run_m.round() as i64,
        "回执应记实跑距离"
    );
    assert_ne!(
        receipt_target_distance(run_m),
        p.min_distance,
        "回执不得记学校门槛（N1）"
    );
}

#[test]
fn ios_identity_is_rejected_before_any_request_or_receipt() {
    let _fixture = Fixture::new();
    let (host, task) = server(vec![Reply::Record(
        json!({"error":10000,"data":{"rrid":"42"}}),
    )]);
    let p = params();
    let mut ios = client(&host);
    ios.identity.platform = "ios".into();
    let message = error(submit_record(&mut ios, &p, &mut |_| {}));
    assert!(message.contains("iOS"), "要指出当前平台：{message}");
    assert!(message.contains("Android"), "要给出可操作出口：{message}");
    assert!(message.contains("设备页"), "要说明去哪儿切身份：{message}");

    // 同一份回执目录下换 Android 身份立刻能提交：说明上面那次没有发请求、也没留 pending。
    let mut android = client(&host);
    assert!(submit_record(&mut android, &p, &mut |_| {}).is_ok());
    assert_eq!(task.join().unwrap(), vec![RECORD_PATH]);
}

#[test]
fn successful_post_then_same_plan_only_sends_once() {
    let _fixture = Fixture::new();
    let (host, task) = server(vec![Reply::Record(
        json!({"error":10000,"data":{"rrid":"42"}}),
    )]);
    let p = params();
    let result = submit_record(&mut client(&host), &p, &mut |_| {}).unwrap();
    assert_eq!(result.rrid, 42);
    assert!(error(submit_record(&mut client(&host), &p, &mut |_| {})).contains("重复提交"));
    assert_eq!(task.join().unwrap(), vec![RECORD_PATH]);
}
/// 预置一条**早已过了静置窗口**的未决回执。
///
/// N2 之后 Unknown 要静置 90 s 才自动对账，真等 90 秒不现实；
/// 这里直接把一条 `at_ms = now − 100 s` 的 Unknown 写进回执日志，
/// 于是下一次提交走的就是「窗口过后 → 拉列表 → 命中就放弃重提」这条真实分支。
fn seed_stale_unknown(uuid: &str, target_dist: i64) -> i64 {
    let p = params();
    let start_ms = p.track.startTime;
    let stop_ms = start_ms + p.track.totalTime * 1000;
    let stale_at = crate::crypto::envelope::now_ms() - 100_000;
    let mut guard = crate::api::submission_guard::Guard::open(42, 77).unwrap();
    guard
        .begin(uuid, start_ms, stop_ms, stale_at, "fp", target_dist)
        .unwrap();
    guard.unknown(uuid, stale_at).unwrap();
    (start_ms, stop_ms);
    start_ms
}

/// 过窗口的 Unknown 对账命中 → 标 Committed、放弃重提（N2 之后走预置回执这条径）。
#[test]
fn stale_unknown_receipt_is_reconciled_without_reposting() {
    let _fixture = Fixture::new();
    let uuid = uuid::Uuid::new_v4().to_string().to_uppercase();
    let start_ms = seed_stale_unknown(&uuid, 1000);
    let p = params();
    let stop_ms = start_ms + p.track.totalTime * 1000;
    let (host, task) = server(vec![Reply::ListRows(
        json!({"rrid":"77","uuid":uuid,"total_dis":1000.0,"total_time":p.track.totalTime,
               "start_time":start_ms,"complete":false,"avg_step_freq":120}),
    )]);
    let message = error(submit_record(&mut client(&host), &p, &mut |_| {}));
    assert!(message.contains("#77"), "要说明已核对到哪条记录：{message}");
    assert!(message.contains("重复提交"), "{message}");
    // 只有一次列表请求，一条提交请求都没有。
    assert_eq!(
        task.join().unwrap(),
        vec![crate::api::records::RECORDS_PATH]
    );
    let _ = stop_ms;
}

/// N2：超时之后**静置窗口内**既不拉列表、也不允许重提。
///
/// 改前的反例：超时 → 立刻拉列表 → 服务端还没写完库 → 读到旧快照 → 判「没有入账」
/// → 放行重提 → 服务端两条记录。现在窗口内直接挡住，且不产生任何多余请求。
#[test]
fn timeout_holds_the_retry_inside_the_settle_window() {
    let _fixture = Fixture::new();
    let (host, task) = server(vec![Reply::Lost]);
    let p = params();
    let first = error(submit_record(&mut client(&host), &p, &mut |_| {}));
    assert!(first.contains("提交结果未知"), "{first}");
    let second = error(submit_record(&mut client(&host), &p, &mut |_| {}));
    assert!(second.contains("上次提交结果未知"), "{second}");
    assert!(
        second.contains("自动对账"),
        "要告诉用户过了窗口会自动核对：{second}"
    );
    // 一次提交请求、零次列表请求：窗口内不去猜。
    assert_eq!(task.join().unwrap(), vec![RECORD_PATH]);
}

/// 对账这一步本身失败（列表拉不到）时必须继续挡住：没有证据就不许重发。
///
/// N2 之后这条只在「窗口已过的 Unknown」上才可能发生，所以先预置回执，
/// 再让列表接口直接断连。
#[test]
fn failed_reconciliation_keeps_blocking_the_retry() {
    let _fixture = Fixture::new();
    let uuid = uuid::Uuid::new_v4().to_string().to_uppercase();
    seed_stale_unknown(&uuid, 1000);
    let (host, task) = server(vec![Reply::DropList]);
    let message = error(submit_record(&mut client(&host), &params(), &mut |_| {}));
    assert!(message.contains("上次提交结果未知"), "{message}");
    // 只尝试了对账，没有发提交请求。
    assert_eq!(
        task.join().unwrap(),
        vec![crate::api::records::RECORDS_PATH]
    );
}
#[test]
fn explicit_rejection_can_be_retried_by_user_but_not_automatically() {
    let _fixture = Fixture::new();
    let (host, task) = server(vec![
        Reply::Record(json!({"error":10603,"message":"fixture rejection"})),
        Reply::Record(json!({"error":10000,"data":{"rrid":43}})),
    ]);
    let p = params();
    assert!(error(submit_record(&mut client(&host), &p, &mut |_| {})).contains("明确拒绝"));
    assert_eq!(
        submit_record(&mut client(&host), &p, &mut |_| {})
            .unwrap()
            .rrid,
        43
    );
    assert_eq!(task.join().unwrap().len(), 2);
}
/// 「响应看着像成功但没有 rrid」= 结果未知：不能当成功，也不能当失败。
///
/// N2：落成 Unknown 之后**不当场对账**（静置 90 s），所以这里只有一次提交请求。
#[test]
fn malformed_success_is_recorded_as_unknown() {
    for response in [
        json!({}),
        json!({"error":10000,"data":{}}),
        json!({"error":10000,"data":"{broken"}),
    ] {
        let _fixture = Fixture::new();
        let (host, task) = server(vec![Reply::Record(response)]);
        assert!(
            error(submit_record(&mut client(&host), &params(), &mut |_| {}))
                .contains("提交结果未知")
        );
        assert_eq!(task.join().unwrap(), vec![RECORD_PATH]);
    }
}
#[test]
fn unauthorized_post_invalidates_session_and_does_not_send_again() {
    let _fixture = Fixture::new();
    let (host, task) = server(vec![Reply::Unauthorized]);
    let mut c = client(&host);
    let p = params();
    assert!(error(submit_record(&mut c, &p, &mut |_| {})).contains("明确拒绝"));
    assert!(crate::api::client::is_session_expired(&error(
        submit_record(&mut c, &p, &mut |_| {})
    )));
    assert_eq!(task.join().unwrap(), vec![RECORD_PATH]);
}
