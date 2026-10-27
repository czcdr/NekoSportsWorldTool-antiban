//! 学期完成度。
//! 汇总：POST /api/v55/runnings/recordssummary/semester {}；
//! 个人完成度：POST /api/v41/running/getPersonalSemesterInfo {}（结构按实际响应落地）。

use super::client::ApiClient;
use serde_json::{json, Value};

pub const SUMMARY_PATH: &str = "/api/v55/runnings/recordssummary/semester";
pub const PERSONAL_PATH: &str = "/api/v41/running/getPersonalSemesterInfo";

/// 学期汇总（B1 实测字段）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SemesterSummary {
    pub sname: String,
    pub semester_dis: f64,
    pub semester_valid_dis: f64,
    pub semester_count: i64,
    pub semester_valid_count: i64,
}

#[allow(dead_code)]
pub struct SemesterResult {
    pub summary: Option<SemesterSummary>,
    /// getPersonalSemesterInfo 原始 data（结构未定型，全量落地供核对）。
    pub personal_raw: Value,
}

/// 次数是否可信：`10000` 是业务成功码，出现在计数里说明取到了错误字段/错误对象。
///
/// 一学期有效次数不可能上千：`0..=1000` 之外一律当"没拿到"（宁可显示 0，
/// 也不要把 10000 这种码当成次数展示 —— 那会让人直接不信整个页面）。
pub fn plausible_count(value: i64) -> Option<i64> {
    (0..=1000).contains(&value).then_some(value)
}

/// 从服务端对象里取一个"次数"字段，取到不合理的值就当没有（并留一条日志）。
fn count_field(source: &serde_json::Value, key: &str, log: &mut dyn FnMut(&str)) -> Option<i64> {
    let raw = source.get(key).and_then(|v| v.as_i64())?;
    match plausible_count(raw) {
        Some(n) => Some(n),
        None => {
            log(&format!(
                "[semester] {key}={raw} 不像次数（10000 是业务成功码），已按 0 处理"
            ));
            None
        }
    }
}

pub fn query(client: &mut ApiClient, log: &mut dyn FnMut(&str)) -> Result<SemesterResult, String> {
    // 汇总
    let biz = client.call("POST", SUMMARY_PATH, "{}", &[])?;
    let data = super::client::parse_data_field(&biz);
    let mut summary = SemesterSummary {
        sname: data
            .get("sname")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        semester_dis: data
            .get("semesterDis")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        semester_valid_dis: data
            .get("semesterValidDis")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        semester_count: plausible_count(
            data.get("semesterCount")
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
        )
        .unwrap_or(0),
        semester_valid_count: plausible_count(
            data.get("semesterValidCount")
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
        )
        .unwrap_or(0),
    };
    let personal_raw = match client.call(
        "POST",
        PERSONAL_PATH,
        &json!({ "runMode": 1 }).to_string(),
        &[],
    ) {
        Ok(biz) => super::client::parse_data_field(&biz),
        Err(e) => {
            log(&format!("[semester] 个人完成度接口失败（不影响汇总）: {e}"));
            Value::Null
        }
    };
    if summary.sname.is_empty() {
        summary.sname = personal_raw
            .get("sname")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
    }

    // 部分学校汇总接口计数恒为 0，有效次数退回个人完成度接口
    if summary.semester_valid_count == 0 {
        if let Some(n) = count_field(&personal_raw, "semesterValid", log) {
            summary.semester_valid_count = n;
        }
    }

    // 目标次数仅存在于完成状态接口（需最近一条记录的 rrid）
    if summary.semester_count == 0 {
        let rrid = super::records::fetch_records(client)
            .ok()
            .and_then(|rows| rows.first().map(|r| r.rrid));
        if let Some(rrid) = rrid {
            let body = json!({ "rrid": rrid }).to_string();
            if let Ok(biz) = client.call("POST", super::user::SEMESTER_COMPLETED_PATH, &body, &[]) {
                let done = super::client::parse_data_field(&biz);
                if let Some(n) = count_field(&done, "semesterGoalCount", log) {
                    summary.semester_count = n;
                }
                if summary.semester_valid_count == 0 {
                    if let Some(n) = count_field(&done, "semesterValidCount", log) {
                        summary.semester_valid_count = n;
                    }
                }
            }
        }
    }

    Ok(SemesterResult {
        summary: Some(summary),
        personal_raw,
    })
}

#[cfg(test)]
mod count_sanity_tests {
    use super::*;
    use serde_json::json;

    /// 用户实测：日志里出现「有效 10000/0 次」——10000 是业务成功码，不是次数。
    #[test]
    fn business_success_code_is_never_shown_as_a_count() {
        // 10000 本身不可信；0..=1000 才是可能的次数
        assert_eq!(plausible_count(10000), None);
        assert_eq!(plausible_count(0), Some(0));
        assert_eq!(plausible_count(12), Some(12));
        assert_eq!(plausible_count(1000), Some(1000));
        assert_eq!(plausible_count(1001), None);
        assert_eq!(plausible_count(-1), None);

        // 服务端把成功码塞进计数字段时，取数要把它丢掉并留日志
        let mut logs: Vec<String> = Vec::new();
        let data = json!({ "error": 10000, "semesterValid": 10000 });
        let mut log = |line: &str| logs.push(line.to_string());
        let dropped = count_field(&data, "semesterValid", &mut log);
        assert_eq!(dropped, None);
        drop(log);
        assert!(
            logs.iter()
                .any(|l| l.contains("10000") && l.contains("不像次数")),
            "必须留一条日志说明为什么丢：{logs:?}"
        );

        // 正常响应照常读
        let healthy = json!({ "error": 10000, "semesterValid": 7 });
        let mut quiet = |_: &str| {};
        assert_eq!(count_field(&healthy, "semesterValid", &mut quiet), Some(7));
    }

    /// 汇总接口的两次计数也要过同一道闸（这里只验证纯函数，不发请求）。
    #[test]
    fn summary_counts_use_the_same_gate() {
        for (raw, want) in [(10000i64, 0i64), (0, 0), (15, 15), (999, 999)] {
            assert_eq!(plausible_count(raw).unwrap_or(0), want, "raw={raw}");
        }
    }
}
