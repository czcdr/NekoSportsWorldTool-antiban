//! 跑步策略：POST /api/v70103/runModePolicy。
//! 返回 data.runRuleModel.minDistance（提交时 selDistance 用它）与 data.policy。

use super::client::{get_field, ApiClient};
use super::fence::point_xy;
use serde_json::{json, Value};

pub const POLICY_PATH: &str = "/api/v70103/runModePolicy";

pub struct PolicyInfo {
    pub timestamp: i64,
    pub policy: i64,
    pub min_distance: i64,
    pub valid_time: i64,
    /// 必经点（BD 系，与打卡点同系），policy 响应里若有则返回。
    pub must_points: Vec<(f64, f64)>,
    /// 服务端是否要求本次跑步做人脸核验。
    ///
    /// **只读**：只用于在日志/界面上提示「这次服务端要核验，而本工具不会执行」，
    /// 不参与提交体构造（`faceCheck` 仍由用户配置决定）。`None` = 响应里没有这个
    /// 字段，**不等于**「不要求」。
    pub face_required: Option<bool>,
    /// 人脸核验类型（官方 `RunFaceCheckConfigDTO.faceVerifyType`），只读。
    pub face_verify_type: Option<i64>,
    /// **剩余冻结秒数**（`data.freezeRunTime`）；0 = 未冻结。
    ///
    /// 这是「禁跑还剩多久」的唯一权威来源：官方 App 用它渲染倒计时，
    /// 冻结期间服务端会独立拒绝提交（客户端绕过没有意义）。
    pub freeze_run_time: i64,
    /// 与冻结一起返回的服务端提示文案（`data.message`），可能为空。
    pub freeze_message: String,
}

impl PolicyInfo {
    /// 是否处于禁跑/冻结期。
    pub fn frozen(&self) -> bool {
        self.freeze_run_time > 0
    }

    /// 把剩余秒数格式化成「3 天 04:07:12」这类可读文本；未冻结返回「未冻结」。
    pub fn freeze_text(&self) -> String {
        if !self.frozen() {
            return "未冻结".into();
        }
        let mut s = self.freeze_run_time;
        let d = s / 86_400;
        s %= 86_400;
        let h = s / 3_600;
        s %= 3_600;
        let m = s / 60;
        let sec = s % 60;
        if d > 0 {
            format!("还剩 {d} 天 {h:02}:{m:02}:{sec:02}")
        } else {
            format!("还剩 {h:02}:{m:02}:{sec:02}")
        }
    }
}

/// 从 policy 响应 `data` 中防御式提取必经点列表（字段名不确定，逐个尝试）。
fn extract_must_points(v: &serde_json::Value) -> Result<Vec<(f64, f64)>, String> {
    let mut points = Vec::new();
    for name in [
        "pointList",
        "runPointList",
        "mustPointList",
        "passPointList",
        "mustPoints",
        "passPoints",
        "runPoints",
        "points",
        "nodeList",
        "checkPointList",
    ] {
        let Some(value) = get_field(v, name).filter(|v| !v.is_null()) else {
            continue;
        };
        let arr = value
            .as_array()
            .ok_or_else(|| format!("策略点位 {name} 不是列表"))?;
        for (index, value) in arr.iter().enumerate() {
            let point =
                point_xy(value).ok_or_else(|| format!("策略点位 {name}[{index}] 坐标无效"))?;
            if !points
                .iter()
                .any(|p: &(f64, f64)| (p.0 - point.0).abs() < 1e-6 && (p.1 - point.1).abs() < 1e-6)
            {
                points.push(point);
            }
        }
    }
    Ok(points)
}

/// body：{"runMode":1,"ruleUpdateTime":0,"geoFenceUpdateTime":0,"selectUnid":<unid>,"operateType":0}
pub fn fetch_policy(client: &mut ApiClient) -> Result<PolicyInfo, String> {
    let unid = client
        .login
        .as_ref()
        .map(|s| s.unid.clone())
        .unwrap_or_default();
    let select_unid = unid.parse::<i64>().unwrap_or(0);
    let body = json!({
        "runMode": 1,
        "ruleUpdateTime": 0,
        "geoFenceUpdateTime": 0,
        "selectUnid": select_unid,
        "operateType": 0,
    })
    .to_string();
    let biz = client.call("POST", POLICY_PATH, &body, &[])?;
    parse_response(&biz)
}

/// 从 policy 响应里读人脸核验配置（**只读**，不参与提交体构造）。
///
/// 字段名来自官方 dex（`com.zjwh.android_wh_physicalfitness`）：
/// `faceEnable` / `faceVerifyType` / `faceCheck` / `serverCheckStatus`，
/// 以及 DTO `entity.RunFaceCheckConfigDTO`。服务端把开关放在顶层还是
/// `runRuleModel` 里**没有抓包证据**，所以三处都找；找不到就是 `None`
/// （= 不知道），不当成「不要求」。
fn extract_face_rule(biz: &Value, rule: &Value) -> (Option<bool>, Option<i64>) {
    // `runFaceCheck` 可能是一个 DTO 对象，把它的字段也纳入查找范围。
    let nested = ["runFaceCheck", "runFaceCheckConfigDTO", "faceCheckConfig"]
        .iter()
        .find_map(|name| get_field(biz, name).filter(|v| v.is_object()).cloned())
        .unwrap_or(Value::Null);

    let pick = |name: &str| -> Option<Value> {
        rule.get(name)
            .cloned()
            .or_else(|| get_field(biz, name).cloned())
            .or_else(|| nested.get(name).cloned())
            .filter(|v| !v.is_null())
    };
    let as_bool = |v: Value| -> Option<bool> {
        match v {
            Value::Bool(b) => Some(b),
            Value::Number(n) => n.as_i64().map(|n| n != 0),
            Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
                "1" | "true" => Some(true),
                "0" | "false" => Some(false),
                _ => None,
            },
            _ => None,
        }
    };

    let face_required = pick("faceEnable")
        .and_then(as_bool)
        .or_else(|| pick("faceCheck").and_then(as_bool))
        .or_else(|| pick("serverCheckStatus").and_then(as_bool));
    let face_verify_type = pick("faceVerifyType").and_then(|v| v.as_i64());
    (face_required, face_verify_type)
}

pub(crate) fn parse_response(biz: &Value) -> Result<PolicyInfo, String> {
    let biz = super::client::normalize_business_data(biz)?;
    let timestamp = get_field(&biz, "timestamp")
        .and_then(|t| t.as_i64())
        .ok_or("policy 响应缺 timestamp")?;
    let policy = get_field(&biz, "policy")
        .and_then(|t| t.as_i64())
        .unwrap_or(0);
    let rule = get_field(&biz, "runRuleModel")
        .cloned()
        .unwrap_or(Value::Null);
    let (face_required, face_verify_type) = extract_face_rule(&biz, &rule);
    // 冻结倒计时：`data.freezeRunTime` 是服务端给的**剩余冻结秒数**（0 = 未冻结）。
    // 官方文案「您已被查出跑步违规.无法进行跑步.将在…」的「将在」就是它换算出来的解禁时刻。
    // 负值按 0 处理（防御服务端偶发返回 -1）。
    let freeze_run_time = get_field(&biz, "freezeRunTime")
        .and_then(|v| v.as_i64())
        .map(|n| n.max(0))
        .unwrap_or(0);
    let freeze_message = get_field(&biz, "message")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    Ok(PolicyInfo {
        timestamp,
        policy,
        min_distance: rule
            .get("minDistance")
            .and_then(|t| t.as_i64())
            .unwrap_or(1000),
        valid_time: rule.get("validTime").and_then(|t| t.as_i64()).unwrap_or(0),
        must_points: extract_must_points(&biz)?,
        face_required,
        face_verify_type,
        freeze_run_time,
        freeze_message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn json_text_policy_preserves_required_points_and_rules() {
        let data = json!({"timestamp":123,"policy":4,"runRuleModel":{"minDistance":2000,"validTime":600},"mustPoints":[{"lat":"39.9","lon":"116.4"}],"freezeRunTime":7200,"message":"数据异常，禁跑"});
        for response in [json!({"data":data}), json!({"data":data.to_string()})] {
            let policy = parse_response(&response).unwrap();
            assert_eq!(policy.timestamp, 123);
            assert_eq!(policy.policy, 4);
            assert_eq!(policy.min_distance, 2000);
            assert_eq!(policy.valid_time, 600);
            assert_eq!(policy.must_points, vec![(39.9, 116.4)]);
            assert_eq!(policy.freeze_run_time, 7200);
            assert!(policy.frozen());
            assert_eq!(policy.freeze_message, "数据异常，禁跑");
        }
        assert!(parse_response(&json!({"data":"{broken"})).is_err());
    }

    #[test]
    fn freeze_seconds_are_clamped_and_rendered_as_a_countdown() {
        let mk = |extra: Value| {
            let mut v = json!({"timestamp": 1, "policy": 1});
            if let (Some(obj), Some(add)) = (v.as_object_mut(), extra.as_object()) {
                for (k, val) in add {
                    obj.insert(k.clone(), val.clone());
                }
            }
            parse_response(&json!({"data": v})).unwrap()
        };
        // 字段缺失 = 未冻结（不能当成"冻结 0 秒"以外的任何语义）
        let p = mk(json!({}));
        assert_eq!(p.freeze_run_time, 0);
        assert!(!p.frozen());
        assert_eq!(p.freeze_text(), "未冻结");
        // 负值按 0 处理
        assert_eq!(mk(json!({"freezeRunTime": -5})).freeze_run_time, 0);
        // 48 小时（子代理从 issue 拿到的现场值）
        let p = mk(json!({"freezeRunTime": 172_800}));
        assert_eq!(p.freeze_text(), "还剩 2 天 00:00:00");
        // 不足一天不带天数前缀
        assert_eq!(mk(json!({"freezeRunTime": 3725})).freeze_text(), "还剩 01:02:05");
    }

    #[test]
    fn merges_all_recognized_lists_without_losing_first_point() {
        let a = json!({"lat":39.9,"lon":116.4});
        let b = json!({"lat":39.91,"lon":116.41});
        let v = json!({"data":{"pointList":[a.clone()],"mustPoints":[a,b]}});
        assert_eq!(
            extract_must_points(&v).unwrap(),
            vec![(39.9, 116.4), (39.91, 116.41)]
        );
        assert!(extract_must_points(&json!({"mustPoints":null}))
            .unwrap()
            .is_empty());
    }
    #[test]
    fn malformed_required_points_are_errors_not_omissions() {
        assert!(
            extract_must_points(&json!({"mustPoints":[{}, {"lat":39.9,"lon":116.4}]})).is_err()
        );
        assert!(extract_must_points(&json!({"mustPoints":"bad"})).is_err());
    }

    /// 人脸开关只做**读取**：三种落点（runRuleModel / 顶层 / DTO 对象）都要认，
    /// 认不出必须是 `None` 而不是 `Some(false)` —— 后者会在日志里撒谎说"不要求核验"。
    #[test]
    fn face_rule_is_read_only_and_lenient() {
        let cases = [
            (json!({"data":{"timestamp":1,"runRuleModel":{"faceEnable":true}}}), Some(true)),
            (json!({"data":{"timestamp":1,"faceCheck":1}}), Some(true)),
            (json!({"data":{"timestamp":1,"faceCheck":"0"}}), Some(false)),
            (
                json!({"data":{"timestamp":1,"runFaceCheck":{"faceEnable":true,"faceVerifyType":2}}}),
                Some(true),
            ),
            (json!({"data":{"timestamp":1,"serverCheckStatus":"1"}}), Some(true)),
        ];
        for (response, want) in cases {
            let policy = parse_response(&response).unwrap();
            assert_eq!(policy.face_required, want, "响应：{response}");
        }

        // 字段缺失 = 不知道，不能当成"不要求"。
        let absent = parse_response(&json!({"data":{"timestamp":1}})).unwrap();
        assert_eq!(absent.face_required, None);
        assert_eq!(absent.face_verify_type, None);

        // faceVerifyType 独立提取（可能只下发类型、不下发开关）。
        let typed =
            parse_response(&json!({"data":{"timestamp":1,"runRuleModel":{"faceVerifyType":3}}}))
                .unwrap();
        assert_eq!(typed.face_required, None);
        assert_eq!(typed.face_verify_type, Some(3));
    }
}
