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
#[test]
fn lost_response_reconciles_uuid_without_reposting() {
    let _fixture = Fixture::new();
    let (host, task) = server(vec![Reply::Lost, Reply::List(true)]);
    let p = params();
    assert!(
        error(submit_record(&mut client(&host), &p, &mut |_| {})).contains("已找到本次记录 #77")
    );
    assert!(error(submit_record(&mut client(&host), &p, &mut |_| {})).contains("重复提交"));
    assert_eq!(
        task.join().unwrap(),
        vec![RECORD_PATH, crate::api::records::RECORDS_PATH]
    );
}
/// 超时 + 列表确认没有 → 允许**一次**重提（0.9.0 的 Unknown 契约）。
///
/// 顺序必须是「先落 Unknown、再只读对账、确认缺失才放行」：断言请求序列里
/// RECORDS_PATH 只出现一次、且它排在第二次 RECORD_PATH 之前 —— 重提不是盲目的，
/// 而是拿着记录列表核对过之后才发生。
#[test]
fn timeout_without_a_matching_record_allows_one_retry() {
    let _fixture = Fixture::new();
    let (host, task) = server(vec![
        Reply::Lost,
        Reply::List(false),
        Reply::Record(json!({"error":10000,"data":{"rrid":43}})),
    ]);
    let p = params();
    let message = error(submit_record(&mut client(&host), &p, &mut |_| {}));
    assert!(message.contains("提交结果未知"), "{message}");
    assert!(message.contains("允许重新提交"), "{message}");
    assert_eq!(
        submit_record(&mut client(&host), &p, &mut |_| {})
            .expect("对账确认没有入账后就该允许重提")
            .rrid,
        43
    );
    assert_eq!(
        task.join().unwrap(),
        vec![RECORD_PATH, crate::api::records::RECORDS_PATH, RECORD_PATH]
    );
}
/// 对账这一步本身失败（列表拉不到）时必须继续挡住：没有证据就不许重发。
#[test]
fn failed_reconciliation_keeps_blocking_the_retry() {
    let _fixture = Fixture::new();
    // 只给一个 Lost：第二次提交的 preflight 拿不到列表，必须被挡住且不发提交请求。
    let (host, task) = server(vec![Reply::Lost]);
    let p = params();
    assert!(error(submit_record(&mut client(&host), &p, &mut |_| {})).contains("提交结果未知"));
    let message = error(submit_record(&mut client(&host), &p, &mut |_| {}));
    assert!(message.contains("上次提交结果未知"), "{message}");
    assert_eq!(task.join().unwrap(), vec![RECORD_PATH]);
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
#[test]
fn malformed_success_is_recorded_as_unknown() {
    for response in [
        json!({}),
        json!({"error":10000,"data":{}}),
        json!({"error":10000,"data":"{broken"}),
    ] {
        let _fixture = Fixture::new();
        let (host, task) = server(vec![Reply::Record(response), Reply::List(false)]);
        assert!(
            error(submit_record(&mut client(&host), &params(), &mut |_| {}))
                .contains("提交结果未知")
        );
        assert_eq!(task.join().unwrap().len(), 2);
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
