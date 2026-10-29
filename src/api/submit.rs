//! 跑步提交：POST /api/v70260/runnings/save/record。
//! Android 身份 + runes/runef 头；body 31+ 字段 + signature/originalSign。

use super::client::{check_business, ApiClient};
use crate::crypto::decrypt::{decrypt_response, derive_paes_key};
use crate::crypto::envelope::{build_envelope, build_envelope_ts, rsa_public_key, OuterOrder};
use crate::crypto::header::build_android_header;
use crate::crypto::sign::{original_sign, signature};
use crate::track::calorie::{avg_power, official_kcal};
use crate::track::geom::round_to;
use crate::track::model::{GenPoint, Track};
use crate::track::wire::validate_five_point_wrapper;
use serde_json::{Map, Value};

pub const RECORD_PATH: &str = "/api/v70260/runnings/save/record";

/// `unCompleteReason` 的「里程未达标」。
///
/// 真机样本里只见过 `complete=true, unCompleteReason=0` 这一种组合，
/// 具体枚举值没有可观测依据；这里取一个非 0 值只为与 `complete=false` 自洽
/// （0 = 无原因，和 false 放在一起才是矛盾）。真要精确对齐官方客户端，
/// 需要一份「未达标记录」的抓包。
const UNCOMPLETE_REASON_DISTANCE: i64 = 1;

/// C3：`complete` / `unCompleteReason` 由**实跑距离**与**学校门槛**共同决定。
///
/// 返回 `(complete, unCompleteReason)`：
/// - 达到门槛 → `(true, 0)`；
/// - 未达到 → `(false, UNCOMPLETE_REASON_DISTANCE)`（false 配 0 才是自相矛盾）；
/// - 门槛未知（≤0）→ 按完成处理，不虚构一个「未达标」。
pub fn completion_flag(total_dis_m: i64, min_distance_m: i64) -> (bool, i64) {
    if min_distance_m <= 0 || total_dis_m >= min_distance_m {
        (true, 0)
    } else {
        (false, UNCOMPLETE_REASON_DISTANCE)
    }
}

/// Record pace uses thousandths of a minute/km; OBS laps use seconds/km.
fn pace_speed_value(seconds: i64, meters: f64) -> i64 {
    let distance = (meters * 100.0).ceil() / 100.0;
    (seconds as f64 / 60.0 / (distance / 1000.0) * 1000.0).round() as i64
}

/// 回执里记的「本次提交距离」：必须是**实跑距离**，不能是学校门槛。
///
/// `Attempt::matches()` 拿它跟记录列表里的 `total_dis` 比，容差只有 5 m。
/// 传门槛会让「实跑 2300 / 门槛 2000」的对账**必然**判「未找到」→ 标 Abandoned
/// → 提示允许重提 —— 恰好制造出 WAL 本来要防的「同一天两条记录」（0.9.0 复审 N1）。
pub(crate) fn receipt_target_distance(track_total_m: f64) -> i64 {
    track_total_m.round() as i64
}

/// 提交体的 10s 窗口**必须与 OBS 对象同一套 id/queueNum**。
///
/// 以前这里把 `id` 改写成 `60000 + i`，而 OBS 的 `speed_json` 用的是
/// `build_windows` 的口径 —— 同一次跑步的两份报文对同一个窗口报了不同的 id，
/// 服务端把 `speedPerTenSec` 与 OBS 的 `speed_json` 对一遍就是硬矛盾。
/// 现在两侧都直接取 `wire::build_windows`，不再有第二套编号。
fn submit_windows(track: &Track) -> (Vec<Value>, Vec<Value>) {
    crate::track::wire::build_windows(track)
}

/// bdA 正差分累计。
pub fn total_ascent(locs: &[GenPoint]) -> f64 {
    let mut ascent = 0.0;
    for i in 1..locs.len() {
        let d = locs[i].bdA - locs[i - 1].bdA;
        if d > 0.0 {
            ascent += d;
        }
    }
    ascent
}

pub struct SubmitParams {
    pub track: Track,
    pub uid: i64,
    pub selected_unid: i64,
    pub policy: i64,
    pub policy_ts: i64,
    pub min_distance: i64,
    pub weight: f64,
    pub face_check: i64,
    pub five_point_json: String,
    pub address: String,
}

#[allow(dead_code)]
pub struct SubmitResult {
    pub rrid: i64,
    pub uuid: String,
    pub start_ms: i64,
    pub complete: Option<bool>,
    pub total_dis: f64,
    pub total_time: i64,
    pub total_steps: i64,
    pub avg_step_freq: i64,
    pub calorie: i64,
    pub avg_power: i64,
    pub sel_distance: i64,
}

/// 提交跑步记录（sportType=1 自由跑 + 实时五点）。
pub fn submit_record(
    client: &mut ApiClient,
    p: &SubmitParams,
    log: &mut dyn FnMut(&str),
) -> Result<SubmitResult, String> {
    // A1（审计 high）：身份平台必须与提交链路一致。
    //
    // 提交报文无条件按 **Android** 身份构造：清 pixel/idfa、`platform="android"`、
    // 机型/系统版本改写成 Android 机型库、UA/appVersion 换成 Android（7.4.0、osType=0）。
    // 但 `DeviceId` 沿用的是当前身份那一个 —— iOS 身份登录 + Android 报文提交，就是
    // 同一条 token 上「登录说 iOS、提交说 Android」的硬矛盾（真机不会在会话中途换平台）。
    //
    // 让整条链跟随 iOS 属于未验证链路（iOS 的提交/OBS/详情口径没有样本），所以这里只做
    // 准入：在**写回执、发请求之前**拒绝，用户到设备页切成 Android 即可。
    if client.identity.platform != "android" {
        let platform = match client.identity.platform.as_str() {
            "ios" => "iOS",
            "" => "未设置",
            other => other,
        };
        return Err(format!(
            "当前设备身份为 {platform}，官方客户端只有 Android；\
             请到设备页切换为 Android 身份后再提交（否则同一条登录里登录与提交分属两个平台）"
        ));
    }
    // 出口闸门必须在写 pending 回执之前：绑不上出口是**本地拒绝、请求没发出**，
    // 若走下面的 Unknown 分支就会留下一条 pending，把后续提交全部锁死。
    client.egress_guard()?;
    client.ensure_session_valid()?;
    let session = client.login.as_ref().ok_or("未登录")?;
    if session.uid != p.uid || session.unid.parse::<i64>().ok() != Some(p.selected_unid) {
        return Err("提交账号或学校与当前会话不一致".into());
    }
    p.track.validate_consistency()?;
    validate_five_point_wrapper(&p.five_point_json)?;
    let stop_ms = p
        .track
        .totalTime
        .checked_mul(1000)
        .and_then(|n| p.track.startTime.checked_add(n))
        .ok_or("时间范围溢出")?;
    let mut guard = super::submission_guard::Guard::open(p.uid, p.selected_unid)?;
    // 先只读对账、再进闸门：Unknown 的回执只有「记录列表里确实没有」才允许重提，
    // 拉不到列表就保持未决（fail-closed），绝不靠猜。
    let now = crate::crypto::envelope::now_ms();
    super::submission_guard::reconcile_if_needed(&mut guard, client, now, log)?;
    let run_uuid = uuid::Uuid::new_v4().to_string().to_uppercase();
    let plan_fp = super::submission_guard::plan_fingerprint(
        p.track.startTime,
        p.track.totalTime,
        p.track.totalDistance,
        p.track.totalSteps,
        p.min_distance,
    );
    // 回执在**发请求之前**落盘：从这里往下，服务端可能已经收到这条记录。
    let at = crate::crypto::envelope::now_ms();
    // N1（0.9.0 复审）：这里必须传**本次实跑距离**，不能传 `p.min_distance`。
    // `min_distance` 是校方门槛（如 2000 m），而 `totalDistance` 是实跑（如 2300 m）；
    // `Attempt::matches()` 用它跟记录列表里的 `total_dis` 比，容差只有 5 m ——
    // 传门槛会让对账**必然**判「未找到」，于是 Unknown 被标 Abandoned、提示允许重提，
    // 恰好制造出 WAL 本来要防的「同一天两条记录」。
    let target_dist = receipt_target_distance(p.track.totalDistance);
    guard.begin(
        &run_uuid,
        p.track.startTime,
        stop_ms,
        at,
        &plan_fp,
        target_dist,
    )?;
    match send_record(client, p, run_uuid.clone(), log) {
        Ok(result) => {
            if let Err(e) = guard.accepted(&run_uuid, result.rrid) {
                log(&format!(
                    "[record] 服务端已接收记录 #{}，但本地回执保存失败：{e}。请勿重复提交",
                    result.rrid
                ));
            }
            Ok(result)
        }
        Err(SubmitFailure::Rejected(error)) => {
            guard.rejected(&run_uuid, "服务端明确拒绝")?;
            Err(format!("服务端明确拒绝本次提交：{error}"))
        }
        Err(SubmitFailure::Unknown(error)) => {
            // 先把「请求已经发出去了」这件事写进回执，再做只读对账 —— 顺序反了的话，
            // 对账前崩溃就只剩一个 Pending，谁也不知道请求到底有没有到服务端。
            let after = crate::crypto::envelope::now_ms();
            if let Err(e) = guard.unknown(&run_uuid, after) {
                log(&format!("[record] 本地回执保存失败：{e}"));
            }
            log("[record] 提交响应不确定，正在只读核对记录；本次不会再次发送提交请求");
            super::submission_guard::reconcile_if_needed(&mut guard, client, after, log)?;
            if let Some(id) = guard.committed_rrid(&run_uuid) {
                return Err(format!(
                    "响应不确定，但已找到本次记录 #{id}。未重复提交，未补传 OBS；请到运动记录查看详情"
                ));
            }
            if guard.is_unresolved(&run_uuid) {
                // N2 之后，Unknown 在沉降窗口内**根本不去拉列表**（避免读到旧快照
                // 把「其实已入账」判成缺失）。所以这句不能再说「对账没跑完（网络或
                // 接口失败）」——那是另一回事，用户会去查网络。
                return Err(format!(
                    "提交结果未知：{error}。还没到自动对账时间（约 {} 秒后自动核对记录列表），\
                     期间已继续阻止再次提交；也可以先到运动记录页确认，确认未入账可 unlock 释放",
                    crate::api::submission_guard::UNKNOWN_SETTLE_MS / 1000
                ));
            }
            Err(format!(
                "提交结果未知：{error}。已对账记录列表：没有找到本次提交（计划指纹 {plan_fp}），\
                 允许重新提交；未查到不代表未入账，重提前请先到运动记录页确认"
            ))
        }
    }
}

enum SubmitFailure {
    Rejected(String),
    Unknown(String),
}
impl From<String> for SubmitFailure {
    fn from(error: String) -> Self {
        Self::Unknown(error)
    }
}
impl From<&str> for SubmitFailure {
    fn from(error: &str) -> Self {
        Self::Unknown(error.into())
    }
}

fn send_record(
    client: &mut ApiClient,
    p: &SubmitParams,
    run_uuid: String,
    log: &mut dyn FnMut(&str),
) -> Result<SubmitResult, SubmitFailure> {
    let track = &p.track;
    let start_coordinate = track.validate_consistency()?;
    if p.five_point_json.is_empty() {
        return Err("五点轨迹不能为空".into());
    }
    validate_five_point_wrapper(&p.five_point_json)?;
    let (speed_windows, step_windows) = submit_windows(track);
    let total_time = track.totalTime;
    let total_dis = track.totalDistance;
    let total_steps = track.totalSteps;
    let start_ms = track.startTime;
    let stop_ms = start_ms + total_time * 1000;
    let ascent = total_ascent(&track.locations);
    let power = avg_power(p.weight, total_dis, total_time);
    let kcal = official_kcal(p.weight, total_time, total_dis);

    let unid = p.selected_unid;

    let speed = pace_speed_value(total_time, total_dis);
    // 汇总步频必须与 `stepsPerTenSec` 的口径一致：后者已按 220 步/分夹过
    // （wire.rs 的 max_steps），汇总却用原始值，短时长就会出现「窗口最大 220、
    // 汇总 250」——服务端做一次除法交叉核对就是矛盾。
    let avg_step_freq = round_to(total_steps as f64 / total_time as f64 * 60.0, 0)
        .clamp(1.0, crate::track::wire::MAX_STEP_FREQ_PER_MIN) as i64;

    // C3：`complete` 不能恒为 true。
    //
    // `selDistance` 是**校方门槛**（策略 minDistance），`totalDis` 是实跑距离 —— 两件事。
    // 恒报 complete=true 时，服务端只要把两者比一下就是自相矛盾：
    // 「选了 2000 m、实际 1200 m、客户端却说已完成」。这里如实反映：
    // 达标才 true，不达标就 false + 非 0 原因码（0 = 无原因，与 false 搭配才是矛盾）。
    //
    // 注意：真正的拦截在 `flow::run_full_flow`（计划距离低于门槛直接不发请求），
    // 所以走到这里的短距离只来自绕过 UI 的调用方（CLI `--dist` / 测试）。
    let total_dis_i = round_to(total_dis, 0) as i64;
    let (complete, un_complete_reason) = completion_flag(total_dis_i, p.min_distance);
    if !complete {
        log(&format!(
            "[record] ⚠ 实跑 {total_dis_i} m 低于学校门槛 {} m：本条按「未完成」提交（complete=false），不会计入有效次数",
            p.min_distance
        ));
    }

    let mut body = Map::new();
    body.insert("allLocJson".into(), Value::String(String::new()));
    body.insert("sportType".into(), Value::from(1));
    body.insert("policy".into(), Value::from(p.policy));
    body.insert("totalTime".into(), Value::from(total_time));
    body.insert("startTime".into(), Value::from(start_ms));
    body.insert("stopTime".into(), Value::from(stop_ms));
    body.insert("getPrize".into(), Value::Bool(false));
    body.insert("status".into(), Value::from(0));
    body.insert("uuid".into(), Value::String(run_uuid.clone()));
    body.insert("uid".into(), Value::from(p.uid));
    body.insert("selectedUnid".into(), Value::from(unid));
    body.insert("selRunTime".into(), Value::from(total_time));
    body.insert("selDistance".into(), Value::from(p.min_distance));
    body.insert("totalDis".into(), Value::from(total_dis_i));
    body.insert("speed".into(), Value::from(speed));
    body.insert(
        "validDis".into(),
        Value::from(round_to(track.validDistance, 0) as i64),
    );
    body.insert("validTime".into(), Value::from(track.validTime));
    body.insert("complete".into(), Value::Bool(complete));
    body.insert("unCompleteReason".into(), Value::from(un_complete_reason));
    body.insert("calorie".into(), Value::from(kcal));
    body.insert("totalSteps".into(), Value::from(total_steps));
    body.insert("avgStepFreq".into(), Value::from(avg_step_freq));
    body.insert("useMobilityTools".into(), Value::from(0));
    body.insert("faceCheck".into(), Value::from(p.face_check));
    body.insert(
        "totalAscent".into(),
        Value::from(round_to(ascent, 0) as i64),
    );
    body.insert("avgPower".into(), Value::from(power));
    body.insert("speedPerTenSec".into(), Value::Array(speed_windows));
    body.insert("stepsPerTenSec".into(), Value::Array(step_windows));
    body.insert("isUpload".into(), Value::Bool(false));
    body.insert("more".into(), Value::Bool(false));
    body.insert("latitude".into(), Value::from(start_coordinate.latitude));
    body.insert("longitude".into(), Value::from(start_coordinate.longitude));
    body.insert("maxRunTime".into(), Value::from(0));
    body.insert("minSteps".into(), Value::from(0));
    if !p.five_point_json.is_empty() {
        body.insert(
            "fivePointJson".into(),
            Value::String(p.five_point_json.clone()),
        );
    }
    // Android 必现扩展字段
    body.insert("errorCode".into(), Value::from(0));
    body.insert("geeToken".into(), Value::String(String::new()));
    body.insert("unauthorized".into(), Value::from(0));
    body.insert("themeId".into(), Value::from(0));
    body.insert("goalId".into(), Value::Null);
    body.insert(
        "address".into(),
        Value::String(p.address.trim().to_string()),
    );

    let body_val = Value::Object(body.clone());
    let sig = signature(&body_val, false);
    let orig = original_sign(&body_val, false);
    body.insert("signature".into(), Value::String(sig));
    body.insert("originalSign".into(), Value::String(orig));
    let body_plain = Value::Object(body).to_string();

    let device_id = if client.identity.device_id.is_empty() {
        crate::crypto::header::stable_fallback_device_id()
    } else {
        client.identity.device_id.clone()
    };
    // 提交接口固定走 Android 身份（osType=0 + androidId）。
    // 关键：机型/分辨率/androidId 必须来自同一套自洽指纹，**并且与非提交链路一致**
    // （策略/围栏/点位/OBS/详情都用 `android_model_os`）。以前这里自己派生一套，
    // 同一个账号一次运行会报两种机型。
    let mut android_identity = client.identity.clone();
    // iOS 身份切到 Android 时，分辨率/IDFA 必须一起清掉：`ensure_fingerprint`
    // 只补空字段，留着 iOS 的 physicPixel 就会报「Android 机型 + iPhone 分辨率」。
    if android_identity.platform != "android" {
        android_identity.pixel.clear();
        android_identity.logic_pixel.clear();
        android_identity.idfa.clear();
    }
    android_identity.platform = "android".into();
    android_identity.device_id = device_id.clone();
    let (model, os) = android_identity.android_model_os();
    android_identity.device_name = model;
    android_identity.os_version = os;
    android_identity.ensure_fingerprint();
    let (hp, hp_extra) =
        build_android_header(&android_identity, p.uid, &client_token(client), None);
    let header_env = build_envelope(&mut client.session, &hp, OuterOrder::Observed);
    // body 与 header 各读一次时钟。以前这里写死 `now + 1`，而普通请求路径早已
    // 改成自然两读：同一账号的提交请求恒定差 1ms，两条链路口径还不一致，
    // 反而更像脚本常量。
    let body_env = build_envelope_ts(
        &mut client.session,
        &body_plain,
        OuterOrder::Insert,
        crate::crypto::envelope::now_ms(),
    );

    let runes = format!("{}{}", p.policy_ts, p.uid);
    let runef = format!("{}{}", run_uuid, start_ms);
    client.ensure_session_valid()?;
    // 克隆 agent：错误分支需要可变借用 client 回报出口健康度。
    let agent = client.agent.clone();
    let mut req = agent.post(&client.record_url());
    req = req
        .set("Content-Type", "application/json; charset=utf-8")
        // UA 由身份拼出，且与其余链路同源（`ua_for` 是全链唯一来源）。
        .set(
            "User-Agent",
            &crate::crypto::header::ua_for(&android_identity),
        )
        // appVersion 与信封同源：提交固定走 Android 身份（`build_android_header`），
        // 信封里的 appVersion 就是 ANDROID_APP_VERSION，外层必须报同一个值。
        .set("appVersion", crate::crypto::header::ANDROID_APP_VERSION)
        .set("headerSign", &header_env.json)
        .set("runes", &runes)
        .set("runef", &runef);
    for (k, v) in &hp_extra {
        req = req.set(k, v);
    }
    let resp = req.send_string(&body_env.json).map_err(|error| {
        let transport = matches!(&error, crate::http::Error::Transport(_));
        client.report_egress(!transport);
        let rejected = matches!(&error, crate::http::Error::Status(401, _));
        let message = client.handle_request_error(error);
        if rejected {
            SubmitFailure::Rejected(message)
        } else {
            SubmitFailure::Unknown(message)
        }
    })?;
    client.report_egress(true);
    let status = resp.status();
    let raw = resp.into_string().unwrap_or_default();
    log(&format!(
        "[record] sportType=1 HTTP {status} len={}",
        raw.len()
    ));

    let key = derive_paes_key(
        &body_env.key_data[0],
        &body_env.key_data[1],
        &body_env.key_data[2],
        &body_env.key_data[3],
    );
    let dec = decrypt_response(raw.as_bytes(), &key, &rsa_public_key())
        .map_err(|e| format!("提交响应解密失败: {e}"))?;
    let biz = check_business(&dec.business).map_err(|error| {
        if dec
            .business
            .get("error")
            .and_then(Value::as_i64)
            .is_some_and(|code| code != 10000)
        {
            SubmitFailure::Rejected(error)
        } else {
            SubmitFailure::Unknown(error)
        }
    })?;
    let rrid = super::client::get_field(&biz, "rrid")
        .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()))
        .unwrap_or(0);
    if rrid <= 0 {
        return Err(format!("提交未返回 rrid: {}", truncate_json(&biz)).into());
    }
    log(&format!("√ 提交成功 rrid={rrid} uuid={run_uuid}"));
    Ok(SubmitResult {
        rrid,
        uuid: run_uuid,
        start_ms,
        complete: super::client::get_field(&biz, "complete").and_then(|v| v.as_bool()),
        total_dis,
        total_time,
        total_steps,
        avg_step_freq,
        calorie: kcal,
        avg_power: power,
        sel_distance: p.min_distance,
    })
}

fn client_token(client: &ApiClient) -> String {
    client
        .login
        .as_ref()
        .map(|s| s.token.clone())
        .unwrap_or_default()
}

fn truncate_json(v: &Value) -> String {
    let s = v.to_string();
    s.chars().take(240).collect()
}

#[cfg(test)]
#[path = "submission_tests.rs"]
mod submission_tests;

#[cfg(test)]
mod metric_tests {
    use super::*;
    #[test]
    fn record_pace_has_decimal_protocol_scale() {
        assert_eq!(pace_speed_value(600, 1000.0), 10000);
        assert_eq!(pace_speed_value(720, 2000.0), 6000);
    }

    /// C3：`complete` 不再是常量 true —— 低于学校门槛必须报未完成 + 非 0 原因码。
    #[test]
    fn completion_flag_tracks_the_school_threshold() {
        // 达标 / 恰好达标：完成，无原因。
        assert_eq!(completion_flag(2000, 2000), (true, 0));
        assert_eq!(completion_flag(2001, 2000), (true, 0));
        // 未达标：未完成，且原因码非 0（false + 0 才是自相矛盾）。
        let (complete, reason) = completion_flag(1200, 2000);
        assert!(!complete, "1200 m 跑不满 2000 m 门槛却报了完成");
        assert_ne!(reason, 0, "未完成却没有原因码");
        // 门槛未知：按完成处理，不虚构未达标。
        assert_eq!(completion_flag(1200, 0), (true, 0));
        assert_eq!(completion_flag(1200, -1), (true, 0));
    }
}
