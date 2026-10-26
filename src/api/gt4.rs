//! GT4 滑块验证（api/）。
//!
//! 流程：GET /load（bg/slice/lot_number/payload/process_token/pow_detail）
//!   → 下载两图 → 缺口识别（gt4image）→ PoW（sha256 前导 00）
//!   → get_w（AES-CBC 明文 + RSA 加密 str16 拼接）→ GET /verify → 四凭证。

use rand::Rng;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::client::http_err;
use super::gt4image::get_distance_checked;

const GEE_HOST: &str = "https://gcaptcha4.geetest.com";
const STATIC_HOST: &str = "https://static.geetest.com/";
const UA_WEB: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";

/// jsbn 28bit limb 数组 → RSA 公钥（e=65537）。
fn gee_rsa_public_key() -> rsa::RsaPublicKey {
    const LIMBS: [u32; 37] = [
        134982529, 254232810, 164556709, 234907349, 134685994, 35463984, 258277946, 12518857,
        44638621, 93783641, 212253739, 62792472, 186688352, 109500232, 182488077, 261196188,
        26354094, 103248217, 106891695, 165771045, 41530993, 263704736, 111785174, 12753611,
        232116673, 155524985, 218291229, 122452343, 248250238, 118739550, 251169095, 129059733,
        149835464, 5498868, 71719731, 154456417, 49635,
    ];
    let dv = rsa::BigUint::from(1u8) << 28usize;
    let mut n = rsa::BigUint::from(0u8);
    for limb in LIMBS.iter().rev() {
        n = n * &dv + rsa::BigUint::from(*limb);
    }
    rsa::RsaPublicKey::new(n, rsa::BigUint::from(65537u32)).expect("GT4 RSA 公钥构造失败")
}

/// GT4 求解失败的原因。区分它们是为了**决定要不要重试**：
///
/// - `Unreliable`：这一张图没认准（或图没取到）。换一张图的成本是再 /load 一次，
///   不会交错误答案，重试是划算的。
/// - `Rejected`：极验明确判错（`/verify` 返回 fail）。这时**绝不能重试** ——
///   每重试一次就是再确认一次「这是个不会动的客户端」，实测同一出口连错会把
///   该会话的容忍度打掉（12 轮 11 对 → 之后 2/16、0/8）。
/// - `Fatal`：网络/格式问题，重试一次没坏处，但也不该无限刷。
#[derive(Debug, Clone)]
pub enum Gt4Error {
    Unreliable(String),
    Rejected(String),
    Fatal(String),
}

impl Gt4Error {
    pub fn message(&self) -> &str {
        match self {
            Self::Unreliable(m) | Self::Rejected(m) | Self::Fatal(m) => m,
        }
    }

    /// 是否值得换一张图再试一次。
    pub fn retryable(&self) -> bool {
        !matches!(self, Self::Rejected(_))
    }
}

impl std::fmt::Display for Gt4Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

/// 4 个随机 hex 字符。
fn four_random_chart(rng: &mut impl Rng) -> String {
    let v = (65536.0 * (1.0 + rng.r#gen::<f64>())) as u32;
    let s = format!("{v:x}");
    s[1..5].to_string()
}

/// 16 个随机 hex 字符。
fn get_str_16(rng: &mut impl Rng) -> String {
    (0..4).map(|_| four_random_chart(rng)).collect()
}

/// 找 nonce（16 个随机 hex 字符）使 sha256(base+nonce) 以 "00" 开头。
fn get_pow_msg_str_16(pow_msg_base: &str, log: &mut dyn FnMut(&str)) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut rng = rand::thread_rng();
    let mut attempts = 0u64;
    loop {
        let nonce: String = (0..16)
            .map(|_| HEX[rng.gen_range(0..16)] as char)
            .collect();
        let mut hasher = Sha256::new();
        hasher.update(pow_msg_base.as_bytes());
        hasher.update(nonce.as_bytes());
        let hash = format!("{:x}", hasher.finalize());
        attempts += 1;
        if hash.starts_with("00") {
            log(&format!("[gt4] PoW 找到 nonce（尝试 {attempts} 次）"));
            return nonce;
        }
    }
}

/// pow_msg = version|bits|hashfunc|datetime|captchaId|lot||nonce。
fn get_pow_msg(
    pow_detail: &Value,
    captcha_id: &str,
    lot_number: &str,
    log: &mut dyn FnMut(&str),
) -> String {
    let base = format!(
        "{}|{}|{}|{}|{}|{}||",
        pow_detail["version"].as_str().unwrap_or(""),
        pow_detail["bits"],
        pow_detail["hashfunc"].as_str().unwrap_or(""),
        pow_detail["datetime"].as_str().unwrap_or(""),
        captcha_id,
        lot_number
    );
    let nonce = get_pow_msg_str_16(&base, log);
    format!("{base}{nonce}")
}

/// pow_sign = sha256(pow_msg) hex。
fn get_pow_sign(pow_msg: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(pow_msg.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// AES-128-CBC(key=str16, IV 为 ASCII "0"x16, PKCS7) → hex。
fn aes_o(plaintext: &str, str_16: &str) -> String {
    let ct = crate::crypto::envelope::aes128_cbc_encrypt_iv(
        str_16.as_bytes(),
        b"0000000000000000",
        plaintext.as_bytes(),
    )
    .expect("GT4 AES 加密失败");
    bytes_to_hex(&ct)
}

fn bytes_to_hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

/// RSA PKCS1v15 加密 → hex。
fn encrypt_data(pub_key: &rsa::RsaPublicKey, plaintext: &str) -> String {
    let mut rng = rand::thread_rng();
    let ct = pub_key
        .encrypt(&mut rng, rsa::Pkcs1v15Encrypt, plaintext.as_bytes())
        .expect("GT4 RSA 加密失败");
    bytes_to_hex(&ct)
}

/// w = hex(AES(plaintext)) + hex(RSA(str16))。
///
/// `passtime` 是「拖动耗时」，必须是本次的真实耗时而不是常量：写死 1887 时，
/// 所有账号、所有次数的取值完全一致，是一条零成本特征。真实用户从看图到拖完
/// 是秒级，所以调用方会把实际 elapsed 传进来并夹到人的区间内。
fn get_w(
    set_left: usize,
    passtime: u64,
    lot_number: &str,
    pow_msg: &str,
    pow_sign: &str,
    str16: &str,
) -> String {
    let pk = gee_rsa_public_key();
    let userresponse = set_left as f64 / 1.005_946_666_666_666_5 + 2.0;
    // 直接切片会在 lot_number 短于 30 字符时 panic。极验正常给 32 位，但
    // 「服务端少给几位」不该把整个登录线程打崩 —— 取不到就留空，服务端自会拒绝。
    let lf = |a: usize, b: usize| lot_number.get(a..b).unwrap_or("").to_string();
    let plaintext = format!(
        "{{\"setLeft\":{},\"passtime\":{},\"userresponse\":{},\"device_id\":\"\",\"lot_number\":\"{}\",\"pow_msg\":\"{}\",\"pow_sign\":\"{}\",\"geetest\":\"captcha\",\"lang\":\"zh\",\"ep\":\"123\",\"biht\":\"1426265548\",\"gee_guard\":{{\"roe\":{{\"aup\":\"3\",\"sep\":\"3\",\"egp\":\"3\",\"auh\":\"3\",\"rew\":\"3\",\"snh\":\"3\",\"res\":\"3\",\"cdc\":\"3\"}}}},\"YciC\":\"P3Vn\",\"{}\":\"{}\",\"em\":{{\"ph\":0,\"cp\":0,\"ek\":\"11\",\"wd\":1,\"nt\":0,\"si\":0,\"sc\":0}}}}",
        set_left,
        passtime,
        userresponse,
        lot_number,
        pow_msg,
        pow_sign,
        lf(26, 30) + &lf(7, 11),
        lf(6, 14),
    );
    let r = encrypt_data(&pk, str16);
    let i = aes_o(&plaintext, str16);
    i + &r
}

fn jsonp_value(text: &str) -> Result<Value, String> {
    let start = text.find('(').ok_or("JSONP 缺左括号")?;
    let end = text.rfind(')').ok_or("JSONP 缺右括号")?;
    if end <= start {
        return Err("JSONP 格式错误".into());
    }
    serde_json::from_str(&text[start + 1..end]).map_err(|e| format!("JSONP JSON 解析失败: {e}"))
}

/// 完整 GT4 求解。返回 {lotNumber, captchaOutput, passToken, genTime}。
///
/// 重试策略按**失败原因**分域（实测同一出口连错会把极验对该会话的容忍度打掉：
/// 12 轮里 11 对 → 之后 2/16、0/8）：
/// - 图没认准 / 网络抖动：最多 2 张图，间隔 3–6s；
/// - 极验明确判错：立刻停手，不重试（重试只是再确认一次「客户端不会动」）。
pub fn solve_gt4(
    captcha_id: &str,
    client: &mut super::client::ApiClient,
    log: &mut dyn FnMut(&str),
) -> Result<Value, Gt4Error> {
    let mut last: Option<Gt4Error> = None;
    for attempt in 1..=2 {
        match solve_once(captcha_id, client, log) {
            Ok(v) => return Ok(v),
            Err(e) => {
                log(&format!("× [gt4] 第 {attempt} 次失败: {e}"));
                if !e.retryable() {
                    return Err(e);
                }
                if attempt < 2 {
                    let gap = rand::thread_rng().gen_range(3u64..6);
                    log(&format!("[gt4] {gap}s 后换一张图重试"));
                    std::thread::sleep(std::time::Duration::from_secs(gap));
                }
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| Gt4Error::Fatal("GT4 未执行".into())))
}

/// 解析 `/load` 的 `ypos`。
///
/// 极验给的是整数，但字段类型变更（浮点/字符串）会让 `as_u64()` 静默返回 None，
/// 一致性校验也就静默失效 —— 这里统一收口，并在调用处把「解析不了」记进日志。
fn parse_ypos(value: &Value) -> Option<usize> {
    value
        .as_u64()
        .or_else(|| value.as_f64().filter(|v| *v >= 0.0).map(|v| v as u64))
        .or_else(|| value.as_str().and_then(|text| text.trim().parse::<u64>().ok()))
        .map(|v| v as usize)
}

fn solve_once(
    captcha_id: &str,
    client: &mut super::client::ApiClient,
    log: &mut dyn FnMut(&str),
) -> Result<Value, Gt4Error> {
    // 从 /load 到 /verify 的真实耗时，就是「人看图 + 拖动」的时间。
    let started = std::time::Instant::now();
    // 复用调用方那只 agent（带同城出口）。极验侧能同时看到「验证码来自 IP A」
    // 和「App 业务请求来自 IP B」，两次出口不一致本身就是一条免费的关联信号。
    let agent = client.agent.clone();
    let challenge = uuid::Uuid::new_v4().to_string().replace('-', "");
    let callback = format!("geetest_{}", crate::crypto::envelope::now_ms());
    let load_url = format!(
        "{GEE_HOST}/load?callback={cb}&captcha_id={cid}&challenge={ch}&client_type=web&risk_type=slide&lang=zh",
        cb = callback,
        cid = captcha_id,
        ch = challenge
    );
    let resp = agent
        .get(&load_url)
        .set("User-Agent", UA_WEB)
        .call()
        .map_err(|e| Gt4Error::Fatal(http_err(e)))?;
    let text = resp
        .into_string()
        .map_err(|e| Gt4Error::Fatal(e.to_string()))?;
    let root = jsonp_value(&text).map_err(Gt4Error::Fatal)?;
    let data = root["data"]
        .as_object()
        .ok_or_else(|| Gt4Error::Fatal("GT4 /load 缺 data".into()))?
        .clone();
    let field = |key: &str| -> Result<String, Gt4Error> {
        data[key]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| Gt4Error::Fatal(format!("GT4 /load 缺 {key}")))
    };
    let bg = field("bg")?;
    let slice = field("slice")?;
    let lot_number = field("lot_number")?;
    let payload = field("payload")?;
    let process_token = field("process_token")?;
    let pow_detail = data["pow_detail"].clone();

    let bg_png = download(&agent, &format!("{STATIC_HOST}{bg}"))?;
    let slice_png = download(&agent, &format!("{STATIC_HOST}{slice}"))?;

    // 取图之后再等：真实的顺序是「拿到图 → 人看图（1–2s）→ 拖动（0.2–0.4s）→ 交卷」。
    // 把延迟放在取图之前，可观测形态会变成「图到手 200ms 就交卷」，反而更假。
    let look_ms = rand::thread_rng().gen_range(900u64..1900);
    std::thread::sleep(std::time::Duration::from_millis(look_ms));

    // /load 的 ypos 用来校验识别结果：y 偏离说明这一张配到了背景里别的结构，
    // 此时直接换图重试，而不是交一个错答案（错答案会消耗极验对本会话的容忍度）。
    let ypos = parse_ypos(&data["ypos"]);
    if ypos.is_none() && !data["ypos"].is_null() {
        log(&format!(
            "[gt4] ypos 无法解析（{}），本次跳过一致性校验",
            data["ypos"]
        ));
    }
    let (dist, score) = get_distance_checked(&bg_png, &slice_png, ypos)
        .map_err(Gt4Error::Unreliable)?;
    log(&format!(
        "[gt4] 缺口识别 distance={dist} 相似度={score:.3} ypos={:?}",
        ypos
    ));

    // 拖动本身也要时间。
    let drag_ms = rand::thread_rng().gen_range(120u64..380);
    std::thread::sleep(std::time::Duration::from_millis(drag_ms));

    let pow_msg = get_pow_msg(&pow_detail, captcha_id, &lot_number, log);
    let pow_sign = get_pow_sign(&pow_msg);
    let str16 = get_str_16(&mut rand::thread_rng());
    // 拖动耗时取真实 elapsed —— 服务端知道 lot 的签发时刻，能框住这个值；
    // 常量（原来的 1887）在所有账号所有次数上完全一致，本身就是特征。
    let passtime = started.elapsed().as_millis() as u64;
    let w = get_w(dist, passtime, &lot_number, &pow_msg, &pow_sign, &str16);

    // verify
    let cb = format!("geetest_{}", crate::crypto::envelope::now_ms());
    let verify_url = format!(
        "{GEE_HOST}/verify?callback={cb}&captcha_id={cid}&client_type=web&lot_number={lot}&risk_type=slide&payload={pl}&process_token={pt}&payload_protocol=1&pt=1&w={w}",
        cid = captcha_id,
        lot = urlencode(&lot_number),
        pl = urlencode(&payload),
        pt = urlencode(&process_token),
        w = urlencode(&w),
    );
    let resp = agent
        .get(&verify_url)
        .set("User-Agent", UA_WEB)
        .call()
        .map_err(|e| Gt4Error::Fatal(http_err(e)))?;
    let text = resp
        .into_string()
        .map_err(|e| Gt4Error::Fatal(e.to_string()))?;
    let root = jsonp_value(&text).map_err(Gt4Error::Fatal)?;
    let result = root["data"]["result"].as_str().unwrap_or("");
    if root["status"].as_str() != Some("success") || result != "success" {
        // 判错：不重试。再打一次只是再消耗一次容忍度。
        return Err(Gt4Error::Rejected(format!(
            "极验判定未通过（result={result:?}）"
        )));
    }
    let seccode = &root["data"]["seccode"];
    Ok(json!({
        "lotNumber": seccode["lot_number"],
        "captchaOutput": seccode["captcha_output"],
        "passToken": seccode["pass_token"],
        "genTime": seccode["gen_time"],
    }))
}

fn download(agent: &crate::http::Agent, url: &str) -> Result<Vec<u8>, Gt4Error> {
    let resp = agent
        .get(url)
        .set("User-Agent", UA_WEB)
        .call()
        .map_err(|e| Gt4Error::Fatal(http_err(e)))?;
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut resp.into_reader(), &mut buf)
        .map_err(|e| Gt4Error::Fatal(e.to_string()))?;
    Ok(buf)
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 仅探针使用（生产路径不需要截断响应体）。
#[cfg(test)]
fn truncate(s: &str, n: usize) -> String {
    crate::textlog::truncate(s, n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::traits::PublicKeyParts;

    /// 联网探针（默认 ignore）：连跑 N 轮 GT4，统计成功率与失败原因。
    ///
    /// **会真实请求极验**，所以必须显式给出 captcha_id 才跑 —— 用生产 captcha_id
    /// 密集打点本身就会把该出口的容忍度打掉，让之后的实测数据无法归因：
    ///
    /// ```text
    /// GT4_PROBE_CAPTCHA_ID=<id> [GT4_PROBE_ROUNDS=3] [GT4_PROXY=socks5h://...] \
    ///   cargo test --lib live_gt4_probe -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "live network probe（会真实请求极验，需显式给 captcha_id）"]
    fn live_gt4_probe() {
        let Ok(cid) = std::env::var("GT4_PROBE_CAPTCHA_ID") else {
            println!("[probe] 未设置 GT4_PROBE_CAPTCHA_ID，跳过（探针会真实请求极验）");
            return;
        };
        let rounds: usize = std::env::var("GT4_PROBE_ROUNDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3);
        let mut ok = 0usize;
        let mut fails: Vec<String> = Vec::new();
        for i in 0..rounds {
            match probe_round(&cid) {
                Ok((dist, score)) => {
                    ok += 1;
                    println!("[probe {}/{rounds}] ok distance={dist} score={score}", i + 1);
                }
                Err(e) => {
                    println!("[probe {}/{rounds}] FAIL {e}", i + 1);
                    fails.push(e);
                }
            }
            // 轮间隔 5–10s：连续打点会把出口状态打坏，也让数据不可信。
            std::thread::sleep(std::time::Duration::from_millis(
                rand::thread_rng().gen_range(5_000u64..10_000),
            ));
        }
        println!(
            "[probe] 成功 {ok}/{rounds}；失败样本 {:#?}",
            fails.iter().take(10).collect::<Vec<_>>()
        );
    }

    /// 单轮：/load → 识别 → /verify，返回 (distance, score)。
    fn probe_round(cid: &str) -> Result<(usize, String), String> {
        // 出口可切：用 GT4_PROXY 换 IP 做对照。
        let proxy = std::env::var("GT4_PROXY").ok();
        let agent =
            crate::http::Agent::new(std::time::Duration::from_secs(30), proxy.as_deref())?;
        let challenge = uuid::Uuid::new_v4().to_string().replace('-', "");
        let cb = format!("geetest_{}", crate::crypto::envelope::now_ms());
        let load_url = format!(
            "{GEE_HOST}/load?callback={cb}&captcha_id={cid}&challenge={ch}&client_type=web&risk_type=slide&lang=zh",
            cb = cb,
            cid = cid,
            ch = challenge
        );
        let resp = agent
            .get(&load_url)
            .set("User-Agent", UA_WEB)
            .call()
            .map_err(|e| format!("/load 请求失败: {e}"))?;
        let text = resp.into_string().map_err(|e| format!("/load 读取失败: {e}"))?;
        let root = jsonp_value(&text)?;
        let data = &root["data"];
        if root["status"].as_str() != Some("success") {
            return Err(format!("/load 未成功: {}", truncate(&text, 300)));
        }
        let bg = data["bg"].as_str().unwrap_or_default().to_string();
        let slice = data["slice"].as_str().unwrap_or_default().to_string();
        let lot = data["lot_number"].as_str().unwrap_or_default().to_string();
        let payload = data["payload"].as_str().unwrap_or_default().to_string();
        let ptoken = data["process_token"].as_str().unwrap_or_default().to_string();
        let pow_detail = data["pow_detail"].clone();

        let bg_png = download(&agent, &format!("{STATIC_HOST}{bg}")).map_err(|e| e.to_string())?;
        let slice_png =
            download(&agent, &format!("{STATIC_HOST}{slice}")).map_err(|e| e.to_string())?;
        let ypos = parse_ypos(&data["ypos"]);

        // 诊断：切片图尺寸与不透明区域（模板裁剪与 ypos 语义核对用）
        let (sl_gray0, sw, sh) = super::super::gt4image::decode_gray(&slice_png)?;
        let opaque = sl_gray0.iter().filter(|&&v| v > 0).count();
        println!("[diag] slice={sw}x{sh} 非黑像素={opaque} ypos={ypos:?}");

        let (dist, score) = get_distance_checked(&bg_png, &slice_png, ypos)?;
        println!("[diag] 识别 distance={dist} 相似度={score:.4}");

        // 落盘：bg + 标注图（红=识别结果），人工核对缺口位置用
        if let Ok(dir) = std::env::var("GT4_PROBE_DUMP") {
            let _ = std::fs::create_dir_all(&dir);
            let stamp = format!("{:x}", crate::crypto::envelope::now_ms());
            let _ = std::fs::write(format!("{dir}\\{stamp}-bg.png"), &bg_png);
            let _ = std::fs::write(format!("{dir}\\{stamp}-slice.png"), &slice_png);
            if let Ok(mut canvas) = image::load_from_memory(&bg_png).map(|i| i.to_rgba8()) {
                let h = canvas.height();
                if dist < canvas.width() as usize {
                    for y in 0..h {
                        canvas.put_pixel(dist as u32, y, image::Rgba([255u8, 0, 0, 255]));
                    }
                }
                let _ = canvas.save(format!("{dir}\\{stamp}-mark.png"));
            }
            println!("[diag] 已落盘 {dir}\\{stamp}-*.png");
        }

        let mut logf = |_s: &str| {};
        let pow_msg = get_pow_msg(&pow_detail, cid, &lot, &mut logf);
        let pow_sign = get_pow_sign(&pow_msg);
        let str16 = get_str_16(&mut rand::thread_rng());
        let w = get_w(dist, 1500, &lot, &pow_msg, &pow_sign, &str16);
        let cb2 = format!("geetest_{}", crate::crypto::envelope::now_ms());
        let verify_url = format!(
            "{GEE_HOST}/verify?callback={cb}&captcha_id={cid}&client_type=web&lot_number={lot}&risk_type=slide&payload={pl}&process_token={pt}&payload_protocol=1&pt=1&w={w}",
            cb = cb2,
            cid = cid,
            lot = urlencode(&lot),
            pl = urlencode(&payload),
            pt = urlencode(&ptoken),
            w = urlencode(&w),
        );
        let resp = agent
            .get(&verify_url)
            .set("User-Agent", UA_WEB)
            .call()
            .map_err(|e| format!("/verify 请求失败: {e}"))?;
        let text = resp.into_string().map_err(|e| format!("/verify 读取失败: {e}"))?;
        let root = jsonp_value(&text)?;
        let result = root["data"]["result"].as_str().unwrap_or("").to_string();
        let score = root["data"]["score"].as_str().unwrap_or("").to_string();
        if root["status"].as_str() != Some("success") || result != "success" {
            return Err(format!(
                "distance={dist} slice={sw}x{sh} ypos={ypos:?} body={}",
                truncate(&text, 400)
            ));
        }
        Ok((dist, score))
    }

    /// web 端 key：t=37 limbs ≈ 1036bit。
    #[test]
    fn test_gee_rsa_key_shape() {
        let pk = gee_rsa_public_key();
        assert_eq!(pk.e().to_bytes_be(), vec![1, 0, 1]); // 65537
        assert!(pk.size() >= 128, "GT4 key size={}", pk.size());
        // get_str_16 长度
        let s = get_str_16(&mut rand::thread_rng());
        assert_eq!(s.len(), 16);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// get_pow_msg 拼接 + pow_sign。
    #[test]
    fn test_pow_msg_and_sign() {
        let detail = json!({
            "version": "v1", "bits": 5, "hashfunc": "sha256",
            "datetime": "2026-09-09T00:00:00"
        });
        let mut logf = |_s: &str| {};
        let msg = get_pow_msg(&detail, "cid123", "lot456", &mut logf);
        assert!(msg.starts_with("v1|5|sha256|2026-09-09T00:00:00|cid123|lot456||"));
        assert_eq!(msg.len() - msg.rfind('|').unwrap() - 1, 16);
        let sign = get_pow_sign(&msg);
        assert_eq!(sign.len(), 64);
        // 自洽：sign == sha256(msg)
        let mut h = Sha256::new();
        h.update(msg.as_bytes());
        assert_eq!(format!("{:x}", h.finalize()), sign);
    }

    /// w 参数：格式 = aes_hex + rsa_hex，长度 = 32 字节块 + key 块。
    #[test]
    fn test_get_w_shape() {
        let lot = "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6";
        let w = get_w(123, 1500, lot, "pow|msg||nonce1234567890ab", "sign", "0123456789abcdef");
        // AES 输出 ≥32 hex + RSA 130 字节 = 260 hex（key≈1036bit）
        assert!(w.len() > 260, "w len={}", w.len());
        assert!(w.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
