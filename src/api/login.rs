//! 登录链。
//!
//! ① GT4 滑块验证→ 四凭证
//! ② POST /api/v70270/security/geevalidate（信封 body + headerSign）
//! ③ POST /api/v70100/login（Authorization: Basic + 信封 body）→ uid/token/unid

use super::client::{check_business, get_field, ApiClient};
use super::gt4::solve_gt4;
use super::model::{Session, HOST};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::{json, Value};

pub const GEEVALIDATE_PATH: &str = "/api/v70270/security/geevalidate";
pub const GEECHECK_PATH: &str = "/api/v65/security/checkGeeUse";
pub const LOGIN_PATH: &str = "/api/v70100/login";
pub const LOGOUT_PATH: &str = "/api/v6/user/logout";

/// bizType=0 的 captchaId（captchaIDForBizType 静态提取值；GT4 会话直接使用，
/// 走 v31 geepreprocess 属 Android GT3 旧链，iOS 登录链不引用）。
pub const CAPTCHA_ID_BIZ0: &str = "8c065103d81f5fd3efec8ad3e3a84c30";

/// checkGeeUse 免验证码探测：data=true 时本次登录可跳过 GT4 与 geevalidate。
///
/// 三态处理（原来是「任何异常都当 false」）：
/// - `Ok(true/false)`：服务端明确回答了要不要验证码；
/// - `Err`：**问不出来**（网络/解密失败）。这时不要进 GT4 —— 否则会在同一个坏
///   出口上继续烧验证码请求，把「网络不通」变成「滑块答错」。一个 GT4 请求都不发。
fn gee_check(
    client: &mut ApiClient,
    username: &str,
    uuid: &str,
    log: &mut dyn FnMut(&str),
) -> Result<bool, String> {
    let body = json!({
        "username": username,
        // 与 geevalidate / login 用同一个 uuid：同一次登录三条请求三个 uuid，
        // 本身就是一条免费的「这不是一个客户端」证据。
        "uuid": uuid,
        "unid": 0,
        "type": 2,
    })
    .to_string();
    let mut attempt = 0;
    let biz = loop {
        attempt += 1;
        match client.call("POST", GEECHECK_PATH, &body, &[]) {
            Ok(biz) => break biz,
            Err(e) if attempt < 2 => {
                log(&format!("[gee] checkGeeUse 失败（{e}），1 秒后重试一次"));
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            Err(e) => return Err(e),
        }
    };
    // 服务端可能给 bool，也可能给 "true" / 1；原来只认 bool，字符串会被当 false。
    let skip = match biz.get("data") {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => text.eq_ignore_ascii_case("true") || text == "1",
        Some(Value::Number(number)) => number.as_i64().map(|v| v != 0).unwrap_or(false),
        _ => false,
    };
    log(&format!(
        "[gee] checkGeeUse data={}（{}）",
        skip,
        if skip { "跳过 GT4" } else { "走 GT4 流程" }
    ));
    Ok(skip)
}

/// 完整登录：checkGeeUse → [GT4 → geevalidate] → login。成功返回 Session。
pub fn login(
    client: &mut ApiClient,
    username: &str,
    password: &str,
    log: &mut dyn FnMut(&str),
) -> Result<Session, String> {
    let uuid_value = uuid::Uuid::new_v4().to_string();

    // 冷却期内一个验证码请求都不发。用户看到「滑块过不去」就会连点，而连点正好
    // 是最坏的动作：每点一次都在加深极验对该会话的降级。
    if let Some(secs) = super::captcha_guard::cooldown_remaining_secs() {
        return Err(format!(
            "验证码校验已被临时限流（不是账号或密码错误），请{}后再试",
            super::captcha_guard::cooldown_hint(secs)
        ));
    }

    match gee_check(client, username, &uuid_value, log) {
        Ok(true) => log("[login] 免验证码路径：直接登录"),
        Ok(false) => {
            let captcha_id = CAPTCHA_ID_BIZ0;
            log("[login] 开始 GT4 滑块验证…");
            let creds = match solve_gt4(captcha_id, client, log) {
                Ok(creds) => creds,
                Err(error) => {
                    // 判错不重试（solve_gt4 内部已保证），这里只负责把话说明白。
                    if matches!(error, super::gt4::Gt4Error::Rejected(_)) {
                        let secs = super::captcha_guard::record_rejection();
                        let wait = if secs > 0 {
                            format!("，请{}后再试", super::captcha_guard::cooldown_hint(secs))
                        } else {
                            "；先别连续点击，过几分钟再试".to_string()
                        };
                        return Err(format!(
                            "验证码判定未通过，已停止本次尝试以免触发风控{wait}（与账号密码无关）"
                        ));
                    }
                    return Err(format!("验证码未能完成：{error}"));
                }
            };
            log(&format!(
                "[login] GT4 通过 lot_number={}",
                creds
                    .get("lotNumber")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
            ));

            let body = json!({
                "lotNumber": creds["lotNumber"],
                "captchaOutput": creds["captchaOutput"],
                "passToken": creds["passToken"],
                "genTime": creds["genTime"],
                "isOffline": false,
                // 必须跟身份走：写死 1（iOS）时，Android 身份的同一条请求会出现
                // 「body 说 iOS、headerSign 说 Android」的跨来源矛盾。
                "osType": if client.identity.platform == "android" { 0 } else { 1 },
                "businessType": 0,
                "uuid": uuid_value,
                "username": username,
            })
            .to_string();
            let url = format!("{HOST}{GEEVALIDATE_PATH}");
            // UA 必须由身份拼出：写死 iOS UA 会让同一账号的登录与其余请求分属两台设备。
            let ua = crate::crypto::header::ua_for(&client.identity);

            // 只重发 geevalidate（凭证可能仍有效，重发比重新破一次滑块便宜）：
            // 不再「重破滑块 3 轮」，也不再在拿不到业务码时当成通过 —— 后者会让
            // 一次「没验证成功」滑到带密码的登录请求，被服务端记成一次失败登录。
            let mut validated = false;
            let mut last_code: Option<i64> = None;
            for attempt in 1..=2 {
                let out = client.envelope_request("POST", &url, &body, &ua, &[])?;
                let code = out
                    .decrypted
                    .as_ref()
                    .and_then(|d| d.business.get("error").cloned())
                    .and_then(|e| e.as_i64());
                match code {
                    Some(10000) => {
                        super::captcha_guard::record_success();
                        log("[geevalidate] 验证通过");
                        validated = true;
                        break;
                    }
                    Some(10003) => {
                        let secs = super::captcha_guard::record_rejection();
                        let wait = if secs > 0 {
                            format!("，请{}后再试", super::captcha_guard::cooldown_hint(secs))
                        } else {
                            "，请稍后再试".to_string()
                        };
                        return Err(format!(
                            "服务端拒绝了验证码校验（10003）{wait}（与账号密码无关）"
                        ));
                    }
                    other => {
                        log(&format!("[geevalidate] err={other:?}（第 {attempt}/2 次）"));
                        last_code = other;
                        if attempt < 2 {
                            let gap = 800 + (crate::crypto::envelope::now_ms().max(0) as u64 % 800);
                            std::thread::sleep(std::time::Duration::from_millis(gap));
                        }
                    }
                }
            }
            if !validated {
                return Err(format!(
                    "验证码校验未通过（错误码 {}），请稍后再试",
                    last_code
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "无（响应无法解密）".into())
                ));
            }
            // 验证码刚过就立刻登录同样是个特征；真实用户点「登录」之间会有停顿。
            let pause = 1_200 + (crate::crypto::envelope::now_ms().max(0) as u64 % 2_300);
            std::thread::sleep(std::time::Duration::from_millis(pause));
        }
        Err(error) => {
            // 问不出来就不进 GT4：在坏出口上刷验证码请求，只会把「网络不通」
            // 变成「滑块答错」，还白烧一次容忍度。
            return Err(format!(
                "无法确认当前是否需要验证码（{error}），本次没有发起验证；请检查网络后重试"
            ));
        }
    }

    // login（Authorization Basic + Android 形态信封 body）
    let identity = client.identity.clone();
    let device_id = if identity.device_id.is_empty() {
        uuid::Uuid::new_v4().to_string().to_uppercase()
    } else {
        identity.device_id.clone()
    };
    let login_body = json!({
        "device_model": identity.device_name,
        "os_version": identity.os_version,
        // Android 6+ 普通应用读到的 MAC 固定是 02:00:00:00:00:00；
        // 这里必须与提交头一致，否则同一账号「登录说随机 MAC、提交说占位 MAC」，
        // 比轨迹分析便宜得多就能对出来。
        "mac_address": crate::api::fingerprint::ANDROID_PLACEHOLDER_MAC,
        "imei": "",
        "loginType": 0,
        "username": username,
        "password": password,
        "uuid": uuid_value,
        // 与 headerSign 里的 osType 同源：iOS 身份却报 "0"（Android）是同一条请求内的自相矛盾。
        "osType": if client.identity.platform == "android" { "0" } else { "1" },
    })
    .to_string();
    let credential = B64.encode(format!("{username}:{password}"));
    let extra = vec![("Authorization".to_string(), format!("Basic {credential}"))];
    let url = format!("{HOST}{LOGIN_PATH}");
    // 与 geevalidate 同一套 UA：登录本身也是这条设备身份的一部分。
    let ua = crate::crypto::header::ua_for(&client.identity);
    let out = client.envelope_request("POST", &url, &login_body, &ua, &extra)?;
    let dec = out
        .decrypted
        .ok_or_else(|| format!("HTTP {} 且响应解密失败", out.http_status))?;
    let biz = check_business(&dec.business)?;
    let uid = get_field(&biz, "uid").and_then(|v| v.as_i64()).unwrap_or(0);
    let token = get_field(&biz, "token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if uid < 1 || token.is_empty() {
        return Err(format!(
            "登录响应缺 uid/token: {}",
            truncate(&crate::textlog::redact_json(&biz).to_string(), 200)
        ));
    }
    let sess = Session {
        uid,
        token,
        unid: {
            let v = get_field(&biz, "unid")
                .cloned()
                .unwrap_or(Value::String("0".into()));
            match &v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => "0".to_string(),
            }
        },
        name: get_field(&biz, "name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        weight: get_field(&biz, "weight")
            .and_then(|v| v.as_f64())
            .unwrap_or(68.0),
        username: username.to_string(),
        device_id,
        profile: super::client::parse_data_field(&biz),
    };
    if let Err(e) = super::model::save_session(&sess) {
        eprintln!("[login] 会话保存失败: {e}");
    }
    eprintln!(
        "[session] uid={} token={} unid={} name={} weight={} device={}",
        sess.uid,
        crate::textlog::mask(&sess.token),
        sess.unid,
        sess.name,
        sess.weight,
        sess.device_id
    );
    eprintln!(
        "[session] profile 字段数={}",
        sess.profile.as_object().map(|o| o.len()).unwrap_or(0)
    );
    Ok(sess)
}

/// 登出并清理 。
pub fn logout(client: &mut ApiClient, log: &mut dyn FnMut(&str)) {
    if let Some(sess) = client.login.clone() {
        let _ = client.call("POST", LOGOUT_PATH, "{}", &[]);
        log(&format!("[logout] 已请求登出 uid={}", sess.uid));
    }
    super::model::clear_session();
    log("[logout] 本地  已删除");
}

fn truncate(s: &str, n: usize) -> String {
    crate::textlog::truncate(s, n)
}
