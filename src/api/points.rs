//! 校园点位：POST /api/v560/get/1/distance/1（sportType=4）。
//!
//! runec = 信封(f"{uid}{经度6位}{纬度6位}{起始时间秒级ms整}"，insert→observed 序列化)；
//! sign = MD5(http版URL + 盐)。带按账号、学校、区域及位置隔离的 300s 缓存。

use super::client::ApiClient;
use super::model;
use crate::crypto::envelope::{build_envelope, OuterOrder};
use crate::crypto::sign::md5_url_sign;
use crate::location::Coordinate;
use serde_json::{json, Value};

/// 点位接口路径前缀：`/api/v560/get/1/distance/<目标距离(米)>`。
///
/// 官方客户端把**本次目标距离**拼在末尾（真机抓包实测为 `.../distance/2000`），
/// 写死 `1` 会让服务端按 1 米返回点位集合，表现为「打卡点拉不全 / 每次都是同样几个点」。
pub const POINTS_PATH_PREFIX: &str = "/api/v560/get/1/distance/";

/// 拼点位接口路径（距离取整，最少 1 米）。
pub fn points_path(distance_m: f64) -> String {
    format!("{POINTS_PATH_PREFIX}{}", distance_m.round().max(1.0) as i64)
}

/// 经纬度六位小数字符串。
fn six_digit(v: f64) -> String {
    format!("{v:.6}")
}

/// 拉取实时点位（仅使用有效期内缓存）。anchor=(lat,lng) 请求锚点。
pub fn fetch_points(
    client: &mut ApiClient,
    anchor: Coordinate,
    log: &mut dyn FnMut(&str),
) -> Result<Vec<Value>, String> {
    fetch_points_ext(client, anchor, None, 2000.0, log)
}

/// 点位请求的明文 body。
///
/// 抽成一个纯函数只为一件事：**锚点偏移只改 `latitude`/`longitude`，不新增字段**
/// 这条约定必须能被测试直接断言 —— 真正的报文会被信封加密，测试看不到明文。
pub(crate) fn request_body(
    anchor: Coordinate,
    run_area_id: Option<&str>,
    unid: &str,
    sign: &str,
    uuid: &str,
    runec: &str,
) -> Value {
    let mut body = json!({
        "sportType": 4,
        "longitude": anchor.longitude,
        "latitude": anchor.latitude,
        "sign": sign,
        "uuid": uuid,
        "selectedUnid": unid,
        "runec": runec,
    });
    if let Some(area) = run_area_id {
        body["runAreaId"] = json!(area);
    }
    body
}

/// run_area_id：学校配置了区域时 App 会附带；未配置则不传（与 App 一致）。
pub fn fetch_points_ext(
    client: &mut ApiClient,
    anchor: Coordinate,
    run_area_id: Option<String>,
    distance_m: f64,
    log: &mut dyn FnMut(&str),
) -> Result<Vec<Value>, String> {
    anchor.validate()?;
    let login = client.login.as_ref().ok_or("请先登录后获取点位")?;
    let scope = model::PointsCacheScope {
        uid: login.uid,
        unid: login.unid.clone(),
        run_area_id: run_area_id.clone(),
        distance_m: distance_m.round() as i64,
    };
    // ① TTL 内命中缓存直接返回
    if let Some((ts, pts)) = model::load_points_cache_for(anchor, &scope) {
        log(&format!(
            "[points] 缓存命中（{} 秒前，{} 点）",
            (crate::crypto::envelope::now_ms() - ts) / 1000,
            pts.len()
        ));
        return Ok(pts);
    }
    // ② 请求接口
    let uid = client.login.as_ref().map(|s| s.uid).unwrap_or(0);
    let unid = client
        .login
        .as_ref()
        .map(|s| s.unid.clone())
        .unwrap_or_else(|| "0".into());
    let lat = anchor.latitude;
    let lon = anchor.longitude;
    let url = format!("{}{}", model::HOST, points_path(distance_m));

    let start_ms = crate::crypto::envelope::now_ms();
    let runec_input = format!(
        "{uid}{}{}{}",
        six_digit(lon),
        six_digit(lat),
        (start_ms / 1000) * 1000
    );
    // runec：共用会话 Four，外层 observed 序
    let runec_env = build_envelope(&mut client.session, &runec_input, OuterOrder::Observed);
    let runec = runec_env.json;

    // 明文 body 由 `request_body` 统一构造：锚点偏移只能影响这里的 latitude/longitude
    // （`runec` 的输入串由同一份 lon/lat 派生，因此自动一致，不会「body 挪了、签名没挪」）。
    let body = request_body(
        anchor,
        run_area_id.as_deref(),
        &unid,
        &md5_url_sign(&url),
        &uuid::Uuid::new_v4().to_string(),
        &runec,
    );
    let body = body.to_string();

    // 点位与策略/围栏同属"这条轨迹"：UA 也必须来自同一套身份。
    let ua = crate::crypto::header::ua_for(&client.identity);
    let out = client.envelope_request("POST", &url, &body, &ua, &[])?;
    let dec = out.decrypted.ok_or("点位接口响应无法解密")?;
    let pts = parse_points(&dec.business)?;
    let _ = model::save_points_cache(anchor, &scope, &pts);
    Ok(pts)
}

pub(crate) fn parse_points(payload: &Value) -> Result<Vec<Value>, String> {
    let payload = super::client::check_business(payload)?;
    // pointsResModels：优先顶层，其次 data 子对象
    let pts: Vec<Value> = super::client::get_field(&payload, "pointsResModels")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default();
    if pts.is_empty() {
        return Err("点位接口未返回有效点位列表".into());
    }
    if points_bd(&pts).len() != pts.len() {
        return Err("点位接口含无效坐标".into());
    }
    Ok(pts)
}

/// 点位中心（BD 系）。
#[allow(dead_code)]
pub fn center_bd(points: &[Value]) -> (f64, f64) {
    let n = points.len().max(1) as f64;
    let lat = points
        .iter()
        .filter_map(|p| p.get("lat").and_then(|v| v.as_f64()))
        .sum::<f64>()
        / n;
    let lon = points
        .iter()
        .filter_map(|p| p.get("lon").and_then(|v| v.as_f64()))
        .sum::<f64>()
        / n;
    (lat, lon)
}

/// 点位 → (lat, lon) BD 系数组（轨迹输入）。
pub fn points_bd(points: &[Value]) -> Vec<(f64, f64)> {
    points.iter().filter_map(super::fence::point_xy).collect()
}

/// Preserve policy order, then add distinct live points in a stable radial order.
pub fn route_points(required: &[(f64, f64)], live: &[Value]) -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    for p in required
        .iter()
        .copied()
        .chain(crate::track::generate_road::radial_order(&points_bd(live)))
    {
        if !out
            .iter()
            .any(|q: &(f64, f64)| (q.0 - p.0).abs() < 1e-6 && (q.1 - p.1).abs() < 1e-6)
        {
            out.push(p);
        }
    }
    out
}

#[cfg(test)]
mod anchor_offset_tests {
    use super::*;

    /// 按 `keys()` 的**原始迭代顺序**取键。
    ///
    /// 不能用 sort 后的集合：`serde_json` 开了 `preserve_order`，请求体的字段顺序
    /// 是稳定契约（服务端按字节看报文的场合不少），排序会把顺序问题掩盖掉。
    fn keys_in_order(value: &Value) -> Vec<String> {
        value
            .as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// anchor_offset_m = 0：请求里的坐标必须与精确锚点**逐位相同**。
    #[test]
    fn zero_offset_request_coordinate_equals_the_exact_anchor() {
        let exact = Coordinate::new(39.9042, 116.4074, 0.0).unwrap();
        let plan_anchor = crate::location::plan_anchor(exact, 0.0, 7);
        let body = request_body(plan_anchor, Some("73"), "77", "sign", "uuid", "runec");
        assert_eq!(
            body["latitude"].as_f64().unwrap().to_bits(),
            exact.latitude.to_bits()
        );
        assert_eq!(
            body["longitude"].as_f64().unwrap().to_bits(),
            exact.longitude.to_bits()
        );
    }

    /// 偏移只改经纬度数值：**字段顺序**与取值都不能动。
    #[test]
    fn offset_changes_only_latitude_and_longitude() {
        let exact = Coordinate::new(39.9042, 116.4074, 0.0).unwrap();
        let base = request_body(exact, Some("73"), "77", "sign", "uuid", "runec");
        let moved_anchor = crate::location::plan_anchor(exact, 50.0, 7);
        let moved = request_body(moved_anchor, Some("73"), "77", "sign", "uuid", "runec");

        assert_eq!(
            keys_in_order(&base),
            keys_in_order(&moved),
            "偏移改变了字段或字段顺序"
        );
        // 除经纬度外每个字段都必须完全一样。
        for key in keys_in_order(&base) {
            if key == "latitude" || key == "longitude" {
                continue;
            }
            assert_eq!(base[&key], moved[&key], "字段 {key} 不该被偏移影响");
        }
        let dy = (moved["latitude"].as_f64().unwrap() - exact.latitude)
            * crate::track::geom::MET_PER_DEG_LAT;
        let dx = (moved["longitude"].as_f64().unwrap() - exact.longitude)
            * crate::track::geom::MET_PER_DEG_LNG;
        let shift = dx.hypot(dy);
        assert!(
            (39.5..=60.5).contains(&shift),
            "50 m 配置下实际偏移 {shift:.2} m"
        );
        // 本地参数名一个都不许出现在请求里。
        let text = moved.to_string();
        for forbidden in ["gps_spread", "anchor_offset", "anchorOffset", "gpsSpread"] {
            assert!(!text.contains(forbidden), "点位请求里出现 {forbidden}");
        }
    }

    /// 同一计划两次取到同一份偏移，新计划（新 seed）必然重抽。
    #[test]
    fn offset_is_stable_within_a_plan_and_redrawn_for_a_new_plan() {
        let exact = Coordinate::new(39.9042, 116.4074, 0.0).unwrap();
        let a = request_body(
            crate::location::plan_anchor(exact, 50.0, 7),
            None,
            "77",
            "s",
            "u",
            "r",
        );
        let b = request_body(
            crate::location::plan_anchor(exact, 50.0, 7),
            None,
            "77",
            "s",
            "u",
            "r",
        );
        assert_eq!(a, b, "同一计划（同 seed）两份请求坐标必须相同");
        let c = request_body(
            crate::location::plan_anchor(exact, 50.0, 8),
            None,
            "77",
            "s",
            "u",
            "r",
        );
        assert_ne!(a, c, "新计划必须重抽偏移");
    }

    /// 请求体的**字段顺序**钉死：`sportType` 首位，`runAreaId` 补在末位，
    /// 且「有区域 / 无区域」「偏移前 / 偏移后」三种情况顺序完全一致。
    ///
    /// 这条替代了原来 sort 后比较的写法 —— 排序比较对顺序错误是盲的。
    #[test]
    fn request_body_key_order_is_stable() {
        let exact = Coordinate::new(39.9042, 116.4074, 0.0).unwrap();
        let with_area = request_body(exact, Some("73"), "77", "sign", "uuid", "runec");
        let order = keys_in_order(&with_area);
        assert_eq!(
            order,
            vec![
                "sportType",
                "longitude",
                "latitude",
                "sign",
                "uuid",
                "selectedUnid",
                "runec",
                "runAreaId",
            ],
            "点位请求的字段顺序被改动"
        );
        assert_eq!(
            order.first().map(String::as_str),
            Some("sportType"),
            "首位变了"
        );
        assert_eq!(
            order.last().map(String::as_str),
            Some("runAreaId"),
            "末位变了"
        );

        // 未配置区域：runAreaId 不出现，其余顺序原样。
        let without_area = request_body(exact, None, "77", "sign", "uuid", "runec");
        let order_without = keys_in_order(&without_area);
        assert_eq!(
            order_without,
            order[..order.len() - 1].to_vec(),
            "无区域时字段顺序应等于「有区域」去掉末位"
        );

        // 50 m 偏移不改变顺序（只改两个数值）。
        let moved = request_body(
            crate::location::plan_anchor(exact, 50.0, 7),
            Some("73"),
            "77",
            "sign",
            "uuid",
            "runec",
        );
        assert_eq!(keys_in_order(&moved), order, "偏移改变了字段顺序");
    }
}

#[cfg(test)]
mod route_tests {
    use super::*;
    #[test]
    fn direct_envelope_points_decode_json_text_business_data() {
        let point = json!({"lat":"39.9","lon":"116.4","isMustPoint":false});
        let data = json!({"pointsResModels":[point.clone()]});
        let response = json!({"error":10000,"data":data.to_string()});
        let points = parse_points(&response).unwrap();
        assert_eq!(points, vec![point]);
        assert_eq!(points_bd(&points), vec![(39.9, 116.4)]);
        assert!(parse_points(&json!({"error":10000,"data":"{broken"})).is_err());
    }

    #[test]
    fn response_errors_and_invalid_coordinates_are_not_cached_as_success() {
        let point = json!({"lat":39.9,"lon":116.4});
        assert_eq!(
            parse_points(&json!({"error":10000,"data":{"pointsResModels":[point.clone()]}}))
                .unwrap(),
            vec![point.clone()]
        );
        for response in [
            json!({"error":10603,"pointsResModels":[point]}),
            json!({"error":10000,"pointsResModels":[]}),
            json!({"error":10000,"pointsResModels":[{}]}),
        ] {
            assert!(parse_points(&response).is_err());
        }
    }
    #[test]
    fn first_required_point_and_order_survive_deduplication() {
        let first = (39.9, 116.4);
        let second = (39.91, 116.41);
        let live = vec![
            json!({"lat": 39.9,"lon":116.4}),
            json!({"lat":39.92,"lon":116.42}),
            json!({"lat":39.92,"lon":116.42}),
        ];
        assert_eq!(
            route_points(&[first, first, second], &live),
            vec![first, second, (39.92, 116.42)]
        );
    }
}
