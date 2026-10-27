//! 登录链。
//!
//! ① GT4 滑块验证→ 四凭证
//! ② POST /api/v70270/security/geevalidate（信封 body + headerSign）
//! ③ POST /api/v70100/login（Authorization: Basic + 信封 body）→ uid/token/unid

use super::client::{check_business, get_field, ApiClient};
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
    // 除 `data` 之外还有字段时原样记一行：服务端若在这里回传 captchaId / businessType /
    // 场景值，那正是「/verify 过了但 geevalidate 判失败」最可能的解释（密钥对不上）。
    // 脱敏后截断，不会带出账号信息。
    let extra_keys = biz
        .as_object()
        .map(|map| map.keys().filter(|key| key.as_str() != "data").count())
        .unwrap_or(0);
    if extra_keys > 0 {
        let detail = crate::textlog::redact_json(&biz);
        log(&format!(
            "[gee] checkGeeUse 原始响应：{}",
            truncate(&detail.to_string(), 300)
        ));
    }
    Ok(skip)
}

/// 过码方式。
///
/// 自动识别缺口是概率性的（实测约 8% 会认错），而被极验降级过的出口上，
/// 认错一次就少一次机会。所以把「谁来拖滑块」做成显式选项：让用户自己拖，
/// 是最稳的一条路。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaptchaMode {
    /// 自动识别缺口（默认）。
    #[default]
    Auto,
    /// 手动拖滑块：图交给界面，由人拖进缺口。
    Manual,
}

impl CaptchaMode {
    /// 从配置字符串解析；认不出的值按自动处理（老配置里没有这个字段）。
    pub fn parse(text: &str) -> Self {
        match text.trim().to_ascii_lowercase().as_str() {
            "manual" | "hand" | "手动" | "手动过码" => Self::Manual,
            _ => Self::Auto,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "自动过码",
            Self::Manual => "手动过码",
        }
    }
}

/// 登录选项。
pub struct LoginOptions<'a> {
    pub captcha: CaptchaMode,
    /// 手动过码的界面回调；`captcha == Manual` 时必须给出。
    pub solver: Option<&'a mut dyn super::gt4::ManualSolver>,
}

impl Default for LoginOptions<'_> {
    fn default() -> Self {
        Self {
            captcha: CaptchaMode::Auto,
            solver: None,
        }
    }
}

/// 完整登录（自动过码）：checkGeeUse → [GT4 → geevalidate] → login。成功返回 Session。
pub fn login(
    client: &mut ApiClient,
    username: &str,
    password: &str,
    log: &mut dyn FnMut(&str),
) -> Result<Session, String> {
    login_with(client, username, password, LoginOptions::default(), log)
}

/// 登录，可指定过码方式（自动识别 / 手动拖滑块）。
pub fn login_with(
    client: &mut ApiClient,
    username: &str,
    password: &str,
    options: LoginOptions<'_>,
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
            let manual = options.captcha == CaptchaMode::Manual;
            log(&format!(
                "[login] 开始 GT4 滑块验证（{}）…",
                options.captcha.label()
            ));
            let solved = match (options.captcha, options.solver) {
                (CaptchaMode::Manual, Some(solver)) => {
                    super::gt4::solve_gt4_manual(captcha_id, client, solver, log)
                }
                (CaptchaMode::Manual, None) => Err(super::gt4::Gt4Error::Fatal(
                    "手动过码缺少界面回调，请改回自动过码".into(),
                )),
                (CaptchaMode::Auto, _) => super::gt4::solve_gt4(captcha_id, client, log),
            };
            let creds = match solved {
                Ok(creds) => creds,
                Err(error) => {
                    // 用户自己取消：不是判错，什么都不记，也别吓人。
                    if error.cancelled() {
                        return Err(error.message().to_string());
                    }
                    // 判错不重试（solve_gt4 / solve_gt4_manual 内部已保证），
                    // 这里只负责把话说明白。
                    if matches!(error, super::gt4::Gt4Error::Rejected(_)) {
                        // 人拖错一次不构成「客户端不会动」的证据：手动模式下不进
                        // 进程级冷却，否则拖歪一下就十分钟不能试。
                        let wait = if manual {
                            "；换一张图再拖一次即可".to_string()
                        } else {
                            let secs = super::captcha_guard::record_rejection();
                            if secs > 0 {
                                format!("，请{}后再试", super::captcha_guard::cooldown_hint(secs))
                            } else {
                                "；先别连续点击，过几分钟再试".to_string()
                            }
                        };
                        return Err(format!(
                            "验证码判定未通过，已停止本次尝试以免触发风控{wait}（与账号密码无关）"
                        ));
                    }
                    return Err(format!("验证码未能完成：{error}"));
                }
            };
            // 极验凭证是一次性的：缺一个就**不要**发 geevalidate —— 交一个 null
            // 换回来的 10003 只是白烧一次机会，还会被记成一次判错。
            if let Err(missing) = required_credentials(&creds) {
                return Err(format!(
                    "验证码凭证不完整（缺 {missing}），本次没有提交校验；请重试"
                ));
            }
            log(&format!(
                "[login] GT4 通过 lot_number={}（captchaOutput {} 字符 / passToken {} 字符）",
                creds
                    .get("lotNumber")
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
                creds
                    .get("captchaOutput")
                    .and_then(|v| v.as_str())
                    .map(str::len)
                    .unwrap_or(0),
                creds
                    .get("passToken")
                    .and_then(|v| v.as_str())
                    .map(str::len)
                    .unwrap_or(0),
            ));

            let body_value = json!({
                "lotNumber": creds["lotNumber"],
                "captchaOutput": creds["captchaOutput"],
                "passToken": creds["passToken"],
                "genTime": creds["genTime"],
                "isOffline": false,
                // 必须是 1。实测（0.6.1 的变体探针，2026-10-27）：
                //   osType=0 → {"error":10003,"message":"验证失败"}
                //   osType=1 → {"error":10000,"message":"成功"}
                // 这个字段不是「平台」：Android 身份的登录体仍写 0 且能登录成功，
                // 说明 geevalidate 这一侧的 osType 是「客户端类型/移动端」语义。
                // 旧实现按 identity.platform 派生（Android→0），于是无论滑块拖得对不对
                // 都会被判「验证失败」——这就是长期 10003 的真正原因。
                "osType": 1,
                "businessType": 0,
                "uuid": uuid_value,
                "username": username,
            });
            let body = body_value.to_string();
            let url = format!("{HOST}{GEEVALIDATE_PATH}");
            // UA 必须由身份拼出：写死 iOS UA 会让同一账号的登录与其余请求分属两台设备。
            let ua = crate::crypto::header::ua_for(&client.identity);

            // 只发一次主请求。
            //
            // 之前「拿不到业务码就重发一次」的写法有三个问题：
            // ① 同一条 `lot_number` 再交给服务端复验，可能拿到「重复校验」的失败；
            // ② 业务码可能是字符串 "10000"（`as_i64()` 读到 None），于是第一次
            //    明明通过了也被当成「读不到码」而重发；
            // ③ 一次失败被记成两次判错，冷却直接跳到 10 分钟。
            // 现在：判码兼容字符串；读不出来就如实报错，不盲目重发。
            let out = client.envelope_request("POST", &url, &body, &ua, &[])?;
            let dec = out.decrypted.ok_or_else(|| {
                format!(
                    "验证码校验结果无法解密（HTTP {}，{} 字节）{}；本次没有重发同一凭证",
                    out.http_status, out.raw_len, out.raw_head
                )
            })?;
            let code = business_code(&dec.business);
            // 三个分支要么赋值要么直接 return，所以不需要初始值。
            let validated;
            match code {
                Some(10000) => validated = true,
                Some(10003) => {
                    // 手动模式下这条不代表「拖错了」：人已经拖对了，是服务端不认这次凭证。
                    // 进冷却只会把一个业务问题变成十分钟不能试。
                    let detail = crate::textlog::redact_json(&dec.business);
                    log(&format!(
                        "[geevalidate] 10003 原始响应：{}",
                        truncate(&detail.to_string(), 300)
                    ));
                    // 服务端只回一句「验证失败」，看不出是凭证被拒、captchaId 不匹配，
                    // 还是某个字段类型不对。用同一凭证换口径各问一次（geetest 的
                    // validate 是读取校验结果，不是一次性消费），哪一变体拿到 10000
                    // 就说明真正原因是字段口径，而不是凭证无效。
                    match try_geevalidate_variants(client, &url, &body_value, &ua, log)? {
                        Some(10000) => {
                            log("[geevalidate] 变体口径通过（10000）—— 之前的字段口径与服务端不一致");
                            validated = true;
                        }
                        _ => {
                            let wait = if manual {
                                "；请稍后重试，或切换另一种过码方式".to_string()
                            } else {
                                let secs = super::captcha_guard::record_rejection();
                                if secs > 0 {
                                    format!(
                                        "，请{}后再试",
                                        super::captcha_guard::cooldown_hint(secs)
                                    )
                                } else {
                                    "，请稍后再试".to_string()
                                }
                            };
                            return Err(format!(
                                "服务端拒绝了验证码校验（10003）{wait}（与账号密码无关；日志里有变体试跑结果）"
                            ));
                        }
                    }
                }
                other => {
                    let detail = crate::textlog::redact_json(&dec.business);
                    log(&format!(
                        "[geevalidate] 业务码 {other:?}：{}",
                        truncate(&detail.to_string(), 300)
                    ));
                    return Err(format!(
                        "验证码校验未通过（错误码 {}），请稍后再试",
                        other
                            .map(|code| code.to_string())
                            .unwrap_or_else(|| "无（响应里没有 error 字段）".into())
                    ));
                }
            }
            if validated {
                super::captcha_guard::record_success();
                log("[geevalidate] 验证通过");
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

/// 四个凭证必须都是非空值；返回第一个缺失的字段名。
///
/// `genTime` 允许是数字（极验有时给毫秒时间戳而不是字符串），所以只校验「不是
/// null / 不是空串」，不强制类型 —— 交给服务端按它自己的口径解析。
fn required_credentials(creds: &Value) -> Result<(), &'static str> {
    for key in ["lotNumber", "captchaOutput", "passToken", "genTime"] {
        let ok = match creds.get(key) {
            Some(Value::String(text)) => !text.trim().is_empty(),
            Some(Value::Number(_)) => true,
            _ => false,
        };
        if !ok {
            return Err(key);
        }
    }
    Ok(())
}

/// 读业务码 `error`。服务端可能给数字，也可能给字符串（"10000"）——
/// 只认 `as_i64()` 会把「字符串形式的成功」误判成「读不到码」。
fn business_code(biz: &Value) -> Option<i64> {
    match biz.get("error") {
        Some(Value::Number(number)) => number.as_i64(),
        Some(Value::String(text)) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// 10003 之后换口径再问一遍：同一凭证、不同字段形状，各发一次。
///
/// 为什么值得这么做：服务端只回一句「验证失败」，看不出是凭证被拒、captchaId 不匹配，
/// 还是某个字段类型不对。geetest 的 validate 是「读取校验结果」而不是一次性消费，
/// 所以同一 lot 在有效期内可以被多次询问。一旦某个变体拿到 10000，就说明真正原因是
/// **字段口径**，而不是「滑块没拖对」——这也是把用户从“怎么拖都失败”里捞出来的唯一
/// 低成本办法（真凶若在服务端密钥，那几次请求也不会比现在更糟）。
///
/// 返回第一个成功的业务码（目前只有 10000 算成功）。
fn try_geevalidate_variants(
    client: &mut ApiClient,
    url: &str,
    base: &Value,
    ua: &str,
    log: &mut dyn FnMut(&str),
) -> Result<Option<i64>, String> {
    let mut variants: Vec<(&str, Value)> = Vec::new();

    // ① businessType 与 `checkGeeUse` 的 type 对齐：现在两条请求一个 0 一个 2，
    //    如果服务端按 businessType 选极验密钥，这一条就是决定性的。
    let mut body = base.clone();
    body["businessType"] = json!(2);
    variants.push(("businessType=2（与 checkGeeUse 的 type 对齐）", body));

    // ② genTime 类型反转：服务端若按 long 解析，字符串会被当成 0 → 过期。
    let mut body = base.clone();
    body["genTime"] = match &base["genTime"] {
        Value::String(text) => text
            .trim()
            .parse::<i64>()
            .map(|value| json!(value))
            .unwrap_or_else(|_| base["genTime"].clone()),
        other => json!(other.to_string()),
    };
    variants.push(("genTime 类型反转", body));

    // ③ osType 反转：占位客户端曾把 Android 身份写成 iOS。
    let mut body = base.clone();
    body["osType"] = if base["osType"].as_i64() == Some(0) {
        json!(1)
    } else {
        json!(0)
    };
    variants.push(("osType 反转", body));

    for (label, body) in variants {
        // 变体之间留间隔：一串等距请求本身就是特征。
        std::thread::sleep(std::time::Duration::from_millis(900));
        let out = client.envelope_request("POST", url, &body.to_string(), ua, &[])?;
        let Some(dec) = out.decrypted else {
            log(&format!(
                "[geevalidate] 变体「{label}」响应无法解密（HTTP {}，{} 字节）",
                out.http_status, out.raw_len
            ));
            continue;
        };
        let code = business_code(&dec.business);
        let detail = crate::textlog::redact_json(&dec.business);
        log(&format!(
            "[geevalidate] 变体「{label}」→ {code:?}：{}",
            truncate(&detail.to_string(), 160)
        ));
        if code == Some(10000) {
            return Ok(Some(10000));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captcha_mode_round_trips_through_config_text() {
        assert_eq!(CaptchaMode::parse("auto"), CaptchaMode::Auto);
        assert_eq!(CaptchaMode::parse(" MANUAL "), CaptchaMode::Manual);
        assert_eq!(CaptchaMode::parse("手动"), CaptchaMode::Manual);
        // 老配置/空值/拼错 → 自动（绝不能因为认不出就当手动，那会卡在等界面）。
        assert_eq!(CaptchaMode::parse(""), CaptchaMode::Auto);
        assert_eq!(CaptchaMode::parse("hand2"), CaptchaMode::Auto);
        for mode in [CaptchaMode::Auto, CaptchaMode::Manual] {
            assert_eq!(CaptchaMode::parse(mode.as_str()), mode);
            assert!(!mode.label().is_empty());
        }
    }

    #[test]
    fn business_code_accepts_numbers_and_strings() {
        assert_eq!(business_code(&json!({"error": 10000})), Some(10000));
        // 字符串形式：旧实现读成 None，于是「其实通过了」被当成「读不到码」→
        // 同一凭证再发一次 → 极验按重放拒掉 → 用户看到 10003。
        assert_eq!(business_code(&json!({"error": "10000"})), Some(10000));
        assert_eq!(business_code(&json!({"error": " 10003 "})), Some(10003));
        assert_eq!(business_code(&json!({"error": null})), None);
        assert_eq!(business_code(&json!({})), None);
    }

    #[test]
    fn credentials_must_all_be_present() {
        let ok = json!({
            "lotNumber": "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6",
            "captchaOutput": "out",
            "passToken": "pass",
            "genTime": "1729999999999",
        });
        assert!(required_credentials(&ok).is_ok());
        // genTime 是数字也要接受
        let numeric = json!({
            "lotNumber": "lot", "captchaOutput": "out",
            "passToken": "pass", "genTime": 1729999999999i64,
        });
        assert!(required_credentials(&numeric).is_ok());
        for key in ["lotNumber", "captchaOutput", "passToken", "genTime"] {
            let mut missing = ok.clone();
            missing[key] = Value::Null;
            assert_eq!(required_credentials(&missing), Err(key));
            let mut blank = ok.clone();
            blank[key] = json!("   ");
            assert_eq!(required_credentials(&blank), Err(key));
        }
    }
}
