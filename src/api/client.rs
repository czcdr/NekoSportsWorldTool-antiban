//! HTTP 客户端封装。
//!
//! 通用请求封装：body = 信封(明文, insert)，headerSign = 信封(header, observed)，
//! 响应两层解密到业务 JSON；同一实例复用同一 keyDataFour。

use crate::api::model::Session;
use crate::crypto::decrypt::{decrypt_response, derive_paes_key, Decrypted};
use crate::crypto::envelope::{
    build_envelope, build_envelope_ts, rsa_public_key, EnvelopeSession, OuterOrder,
};
use crate::crypto::header::{build_header_for, HeaderIdentity};

const AGENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub fn make_agent() -> crate::http::Agent {
    match try_agent_with_proxy(None) {
        Ok(agent) => agent,
        // 直连 agent 构造失败（TLS 后端初始化异常）时退回同一实现的兜底构造；
        // 这里没有"代理配了却没走"的问题，因为本来就是要直连。
        Err(_) => crate::http::Agent::with_timeout(AGENT_TIMEOUT),
    }
}

/// 构造 HTTP agent，代理串无效时**返回错误而不是直连**。
///
/// 直连与走代理的出口 IP 不同，两者绝不能在同一次任务里混用。以前这里失败会
/// 静默回退到直连，调用方却仍在日志里写"已绑定同城出口"。
fn try_agent_with_proxy(proxy_url: Option<&str>) -> Result<crate::http::Agent, String> {
    crate::http::Agent::new(AGENT_TIMEOUT, proxy_url).map_err(|error| {
        let detail = crate::textlog::redact_text(&error.to_string());
        match proxy_url {
            Some(url) => format!(
                "代理 {} 不可用（{detail}）；拒绝改为直连",
                crate::textlog::redact_url(url)
            ),
            None => format!("无法创建直连 HTTP 客户端：{detail}"),
        }
    })
}

/// 按指定代理构造 agent（供出口探测之类的旁路使用），失败即错误。
pub(crate) fn agent_with_proxy(proxy_url: &str) -> Result<crate::http::Agent, String> {
    try_agent_with_proxy(Some(proxy_url))
}

/// 从数据目录读取 `proxies.json`。
///
/// - 文件不存在 / 空数组 → `Ok(None)`：未配置代理，直连（与改造前一致）；
/// - 解析失败 / 协议不支持 → `Err`：**不能当没配**，否则一个写坏的
///   `proxies.json` 就会把整条链路静默降级成真实 IP 直连。
fn load_egress_from_disk() -> Result<Option<crate::api::egress::EgressPool>, String> {
    let path = crate::platform::data_dir().join("proxies.json");
    match crate::api::egress::EgressPool::load(&path) {
        Ok(pool) if !pool.is_empty() => Ok(Some(pool)),
        Ok(_) => Ok(None),
        Err(error) => Err(format!(
            "出口配置 {} 无法使用：{}（已拒绝发请求，避免回落到直连）",
            path.display(),
            crate::textlog::redact_text(&format!("{error:?}"))
        )),
    }
}

#[derive(Clone)]
pub struct AuthContext {
    pub epoch: u64,
    pub rejected: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub notify: std::sync::mpsc::Sender<AuthFailure>,
}

pub struct AuthFailure {
    pub epoch: u64,
    pub uid: i64,
    pub token: String,
}

pub struct ApiClient {
    auth_context: Option<AuthContext>,
    auth_rejected: bool,
    #[cfg(test)]
    test_host: Option<String>,
    pub agent: crate::http::Agent,
    pub session: EnvelopeSession,
    pub identity: HeaderIdentity,
    pub login: Option<Session>,
    /// 出口池；None 或空池都表示直连（与改造前行为一致）。
    pub egress: Option<crate::api::egress::EgressPool>,
    /// 本次任务实际绑定的出口，用于失败回报与展示。
    pub egress_entry: Option<crate::api::egress::ProxyEntry>,
    /// 配了同城出口但绑不上（全部冷却/配置坏了）时的错误。
    ///
    /// **必须在发请求前拦住**：一旦放行，这条链路就变成直连，等于把真实 IP
    /// 交给服务端，而调用方还以为自己走了代理。
    egress_error: Option<String>,
}

pub struct RequestOutcome {
    pub http_status: u16,
    pub decrypted: Option<Decrypted>,
    pub raw_len: usize,
    /// 解密失败时的服务端原始返回摘要（诊断用）。
    pub raw_head: String,
}

impl ApiClient {
    pub fn new(identity: HeaderIdentity, login: Option<Session>) -> Self {
        // 配置坏了不能当成「没配」：记下错误，之后每个请求都会被 egress_guard 拦下。
        let (egress, egress_error) = match load_egress_from_disk() {
            Ok(pool) => (pool, None),
            Err(error) => (None, Some(error)),
        };
        let mut client = Self {
            auth_context: None,
            auth_rejected: false,
            #[cfg(test)]
            test_host: None,
            agent: make_agent(),
            session: EnvelopeSession::new(),
            identity,
            login,
            egress,
            egress_entry: None,
            egress_error,
        };
        client.bootstrap_egress();
        client
    }

    /// 构造期就把出口绑好。
    ///
    /// 原来绑定只发生在 `flow::run_full_flow` 一处，于是登录、AI 提交、读记录、
    /// 排行榜、违规自查全部直连真实 IP：同一个账号几分钟内「家宽 → 代理 → 家宽」
    /// 来回跳。一直用一个出口是「一个客户端」，来回跳是「一个人在两地」——
    /// 后者比不配代理更容易被关联。绑定必须在所有请求之前发生。
    fn bootstrap_egress(&mut self) {
        if self.egress.is_none() || self.egress_entry.is_some() {
            return;
        }
        let city = self.identity.city.clone();
        if let Err(error) = self.bind_egress_for_city(&city) {
            // 配了同城出口却绑不上：记下来，请求前拦下，绝不降级直连。
            self.egress_error = Some(error);
        }
    }

    /// 按城市绑定出口，并把代理装到 HTTP agent 上。
    ///
    /// - `Ok(Some(entry))`：已绑定同城出口；
    /// - `Ok(None)`：**没有配置**出口池（`proxies.json` 不存在或为空）—— 直连；
    /// - `Err`：配置坏了 / 没有该城市的节点 / 全部冷却 / 代理无法装载。
    ///   这几种都**不降级直连**：放行就等于把真实 IP 交给服务端，而调用方
    ///   还以为自己走了同城代理。
    pub fn bind_egress_for_city(
        &mut self,
        city: &str,
    ) -> Result<Option<crate::api::egress::ProxyEntry>, String> {
        // 配置阶段就坏了（JSON 解析/协议不支持）：不再尝试，直接沿用错误。
        if let Some(error) = &self.egress_error {
            return Err(error.clone());
        }
        // 幂等：已经绑过就直接返回。重复 acquire 会把游标推走，
        // 让同一次任务在中途换出口 —— 那本身就是可疑信号。
        if let Some(entry) = &self.egress_entry {
            return Ok(Some(entry.clone()));
        }
        let Some(pool) = self.egress.as_mut() else {
            return Ok(None);
        };
        match pool.acquire(city) {
            Ok(entry) => {
                // 代理串必须真正能装进 HTTP 客户端；失败就停，不退化成直连。
                match try_agent_with_proxy(Some(&entry.url)) {
                    Ok(agent) => self.agent = agent,
                    Err(message) => {
                        self.egress_error = Some(message.clone());
                        return Err(message);
                    }
                }
                self.egress_entry = Some(entry.clone());
                self.egress_error = None;
                Ok(Some(entry))
            }
            Err(crate::api::egress::PoolError::NoCandidate) => {
                let message = format!(
                    "出口池里没有 {city} 的可用节点：已拒绝发请求（直连会把真实 IP 暴露给服务端）。\
                     请给该城市补节点，或把设备城市改成已有节点的城市"
                );
                self.egress_error = Some(message.clone());
                Err(message)
            }
            Err(crate::api::egress::PoolError::Exhausted) => {
                let message = "同城出口全部处于冷却中，本次不发请求（避免暴露异地 IP）".to_string();
                self.egress_error = Some(message.clone());
                Err(message)
            }
            Err(crate::api::egress::PoolError::Config(error)) => {
                self.egress_error = Some(error.clone());
                Err(error)
            }
        }
    }

    /// 请求前的出口闸门。
    ///
    /// 绑定失败（全部冷却 / 配置坏了）时**不发请求**：继续发就等于直连，
    /// 而调用方还以在走代理。
    pub fn egress_guard(&self) -> Result<(), String> {
        match &self.egress_error {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }

    /// 回报当前出口的成败，驱动冷却与轮换。
    pub fn report_egress(&mut self, ok: bool) {
        let Some(entry) = self.egress_entry.clone() else {
            return;
        };
        if let Some(pool) = self.egress.as_mut() {
            let outcome = if ok {
                crate::api::egress::Outcome::Ok
            } else {
                crate::api::egress::Outcome::Failed
            };
            pool.report(entry.key(), outcome);
        }
    }

    pub fn with_auth_context(mut self, context: Option<AuthContext>) -> Self {
        self.auth_context = context;
        self
    }

    #[cfg(test)]
    pub(crate) fn set_test_host(&mut self, host: String) {
        assert!(host.starts_with("http://127.0.0.1:"));
        self.test_host = Some(host);
    }

    pub(crate) fn handle_request_error(&mut self, error: crate::http::Error) -> String {
        let authenticated = self.login.as_ref().is_some_and(Session::is_logged_in);
        let error = request_error(error, authenticated);
        if is_session_expired(&error) {
            self.auth_rejected = true;
            if let (Some(context), Some(session)) = (&self.auth_context, &self.login) {
                if !context
                    .rejected
                    .swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    let _ = context.notify.send(AuthFailure {
                        epoch: context.epoch,
                        uid: session.uid,
                        token: session.token.clone(),
                    });
                }
            }
        }
        error
    }

    pub(crate) fn ensure_session_valid(&self) -> Result<(), String> {
        if self.login.as_ref().is_some_and(Session::is_logged_in)
            && (self.auth_rejected
                || self
                    .auth_context
                    .as_ref()
                    .is_some_and(|c| c.rejected.load(std::sync::atomic::Ordering::SeqCst)))
        {
            return Err(SESSION_EXPIRED.into());
        }
        Ok(())
    }

    pub(crate) fn record_url(&self) -> String {
        #[cfg(test)]
        if let Some(host) = &self.test_host {
            return format!(
                "{}{}",
                host.trim_end_matches('/'),
                super::submit::RECORD_PATH
            );
        }
        format!("{}{}", super::model::HOST, super::submit::RECORD_PATH)
    }

    fn uid(&self) -> i64 {
        self.login.as_ref().map(|s| s.uid).unwrap_or(-1)
    }

    fn token(&self) -> String {
        self.login
            .as_ref()
            .map(|s| s.token.clone())
            .unwrap_or_default()
    }

    /// 发送信封请求（header/headerSign 用 observed 序，body 用 insert 序）。
    /// method 为 "POST" 或 "GET"（GET 同样携带信封 body）。
    pub fn envelope_request(
        &mut self,
        method: &str,
        url: &str,
        body_plain: &str,
        ua: &str,
        extra_headers: &[(String, String)],
    ) -> Result<RequestOutcome, String> {
        self.egress_guard()?;
        self.ensure_session_valid()?;
        // UA 只有一个来源。留一个调试期断言，防止以后有人传进来"看起来像"的串，
        // 让外层 appVersion（按 platform 取）与 UA（按参数取）静默错配。
        debug_assert_eq!(
            ua,
            crate::crypto::header::ua_for(&self.identity),
            "envelope_request 的 UA 必须来自 ua_for(&self.identity)"
        );
        let uid = self.uid();
        let token = self.token();
        // headerSign 的附加头（nonce/timeStamp/tokenSign）以前被丢弃，只有提交那一条
        // 请求带出去 —— 同 token 的六七个请求做一次头集合 diff，提交会突兀地多三个
        // 防重放字段。已登录请求统一带上，口径与提交一致。
        let (header_plain, header_extra) = build_header_for(&self.identity, uid, &token, None);
        let authenticated = self.login.as_ref().is_some_and(Session::is_logged_in);
        let header_env = build_envelope(&mut self.session, &header_plain, OuterOrder::Observed);
        // body 与 header 各读一次时钟。以前写死 `header + 1`，服务端把两层都解密、
        // 做一次减法就得到恒定的 1 —— 真机两次读毫秒是「多数相等、偶尔差 1~2」的分布，
        // 恒定 1 反而是人为常量。这里让它们自然相等或相差 1，不做补偿。
        let body_env = build_envelope_ts(
            &mut self.session,
            body_plain,
            OuterOrder::Insert,
            crate::crypto::envelope::now_ms(),
        );

        #[cfg(test)]
        let test_url = self.test_host.as_ref().map(|host| {
            let authority_and_path = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
            let path = authority_and_path
                .find('/')
                .map(|at| &authority_and_path[at..])
                .unwrap_or("/");
            format!("{}{path}", host.trim_end_matches('/'))
        });
        #[cfg(test)]
        let url = test_url.as_deref().unwrap_or(url);
        // 克隆 agent（内部 Arc）：让下面的错误分支仍能可变借用 self 回报出口健康度。
        let agent = self.agent.clone();
        let mut req = agent.request(method, url);
        req = req.set("Content-Type", "application/json; charset=utf-8");
        req = req.set("User-Agent", ua);
        // appVersion 取自身份平台，**不能靠 UA 字符串里有没有 "Android" 判断**：
        // Android 侧的 UA 现在是 OkHttp 默认值（`okhttp/4.12.0`），
        // 里面根本没有 "Android" 字样，按字符串判断会退回 iOS 的 7.3.40。
        req = req.set(
            "appVersion",
            if self.identity.platform == "android" {
                crate::crypto::header::ANDROID_APP_VERSION
            } else {
                crate::crypto::header::IOS_APP_VERSION
            },
        );
        req = req.set("headerSign", &header_env.json);
        if authenticated {
            for (k, v) in &header_extra {
                req = req.set(k, v);
            }
        }
        for (k, v) in extra_headers {
            req = req.set(k, v);
        }
        let resp = req.send_string(&body_env.json).map_err(|e| {
            // 只有传输层失败才算"出口坏了"；拿到 HTTP 响应说明代理是通的。
            let transport = matches!(&e, crate::http::Error::Transport(_));
            self.report_egress(!transport);
            self.handle_request_error(e)
        })?;
        let status = resp.status();
        let raw = resp.into_string().unwrap_or_default();
        let raw_len = raw.len();
        let key = derive_paes_key(
            &body_env.key_data[0],
            &body_env.key_data[1],
            &body_env.key_data[2],
            &body_env.key_data[3],
        );
        let (decrypted, raw_head) = match decrypt_response(raw.as_bytes(), &key, &rsa_public_key())
        {
            Ok(d) => (Some(d), String::new()),
            // 解密失败时这段「原文」会被拼进最终错误串，一路进终端、截图和 CI 日志。
            // 它可能是网关报错页、明文 JSON 或带手机号/令牌的调试输出，
            // 所以必须先过 `redact_snippet`（截断 + 结构化脱敏 + 令牌/手机号打码），
            // 不能直接把原始响应切片塞进去。
            Err(e) => (
                None,
                format!("{e} | 原文: {}", crate::textlog::redact_snippet(&raw, 160)),
            ),
        };
        Ok(RequestOutcome {
            http_status: status,
            decrypted,
            raw_len,
            raw_head,
        })
    }

    /// 标准业务请求（RUN 域名）：返回业务 JSON；error != 10000 时 Err。
    pub fn call(
        &mut self,
        method: &str,
        path: &str,
        body_plain: &str,
        extra_headers: &[(String, String)],
    ) -> Result<serde_json::Value, String> {
        self.call_host(
            crate::api::model::HOST,
            method,
            path,
            body_plain,
            extra_headers,
        )
    }

    /// 指定域名业务请求（排行榜/违规名单走 DISCOVERY，信封链相同）。
    /// 每次调用打印敏感字段打码后的解密响应日志。
    pub fn call_host(
        &mut self,
        host: &str,
        method: &str,
        path: &str,
        body_plain: &str,
        extra_headers: &[(String, String)],
    ) -> Result<serde_json::Value, String> {
        let url = format!("{}{}", host, path);
        // UA 必须与信封同源：信封按 identity.platform 生成（android 是 osType=0 +
        // androidId），UA 却恒为 iOS 时，同一条请求会同时出现「Android 指纹 +
        // iPhone UA + 外层 appVersion 7.3.40 / 信封 7.4.0」三方矛盾。
        let ua = crate::crypto::header::ua_for(&self.identity);
        let out = self.envelope_request(method, &url, body_plain, &ua, extra_headers)?;
        let dec = out.decrypted.ok_or_else(|| {
            format!(
                "HTTP {} 响应解密失败（len={}）原文: {}",
                out.http_status, out.raw_len, out.raw_head
            )
        })?;
        let business = match check_business(&dec.business) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[resp] {} -> Err: {}", path, e);
                return Err(e);
            }
        };
        let biz_str =
            serde_json::to_string(&crate::textlog::redact_json(&business)).unwrap_or_default();
        // 日志是排障用的，不是账号数据的第二份副本：默认只留一小段摘要。
        // 名字、坐标、学号这类字段不在脱敏键表里，整段打印等于把它们写进
        // 终端回滚区、截图和 CI 日志。需要完整报文时用 NSWT_LOG_VERBOSE=1 显式打开。
        if log_verbose() {
            eprintln!(
                "[resp] {} -> {} ({}B) {}",
                path, out.http_status, out.raw_len, biz_str
            );
        } else {
            eprintln!(
                "[resp] {} -> {} ({}B) {}",
                path,
                out.http_status,
                out.raw_len,
                crate::textlog::truncate(&biz_str, RESP_LOG_CHARS)
            );
        }
        Ok(business)
    }
}

/// 响应正文摘要的默认字符上限。
const RESP_LOG_CHARS: usize = 200;

/// 完整报文日志开关（默认关闭）。只认显式设置的值，避免误继承。
fn log_verbose() -> bool {
    matches!(
        std::env::var("NSWT_LOG_VERBOSE").map(|v| v.trim().to_ascii_lowercase()),
        Ok(v) if v == "1" || v == "true" || v == "yes"
    )
}

/// 业务结果检查：error == 10000 → Ok(business)；否则 Err(描述)。
pub fn check_business(v: &serde_json::Value) -> Result<serde_json::Value, String> {
    let err = v.get("error").and_then(|e| e.as_i64()).unwrap_or(0);
    if err == 10000 {
        normalize_business_data(v)
    } else {
        let msg = v
            .get("message")
            .or_else(|| v.get("msg"))
            .and_then(|m| m.as_str())
            .unwrap_or("(无消息)");
        Err(format!("业务错误 {err}: {msg}"))
    }
}

/// Some successful endpoints encode the business data object/array as JSON text.
/// Decode only that known wrapper, retaining ordinary text results unchanged.
pub(crate) fn normalize_business_data(v: &serde_json::Value) -> Result<serde_json::Value, String> {
    let mut normalized = v.clone();
    if let Some(serde_json::Value::String(text)) = v.get("data") {
        let text = text.trim();
        if text.starts_with('{') || text.starts_with('[') {
            if text.len() > 8 * 1024 * 1024 {
                return Err("业务 data JSON 超过 8 MiB".into());
            }
            normalized["data"] =
                serde_json::from_str(text).map_err(|_| "业务 data JSON 格式无效".to_string())?;
        }
    }
    Ok(normalized)
}

/// 业务字段查找：先顶层，再 data 子对象。
pub fn get_field<'a>(biz: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
    if let Some(v) = biz.get(name) {
        if !v.is_null() {
            return Some(v);
        }
    }
    biz.get("data").and_then(|d| d.get(name))
}

/// data 字段如果是 JSON 字符串则解析（Obs 一层包裹语义）。
pub fn parse_data_field(biz: &serde_json::Value) -> serde_json::Value {
    match biz.get("data") {
        Some(serde_json::Value::String(s)) => {
            serde_json::from_str(s).unwrap_or(serde_json::Value::Null)
        }
        Some(v) => v.clone(),
        None => serde_json::Value::Null,
    }
}

pub fn http_err(e: crate::http::Error) -> String {
    match e {
        crate::http::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            // 服务端错误体同样可能回带 token/手机号；能解析就按键打码，再按字符截断。
            let safe = match serde_json::from_str::<serde_json::Value>(&body) {
                Ok(json) => serde_json::to_string(&crate::textlog::redact_json(&json))
                    .unwrap_or_else(|_| body.clone()),
                Err(_) => body.clone(),
            };
            format!(
                "HTTP {code}: {}",
                crate::textlog::redact_text(&crate::textlog::truncate(&safe, 300))
            )
        }
        // Transport 的 Display 会把完整 URL 拼进去，带认证的代理地址会泄露口令。
        crate::http::Error::Transport(t) => {
            format!("网络错误: {}", crate::textlog::redact_text(&t.to_string()))
        }
    }
}

pub const SESSION_EXPIRED: &str = "登录已失效（HTTP 401）：请重新登录获取有效凭证";

pub fn is_session_expired(error: &str) -> bool {
    error.contains(SESSION_EXPIRED)
}

// Only authenticated school API requests may invalidate a school session.
// Public map services and unauthenticated login failures use their own errors.
fn request_error(error: crate::http::Error, authenticated: bool) -> String {
    if authenticated && matches!(&error, crate::http::Error::Status(401, _)) {
        SESSION_EXPIRED.into()
    } else {
        http_err(error)
    }
}

#[cfg(test)]
mod business_data_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn successful_json_text_data_matches_object_and_array_responses() {
        for data in [json!({"geoFences":[],"runAreaId":42}), json!([{"id":1}])] {
            let object = json!({"error":10000,"data":data});
            let encoded = json!({"error":10000,"data":data.to_string()});
            assert_eq!(
                check_business(&encoded).unwrap(),
                check_business(&object).unwrap()
            );
        }
        for data in ["操作成功", "https://example.invalid/file", "null", "42", ""] {
            let response = json!({"error":10000,"data":data});
            assert_eq!(check_business(&response).unwrap(), response);
        }
    }

    #[test]
    fn invalid_or_excessive_json_text_is_not_silently_empty() {
        for data in [
            "{broken-private-payload".to_string(),
            format!("[\"{}\"]", "x".repeat(8 * 1024 * 1024)),
        ] {
            let error = check_business(&json!({"error":10000,"data":data})).unwrap_err();
            assert!(error.contains("data JSON"));
            assert!(!error.contains("private-payload"));
        }
        assert!(check_business(&json!({"error":10603,"data":"{broken"}))
            .unwrap_err()
            .starts_with("业务错误 10603"));
    }
}

#[cfg(test)]
mod auth_tests {
    use super::*;
    fn response_error(code: u16, body: &str) -> crate::http::Error {
        crate::http::Error::Status(
            code,
            crate::http::Response::new(code, "fixture", body).unwrap(),
        )
    }
    #[test]
    fn authenticated_401_requires_login_without_exposing_raw_response() {
        let error = request_error(
            response_error(
                401,
                r#"{"message":"token有误或缓存中token不存在","token":"secret-fixture"}"#,
            ),
            true,
        );
        assert_eq!(error, SESSION_EXPIRED);
        assert!(is_session_expired(&error));
        assert!(!error.contains("secret-fixture"));
    }
    #[test]
    fn login_and_non_auth_http_failures_do_not_expire_a_session() {
        assert!(!is_session_expired(&request_error(
            response_error(401, "login rejected"),
            false
        )));
        for code in [403, 429, 500] {
            assert!(!is_session_expired(&request_error(
                response_error(code, "fixture"),
                true
            )));
        }
    }
    #[test]
    fn chinese_http_errors_are_truncated_on_character_boundaries() {
        let body = format!("a{}", "错误".repeat(100));
        let error = http_err(response_error(500, &body));
        assert!(error.starts_with("HTTP 500: a错误"));
    }
}

#[cfg(test)]
mod shared_auth_tests {
    use super::*;
    #[test]
    fn one_rejection_stops_other_school_reads_and_only_notifies_once() {
        let (tx, rx) = std::sync::mpsc::channel();
        let context = AuthContext {
            epoch: 7,
            rejected: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            notify: tx,
        };
        let session = Session {
            uid: 42,
            token: "test-token".into(),
            ..Default::default()
        };
        let mut first = ApiClient::new(HeaderIdentity::default(), Some(session.clone()))
            .with_auth_context(Some(context.clone()));
        // Exercise the actual request/error path using an isolated loopback HTTP server.
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut stream = loop {
                if let Ok((stream, _)) = listener.accept() {
                    break stream;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "mock HTTP request did not arrive"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            };
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let header_end = loop {
                let mut chunk = [0u8; 4096];
                let n = stream.read(&mut chunk).unwrap();
                assert!(n > 0);
                request.extend_from_slice(&chunk[..n]);
                if let Some(at) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    break at + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]).to_lowercase();
            assert!(headers.contains("headersign:"));
            let length: usize = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            while request.len() < header_end + length {
                let mut chunk = [0u8; 4096];
                let n = stream.read(&mut chunk).unwrap();
                assert!(n > 0);
                request.extend_from_slice(&chunk[..n]);
            }
            let body = r#"{"status":401,"message":"token有误或缓存中token不存在"}"#;
            write!(
                stream,
                "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        assert_eq!(
            first
                .call_host(&host, "POST", crate::api::fence::FENCE_PATH, "{}", &[])
                .unwrap_err(),
            SESSION_EXPIRED
        );
        server.join().unwrap();
        let failure = rx.try_recv().unwrap();
        assert_eq!(failure.epoch, 7);
        assert_eq!(failure.uid, 42);
        for path in [
            crate::api::fence::FENCE_PATH,
            crate::api::cheat::CHEATLIST_PATH,
            "/api/v70230/runnings/records",
            crate::api::user::HOME_PAGE_INFO_PATH,
        ] {
            let mut client = ApiClient::new(HeaderIdentity::default(), Some(session.clone()))
                .with_auth_context(Some(context.clone()));
            // Rejection must happen before contacting this unreachable loopback address.
            assert_eq!(
                client
                    .call_host("http://127.0.0.1:1", "POST", path, "{}", &[])
                    .unwrap_err(),
                SESSION_EXPIRED
            );
        }
        assert!(rx.try_recv().is_err());
    }
}

#[cfg(test)]
mod egress_fail_closed_tests {
    use super::*;

    /// 每个用例独立数据目录：proxies.json 落在 TEST_DATA_DIR 里。
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "neko-egress-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create temp dir");
            crate::platform::TEST_DATA_DIR.with(|cell| *cell.borrow_mut() = Some(dir.clone()));
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            crate::platform::TEST_DATA_DIR.with(|cell| *cell.borrow_mut() = None);
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 写坏的 proxies.json 不能被当成「没配代理」：一旦放过就是真实 IP 直连。
    #[test]
    fn broken_proxy_config_blocks_requests_instead_of_going_direct() {
        let dir = TempDir::new("broken");
        std::fs::write(dir.0.join("proxies.json"), "{ this is not json").unwrap();
        let client = ApiClient::new(HeaderIdentity::default(), None);
        assert!(client.egress.is_none(), "坏配置不应生成出口池");
        let error = client.egress_guard().expect_err("坏配置必须拦住请求");
        assert!(error.contains("出口配置"), "{error}");
        assert!(
            error.contains("拒绝") || error.contains("无法使用"),
            "错误信息要说清为什么不发请求：{error}"
        );
    }

    /// 配了池子但没有当前城市节点：拒绝，而不是静默直连。
    #[test]
    fn missing_city_node_refuses_instead_of_going_direct() {
        let dir = TempDir::new("nocity");
        std::fs::write(
            dir.0.join("proxies.json"),
            r#"[{"url":"socks5://127.0.0.1:1080","city":"北京"}]"#,
        )
        .unwrap();
        let mut identity = HeaderIdentity::default();
        identity.city = "上海".into();
        let client = ApiClient::new(identity, None);
        let error = client.egress_guard().expect_err("没有同城节点必须拦住请求");
        assert!(error.contains("上海"), "{error}");
        assert!(error.contains("拒绝"), "{error}");
    }

    /// 完全没有 proxies.json：保持原行为（直连），不误伤默认用户。
    #[test]
    fn absent_proxy_config_keeps_direct_mode() {
        let _dir = TempDir::new("absent");
        let client = ApiClient::new(HeaderIdentity::default(), None);
        assert!(client.egress.is_none());
        assert!(client.egress_guard().is_ok());
    }
}
