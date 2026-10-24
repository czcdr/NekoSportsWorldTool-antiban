//! 提交行为护栏：时段、每日条数、批量补签。
//!
//! 先说清楚**上游原本已经有什么**，避免重复造轮子：
//! - 开始时刻：跑步页「随机时刻」在该日 **7:00-20:00** 内抽样（`ui::run::RAND_HOUR_LO/HI`），
//!   「指定时刻」由用户自填；批量补签每日取 **7:00-21:00** 随机时刻（`ui::ai`）。
//! - 因此**时段本身**原本就有设计，这里不再重造一套窗口，只做三件原本不存在的事：
//!   1. 拦截「落在深夜的开始时刻/提交时刻」（指定时刻可填任意小时；凌晨用今天时随机模式会退化成"现在"）；
//!   2. 记录**每个自然日**已提交的条数，防止同日多条与一次性补齐大量历史；
//!   3. 批量补签的规模上限（天数 / 同日重复 / 单批总量）。
//!
//! 设计约束：
//! - 默认开启，规则从 `config.json` 的 `safety` 读，缺省即默认值（升级用户自动生效）；
//! - `enabled=false` 可关，关掉后所有检查直接放行；
//! - 状态文件损坏时**拒绝提交**而不是当作"今天还没跑"（fail-closed）；
//! - 计数按**记录所属的那一天**算（不是提交的那一天），否则跑步页的
//!   「今天 / 昨天 / 1-3 天前」补跑第一个就会把后面三个全堵死；
//! - **先占位再提交**：`reserve_run` 在锁内完成「校验 + 计数 +1」，提交失败由 RAII 归还。
//!   这样并发提交不可能同时读到 0 条，写盘失败也不会变成"永远 0 条"的 fail-open；
//! - 时间一律按北京时间（UTC+8），与协议层的时间口径一致。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const STATE_FILE: &str = "safety_state.json";
/// 跨进程写计数用的锁文件。
const LOCK_FILE: &str = "safety_state.lock";
/// 状态文件里保留多少天：够覆盖跑步页的 3 天回溯与补签上限即可。
const KEEP_DAYS: usize = 10;
/// 抢锁最长等待。超过就明确报错，而不是无限期挂住提交线程。
const LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const LOCK_RETRY: Duration = Duration::from_millis(50);

/// 护栏策略。字段名就是 `config.json` 里的键名。
///
/// 容器级 `#[serde(default)]`：配置里只写了一部分字段时，缺的字段回落到默认值。
/// 关键点 —— 少了它，任何"只写一半"的 safety 对象都会让整份 `config.json`
/// 反序列化失败，进而把账号、密码、学校、城市一起静默重置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SafetyPolicy {
    /// 总开关。false = 所有检查放行。
    pub enabled: bool,
    /// 深夜时段起点（含），默认 23 时。
    pub night_from: u8,
    /// 深夜时段终点（不含），默认 6 时。
    pub night_to: u8,
    /// 每个自然日允许的跑步记录条数。
    pub daily_runs: u32,
    /// 每个自然日允许的 AI 运动记录条数。
    pub daily_ai: u32,
    /// 批量补签最多能回溯多少天。
    pub backfill_days: i64,
    /// 批量补签里「同一天同项目」最多重复几次（原界面可拖到 10）。
    pub batch_per_day: u32,
    /// 单次批量补签的提交总条数上限（项目数 × 天数 × 每日次数）。
    pub batch_max_records: u32,
}

impl Default for SafetyPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            night_from: 23,
            night_to: 6,
            daily_runs: 1,
            daily_ai: 2,
            backfill_days: 3,
            batch_per_day: 2,
            batch_max_records: 6,
        }
    }
}

impl SafetyPolicy {
    /// 校验配置合法性。0 值不是"不限制"，而是配置错误 —— 要关闭请用 `enabled`。
    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in [("night_from", self.night_from), ("night_to", self.night_to)] {
            if value > 23 {
                return Err(format!("safety.{name} 必须是 0-23 时，当前 {value}"));
            }
        }
        // from == to 会让深夜判定永远为 false（静默关闭），和"0 值不是不限制"的原则冲突。
        if self.night_from == self.night_to {
            return Err(
                "safety.night_from 与 night_to 不能相同；要关闭深夜拦截请设 safety.enabled=false"
                    .into(),
            );
        }
        for (name, value) in [
            ("daily_runs", self.daily_runs),
            ("daily_ai", self.daily_ai),
            ("batch_per_day", self.batch_per_day),
            ("batch_max_records", self.batch_max_records),
        ] {
            if value == 0 {
                return Err(format!(
                    "safety.{name} 不能为 0；要关闭护栏请设 safety.enabled=false"
                ));
            }
        }
        if self.backfill_days < 0 {
            return Err("safety.backfill_days 不能为负".into());
        }
        Ok(())
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct DayCount {
    #[serde(default)]
    runs: u32,
    #[serde(default)]
    ai: u32,
}

/// 按自然日（北京时间 `YYYY-MM-DD`）分桶的计数。
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    days: BTreeMap<String, DayCount>,
}

impl State {
    fn get(&self, day: &str) -> DayCount {
        self.days.get(day).cloned().unwrap_or_default()
    }

    fn bump(&mut self, day: &str, f: impl Fn(&mut DayCount)) {
        let entry = self.days.entry(day.to_string()).or_default();
        f(entry);
        // 只保留最近 KEEP_DAYS 天（BTreeMap 按字符串日期有序）。
        while self.days.len() > KEEP_DAYS {
            match self.days.keys().next().cloned() {
                Some(oldest) => {
                    self.days.remove(&oldest);
                }
                None => break,
            }
        }
    }
}

/// 北京时间下的 (日期, 小时)。
fn cst_parts(ms: i64) -> (String, u8) {
    use chrono::{TimeZone, Timelike};
    let tz = chrono::FixedOffset::east_opt(8 * 3600).expect("UTC+8 偏移合法");
    let dt = tz.timestamp_millis_opt(ms).single().unwrap_or_else(|| {
        tz.timestamp_millis_opt(0)
            .single()
            .expect("纪元时间在 UTC+8 下必然唯一")
    });
    (dt.format("%Y-%m-%d").to_string(), dt.hour() as u8)
}

/// `[from, to)` 是否命中；`from > to` 表示跨午夜（默认 23 → 6）。
fn in_night(policy: &SafetyPolicy, hour: u8) -> bool {
    let (from, to) = (policy.night_from, policy.night_to);
    if from == to {
        // validate() 会拒绝这种配置；这里保守当作"没命中"，不会误拦。
        return false;
    }
    if from < to {
        hour >= from && hour < to
    } else {
        hour >= from || hour < to
    }
}

fn state_path(root: &Path) -> PathBuf {
    root.join(STATE_FILE)
}

fn backup_path(root: &Path) -> PathBuf {
    root.join(format!("{STATE_FILE}.bak"))
}

fn parse_state(text: &str, path: &Path) -> Result<State, String> {
    if text.trim().is_empty() {
        return Ok(State::default());
    }
    serde_json::from_str(text)
        .map_err(|_| format!("安全状态文件 {} 损坏", path.display()))
}

fn load_state_at(root: &Path) -> Result<State, String> {
    let main = state_path(root);
    let bak = backup_path(root);
    // 主文件坏了先试 .bak（崩溃时可能留下半截 JSON）。
    if let Ok(text) = std::fs::read_to_string(&main) {
        match parse_state(&text, &main) {
            Ok(state) => return Ok(state),
            Err(error) => {
                if let Ok(backup) = std::fs::read_to_string(&bak) {
                    if let Ok(state) = parse_state(&backup, &bak) {
                        return Ok(state);
                    }
                }
                // 损坏不等于"今天还没跑"：宁可拒绝，也不要悄悄放开计数。
                return Err(format!(
                    "{error}，且没有可用备份；已停止提交。请检查或删除 {} 后重试（删除会重置计数）",
                    main.display()
                ));
            }
        }
    }
    if main.exists() {
        return Err(format!("读取安全状态失败（{}）", main.display()));
    }
    Ok(State::default())
}

/// 原子写：先写 `.tmp` 再 rename，崩溃不会留下半截 JSON。
fn save_state_at(root: &Path, state: &State) -> Result<(), String> {
    std::fs::create_dir_all(root).map_err(|e| format!("创建数据目录失败：{e}"))?;
    let path = state_path(root);
    let tmp = root.join(format!("{STATE_FILE}.tmp"));
    let json =
        serde_json::to_string_pretty(state).map_err(|e| format!("序列化安全状态失败：{e}"))?;
    std::fs::write(&tmp, json).map_err(|e| format!("写入 {} 失败：{e}", tmp.display()))?;
    if path.exists() {
        // 保留上一版，供损坏时回退/人工救回。
        let _ = std::fs::copy(&path, backup_path(root));
    }
    std::fs::rename(&tmp, &path)
        .map_err(|e| format!("提交安全状态失败（{}）：{e}", path.display()))
}

/// 在跨进程文件锁内做「读 - 改 - 写」。抢锁有超时，不会无限期挂住。
fn with_lock<T>(root: &Path, action: impl FnOnce(&mut State) -> Result<T, String>) -> Result<T, String> {
    std::fs::create_dir_all(root).map_err(|e| format!("创建数据目录失败：{e}"))?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(root.join(LOCK_FILE))
        .map_err(|e| format!("无法建立安全状态锁：{e}"))?;
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(_) if Instant::now() < deadline => std::thread::sleep(LOCK_RETRY),
            Err(_) => {
                return Err(
                    "安全状态正被另一个实例写入（等待 2 秒超时），请确认没有第二个窗口在提交后重试"
                        .into(),
                )
            }
        }
    }
    let mut state = load_state_at(root)?;
    let out = action(&mut state)?;
    save_state_at(root, &state)?;
    Ok(out)
}

/// 跑步提交的额度占位。
///
/// `reserve_run` 在锁内完成「深夜校验 + 每日上限校验 + 计数 +1」，返回的凭据在
/// **提交成功**后 `commit()`；任何提前返回（网络失败、用户取消、panic 前的正常 drop）
/// 都会自动归还额度 —— 既堵住"检查与记账之间隔着一次网络请求"的竞态，
/// 也避免"写盘失败导致计数永远为 0"的 fail-open。
#[derive(Debug)]
pub struct RunReservation {
    root: PathBuf,
    day: String,
    committed: bool,
    enabled: bool,
}

impl RunReservation {
    /// 提交已被服务端接受：保留这次计数。
    pub fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for RunReservation {
    fn drop(&mut self) {
        if self.committed || !self.enabled {
            return;
        }
        // 归还失败只能记在状态上（多算一次 = 偏保守，不会放行），不影响主流程。
        let _ = with_lock(&self.root, |state| {
            if let Some(day) = state.days.get_mut(&self.day) {
                day.runs = day.runs.saturating_sub(1);
            }
            Ok(())
        });
    }
}

/// 提交跑步前占位。`start_ms` 是本次跑步的**开始时间**（记录所属的那一天）。
pub fn reserve_run(policy: &SafetyPolicy, start_ms: i64) -> Result<RunReservation, String> {
    reserve_run_at(
        &crate::platform::data_dir(),
        policy,
        start_ms,
        crate::crypto::envelope::now_ms(),
    )
}

fn reserve_run_at(
    root: &Path,
    policy: &SafetyPolicy,
    start_ms: i64,
    now_ms: i64,
) -> Result<RunReservation, String> {
    policy.validate()?;
    if !policy.enabled {
        return Ok(RunReservation {
            root: root.to_path_buf(),
            day: String::new(),
            committed: false,
            enabled: false,
        });
    }
    // 开始时间和提交时刻都要看：只改其中一个字段仍能绕过去。
    for (label, ms) in [("开始时间", start_ms), ("当前时刻", now_ms)] {
        let (_, hour) = cst_parts(ms);
        if in_night(policy, hour) {
            return Err(format!(
                "安全护栏：{label}是 {hour} 点，落在深夜 {}-{} 点。深夜提交是最容易被人工审查的时段。\n\
                 随机时刻模式本身就在 7:00-20:00 内抽样；确需深夜提交请把 config.json 的 safety.enabled 改成 false。",
                policy.night_from, policy.night_to
            ));
        }
    }
    // 计数键是**记录所属的那一天**：跑步页支持「今天 / 昨天 / 1-3 天前」，
    // 若按提交日计数，补跑第一天就会把后面三天全部堵死。
    let (day, _) = cst_parts(start_ms);
    let key = day.clone();
    with_lock(root, |state| {
        let runs = state.get(&key).runs;
        if runs >= policy.daily_runs {
            return Err(format!(
                "安全护栏：{key} 已经提交过 {runs} 次跑步（每日上限 {}）。同一自然日多条记录是最明显的异常特征。\n\
                 确需提交：把 config.json 的 safety.enabled 改成 false。",
                policy.daily_runs
            ));
        }
        state.bump(&key, |c| c.runs = c.runs.saturating_add(1));
        Ok(())
    })?;
    Ok(RunReservation {
        root: root.to_path_buf(),
        day,
        committed: false,
        enabled: true,
    })
}

/// 提交 AI 运动前的检查。`count` 是本批条数（单发固定 1）。
pub fn check_ai(policy: &SafetyPolicy, count: u32) -> Result<(), String> {
    check_ai_at(
        &crate::platform::data_dir(),
        policy,
        count,
        crate::crypto::envelope::now_ms(),
    )
}

fn check_ai_at(
    root: &Path,
    policy: &SafetyPolicy,
    count: u32,
    now_ms: i64,
) -> Result<(), String> {
    policy.validate()?;
    if !policy.enabled {
        return Ok(());
    }
    let (today, hour) = cst_parts(now_ms);
    if in_night(policy, hour) {
        return Err(format!(
            "安全护栏：现在是 {hour} 点，落在深夜 {}-{} 点，AI 运动同样会被判异常。\n\
             确需提交：把 config.json 的 safety.enabled 改成 false。",
            policy.night_from, policy.night_to
        ));
    }
    let done = load_state_at(root)?.get(&today).ai;
    if done.saturating_add(count) > policy.daily_ai {
        return Err(format!(
            "安全护栏：{today} 已提交 AI 运动 {done} 条，本批 {count} 条会超过每日上限 {}。\n\
             确需提交：把 config.json 的 safety.enabled 改成 false。",
            policy.daily_ai
        ));
    }
    Ok(())
}

/// AI 提交成功后记账（记在"现在"这一天）。
pub fn record_ai(policy: &SafetyPolicy, count: u32) -> Result<(), String> {
    record_ai_on(policy, crate::crypto::envelope::now_ms(), count)
}

/// AI 提交成功后记账，按**记录所属的那一天**分桶（批量补签要补历史日期）。
pub fn record_ai_on(policy: &SafetyPolicy, at_ms: i64, count: u32) -> Result<(), String> {
    if !policy.enabled {
        return Ok(());
    }
    let (day, _) = cst_parts(at_ms);
    let key = day;
    with_lock(&crate::platform::data_dir(), |state| {
        state.bump(&key, |c| c.ai = c.ai.saturating_add(count));
        Ok(())
    })
}

/// 只检查"当前时刻是否深夜"。
///
/// 批量补签补的是**历史日期**，不应该受"每日条数"限制（历史日期各自算各的），
/// 但它的**提交时刻**是现在，深夜提交一样会被注意到 —— 所以这一项单独暴露出来。
pub fn check_night(policy: &SafetyPolicy) -> Result<(), String> {
    policy.validate()?;
    if !policy.enabled {
        return Ok(());
    }
    let (_, hour) = cst_parts(crate::crypto::envelope::now_ms());
    if in_night(policy, hour) {
        return Err(format!(
            "安全护栏：现在是 {hour} 点，落在深夜 {}-{} 点。批量提交放到白天做。\n\
             确需提交：把 config.json 的 safety.enabled 改成 false。",
            policy.night_from, policy.night_to
        ));
    }
    Ok(())
}

/// 批量补签的规模检查：回溯天数 + 同日重复次数 + 单批总条数。
pub fn check_backfill(
    policy: &SafetyPolicy,
    days: i64,
    per_day: i64,
    total: i64,
) -> Result<(), String> {
    policy.validate()?;
    if !policy.enabled {
        return Ok(());
    }
    if days > policy.backfill_days {
        return Err(format!(
            "安全护栏：批量补签 {days} 天超过上限 {} 天。一次性补齐几十天是最典型的刷量特征。\n\
             确需提交：把 config.json 的 safety.enabled 改成 false。",
            policy.backfill_days
        ));
    }
    if per_day > i64::from(policy.batch_per_day) {
        return Err(format!(
            "安全护栏：同一项目每天重复 {per_day} 次超过上限 {} 次。同一天同项目刷多条没有真实运动能对应。\n\
             确需提交：把 config.json 的 safety.enabled 改成 false，或调大 safety.batch_per_day。",
            policy.batch_per_day
        ));
    }
    if total > i64::from(policy.batch_max_records) {
        return Err(format!(
            "安全护栏：本批共 {total} 条记录超过单批上限 {} 条（项目数 × 天数 × 每日次数）。\n\
             确需提交：把 config.json 的 safety.enabled 改成 false，或调大 safety.batch_max_records。",
            policy.batch_max_records
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("safety-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 相对于 2026-10-24 的 `days_ago` 天、指定小时（北京时间）。
    fn day(days_ago: i64, hour: u8) -> i64 {
        use chrono::{Datelike, Duration, TimeZone};
        let tz = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        let anchor = tz
            .with_ymd_and_hms(2026, 10, 24, 12, 0, 0)
            .single()
            .expect("锚点时刻合法");
        let base = anchor - Duration::days(days_ago);
        tz.with_ymd_and_hms(base.year(), base.month(), base.day(), hour as u32, 0, 0)
            .single()
            .expect("合法时刻")
            .timestamp_millis()
    }

    fn at(hour: u8) -> i64 {
        day(0, hour)
    }

    #[test]
    fn night_window_blocks_late_and_early_submissions() {
        let p = SafetyPolicy::default();
        let dir = tmp();
        for hour in [23, 0, 3, 5] {
            let err = reserve_run_at(&dir, &p, at(hour), at(hour)).unwrap_err();
            assert!(err.contains("深夜"), "{hour} 点应被拦截: {err}");
        }
        for hour in [6, 12, 18, 22] {
            assert!(
                reserve_run_at(&dir, &p, at(hour), at(hour)).is_ok(),
                "{hour} 点应放行"
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn start_time_and_now_are_both_checked() {
        let p = SafetyPolicy::default();
        let dir = tmp();
        assert!(reserve_run_at(&dir, &p, at(3), at(12))
            .unwrap_err()
            .contains("开始时间"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 核心回归：跑步页支持「今天 / 昨天 / 1-3 天前」补跑，
    /// 计数必须按**记录所属的那一天**，否则补第一天就把后面三天全堵死。
    #[test]
    fn backfill_across_different_dates_is_not_blocked() {
        let p = SafetyPolicy::default();
        let dir = tmp();
        let now = at(12);
        for days_ago in [3i64, 2, 1, 0] {
            let start = day(days_ago, 12);
            let reservation = reserve_run_at(&dir, &p, start, now)
                .unwrap_or_else(|e| panic!("{days_ago} 天前应放行: {e}"));
            reservation.commit();
        }
        let err = reserve_run_at(&dir, &p, day(0, 13), now).unwrap_err();
        assert!(err.contains("上限 1"), "{err}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 提交失败（未 commit）必须归还额度，否则一次网络失败会占掉当天名额。
    #[test]
    fn failed_submission_releases_the_slot() {
        let p = SafetyPolicy::default();
        let dir = tmp();
        let now = at(12);
        {
            let _reservation = reserve_run_at(&dir, &p, at(12), now).unwrap();
            // 模拟提交失败：不 commit，drop 时自动归还
        }
        assert!(
            reserve_run_at(&dir, &p, at(13), now).is_ok(),
            "失败后应能重试"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 占位必须在锁内完成：第二次占位要看到第一次的计数，而不是同时读到 0。
    #[test]
    fn reservation_blocks_a_second_same_day_slot_immediately() {
        let p = SafetyPolicy::default();
        let dir = tmp();
        let now = at(12);
        let first = reserve_run_at(&dir, &p, at(12), now).unwrap();
        let err = reserve_run_at(&dir, &p, at(15), now).unwrap_err();
        assert!(err.contains("上限 1"), "{err}");
        first.commit();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn disabled_policy_lets_everything_through_without_writing_state() {
        let p = SafetyPolicy { enabled: false, ..Default::default() };
        let dir = tmp();
        assert!(reserve_run_at(&dir, &p, at(3), at(3)).is_ok());
        let reservation = reserve_run_at(&dir, &p, at(3), at(3)).unwrap();
        reservation.commit();
        assert!(!state_path(&dir).exists(), "关闭后不应写状态文件");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupted_state_fails_closed() {
        let p = SafetyPolicy::default();
        let dir = tmp();
        std::fs::write(state_path(&dir), "{ not json").unwrap();
        let err = reserve_run_at(&dir, &p, at(12), at(12)).unwrap_err();
        assert!(err.contains("损坏"), "{err}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 崩溃留下的半截 JSON 应能靠 .bak 自动恢复，而不是永久拒绝提交。
    #[test]
    fn broken_state_recovers_from_backup() {
        let p = SafetyPolicy::default();
        let dir = tmp();
        let reservation = reserve_run_at(&dir, &p, day(2, 12), at(12)).unwrap();
        reservation.commit();
        // 再写一次以产生 .bak
        let reservation = reserve_run_at(&dir, &p, day(1, 12), at(12)).unwrap();
        reservation.commit();
        assert!(backup_path(&dir).exists(), "第二次写入应留下 .bak");
        std::fs::write(state_path(&dir), "{ half").unwrap();
        let recovered = reserve_run_at(&dir, &p, at(12), at(12));
        assert!(recovered.is_ok(), "应回退到 .bak 而不是拒绝: {:?}", recovered.err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ai_daily_cap_includes_batch_size() {
        let p = SafetyPolicy::default();
        let dir = tmp();
        assert!(check_ai_at(&dir, &p, 2, at(12)).is_ok());
        assert!(check_ai_at(&dir, &p, 3, at(12)).is_err(), "3 条超过上限 2");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn backfill_limits_cover_days_per_day_and_total() {
        let p = SafetyPolicy::default();
        assert!(check_backfill(&p, 3, 1, 3).is_ok());
        assert!(check_backfill(&p, 30, 1, 30).unwrap_err().contains("上限 3"));
        assert!(check_backfill(&p, 3, 10, 30).unwrap_err().contains("重复 10 次"));
        assert!(check_backfill(&p, 3, 2, 30).unwrap_err().contains("单批上限"));
        let off = SafetyPolicy { enabled: false, ..Default::default() };
        assert!(check_backfill(&off, 365, 10, 3650).is_ok());
    }

    #[test]
    fn zero_limits_and_equal_night_bounds_are_config_errors() {
        let dir = tmp();
        let zero = SafetyPolicy { daily_runs: 0, ..Default::default() };
        assert!(reserve_run_at(&dir, &zero, at(12), at(12))
            .unwrap_err()
            .contains("daily_runs"));
        let equal = SafetyPolicy { night_from: 3, night_to: 3, ..Default::default() };
        assert!(reserve_run_at(&dir, &equal, at(12), at(12))
            .unwrap_err()
            .contains("不能相同"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn deep_night_constant_is_three_am() {
        let (_, hour) = cst_parts(at(3));
        assert_eq!(hour, 3);
        assert_eq!(cst_parts(at(12)).1, 12);
        assert_eq!(cst_parts(at(3)).0, cst_parts(at(12)).0);
        assert_ne!(cst_parts(day(0, 12)).0, cst_parts(day(1, 12)).0);
    }

    /// 只写一半字段的 safety 对象必须能用默认值补齐 ——
    /// 否则整份 config.json 反序列化失败，账号/密码/学校会被静默重置。
    #[test]
    fn partial_config_object_falls_back_to_defaults() {
        let p: SafetyPolicy = serde_json::from_str(r#"{"enabled":false}"#).unwrap();
        assert!(!p.enabled, "显式关闭必须生效");
        assert_eq!(p.daily_runs, 1, "缺失字段应回落默认值");
        assert_eq!(p.batch_per_day, 2);
        assert_eq!(p.night_from, 23);
        // 一份完整默认值序列化后必须能原样读回
        let round = serde_json::to_string(&SafetyPolicy::default()).unwrap();
        let back: SafetyPolicy = serde_json::from_str(&round).unwrap();
        assert!(back.enabled);
        assert_eq!(back.batch_max_records, 6);
    }

    #[test]
    fn state_keeps_only_recent_days() {
        let p = SafetyPolicy::default();
        let dir = tmp();
        for days_ago in 0..25i64 {
            let reservation = reserve_run_at(&dir, &p, day(days_ago, 12), day(0, 12)).unwrap();
            reservation.commit();
        }
        let state = load_state_at(&dir).unwrap();
        assert!(state.days.len() <= KEEP_DAYS, "状态膨胀到 {} 天", state.days.len());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 批量补签补的是历史日期：记账必须落到那一天，而不是"今天"。
    #[test]
    fn ai_records_are_bucketed_by_record_date() {
        let dir = tmp();
        crate::platform::TEST_DATA_DIR.with(|slot| *slot.borrow_mut() = Some(dir.clone()));
        let p = SafetyPolicy::default();
        let historic = day(2, 12);
        record_ai_on(&p, historic, 1).unwrap();
        let state = load_state_at(&dir).unwrap();
        let (historic_day, _) = cst_parts(historic);
        let (today, _) = cst_parts(crate::crypto::envelope::now_ms());
        assert_eq!(state.get(&historic_day).ai, 1, "历史日应记到历史日");
        assert_eq!(state.get(&today).ai, 0, "不应该算到今天头上");
        crate::platform::TEST_DATA_DIR.with(|slot| *slot.borrow_mut() = None);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
