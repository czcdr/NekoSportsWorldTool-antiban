//! 记录三态判定：**达标 / 未达标 / 通过但异常**。
//!
//! # 为什么必须分开
//! `review/SERVER-EVIDENCE.md` 里那条被封记录：点位 / 距离 / 时长 / 步频 / 配速 / 人脸
//! **规则项全部打勾**，详情页却仍有一行 ✗「运动数据有问题」，成绩被判「可疑」。也就是说
//! 服务端有两层判定：
//!
//! - **规则层**：距离、时长、步频、配速、点位、人脸 —— 没过就是「未达标」，用户看得懂，
//!   改了参数就能过，**不是风控信号**；
//! - **规则之外**：轨迹形态 / 参数一致性 / 数据可疑 —— 规则全过也可能被标记，这才是
//!   真正该让人警觉的东西。
//!
//! 旧界面把这两种都画成一行「×」，于是「规则没过」和「被风控标记」长得一模一样，
//! 最重要的信号被淹没。这里给出唯一的三态结论，详情页与列表徽标共用同一份判断。
//!
//! # 判定顺序（这也是唯一的口径）
//! 1. 顶层 `complete=false`，或**规则项**里任一条 `complete=false` → [`RecordStatus::Failed`]；
//! 2. 否则（规则全过）`reasonList` 里出现**非规则项** → [`RecordStatus::PassedButAnomalous`]；
//! 3. 否则 → [`RecordStatus::Passed`]。
//!
//! 注意两头都不能省：只按顶层 `complete` 判会漏掉「通过但异常」；只按 `reasonList`
//! 有没有失败项判，会把「规则全过但被标记」也算成未达标（= 丢掉风控信号）。
//!
//! # 规则项 vs 非规则项：先结构，后文案
//! 见 [`item_kind`]。结构优先是硬要求：能按字段判就不许按字符串猜；文案匹配只是
//! 实测 payload 缺少结构键时的**退路**，并在 [`kind_from_text`] 里写明。

use crate::api::records::ReasonItem;

/// 一条判定属于哪一类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasonKind {
    /// 规则项（距离 / 时长 / 步频 / 配速 / 点位 / 人脸…）：未达标由它决定。
    Rule,
    /// 非规则项（数据可疑 / 轨迹异常 / 参数异常…）：规则全过时的风控信号。
    NonRule,
    /// 认不出来。**不算异常**（不当狼来了），但界面上要如实标出来，不能假装它是规则项。
    Unclassified,
}

/// 记录整体结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordStatus {
    /// 规则全过，也没有非规则标记。
    Passed,
    /// 规则没过。普通状态，**不标红**。
    Failed,
    /// 规则全过，但被规则之外的东西标记了。**这才是风控信号，标红。**
    PassedButAnomalous,
}

impl RecordStatus {
    /// 展示用的短标签（也是 UI 测试用来找控件的那串字）。
    pub fn label(self) -> &'static str {
        match self {
            RecordStatus::Passed => "达标",
            RecordStatus::Failed => "未达标",
            RecordStatus::PassedButAnomalous => "通过但异常",
        }
    }

    /// 是否属于「规则全过仍被标记」—— 只有这一种才允许标红。
    pub fn is_anomalous(self) -> bool {
        matches!(self, RecordStatus::PassedButAnomalous)
    }

    /// 稳定的消息编码（后台线程 → UI 只传这三个 ASCII 词，不传中文）。
    pub fn code(self) -> &'static str {
        match self {
            RecordStatus::Passed => "passed",
            RecordStatus::Failed => "failed",
            RecordStatus::PassedButAnomalous => "anomalous",
        }
    }

    /// [`Self::code`] 的逆运算；认不出的编码返回 `None`（坏消息当没收到）。
    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "passed" => Some(RecordStatus::Passed),
            "failed" => Some(RecordStatus::Failed),
            "anomalous" => Some(RecordStatus::PassedButAnomalous),
            _ => None,
        }
    }
}

/// 规则项文案特征。
///
/// ⚠️ 这是 [`item_kind`] 的**退路**，只在条目没有规则标识字段时才用；能拿到
/// `ruleId` / `type` 就不许走这里（见 [`item_kind`] 的选择顺序）。
pub const RULE_KEYWORDS: &[&str] = &[
    "点位", "公里", "时长", "分钟", "步频", "配速", "人脸", "距离", "有效", "速度", "km", "spm",
];

/// 非规则（风控/数据）文案特征。同样是退路。
///
/// 收录的是服务端已知的措辞：「运动数据有问题」（详情页那行 ✗）、
/// 「APP参数数据异常201」（7 天后审核意见）、以及同族说法。
pub const ANOMALY_KEYWORDS: &[&str] = &[
    "运动数据",
    "数据异常",
    "数据有问题",
    "参数异常",
    "参数数据",
    "轨迹异常",
    "可疑",
    "异常",
    "违规",
    "作弊",
];

/// 一条判定的类别：**先看结构，结构确实没有区分时才退回文案**。
pub fn item_kind(item: &ReasonItem) -> ReasonKind {
    // ① 结构：条目自带规则标识（ruleId / rule / ruleCode / ruleKey）→ 就是规则项。
    if item
        .rule_id
        .as_deref()
        .is_some_and(|id| !id.trim().is_empty())
    {
        return ReasonKind::Rule;
    }
    // ② 结构：条目自带类型（type / kind / category）。只认明确的取值；认不出的
    //    类型值不能当判据，否则又变成猜。
    if let Some(kind) = item.kind_hint.as_deref().and_then(kind_from_hint) {
        return kind;
    }
    // ③ 退路：实测 payload 只有 `reason` + `complete`，没有上面的结构键，
    //    这时才允许按文案判断（`kind_from_text` 里写明兜底范围）。
    kind_from_text(&item.reason)
}

/// 结构键取值 → 类别；认不出返回 `None`（交给文案退路）。
fn kind_from_hint(hint: &str) -> Option<ReasonKind> {
    let h = hint.trim().to_ascii_lowercase();
    if h.is_empty() {
        return None;
    }
    if h.contains("rule") || h.contains("criteri") || h.contains("requirement") {
        return Some(ReasonKind::Rule);
    }
    if h.contains("anomal")
        || h.contains("abnormal")
        || h.contains("suspect")
        || h.contains("risk")
        || h.contains("flag")
        || h.contains("data")
    {
        return Some(ReasonKind::NonRule);
    }
    None
}

/// **退路**：按文案判断类别。
///
/// 只在 `reasonList` 条目没有规则标识/类型时使用（当前服务端实测就是这个形态）。
/// 先查风控词：「配速数据异常」这种既含规则词又含异常词的，必须判成异常，
/// 否则风控信号会被规则词吃掉。都不匹配 → [`ReasonKind::Unclassified`]：
/// 宁可如实说「认不出」，也不硬塞进规则项去冒充「达标」，更不冒充红色告警。
fn kind_from_text(text: &str) -> ReasonKind {
    let t = text.trim().to_lowercase();
    if t.is_empty() {
        return ReasonKind::Unclassified;
    }
    if ANOMALY_KEYWORDS.iter().any(|k| t.contains(k)) {
        return ReasonKind::NonRule;
    }
    if RULE_KEYWORDS.iter().any(|k| t.contains(k)) {
        return ReasonKind::Rule;
    }
    ReasonKind::Unclassified
}

/// 三态判定的**纯函数**：输入顶层 `complete` 和 `reasonList`，输出结论。
///
/// 无 IO、无 egui、无全局状态 —— 详情页、列表徽标（B 档）与测试共用这一份口径。
pub fn classify(complete: bool, reasons: &[ReasonItem]) -> RecordStatus {
    // 规则层没过：顶层说没过，或有规则项明确失败（服务端偶尔顶层 complete=true
    // 但规则项里有一条 false，与 `Verification::from_result` 同口径：显式失败优先）。
    let rule_failed = reasons
        .iter()
        .any(|r| !r.complete && item_kind(r) == ReasonKind::Rule);
    if !complete || rule_failed {
        return RecordStatus::Failed;
    }
    // 规则全过，却有非规则项 → 规则之外被单独标记，这才是风控信号。
    if reasons.iter().any(|r| item_kind(r) == ReasonKind::NonRule) {
        return RecordStatus::PassedButAnomalous;
    }
    RecordStatus::Passed
}

/// 取出 `reasonList`（详情 JSON 或列表 JSON 都可以）。
pub fn reasons_of(raw: &serde_json::Value) -> Vec<ReasonItem> {
    ReasonItem::from_list(raw.get("reasonList"))
}

/// 详情 JSON → 结论。
///
/// 返回 `None` = 这份数据里**根本没有判定信息**（例如拉详情失败时只有
/// `statusInfo` 错误文案的空壳）。这时不猜状态：既不说达标，也不说未达标。
/// 顶层 `complete` 缺失但确有 `reasonList` 时按「未达标」处理 —— 服务端没说达标，
/// 就不能当成达标（与 `fetch_detail` 里 `unwrap_or(false)` 一致）。
pub fn classify_detail(raw: &serde_json::Value) -> Option<RecordStatus> {
    let complete = raw.get("complete").and_then(|v| v.as_bool());
    let reasons = reasons_of(raw);
    if complete.is_none() && reasons.is_empty() {
        return None;
    }
    Some(classify(complete.unwrap_or(false), &reasons))
}

/// 非规则项（风控信号）。
pub fn anomaly_reasons(reasons: &[ReasonItem]) -> Vec<&ReasonItem> {
    reasons
        .iter()
        .filter(|r| item_kind(r) == ReasonKind::NonRule)
        .collect()
}

/// 认不出来的条目数（界面上如实提示，不参与三态判定）。
pub fn unclassified_count(reasons: &[ReasonItem]) -> usize {
    reasons
        .iter()
        .filter(|r| item_kind(r) == ReasonKind::Unclassified)
        .count()
}

/// 展示顺序：非规则项置顶 → 认不出的 → 规则项。组内保持服务端原顺序（稳定排序）。
pub fn sorted_for_display(reasons: &[ReasonItem]) -> Vec<ReasonItem> {
    let rank = |kind: ReasonKind| match kind {
        ReasonKind::NonRule => 0,
        ReasonKind::Unclassified => 1,
        ReasonKind::Rule => 2,
    };
    let mut out = reasons.to_vec();
    out.sort_by_key(|r| rank(item_kind(r)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(text: &str, complete: bool) -> ReasonItem {
        ReasonItem {
            reason: text.into(),
            complete,
            ..Default::default()
        }
    }

    fn anomalous(text: &str) -> ReasonItem {
        ReasonItem {
            reason: text.into(),
            complete: false,
            ..Default::default()
        }
    }

    /// 一整套真实形态的规则项（来自 SERVER-EVIDENCE 的截图原文）。
    fn passing_rules() -> Vec<ReasonItem> {
        vec![
            rule("按顺序通过所有点位", true),
            rule("最少要跑 2.00 公里", true),
            rule("跑 14 分钟以上", true),
            rule("步频 40–300", true),
            rule("配速 3'00\"–10'00\"", true),
            rule("人脸验证", true),
        ]
    }

    /// ① 达标：规则全过，没有额外标记。
    #[test]
    fn passed_when_every_rule_passes_and_nothing_else_is_flagged() {
        assert_eq!(
            classify(true, &passing_rules()),
            RecordStatus::Passed,
            "规则全过且无额外项必须是达标"
        );
        assert!(!RecordStatus::Passed.is_anomalous());
    }

    /// ② 未达标：顶层 complete=false。**不标红**（不是风控信号）。
    #[test]
    fn failed_when_rule_layer_did_not_pass_and_never_red() {
        let mut reasons = passing_rules();
        reasons[1] = rule("最少要跑 2.00 公里", false);
        let status = classify(false, &reasons);
        assert_eq!(status, RecordStatus::Failed);
        assert_eq!(status.label(), "未达标");
        assert!(!status.is_anomalous(), "规则没过不是风控信号，绝不能标红");
        // 顶层 complete=false、reasonList 为空也要判未达标。
        assert_eq!(classify(false, &[]), RecordStatus::Failed);
    }

    /// 顶层 complete=true 但规则项里有明确失败 → 显式失败优先（与 Verification 同口径）。
    #[test]
    fn explicit_rule_failure_beats_contradictory_top_level_flag() {
        let mut reasons = passing_rules();
        reasons[3] = rule("步频 40–300", false);
        assert_eq!(classify(true, &reasons), RecordStatus::Failed);
    }

    /// ③ 通过但异常：规则全过，但多一行非规则项（「运动数据有问题」）。
    /// 这是 SERVER-EVIDENCE 里那条被封记录的关键信号，必须标红。
    #[test]
    fn passed_but_anomalous_when_non_rule_item_appears_after_all_rules_pass() {
        let mut reasons = passing_rules();
        reasons.push(anomalous("运动数据有问题"));
        let status = classify(true, &reasons);
        assert_eq!(status, RecordStatus::PassedButAnomalous);
        assert_eq!(status.label(), "通过但异常");
        assert!(status.is_anomalous(), "规则全过仍被标记才是风控信号");
        // 服务端审核意见的另一种措辞也要认得。
        let mut other = passing_rules();
        other.push(anomalous("APP参数数据异常201"));
        assert_eq!(classify(true, &other), RecordStatus::PassedButAnomalous);
    }

    /// ④ 字段缺失：不 panic、不乱猜。
    #[test]
    fn missing_fields_degrade_without_guessing() {
        // 完全没有判定信息（拉详情失败的空壳）→ 不给结论。
        let stub = serde_json::json!({ "rrid": 42, "statusInfo": "详情拉取失败：超时" });
        assert_eq!(classify_detail(&stub), None);
        // 有 complete 没有 reasonList → 按 complete 判。
        assert_eq!(
            classify_detail(&serde_json::json!({ "complete": true })),
            Some(RecordStatus::Passed)
        );
        // 有 reasonList 没有 complete → 服务端没说达标，不能当达标。
        assert_eq!(
            classify_detail(&serde_json::json!({
                "reasonList": [{ "reason": "配速 3'00\"–10'00\"", "complete": true }]
            })),
            Some(RecordStatus::Failed)
        );
        // reason 字段为空 / 认不出的文案 → 认不出，不是异常（不当狼来了）。
        let unknown = vec![rule("", false), rule("重复路段过多", false)];
        assert_eq!(unclassified_count(&unknown), 2);
        assert_eq!(
            classify(true, &unknown),
            RecordStatus::Passed,
            "认不出的条目不能凭空变成红色告警"
        );
        assert!(anomaly_reasons(&unknown).is_empty());
    }

    /// 结构优先：条目带规则标识时，即使文案里有风控词也按规则项算（不靠文案猜）。
    #[test]
    fn structure_fields_win_over_text_matching() {
        let structured = ReasonItem {
            reason: "运动数据有问题".into(),
            complete: true,
            rule_id: Some("data_check".into()),
            kind_hint: None,
        };
        assert_eq!(item_kind(&structured), ReasonKind::Rule);
        assert_eq!(
            classify(true, &[structured]),
            RecordStatus::Passed,
            "结构说是规则项，就不能按文案判成异常"
        );
        // 类型字段同样优先于文案。
        let typed = ReasonItem {
            reason: "配速 3'00\"–10'00\"".into(),
            complete: false,
            rule_id: None,
            kind_hint: Some("anomaly".into()),
        };
        assert_eq!(item_kind(&typed), ReasonKind::NonRule);
        assert_eq!(classify(true, &[typed]), RecordStatus::PassedButAnomalous);
        // 认不出的类型值不硬判，退回文案。
        let vague = ReasonItem {
            reason: "步频 40–300".into(),
            complete: true,
            rule_id: None,
            kind_hint: Some("7".into()),
        };
        assert_eq!(item_kind(&vague), ReasonKind::Rule);
    }

    /// 异常项置顶：非规则项排在最前，组内保持原顺序。
    #[test]
    fn non_rule_items_are_hoisted_to_the_top_for_display() {
        let mut reasons = passing_rules();
        reasons.insert(2, anomalous("运动数据有问题"));
        let ordered = sorted_for_display(&reasons);
        assert_eq!(ordered[0].reason, "运动数据有问题");
        assert_eq!(ordered[1].reason, "按顺序通过所有点位");
        assert_eq!(ordered.len(), reasons.len());
    }

    /// 列表徽标（B 档）用的同一入口：`is_anomalous` 只有一种情况为真。
    #[test]
    fn only_passed_but_anomalous_is_flagged_for_badges() {
        let flags: Vec<bool> = [
            RecordStatus::Passed,
            RecordStatus::Failed,
            RecordStatus::PassedButAnomalous,
        ]
        .iter()
        .map(|s| s.is_anomalous())
        .collect();
        assert_eq!(flags, vec![false, false, true]);
    }
}
