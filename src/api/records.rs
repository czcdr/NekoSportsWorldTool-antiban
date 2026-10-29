//! 跑步记录。
//! 列表：POST /api/v70230/runnings/records {}；单条：POST /api/v70260/runnings/get_one_record。

use super::client::{get_field, parse_data_field, ApiClient};
use serde_json::{json, Value};

pub const RECORDS_PATH: &str = "/api/v70230/runnings/records";
pub const GET_ONE_PATH: &str = "/api/v70260/runnings/get_one_record";

/// 记录列表行（表格展示用）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RecordRow {
    pub rrid: i64,
    pub total_dis: f64,
    pub total_time: i64,
    pub start_time: i64,
    pub complete: bool,
    pub avg_step_freq: i64,
    #[serde(default)]
    pub calorie: i64,
    #[serde(default)]
    pub avg_power: i64,
    #[serde(default)]
    pub total_steps: i64,
    #[serde(default)]
    pub uuid: String,
}

/// 拉取记录列表并归一化为行。
pub fn fetch_records(client: &mut ApiClient) -> Result<Vec<RecordRow>, String> {
    let biz = client.call("POST", RECORDS_PATH, "{}", &[])?;
    let arr: Vec<Value> = match parse_data_field(&biz) {
        Value::Array(a) => a,
        Value::Object(ref o) => o
            .get("list")
            .or_else(|| o.get("records"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
        _ => get_field(&biz, "list")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
    };
    Ok(arr
        .iter()
        .map(|r| RecordRow {
            rrid: r
                .get("rrid")
                .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()))
                .unwrap_or(0),
            total_dis: r.get("totalDis").and_then(|v| v.as_f64()).unwrap_or(0.0),
            total_time: r.get("totalTime").and_then(|v| v.as_i64()).unwrap_or(0),
            start_time: r.get("startTime").and_then(|v| v.as_i64()).unwrap_or(0),
            complete: r.get("complete").and_then(|v| v.as_bool()).unwrap_or(false),
            avg_step_freq: r.get("avgStepFreq").and_then(|v| v.as_i64()).unwrap_or(0),
            calorie: r.get("calorie").and_then(|v| v.as_i64()).unwrap_or(0),
            avg_power: r.get("avgPower").and_then(|v| v.as_i64()).unwrap_or(0),
            total_steps: r.get("totalSteps").and_then(|v| v.as_i64()).unwrap_or(0),
            uuid: r.get("uuid").and_then(|v| v.as_str()).unwrap_or("").into(),
        })
        .collect())
}

/// 单条详情（验证提交结果用）：返回内层 data 对象。
pub fn fetch_one_record(client: &mut ApiClient, rrid: i64) -> Result<Value, String> {
    let body = json!({ "rrid": rrid, "uuid": Value::Null, "calculateBadge": false }).to_string();
    let biz = client.call("POST", GET_ONE_PATH, &body, &[])?;
    // business.data 可能是 JSON 字符串或对象，其内层再取 data
    let inner = parse_data_field(&biz);
    let record = if inner.get("data").is_some() {
        parse_data_field(&inner)
    } else {
        inner
    };
    if record.is_null() {
        return Err("记录详情为空".into());
    }
    Ok(record)
}

/// 单条跑步详情（get_one_record 返回的完整 data）。
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunDetail {
    pub rrid: i64,
    pub uuid: String,
    pub sport_type: i64,
    pub total_time: i64,
    pub total_dis: f64,
    pub speed: f64,
    pub calorie: i64,
    pub avg_power: i64,
    pub total_steps: i64,
    pub total_ascent: i64,
    pub total_descent: i64,
    pub avg_step_freq: i64,
    pub valid_dis: f64,
    pub valid_time: i64,
    pub address: String,
    pub status_info: String,
    pub complete: bool,
    pub reason_list: Vec<ReasonItem>,
    pub start_time: i64,
}

/// `reasonList` 的一项：服务端给的一条判定。
///
/// # 为什么多留两个「结构」字段
/// 用户要的区分是「规则没过（未达标）」和「规则全过但被另外标记（异常）」。实测
/// v70260 的条目只有 `reason` + `complete`，但那只是**当前版本**的形态；有的版本/
/// 有的条目会带规则标识（`ruleId` / `rule` / `type` …）。所以解析阶段把这类字段
/// **原样留下**，让 `ui::record_status` 能先按结构判断，结构确实没有区分时再退回
/// 文案匹配（见那里的注释）。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasonItem {
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub complete: bool,
    /// 规则标识：来自 `ruleId` / `rule` / `ruleCode` / `ruleKey`（取第一个非空）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    /// 条目类型：来自 `type` / `kind` / `category`（取第一个非空）。
    ///
    /// 序列化名是 `kindHint` —— 它既可能是服务端原始 `type` 的值，也可能是本结构
    /// 归一化后的结果；两种形态 `from_json` 都认。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind_hint: Option<String>,
}

impl ReasonItem {
    /// 解析一条判定（`reasonList` 的一项）。
    ///
    /// 只读字段，不解释语义：`reason` / `complete` 是展示与「通过与否」的依据，
    /// 结构键只用来区分「规则项」与「非规则项」。
    pub fn from_json(r: &Value) -> Self {
        let text = |k: &str| -> Option<String> {
            match r.get(k) {
                Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
                Some(Value::Number(n)) => Some(n.to_string()),
                _ => None,
            }
        };
        Self {
            reason: r
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            complete: r.get("complete").and_then(|v| v.as_bool()).unwrap_or(false),
            rule_id: ["ruleId", "rule", "ruleCode", "ruleKey"]
                .into_iter()
                .find_map(text),
            kind_hint: ["kindHint", "type", "kind", "category"]
                .into_iter()
                .find_map(text),
        }
    }

    /// 解析整个 `reasonList`；缺失/不是数组一律按空列表（调用方不需要再包一层 match）。
    pub fn from_list(v: Option<&Value>) -> Vec<Self> {
        v.and_then(|v| v.as_array())
            .map(|arr| arr.iter().map(ReasonItem::from_json).collect())
            .unwrap_or_default()
    }
}

/// 拉取单条详情并解析为结构体。
pub fn fetch_detail(client: &mut ApiClient, rrid: i64) -> Result<RunDetail, String> {
    let raw = fetch_one_record(client, rrid)?;
    if let Some(id) = raw.get("rrid") {
        if id.as_i64().or_else(|| id.as_str()?.parse().ok()) != Some(rrid) {
            return Err("详情返回的记录编号与请求不一致，请刷新重试".into());
        }
    }
    let reasons = ReasonItem::from_list(raw.get("reasonList"));
    Ok(RunDetail {
        rrid,
        uuid: raw
            .get("uuid")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .into(),
        sport_type: raw.get("sportType").and_then(|v| v.as_i64()).unwrap_or(0),
        total_time: raw.get("totalTime").and_then(|v| v.as_f64()).unwrap_or(0.0) as i64,
        total_dis: raw.get("totalDis").and_then(|v| v.as_f64()).unwrap_or(0.0),
        speed: raw.get("speed").and_then(|v| v.as_f64()).unwrap_or(0.0),
        calorie: raw.get("calorie").and_then(|v| v.as_i64()).unwrap_or(0),
        avg_power: raw.get("avgPower").and_then(|v| v.as_i64()).unwrap_or(0),
        total_steps: raw.get("totalSteps").and_then(|v| v.as_i64()).unwrap_or(0),
        total_ascent: raw.get("totalAscent").and_then(|v| v.as_i64()).unwrap_or(0),
        total_descent: raw
            .get("totalDescent")
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        avg_step_freq: raw.get("avgStepFreq").and_then(|v| v.as_i64()).unwrap_or(0),
        valid_dis: raw.get("validDis").and_then(|v| v.as_f64()).unwrap_or(0.0),
        valid_time: raw.get("validTime").and_then(|v| v.as_f64()).unwrap_or(0.0) as i64,
        address: raw
            .get("address")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .into(),
        status_info: raw
            .get("statusInfo")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .into(),
        complete: raw
            .get("complete")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        reason_list: reasons,
        start_time: raw.get("startTime").and_then(|v| v.as_i64()).unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detail_serializes_camel_case() {
        let d = RunDetail {
            rrid: 42,
            total_dis: 1234.0,
            valid_dis: 1200.0,
            status_info: "有效".into(),
            reason_list: vec![ReasonItem {
                reason: "里程达标".into(),
                complete: true,
                ..Default::default()
            }],
            start_time: 1_700_000_000_000,
            ..Default::default()
        };
        let v: serde_json::Value = serde_json::to_value(&d).unwrap();
        for key in [
            "totalDis",
            "validDis",
            "statusInfo",
            "reasonList",
            "startTime",
            "sportType",
            "avgStepFreq",
        ] {
            assert!(v.get(key).is_some(), "缺少键 {key}");
        }
        assert!(v.get("total_dis").is_none());
    }

    /// 实测形态（只有 reason + complete）：结构字段为 None，交给上层按文案兜底。
    #[test]
    fn reason_item_keeps_absent_structure_keys_absent() {
        let item = ReasonItem::from_json(&serde_json::json!({
            "reason": "运动数据有问题",
            "complete": false
        }));
        assert_eq!(item.reason, "运动数据有问题");
        assert!(!item.complete);
        assert!(item.rule_id.is_none());
        assert!(item.kind_hint.is_none());
    }

    /// 结构键原样保留，且序列化后（详情回传 UI 的形态）不丢。
    #[test]
    fn reason_item_preserves_rule_id_and_kind_hint() {
        let item = ReasonItem::from_json(&serde_json::json!({
            "reason": "step_freq 40-300",
            "complete": true,
            "ruleId": "step_freq",
            "type": "rule"
        }));
        assert_eq!(item.rule_id.as_deref(), Some("step_freq"));
        assert_eq!(item.kind_hint.as_deref(), Some("rule"));
        let round_trip: ReasonItem =
            serde_json::from_value(serde_json::to_value(&item).expect("serialize"))
                .expect("deserialize");
        assert_eq!(round_trip.rule_id, item.rule_id);
        assert_eq!(round_trip.kind_hint, item.kind_hint);
    }

    /// 字段缺失不 panic：reasonList 不是数组 / 项里没有 reason。
    #[test]
    fn reason_list_tolerates_missing_fields() {
        assert!(ReasonItem::from_list(None).is_empty());
        assert!(ReasonItem::from_list(Some(&serde_json::json!("oops"))).is_empty());
        let list = ReasonItem::from_list(Some(&serde_json::json!([{"complete": true}])));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].reason, "");
    }
}
