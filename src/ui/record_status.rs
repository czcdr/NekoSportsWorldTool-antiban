//! 记录判定：**达标 / 未达标 / 通过但异常 / 通过（含未识别）**。
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
//! 最重要的信号被淹没。这里给出唯一结论，详情页与列表徽标共用同一份判断。
//!
//! # 判定顺序（这也是唯一的口径）
//! 1. 顶层 `complete=false`，或**规则项**里任一条明确 `false` → [`RecordStatus::Failed`]；
//! 2. 否则（规则全过）出现**风控信号**（非规则项 + 该条未通过 + 文案不是「正常/无异常」）
//!    → [`RecordStatus::PassedButAnomalous`]；
//! 3. 否则有**认不出来源**的条目 → [`RecordStatus::PassedUnverified`]
//!    （**不是**「达标」：认不出就不能替服务端下肯定结论）；
//! 4. 否则 → [`RecordStatus::Passed`]。
//!
//! # 三条不许犯的错（都踩过）
//! - **不能把 `None` 当 `false`**：`complete` 缺失是「没写」，不是「判负」。折叠会把
//!   一条好好的记录打成「未达标」（见 `classify_detail_parts`）。
//! - **不能靠关键词就把风控项当规则项**：`「配速与时长不自洽」` 同时含规则词和异常语义，
//!   异常词必须优先（见 [`kind_from_text`]）。
//! - **不能不做极性判断**：`「运动数据正常」/「无异常」/「无违规记录」` 不是风控信号
//!   （见 [`is_negated`]）。
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
    /// 非规则项（数据可疑 / 轨迹异常 / 参数不一致…）：规则全过时的风控信号来源。
    NonRule,
    /// 认不出来。**不算异常**（不当狼来了），但也**不许**因此给出「达标」。
    Unclassified,
}

/// 记录整体结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordStatus {
    /// 规则全过，没有风控信号，也没有认不出的条目。
    Passed,
    /// 规则没过。普通状态，**不标红**。
    Failed,
    /// 规则全过，但被规则之外的东西标记了。**这才是风控信号，标红。**
    PassedButAnomalous,
    /// 规则全过、也没有风控信号，但**有条目认不出来源**。
    ///
    /// 这不是「达标」：我们没法确认那些条目是什么。也不标红：认不出不等于被判可疑。
    PassedUnverified,
}

impl RecordStatus {
    /// 展示用的完整标签（详情页结论行、UI 测试用来找控件的那串字）。
    pub fn label(self) -> &'static str {
        match self {
            RecordStatus::Passed => "达标",
            RecordStatus::Failed => "未达标",
            RecordStatus::PassedButAnomalous => "通过但异常",
            RecordStatus::PassedUnverified => "通过（含未识别）",
        }
    }

    /// 列表徽标用的短标签（表格一列、手机卡片一行都要塞得下）。
    pub fn short_label(self) -> &'static str {
        match self {
            RecordStatus::Passed => "达标",
            RecordStatus::Failed => "未达标",
            RecordStatus::PassedButAnomalous => "异常",
            RecordStatus::PassedUnverified => "含未识别",
        }
    }

    /// 是否属于「规则全过仍被标记」—— 只有这一种才允许标红。
    pub fn is_anomalous(self) -> bool {
        matches!(self, RecordStatus::PassedButAnomalous)
    }

    /// 是否给了「规则层没问题」的结论（达标或含未识别）。
    pub fn is_rule_layer_clean(self) -> bool {
        matches!(
            self,
            RecordStatus::Passed
                | RecordStatus::PassedUnverified
                | RecordStatus::PassedButAnomalous
        )
    }

    /// 稳定的消息编码（后台线程 → UI 只传 ASCII 词，不传中文）。
    pub fn code(self) -> &'static str {
        match self {
            RecordStatus::Passed => "passed",
            RecordStatus::Failed => "failed",
            RecordStatus::PassedButAnomalous => "anomalous",
            RecordStatus::PassedUnverified => "unverified",
        }
    }

    /// [`Self::code`] 的逆运算；认不出的编码返回 `None`（坏消息当没收到）。
    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "passed" => Some(RecordStatus::Passed),
            "failed" => Some(RecordStatus::Failed),
            "anomalous" => Some(RecordStatus::PassedButAnomalous),
            "unverified" => Some(RecordStatus::PassedUnverified),
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

/// 非规则（风控/一致性/轨迹形态）文案特征。同样是退路。
///
/// 收录三类已知措辞：
/// - 服务端原始提示：「运动数据有问题」（详情页那行 ✗）、「APP参数数据异常201」（审核意见）；
/// - **参数一致性**：「配速与时长不自洽」「步频数据不合理」—— 这类即使含规则词也必须算风控，
///   否则规则全过却被标记的记录会被降级成「未达标」（见 [`kind_from_text`] 的检查顺序）；
/// - **轨迹形态**：「重复路段过多」「与已有边重合」—— SERVER-EVIDENCE 里那条被封记录的第一层触发。
pub const ANOMALY_KEYWORDS: &[&str] = &[
    // 服务端原始措辞
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
    // 「非正常」不含「异常」二字，但意思就是异常。复审 N1：`「数据非正常」` 曾落到
    // Unclassified，于是只显示「含未识别」而没有标红。
    "非正常",
    // 参数一致性 / 自洽性
    "不自洽",
    "不合理",
    "不连续",
    "不匹配",
    "跳变",
    "自相矛盾",
    "失真",
    // 轨迹形态
    "重复路段",
    "重复段",
    "重合",
    "折返异常",
];

/// 「正常/没问题」的结论词：出现即说明这一行**不是**风控信号。
const NORMAL_MARKERS: &[&str] = &["正常", "无误", "没有问题", "已恢复", "符合要求"];

/// 紧贴在异常词前面时构成否定的词。
///
/// `"不"` 必须有：`「不符合要求」`、`「不异常」` 都靠它。复审 N1 发现缺了它时，
/// `「不符合要求」` 会因为含 `「符合要求」` 子串被判成正常性陈述。
const NEGATIONS: &[&str] = &["无", "没有", "未", "非", "不存在", "通过", "不"];

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
///
/// 「先看异常再规则」是有意的：`rule_anomaly` 这种值两边都能匹配，
/// 风控语义优先 —— 宁可多看一眼，也不要把标记吞掉。
fn kind_from_hint(hint: &str) -> Option<ReasonKind> {
    let h = hint.trim().to_ascii_lowercase();
    if h.is_empty() {
        return None;
    }
    if h.contains("anomal")
        || h.contains("abnormal")
        || h.contains("suspect")
        || h.contains("risk")
        || h.contains("flag")
    {
        return Some(ReasonKind::NonRule);
    }
    if h.contains("rule") || h.contains("criteri") || h.contains("requirement") {
        return Some(ReasonKind::Rule);
    }
    // 只认 `data_*` 这种明确的「数据类」标记；裸 `data` 会把 `metadata` 之类误判成风控。
    if h.starts_with("data_") || h.starts_with("data-") || h == "data_check" {
        return Some(ReasonKind::NonRule);
    }
    None
}

/// **退路**：按文案判断类别。
///
/// 只在 `reasonList` 条目没有规则标识/类型时使用（当前服务端实测就是这个形态）。
/// **异常词优先于规则词**：「配速与时长不自洽」同时命中 `配速`（规则词）和 `不自洽`
/// （风控语义），先查规则词就会把风控降级成「未达标」，这正是要修的 bug。
/// 都不匹配 → [`ReasonKind::Unclassified`]：如实说「认不出」，既不冒充规则项，也不冒充告警。
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

/// 这句话是不是「正常/没问题」的陈述。
///
/// 判据必须比「含『正常』二字」严格得多：复审 N1 抓到过一个**回归** ——
/// `「运动数据异常，其余正常」` 因为含「正常」被判成正常性陈述，于是一条真风控信号
/// 显示成绿色「达标」（旧实现本来是红的）。原则：**宁可误报红，不可漏报**。
fn has_normal_marker(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    // ① 否定之否定：「不正常」「非正常」「未恢复」本身就是坏消息。
    if t.contains("不正常") || t.contains("非正常") || t.contains("未正常") || t.contains("未恢复")
    {
        return false;
    }
    // ② 正常词前面**直接**跟否定词：「不符合要求」「未达标」不是正常。
    if NEGATIONS.iter().any(|n| {
        NORMAL_MARKERS
            .iter()
            .any(|m| t.contains(&format!("{n}{m}")))
    }) {
        return false;
    }
    // ③ 句子里还有**未被否定、也不是「X 正常」形式**的异常词 → 整条不是正常性陈述。
    let has_real_signal = ANOMALY_KEYWORDS
        .iter()
        .any(|k| t.contains(k) && !keyword_followed_by_normal(t, k));
    if has_real_signal && !anomaly_is_negated(t) {
        return false;
    }
    NORMAL_MARKERS.iter().any(|m| t.contains(m))
}

/// 异常词后面**紧跟**正常词：`「运动数据正常」` 是在说该数据没问题，不是异常。
fn keyword_followed_by_normal(t: &str, keyword: &str) -> bool {
    t.match_indices(keyword).any(|(start, _)| {
        let rest = &t[start + keyword.len()..];
        NORMAL_MARKERS
            .iter()
            .any(|m| rest.trim_start().starts_with(m))
    })
}

/// 异常词是否全部被否定（`无异常` / `未发现违规` / `没有作弊`）。
///
/// 两条硬规则（复审 N1）：
/// - 含转折词（但/却/然而/不过）时**一律不算否定**——前后是两件事，
///   `「无重复路段，但运动数据异常」` 后半句是真信号，不能被前半句抹掉；
/// - 必须**每一处**异常词都被否定才算整条否定（旧实现「任一命中即 return true」
///   会把同句里未被否定的异常一起吞掉）。
fn anomaly_is_negated(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    if ["但", "却", "然而", "不过", "只是"]
        .iter()
        .any(|w| t.contains(w))
    {
        return false;
    }
    let mut saw = false;
    for keyword in ANOMALY_KEYWORDS {
        let mut search_from = 0;
        while let Some(offset) = t[search_from..].find(keyword) {
            let start_of_keyword = search_from + offset;
            saw = true;
            let window_start = t[..start_of_keyword]
                .char_indices()
                .rev()
                .take(3)
                .last()
                .map(|(index, _)| index)
                .unwrap_or(start_of_keyword);
            let before = &t[window_start..start_of_keyword];
            if !NEGATIONS.iter().any(|n| before.contains(n)) {
                return false; // 有一处没被否定 → 整条算真信号
            }
            search_from = start_of_keyword + keyword.len();
            if search_from >= t.len() {
                break;
            }
        }
    }
    saw
}

/// 这条判定是不是**风控信号**：非规则项 + 自己没过 + 文案不是「正常/无异常」。
///
/// 三个条件缺一不可：
/// - 只看类别会把 `complete=true` 的非规则项（服务端自己说通过了）也染红；
/// - 不看极性会让「运动数据正常」变成红色告警；
/// - 不看 `complete` 会让一条已经通过的检查冒充风控。
pub fn anomaly_signal(item: &ReasonItem) -> bool {
    item_kind(item) == ReasonKind::NonRule
        && item.complete != Some(true)
        && !has_normal_marker(&item.reason)
        && !anomaly_is_negated(&item.reason)
}

/// 判定的**纯函数**：输入顶层 `complete` 和 `reasonList`，输出结论。
///
/// 无 IO、无 egui、无全局状态 —— 详情页、列表徽标（B 档）与测试共用这一份口径。
pub fn classify(complete: bool, reasons: &[ReasonItem]) -> RecordStatus {
    // 规则层没过：顶层说没过，或有规则项**明确** false（`None` 不算：没写不等于没过）。
    let rule_failed = reasons
        .iter()
        .any(|r| r.complete == Some(false) && item_kind(r) == ReasonKind::Rule);
    if !complete || rule_failed {
        return RecordStatus::Failed;
    }
    // 规则全过，却有风控信号 → 规则之外被单独标记，这才是要标红的东西。
    if reasons.iter().any(anomaly_signal) {
        return RecordStatus::PassedButAnomalous;
    }
    // 规则项**没给结果**（`None`）也不能算通过：那是「没核对」，不是「通过」。
    // 顶层 true、规则项全是 None 时输出 Unverified，而不是替服务端宣布达标（复审 N8）。
    if reasons
        .iter()
        .any(|r| item_kind(r) == ReasonKind::Rule && r.complete.is_none())
    {
        return RecordStatus::PassedUnverified;
    }
    // 有**真正认不出来源**的条目：不能替服务端说「达标」。
    //
    // 但「没问题」「符合要求」这类**正常性陈述**不算认不出：它们明确表达了通过，
    // 若一并算作 Unverified，正常记录会显示成「通过（含未识别）」，等于用噪声
    // 掩盖真信号。只有既非规则、又非风控、也不是正常性陈述的条目才降级。
    if reasons.iter().any(|r| {
        item_kind(r) == ReasonKind::Unclassified
            && !has_normal_marker(&r.reason)
            && !anomaly_is_negated(&r.reason)
    }) {
        return RecordStatus::PassedUnverified;
    }
    RecordStatus::Passed
}

/// 取出 `reasonList`（详情 JSON 或列表 JSON 都可以）。
pub fn reasons_of(raw: &serde_json::Value) -> Vec<ReasonItem> {
    ReasonItem::from_list(raw.get("reasonList"))
}

/// 明细 + 顶层标记 → 结论；**`None` = 这份数据里没有可用的判定信息**。
///
/// 详情页与徽标回填必须走同一个入口：同一份数据在两条路径上给出不同答案，比没有答案更糟。
///
/// - 顶层 `complete` 缺失 → `None`：服务端没说达标也没说未达标，我们不猜。
///   （早先把它折叠成 `false`，于是「没写」被显示成「未达标」——那是编出来的结论。）
/// - 只有顶层标记、没有明细（`reasonList` 为空）→ 按顶层标记给结论。
pub fn classify_detail_parts(
    complete: Option<bool>,
    reasons: &[ReasonItem],
) -> Option<RecordStatus> {
    let complete = complete?;
    Some(classify(complete, reasons))
}

/// 详情 JSON → 结论（`None` 同 [`classify_detail_parts`]）。
pub fn classify_detail(raw: &serde_json::Value) -> Option<RecordStatus> {
    let complete = raw.get("complete").and_then(|v| v.as_bool());
    let reasons = reasons_of(raw);
    classify_detail_parts(complete, &reasons)
}

/// 风控信号条目（红色徽标与置顶展示都用它）。
pub fn anomaly_reasons(reasons: &[ReasonItem]) -> Vec<&ReasonItem> {
    reasons.iter().filter(|r| anomaly_signal(r)).collect()
}

/// 非规则但**不构成**风控的条目（服务端自己说通过了 / 文案是「正常」）：只做展示。
pub fn informational_reasons(reasons: &[ReasonItem]) -> Vec<&ReasonItem> {
    reasons
        .iter()
        .filter(|r| item_kind(r) == ReasonKind::NonRule && !anomaly_signal(r))
        .collect()
}

/// 明确失败了的规则项数（用来把「下方 N 项 ×」写成真话）。
pub fn failed_rule_count(reasons: &[ReasonItem]) -> usize {
    reasons
        .iter()
        .filter(|r| r.complete == Some(false) && item_kind(r) == ReasonKind::Rule)
        .count()
}

/// 认不出来的条目数（界面上如实提示，不参与风控判定，但会压掉「达标」结论）。
///
/// **口径必须与 [`classify`] 一致**（复审 N7）：那边排除了「正常性陈述」与「被否定的
/// 异常」，这里若只按 `item_kind` 数，文案里的 N 会大于实际压掉达标的条目数 ——
/// 于是「有 3 项认不出来源」可能对应 0 项实际影响，数字吓人而结论没变。
pub fn unclassified_count(reasons: &[ReasonItem]) -> usize {
    reasons
        .iter()
        .filter(|r| {
            item_kind(r) == ReasonKind::Unclassified
                && !has_normal_marker(&r.reason)
                && !anomaly_is_negated(&r.reason)
        })
        .count()
}

/// 展示顺序：风控信号置顶 → 其它非规则项 → 认不出的 → 规则项。
/// 组内保持服务端原顺序（稳定排序）。
pub fn sorted_for_display(reasons: &[ReasonItem]) -> Vec<ReasonItem> {
    let rank = |item: &ReasonItem| match item_kind(item) {
        ReasonKind::NonRule if anomaly_signal(item) => 0,
        ReasonKind::NonRule => 1,
        ReasonKind::Unclassified => 2,
        ReasonKind::Rule => 3,
    };
    let mut out = reasons.to_vec();
    out.sort_by_key(rank);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 复审 N1 的回归锁：极性判断不许把真风控信号吞成绿色「达标」。
    ///
    /// 上一版用整串 `contains` 判「正常」，于是「运动数据异常，其余正常」因为含
    /// 「正常」被判成正常性陈述 —— 一条真信号显示成绿色，而且这是相对旧实现的**回归**
    /// （旧实现本来会标红）。原则：**宁可误报红，不可漏报**。
    #[test]
    fn a_real_anomaly_is_never_swallowed_by_a_normal_word_elsewhere() {
        let rules = vec![rule("最少要跑2.00公里", true)];

        for text in [
            "运动数据异常，其余正常",     // 含「正常」，但真异常在前
            "数据非正常",                 // 「非正常」= 异常
            "无重复路段，但运动数据异常", // 转折词后是真信号
            "无违规，但轨迹异常",         // 同上
        ] {
            let mut reasons = rules.clone();
            reasons.push(anomalous(text));
            assert_eq!(
                classify(true, &reasons),
                RecordStatus::PassedButAnomalous,
                "「{text}」是真风控信号，必须标红"
            );
        }
    }

    /// 反向：真正的正常性陈述不该误报红（否则红色就没有含义了）。
    #[test]
    fn genuinely_normal_statements_stay_green() {
        let rules = vec![rule("最少要跑2.00公里", true)];
        for text in ["运动数据正常", "无异常", "没有问题", "符合要求"] {
            let mut reasons = rules.clone();
            reasons.push(ReasonItem {
                reason: text.into(),
                complete: Some(true),
                ..Default::default()
            });
            assert_eq!(
                classify(true, &reasons),
                RecordStatus::Passed,
                "「{text}」是正常性陈述，不该标红"
            );
        }
    }

    /// 「不符合要求」不是正常性陈述：它含「符合要求」子串，但前置否定。
    #[test]
    fn negated_requirement_is_not_a_normal_statement() {
        assert!(!has_normal_marker("不符合要求"));
        assert!(!has_normal_marker("未达到要求"));
        assert!(!has_normal_marker("不正常"));
        assert!(has_normal_marker("符合要求"));
        assert!(has_normal_marker("运动数据正常"));
    }

    fn item(text: &str, complete: Option<bool>) -> ReasonItem {
        ReasonItem {
            reason: text.into(),
            complete,
            ..Default::default()
        }
    }

    fn rule(text: &str, complete: bool) -> ReasonItem {
        item(text, Some(complete))
    }

    fn anomalous(text: &str) -> ReasonItem {
        item(text, Some(false))
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
        assert_eq!(classify(false, &[]), RecordStatus::Failed);
    }

    /// 顶层 complete=true 但规则项里有明确失败 → 显式失败优先（与 Verification 同口径）。
    #[test]
    fn explicit_rule_failure_beats_contradictory_top_level_flag() {
        let mut reasons = passing_rules();
        reasons[3] = rule("步频 40–300", false);
        assert_eq!(classify(true, &reasons), RecordStatus::Failed);
    }

    /// **`complete` 缺失 ≠ 判负**：没写就不能把记录打成「未达标」。
    #[test]
    fn missing_complete_is_unknown_not_failed() {
        let mut reasons = passing_rules();
        reasons[2] = item("跑 14 分钟以上", None);
        // 规则项没写通过与否：**不算失败**（不标红），但也**不算通过** ——
        // 「没核对」不能替服务端宣布达标（复审 N8）。
        assert_eq!(classify(true, &reasons), RecordStatus::PassedUnverified);
        // 顶层没写 → 不给结论（而不是「未达标」）。
        assert_eq!(classify_detail_parts(None, &reasons), None);
        assert_eq!(
            classify_detail(&serde_json::json!({
                "reasonList": [{"reason": "步频 40–300"}]
            })),
            None,
            "只有明细没有顶层标记时不许猜"
        );
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
        let mut other = passing_rules();
        other.push(anomalous("APP参数数据异常201"));
        assert_eq!(classify(true, &other), RecordStatus::PassedButAnomalous);
    }

    /// HIGH-1：含规则词但语义是「不自洽」的条目，必须算风控，不能被规则词吃掉。
    #[test]
    fn rule_sounding_anomalies_are_not_downgraded_to_failed() {
        for text in [
            "配速与时长不自洽",
            "步频数据不合理",
            "点位跳变",
            "距离增长不连续",
        ] {
            let mut reasons = passing_rules();
            reasons.push(anomalous(text));
            assert_eq!(
                item_kind(&anomalous(text)),
                ReasonKind::NonRule,
                "「{text}」是参数一致性/轨迹形态问题，属风控层"
            );
            let status = classify(true, &reasons);
            assert_eq!(
                status,
                RecordStatus::PassedButAnomalous,
                "「{text}」被降级成未达标就是吞掉了风控信号"
            );
            assert!(status.is_anomalous());
        }
    }

    /// HIGH-2：认不出的条目不许变成绿色「达标」。
    #[test]
    fn unclassified_items_block_a_clean_pass() {
        let mut reasons = passing_rules();
        reasons.push(item("重复路段过多", Some(false))); // 现在有关键词，先确认它算风控
        assert_eq!(item_kind(&reasons[6]), ReasonKind::NonRule);
        // 真正认不出的句式：
        let mut odd = passing_rules();
        odd.push(item("第七项校验", Some(false)));
        assert_eq!(item_kind(&odd[6]), ReasonKind::Unclassified);
        let status = classify(true, &odd);
        assert_eq!(status, RecordStatus::PassedUnverified);
        assert_eq!(status.short_label(), "含未识别");
        assert!(!status.is_anomalous(), "认不出不等于被判可疑，不许标红");
        assert_ne!(status, RecordStatus::Passed, "不能替服务端下「达标」的结论");
    }

    /// HIGH-3：极性判断 —— 「正常/无异常/无违规」不是风控信号。
    #[test]
    fn normal_and_negated_statements_are_not_anomalies() {
        for text in [
            "运动数据正常",
            "无异常",
            "无违规记录",
            "未发现异常",
            "没问题，符合要求",
        ] {
            let mut reasons = passing_rules();
            reasons.push(item(text, Some(false)));
            assert!(!anomaly_signal(&reasons[6]), "「{text}」不该被当成风控信号");
            assert_eq!(
                classify(true, &reasons),
                RecordStatus::Passed,
                "「{text}」说明没问题，结论应保持达标"
            );
        }
        // 但「不正常」是否定之否定，仍然是异常。
        assert!(anomaly_signal(&item("运动数据不正常", Some(false))));
        // 否定词在异常词后面不构成否定。
        assert!(anomaly_signal(&item("数据异常，无有效轨迹", Some(false))));
    }

    /// 非规则项如果自己 `complete=true`，只做展示，不升级成红色风控。
    #[test]
    fn passing_non_rule_item_is_informational_only() {
        let mut reasons = passing_rules();
        reasons.push(item("运动数据检查", Some(true)));
        assert_eq!(item_kind(&reasons[6]), ReasonKind::NonRule);
        assert!(!anomaly_signal(&reasons[6]));
        assert_eq!(classify(true, &reasons), RecordStatus::Passed);
        assert!(anomaly_reasons(&reasons).is_empty());
        assert_eq!(informational_reasons(&reasons).len(), 1);
    }

    /// 字段缺失：不 panic、不乱猜。
    #[test]
    fn missing_fields_degrade_without_guessing() {
        let stub = serde_json::json!({ "rrid": 42, "statusInfo": "详情拉取失败：超时" });
        assert_eq!(classify_detail(&stub), None);
        assert_eq!(
            classify_detail(&serde_json::json!({ "complete": true })),
            Some(RecordStatus::Passed)
        );
        // 有明细但顶层没写 → 不猜（旧行为是折叠成 false 显示「未达标」）。
        assert_eq!(
            classify_detail(&serde_json::json!({
                "reasonList": [{ "reason": "配速 3'00\"–10'00\"", "complete": true }]
            })),
            None
        );
        // reason 为空 / 认不出的文案 → 认不出，不是异常。
        let unknown = vec![item("", Some(false)), item("重复路", Some(false))];
        assert_eq!(unclassified_count(&unknown), 2);
        assert_eq!(classify(true, &unknown), RecordStatus::PassedUnverified);
        assert!(anomaly_reasons(&unknown).is_empty());
    }

    /// 结构优先：条目带规则标识时，即使文案里有风控词也按规则项算（不靠文案猜）。
    #[test]
    fn structure_fields_win_over_text_matching() {
        let structured = ReasonItem {
            reason: "运动数据有问题".into(),
            complete: Some(true),
            rule_id: Some("data_check".into()),
            kind_hint: None,
        };
        assert_eq!(item_kind(&structured), ReasonKind::Rule);
        assert_eq!(
            classify(true, &[structured]),
            RecordStatus::Passed,
            "结构说是规则项，就不能按文案判成异常"
        );
        // 类型字段同样优先于文案；且 `rule_anomaly` 这种两边都像的值按风控处理。
        let typed = ReasonItem {
            reason: "配速 3'00\"–10'00\"".into(),
            complete: Some(false),
            rule_id: None,
            kind_hint: Some("rule_anomaly".into()),
        };
        assert_eq!(item_kind(&typed), ReasonKind::NonRule);
        assert_eq!(classify(true, &[typed]), RecordStatus::PassedButAnomalous);
        // 裸 `data` 不再当成风控（`metadata` 不是数据类标记）。
        assert_eq!(kind_from_hint("metadata"), None);
        assert_eq!(kind_from_hint("data_check"), Some(ReasonKind::NonRule));
        // 认不出的类型值不硬判，退回文案。
        let vague = ReasonItem {
            reason: "步频 40–300".into(),
            complete: Some(true),
            rule_id: None,
            kind_hint: Some("7".into()),
        };
        assert_eq!(item_kind(&vague), ReasonKind::Rule);
    }

    /// 异常项置顶：风控信号排最前，组内保持原顺序。
    #[test]
    fn non_rule_items_are_hoisted_to_the_top_for_display() {
        let mut reasons = passing_rules();
        reasons.insert(2, anomalous("运动数据有问题"));
        let ordered = sorted_for_display(&reasons);
        assert_eq!(ordered[0].reason, "运动数据有问题");
        assert_eq!(ordered[1].reason, "按顺序通过所有点位");
        assert_eq!(ordered.len(), reasons.len());
    }

    /// 列表徽标（B 档）用的同一入口：只有「通过但异常」是红色风控。
    #[test]
    fn only_passed_but_anomalous_is_flagged_for_badges() {
        let flags: Vec<bool> = [
            RecordStatus::Passed,
            RecordStatus::Failed,
            RecordStatus::PassedButAnomalous,
            RecordStatus::PassedUnverified,
        ]
        .iter()
        .map(|s| s.is_anomalous())
        .collect();
        assert_eq!(flags, vec![false, false, true, false]);
        for status in [
            RecordStatus::Passed,
            RecordStatus::Failed,
            RecordStatus::PassedButAnomalous,
            RecordStatus::PassedUnverified,
        ] {
            assert_eq!(RecordStatus::from_code(status.code()), Some(status));
        }
    }
}
