//! OBS 上传：POST /api/obs/temporary/url 换签名 URL → PUT JSON。

use super::client::{get_field, parse_data_field, ApiClient};
use serde_json::{json, Value};

pub const OBS_SIGN_PATH: &str = "/api/obs/temporary/url";

/// 换取签名 URL。
pub fn sign_url(client: &mut ApiClient, method: &str, key: &str) -> Result<String, String> {
    let body = json!({
        "bucketName": "iydsj-hbase-hot",
        "objectKey": key,
        "method": method,
        "contentType": "application/json",
    })
    .to_string();
    let biz = client.call("POST", OBS_SIGN_PATH, &body, &[])?;
    // business.data 可能是 JSON 字符串 {"signedUrl": ...}
    let data = parse_data_field(&biz);
    let signed = data
        .get("signedUrl")
        .and_then(|v| v.as_str())
        .or_else(|| get_field(&biz, "signedUrl").and_then(|v| v.as_str()))
        .ok_or("OBS 签名响应缺 signedUrl")?;
    Ok(signed.to_string())
}

/// PUT 上传 OBS 对象。
///
/// 两个参数都是**必须显式传入**的，因为它们各自堵住过一类真实缺陷：
/// - `agent`：必须用调用方那只（带同城出口代理）。以前这里自己 `make_agent()`，
///   配了出口池时 OBS 直传会从家宽直连 —— 唯一携带轨迹数据的那一步换了出口 IP，
///   正好是「同一账号几分钟内 IP 跳变」里最难解释的一次。
/// - `ua`：不设就是底层 HTTP 库的默认 UA（曾实测发出 `ureq/2.12.1`），等于自报"我是脚本"。
pub fn put_object(
    agent: &crate::http::Agent,
    signed_url: &str,
    ua: &str,
    payload: &[u8],
    log: &mut dyn FnMut(&str),
) -> Result<(), String> {
    let resp = agent
        .put(signed_url)
        .set("Content-Type", "application/json")
        .set("User-Agent", ua)
        .send_bytes(payload)
        .map_err(super::client::http_err)?;
    log(&format!(
        "[obs] PUT {} -> {}",
        shorten(signed_url),
        resp.status()
    ));
    Ok(())
}

/// 上传到两个 objectKey（详情页 + 兜底路径）。
pub fn upload_both_keys(
    client: &mut ApiClient,
    keys: &[String],
    payload: &[u8],
    log: &mut dyn FnMut(&str),
) -> usize {
    let mut ok = 0;
    // 出口闸门：代理配置坏了/没有同城节点时**不要**拿直连 agent 上传轨迹，
    // 宁可让这一步明确失败（调用方会看到 OBS 0/2）。
    if let Err(error) = client.egress_guard() {
        log(&format!("[obs] 跳过上传：{error}"));
        return 0;
    }
    // OBS 直传是这条链上唯一上传轨迹数据的步骤：UA 与**出口**都必须与业务请求同源。
    // 这里克隆 client 那只 agent（内部 Arc，克隆很便宜），它已经带上同城出口代理。
    let ua = crate::crypto::header::ua_for(&client.identity);
    let agent = client.agent.clone();
    for key in keys {
        match sign_url(client, "Put", key)
            .and_then(|url| put_object(&agent, &url, &ua, payload, log))
        {
            Ok(()) => ok += 1,
            Err(e) => log(&format!("[obs] PUT {key} 失败: {e}")),
        }
    }
    ok
}

fn shorten(url: &str) -> &str {
    // 只显示 objectKey 部分，避免日志过长
    let start = url.find("run_data").unwrap_or(0);
    let end = (start + 60).min(url.len());
    &url[start..end]
}

/// 回读验证（GET 签名）。
#[allow(dead_code)]
pub fn fetch_object(
    client: &mut ApiClient,
    key: &str,
    log: &mut dyn FnMut(&str),
) -> Result<Value, String> {
    let signed = sign_url(client, "Get", key)?;
    let ua = crate::crypto::header::ua_for(&client.identity);
    // 必须复用业务 client 的 agent（带同城出口）：以前这里自己 make_agent()，
    // 一旦回读被接进主流程，就会出现「上传走代理、回读走家宽」的出口跳变。
    let agent = client.agent.clone();
    let resp = agent
        .get(&signed)
        .set("User-Agent", &ua)
        .call()
        .map_err(super::client::http_err)?;
    let text = resp.into_string().unwrap_or_default();
    log(&format!("[obs] GET 回读 {} 字节", text.len()));
    serde_json::from_str(&text).map_err(|e| format!("OBS 对象解析失败: {e}"))
}
