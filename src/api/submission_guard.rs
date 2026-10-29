//! Write-ahead receipts, scoped to an account and school. No tokens or coordinates.
//!
//! 提交幂等 WAL：**先落回执，再发请求**。一次提交只要可能已经到达服务端，这件事就不能
//! 因为进程被杀、网络超时、响应解不开而丢失 —— 一旦丢了，下一次「重试」就会在服务端
//! 产生第二条记录，而「同一天同一时段两条几乎一样的记录」是最容易被人工审查看到的形态。
//!
//! 状态机（每条事件一行 JSON，只追加、不改写）：
//!
//! ```text
//!   begin ──▶ Pending ──accepted(rrid)──▶ Committed(rrid)
//!                │
//!                ├── unknown ──────────▶ Unknown ──accepted(rrid)──▶ Committed(rrid)
//!                │                           └── rejected（对账确认没有入账）──▶ Abandoned
//!                └── rejected ─────────▶ Abandoned
//! ```
//!
//! - `Pending`：回执写下了，但还没有任何结果事件（请求在飞，或者发出后进程直接死了）。
//!   服务端到底收没收到是**未知**的，所以对账窗口内一律拒绝重提，只有人能到运动记录页
//!   或官方 App 上确认；`unlock --yes` 是显式的人工释放出口。
//! - `Unknown`：请求确定发出去了，但拿不到可信结果（超时、连接中断、响应解不开）。
//!   这类回执**必须先拿既有记录列表对账一次**：查到就标 Committed 并放弃重提，查不到才
//!   允许重提。对账只读（`records::fetch_records`），不新增端点、不改任何提交字段。
//! - `Committed` / `Abandoned` 是终态，只用来挡「同一时间窗重复提交」。
//!
//! 落盘与并发：
//! - 目录 `data_dir()/submission_receipts/`，每个 `(uid, unid)` 一个 JSONL 文件；
//! - 同一文件用操作系统排他锁（`File::try_lock`）：跨进程、跨线程都只有一个写者，
//!   同一进程内第二次 `open` 也会被拒 —— 不会出现两个写者交错追加；
//! - 每条事件 `write_all` + `sync_all` 之后才更新内存状态，崩溃最多留下末尾半行；
//!   末尾半行/非法 JSON 一律判「记录不完整」并 **fail-closed**，绝不当作「没有回执」。
//!   这里刻意不用 tmp+rename：这是一份追加日志，整文件重写在崩溃窗口里会把**整份**回执
//!   变成「不存在」；`O_APPEND` + fsync 对单条小记录本身就是原子的，而且没有「读到旧文件」
//!   的中间态。需要"整文件替换原子性"的是快照，不是 WAL。
//!
//! 回执**只存本地**：不进任何请求体 / 请求头，也不含 token、坐标或轨迹。
use super::{client::ApiClient, records::RecordRow};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Write},
    path::Path,
};

/// 对账窗口。未闭合的回执在这个窗口内不自动核销：这段时间里服务端列表最可能还没刷新。
pub const RECONCILE_WINDOW_MS: i64 = 30 * 60 * 1000;
/// `Unknown` 回执的**静置窗口**：等这么久再去拉记录列表对账。
///
/// 与 [`RECONCILE_WINDOW_MS`] 解决的不是同一件事：
/// - 30 min 护的是 **Pending**（请求可能还在飞，只有人能确认）；
/// - 90 s 护的是 **Unknown**（请求确定发出去了，但网关 504 / 连接被重置 / 响应解不开）。
///   这种时刻服务端可能「已经收下、还没来得及写进列表库」，立刻对账会读到旧快照 →
///   命中不了 → 误判「没有入账」→ 放行重提 → 服务端出现两条记录（复审 N2）。
///
/// 为什么是 90 s：列表写入的延迟是秒级，30 min 对 Unknown 太长（用户以为卡死），
/// 60 s 对慢网关偏紧，90 s 两侧都留了余量。
///
/// **窗口内仍然挡住提交**（fail-closed），只是不去拉列表 ——
/// 「还没到点」绝不能被解释成「允许重提」。
pub const UNKNOWN_SETTLE_MS: i64 = 90_000;
/// 单个账号的回执文件上限。超过就拒绝提交，而不是把整份历史读进内存。
const MAX_JOURNAL_BYTES: u64 = 16 * 1024 * 1024;
/// 记录里的 `totalDis` 是取整后的米，与回执里的目标距离比对时留一点余量。
const DISTANCE_TOLERANCE_M: f64 = 5.0;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Event {
    /// 提交请求**发出之前**落下。老版本（0.8.0）写的事件没有后面三个字段，读的时候补默认值。
    Pending {
        uuid: String,
        start_ms: i64,
        stop_ms: i64,
        /// 回执写入时刻（毫秒）。0 表示老格式，按"时间未知"处理（fail-closed）。
        #[serde(default)]
        at_ms: i64,
        /// 本地计划指纹，只用于人看与排障，不进请求。
        #[serde(default)]
        plan_fp: String,
        /// 本次提交的目标距离（米），对账退化匹配用。
        #[serde(default)]
        target_dist: i64,
    },
    /// 服务端确认入账（拿到 rrid）。
    Accepted { uuid: String, rrid: i64 },
    /// 请求确定发出去了，但结果不可信（超时、断连、响应解不开）。
    Unknown {
        uuid: String,
        #[serde(default)]
        at_ms: i64,
    },
    /// 本次尝试确定没有产生记录：服务端明确拒绝 / 人工释放 / 对账确认缺失。
    Rejected {
        uuid: String,
        #[serde(default)]
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Pending,
    Unknown,
    Committed(i64),
    Abandoned,
}

#[derive(Debug)]
struct Attempt {
    uuid: String,
    start_ms: i64,
    stop_ms: i64,
    at_ms: i64,
    plan_fp: String,
    target_dist: i64,
    state: State,
}

/// 一条未决回执与记录列表的比对结论。
enum Matched {
    /// 列表里没有能对上它的记录。
    None,
    /// 唯一命中。
    One(i64),
    /// 命中多条且 rrid 不同 —— 说不清是哪条，按未决继续挡。
    Ambiguous,
}

impl Attempt {
    fn is_unresolved(&self) -> bool {
        matches!(self.state, State::Pending | State::Unknown)
    }

    /// 未闭合的 Pending 超过对账窗口：进程不可能还在飞了，可以拿列表核销。
    fn expired(&self, now_ms: i64) -> bool {
        if self.state != State::Pending {
            return false;
        }
        // `at_ms == 0` 只可能来自 0.8.0 的老回执（那时还没有这个字段）。它没有
        // 时间戳可比，必须按「早已过期」处理 —— 否则老用户升级后 `expired()` 恒 false，
        // 永远不参与对账，`check()` 就一直回「上次提交尚未闭合」，直到他知道
        // 0.9.0 才有的 `unlock --yes`（0.9.0 复审 N3）。
        self.at_ms <= 0 || now_ms.saturating_sub(self.at_ms) > RECONCILE_WINDOW_MS
    }

    /// `Unknown` 是否已经静置够久、可以拿列表对账（复审 N2）。
    ///
    /// `at_ms <= 0` 只可能来自 0.8.0 的老回执：没有时间戳可比，按「早已过窗口」
    /// 处理 —— 与 [`Self::expired`] 的 N3 口径一致，不能因为加了静置窗口
    /// 又把老用户永久挡回去。
    fn settled(&self, now_ms: i64) -> bool {
        if self.state != State::Unknown {
            return false;
        }
        self.at_ms <= 0 || now_ms.saturating_sub(self.at_ms) >= UNKNOWN_SETTLE_MS
    }

    /// 这条未决回执现在能不能对账（Pending 过 30 min / Unknown 过 90 s）。
    ///
    /// 两个调用点（`needs_reconcile` 与 `resolve`）都只走这里，口径不会分叉。
    fn ready_for_reconcile(&self, now_ms: i64) -> bool {
        self.settled(now_ms) || self.expired(now_ms)
    }

    fn short_uuid(&self) -> String {
        self.uuid.chars().take(8).collect()
    }

    fn plan_hint(&self) -> String {
        if self.plan_fp.is_empty() {
            String::new()
        } else {
            format!("，计划指纹 {}", self.plan_fp)
        }
    }

    fn written_hint(&self) -> String {
        if self.at_ms > 0 {
            crate::track::geom::fmt_cst(self.at_ms, "%m-%d %H:%M")
        } else {
            "更早".into()
        }
    }

    /// 记录列表里的这一行，是不是**这次尝试**。
    ///
    /// 服务端回了 uuid 就以它为准（同一个 uuid 是我们自己生成的，唯一的身份证明）；
    /// 没回 uuid 时才退化成「同一开始时刻 + 同一目标距离 + 同一时长」，
    /// 也就是任务里说的「同一开始时间 / 同一距离 / 相近提交时刻」。
    fn matches(&self, row: &RecordRow) -> bool {
        if row.rrid <= 0 {
            return false;
        }
        if !row.uuid.is_empty() {
            return row.uuid.eq_ignore_ascii_case(&self.uuid);
        }
        if row.start_time != self.start_ms {
            return false;
        }
        let duration_s = (self.stop_ms - self.start_ms) / 1000;
        if row.total_time > 0 && duration_s > 0 && row.total_time != duration_s {
            return false;
        }
        if self.target_dist > 0
            && (row.total_dis - self.target_dist as f64).abs() > DISTANCE_TOLERANCE_M
        {
            return false;
        }
        true
    }

    fn matched(&self, rows: &[RecordRow]) -> Matched {
        let mut ids: Vec<i64> = rows
            .iter()
            .filter(|r| self.matches(r))
            .map(|r| r.rrid)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        match ids.len() {
            0 => Matched::None,
            1 => Matched::One(ids[0]),
            _ => Matched::Ambiguous,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Guard {
    file: File,
    attempts: Vec<Attempt>,
}

/// 「同一账号正被另一个实例占用」的统一文案。
///
/// 为什么需要统一：Windows 上第二个 `Guard::open` 有时拿到的不是 `lock` 失败，
/// 而是 `ERROR_SHARING_VIOLATION`（被映射成 `PermissionDenied`, os error 5）——
/// 对用户是同一件事（有人在用），但原始文案会写成「拒绝访问」，排障时被误读成
/// 文件权限问题（复审 N6）。两种错误都映射到这里，仍然 fail-closed。
const BUSY_MESSAGE: &str = "同一账号正在提交或核对记录，请稍后再试";

/// 这个 IO 错误是不是「文件正被占用」而不是真的权限/损坏问题。
fn is_lock_contention(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock
    )
}

impl Guard {
    pub fn open(uid: i64, unid: i64) -> Result<Self, String> {
        Self::open_in(&crate::platform::data_dir(), uid, unid)
    }

    fn open_in(root: &Path, uid: i64, unid: i64) -> Result<Self, String> {
        if uid <= 0 || unid <= 0 {
            return Err("账号或学校编号无效，无法启用重复提交保护".into());
        }
        let directory = root.join("submission_receipts");
        std::fs::create_dir_all(&directory).map_err(|e| format!("无法建立提交保护目录：{e}"))?;
        let name = format!("{:x}.jsonl", Sha256::digest(format!("{uid}:{unid}")));
        let path = directory.join(name);
        let mut file = match File::options()
            .read(true)
            .append(true)
            .create(true)
            .open(&path)
        {
            Ok(file) => file,
            // 另一实例正开着这个文件：Windows 会给 PermissionDenied（见 `BUSY_MESSAGE`）。
            Err(error) if is_lock_contention(&error) => return Err(BUSY_MESSAGE.into()),
            Err(error) => return Err(format!("无法读取提交保护记录：{error}")),
        };
        // 排他锁是「同一账号只有一个人能改回执」的唯一保证：进程之间靠操作系统的文件锁，
        // 进程内靠这把锁本身（第二次 open 直接失败），所以不存在两个写者交错追加。
        file.try_lock().map_err(|error| match error {
            // 正常的「有人在用」。
            std::fs::TryLockError::WouldBlock => BUSY_MESSAGE.to_string(),
            // 底层 IO 报错：Windows 上常常就是同一种占用（N6），按同一句文案处理。
            std::fs::TryLockError::Error(io) if is_lock_contention(&io) => BUSY_MESSAGE.to_string(),
            std::fs::TryLockError::Error(io) => {
                format!("提交保护记录加锁失败（已停止提交）：{io}")
            }
        })?;
        if file.metadata().map_err(|e| e.to_string())?.len() > MAX_JOURNAL_BYTES {
            return Err("提交保护记录过大，已停止提交，请检查本地记录".into());
        }
        let mut text = String::new();
        file.read_to_string(&mut text)
            .map_err(|e| format!("提交保护记录读取失败：{e}"))?;
        // 崩溃留下的末尾半行，恰恰是「请求可能已经发出去了」的那一条：不能当它不存在。
        if !text.is_empty() && !text.ends_with('\n') {
            return Err("提交保护记录不完整，已停止提交；请保留文件并核对官方记录".into());
        }
        let mut guard = Self {
            file,
            attempts: Vec::new(),
        };
        for line in text.lines() {
            let event = serde_json::from_str(line).map_err(|_| {
                "提交保护记录损坏，已停止提交；请保留文件并核对官方记录".to_string()
            })?;
            guard.apply(event)?;
        }
        Ok(guard)
    }

    fn apply(&mut self, event: Event) -> Result<(), String> {
        match event {
            Event::Pending {
                uuid,
                start_ms,
                stop_ms,
                at_ms,
                plan_fp,
                target_dist,
            } => {
                if uuid::Uuid::parse_str(&uuid).is_err()
                    || start_ms <= 0
                    || stop_ms <= start_ms
                    || self.attempts.iter().any(|a| a.uuid == uuid)
                {
                    return Err("提交保护记录包含无效请求，已停止提交".into());
                }
                self.attempts.push(Attempt {
                    uuid,
                    start_ms,
                    stop_ms,
                    at_ms,
                    plan_fp,
                    target_dist,
                    state: State::Pending,
                });
            }
            Event::Unknown { uuid, at_ms } => {
                let a = self.unresolved_mut(&uuid, "提交保护回执没有对应请求")?;
                a.state = State::Unknown;
                if at_ms > 0 {
                    a.at_ms = at_ms;
                }
            }
            Event::Accepted { uuid, rrid } => {
                if rrid <= 0 {
                    return Err("提交回执编号无效".into());
                }
                let a = self.unresolved_mut(&uuid, "提交保护回执没有对应请求")?;
                a.state = State::Committed(rrid);
            }
            Event::Rejected { uuid, .. } => {
                let a = self.unresolved_mut(&uuid, "提交保护拒绝结果没有对应请求")?;
                a.state = State::Abandoned;
            }
        }
        Ok(())
    }

    fn unresolved_mut(&mut self, uuid: &str, missing: &str) -> Result<&mut Attempt, String> {
        self.attempts
            .iter_mut()
            .find(|a| a.uuid == uuid && a.is_unresolved())
            .ok_or_else(|| missing.to_string())
    }

    fn append(&mut self, event: Event) -> Result<(), String> {
        let mut bytes = serde_json::to_vec(&event).map_err(|e| e.to_string())?;
        bytes.push(b'\n');
        // 先落盘再改内存：写失败时内存里也不会出现一个服务端并不知道的终态。
        self.file
            .write_all(&bytes)
            .and_then(|_| self.file.sync_all())
            .map_err(|e| format!("提交保护记录保存失败，禁止重试提交：{e}"))?;
        self.apply(event)
    }

    /// 有需要靠记录列表才能定论的未决回执吗？
    ///
    /// Unknown 要先静置 [`UNKNOWN_SETTLE_MS`]（复审 N2），Pending 要过 30 min 对账窗口。
    /// **没到点时返回 false 只是「先别拉列表」，`check()` 仍然挡住提交**。
    pub fn needs_reconcile(&self, now_ms: i64) -> bool {
        self.attempts.iter().any(|a| a.ready_for_reconcile(now_ms))
    }

    /// 只读对账：拿**既有**的记录列表把能定论的未决回执落成终态。
    ///
    /// - 命中 → `Committed`（返回 rrid，调用方据此放弃重提）—— **命中随时可以定论**，
    ///   记录在列表里就是最强的证据，不必等窗口；
    /// - 没命中 → `Abandoned`（本次确实没入账，允许重提）—— **只有过了各自窗口才算证据**：
    ///   Unknown 要静置 [`UNKNOWN_SETTLE_MS`]（N2：服务端可能刚收下还没写进列表库），
    ///   Pending 要过 [`RECONCILE_WINDOW_MS`]。窗口内的「列表里没有」说明不了任何事，
    ///   保持未决、继续挡；
    /// - 命中多条或说不清 → 保持未决（fail-closed）。
    pub fn resolve(&mut self, rows: &[RecordRow], now_ms: i64) -> Result<Vec<i64>, String> {
        let mut confirmed = Vec::new();
        let mut todo: Vec<usize> = Vec::new();
        for (i, a) in self.attempts.iter().enumerate() {
            if a.is_unresolved() {
                todo.push(i);
            }
        }
        for i in todo {
            let (uuid, was_pending, ready, matched) = {
                let a = &self.attempts[i];
                (
                    a.uuid.clone(),
                    a.state == State::Pending,
                    a.ready_for_reconcile(now_ms),
                    a.matched(rows),
                )
            };
            match matched {
                Matched::One(rrid) => {
                    self.accepted(&uuid, rrid)?;
                    confirmed.push(rrid);
                }
                // 窗口内的「没查到」不是证据：宁可继续挡，也不放行一条可能的重复
                // 提交（N2）。过了窗口才把缺失当作「确实没入账」。
                Matched::None if !ready => {}
                Matched::None => {
                    self.rejected(
                        &uuid,
                        if was_pending {
                            "对账窗口过后仍查不到记录"
                        } else {
                            "对账确认没有入账"
                        },
                    )?;
                }
                // 说不清是哪条 → 不猜，保持未决（下一条新提交继续被挡）。
                Matched::Ambiguous => {}
            }
        }
        Ok(confirmed)
    }

    /// 新提交的闸门：有未闭合的回执就拒绝，有已入账的重叠时间窗也拒绝。
    pub fn check(&self, start_ms: i64, stop_ms: i64, now_ms: i64) -> Result<(), String> {
        if start_ms <= 0 || stop_ms <= start_ms {
            return Err("提交时间范围无效".into());
        }
        for a in &self.attempts {
            match a.state {
                State::Pending => {
                    let age = if a.at_ms > 0 {
                        format!(
                            "{:.0} 分钟前",
                            now_ms.saturating_sub(a.at_ms) as f64 / 60_000.0
                        )
                    } else {
                        "时间未知的".to_string()
                    };
                    return Err(format!(
                        "上一次提交尚未闭合（回执 {}，{age}写入{}）：请先到运动记录页确认，\
                         确认确实没有入账再执行 `unlock --yes` 释放；\
                         窗口内不会自动重发，避免同一条记录被提交两次",
                        a.short_uuid(),
                        a.plan_hint(),
                    ));
                }
                State::Unknown => {
                    // 静置窗口内还没对账过：告诉用户「多久之后会自动核对」，
                    // 免得他以为界面卡死（N2 加窗口后新增的状态）。
                    let wait = if a.at_ms > 0 && !a.settled(now_ms) {
                        let left = UNKNOWN_SETTLE_MS.saturating_sub(now_ms.saturating_sub(a.at_ms));
                        format!("，约 {} 秒后自动对账", (left / 1000).max(1))
                    } else {
                        String::new()
                    };
                    return Err(format!(
                        "上次提交结果未知（回执 {}，{} 写入{wait}）：已阻止新的提交。\
                         请到运动记录页刷新核对；对账确认没有入账之后才会允许重发",
                        a.short_uuid(),
                        a.written_hint(),
                    ));
                }
                _ => {}
            }
        }
        for a in &self.attempts {
            if let State::Committed(rrid) = a.state {
                if start_ms < a.stop_ms && stop_ms > a.start_ms {
                    return Err(format!(
                        "该时间段已有本工具提交的记录 #{rrid}，已阻止重复提交；请到运动记录查看详情"
                    ));
                }
            }
        }
        Ok(())
    }

    /// 发请求**之前**调用：闸门 + 落 Pending。
    pub fn begin(
        &mut self,
        uuid: &str,
        start_ms: i64,
        stop_ms: i64,
        now_ms: i64,
        plan_fp: &str,
        target_dist: i64,
    ) -> Result<(), String> {
        self.check(start_ms, stop_ms, now_ms)?;
        self.append(Event::Pending {
            uuid: uuid.into(),
            start_ms,
            stop_ms,
            at_ms: now_ms,
            plan_fp: plan_fp.into(),
            target_dist,
        })
    }

    /// 服务端确认入账。
    pub fn accepted(&mut self, uuid: &str, rrid: i64) -> Result<(), String> {
        self.append(Event::Accepted {
            uuid: uuid.into(),
            rrid,
        })
    }

    /// 请求发出去了、结果不可信。**必须在只读对账之前落盘** —— 否则进程一死，
    /// 「请求可能已经到服务端」这条事实就只剩下一个 Pending。
    pub fn unknown(&mut self, uuid: &str, now_ms: i64) -> Result<(), String> {
        self.append(Event::Unknown {
            uuid: uuid.into(),
            at_ms: now_ms,
        })
    }

    /// 本次尝试确定没有产生记录（服务端明确拒绝 / 人工释放 / 对账确认缺失）。
    pub fn rejected(&mut self, uuid: &str, reason: &str) -> Result<(), String> {
        self.append(Event::Rejected {
            uuid: uuid.into(),
            reason: reason.into(),
        })
    }

    /// 还没闭合（Pending / Unknown）的提交条数。
    pub fn pending_count(&self) -> usize {
        self.attempts.iter().filter(|a| a.is_unresolved()).count()
    }

    /// 这一次尝试是否仍未闭合（调用方据此决定文案：允许重提 / 继续挡住）。
    pub fn is_unresolved(&self, uuid: &str) -> bool {
        self.attempts
            .iter()
            .find(|a| a.uuid == uuid)
            .is_none_or(|a| a.is_unresolved())
    }

    /// 这一次尝试是否已经被服务端确认入账（提交成功后 / 对账命中后）。
    pub fn committed_rrid(&self, uuid: &str) -> Option<i64> {
        self.attempts
            .iter()
            .find(|a| a.uuid == uuid)
            .and_then(|a| match a.state {
                State::Committed(rrid) => Some(rrid),
                _ => None,
            })
    }

    /// 给人看的未决回执清单（`unlock` 用）。
    pub fn describe_unresolved(&self, now_ms: i64) -> Vec<String> {
        self.attempts
            .iter()
            .filter(|a| a.is_unresolved())
            .map(|a| {
                let age = if a.at_ms > 0 {
                    format!(
                        "{:.0} 分钟前写入",
                        now_ms.saturating_sub(a.at_ms) as f64 / 60_000.0
                    )
                } else {
                    "写入时间未知".to_string()
                };
                format!(
                    "{} 起跑 / 目标 {} m / 回执 {}{} / {age}",
                    crate::track::geom::fmt_cst(a.start_ms, "%m-%d %H:%M"),
                    a.target_dist,
                    a.short_uuid(),
                    a.plan_hint(),
                )
            })
            .collect()
    }

    /// 用户核对过官方记录、确认这些提交没有入账后的**手动释放**。
    ///
    /// journal 只能追加，所以"释放"不是删除，而是给每个未决请求补一条 Rejected
    /// 事件 —— 记录仍然完整可追溯，同时解开对后续提交的封锁。
    pub fn clear_pending(&mut self) -> Result<usize, String> {
        let uuids: Vec<String> = self
            .attempts
            .iter()
            .filter(|a| a.is_unresolved())
            .map(|a| a.uuid.clone())
            .collect();
        for uuid in &uuids {
            self.rejected(uuid, "人工释放（unlock）")?;
        }
        Ok(uuids.len())
    }
}

/// 本地计划指纹：**只是这一次要提交的方案的本地标签**，不进任何请求。
///
/// 它让「卡住的是哪一条回执」在 `unlock` 输出与日志里可读、可对账，而不是一串 uuid。
pub fn plan_fingerprint(
    start_ms: i64,
    total_time: i64,
    total_dis: f64,
    total_steps: i64,
    target_dist: i64,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!(
        "{start_ms}|{total_time}|{}|{total_steps}|{target_dist}",
        (total_dis * 100.0).round() as i64
    ));
    hasher
        .finalize()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 未决回执的只读对账：需要时才拉一次记录列表，拉不到就保持未决（fail-closed）。
///
/// 三个调用点（全链 preflight / 提交本体 / 记录页刷新）走同一段逻辑，口径不会分叉。
pub(crate) fn reconcile_if_needed(
    guard: &mut Guard,
    client: &mut ApiClient,
    now_ms: i64,
    log: &mut dyn FnMut(&str),
) -> Result<(), String> {
    if !guard.needs_reconcile(now_ms) {
        return Ok(());
    }
    match super::records::fetch_records(client) {
        Ok(rows) => {
            for id in guard.resolve(&rows, now_ms)? {
                log(&format!("[record] 已核对先前提交的记录 #{id}；未重复提交"));
            }
            Ok(())
        }
        Err(e) => {
            // 拉不到列表就不放行：`check` 会继续挡住未决回执。
            log(&format!("[record] 只读对账失败：{e}；提交保护继续生效"));
            Ok(())
        }
    }
}

pub(crate) fn preflight(
    client: &mut ApiClient,
    start_ms: i64,
    duration: i64,
    log: &mut dyn FnMut(&str),
) -> Result<(), String> {
    let session = client.login.as_ref().ok_or("未登录")?;
    let mut guard = Guard::open(
        session.uid,
        session.unid.parse().map_err(|_| "学校编号无效")?,
    )?;
    let now = crate::crypto::envelope::now_ms();
    reconcile_if_needed(&mut guard, client, now, log)?;
    let stop = duration
        .checked_mul(1000)
        .and_then(|n| start_ms.checked_add(n))
        .ok_or("时间范围溢出")?;
    guard.check(start_ms, stop, now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    const T0: i64 = 1_700_000_000_000;

    fn root() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("submit-guard-{}", uuid::Uuid::new_v4()))
    }
    fn row(uuid: &str, id: i64) -> RecordRow {
        serde_json::from_value(serde_json::json!({"rrid":id,"uuid":uuid,"total_dis":1000.0,
            "total_time":600,"start_time":1000,"complete":false,"avg_step_freq":120}))
        .unwrap()
    }
    fn row_at(uuid: &str, id: i64, start: i64, dis: f64, time: i64) -> RecordRow {
        serde_json::from_value(serde_json::json!({"rrid":id,"uuid":uuid,"total_dis":dis,
            "total_time":time,"start_time":start,"complete":false,"avg_step_freq":120}))
        .unwrap()
    }
    fn new_attempt(g: &mut Guard, id: &str) {
        g.begin(id, 1000, 601_000, T0, "fp", 1000).unwrap();
    }

    /// 并发用例里「第二个写者」的失败文案是否可接受。
    ///
    /// 正常路径是 [`BUSY_MESSAGE`]；Windows 上偶尔是系统级的占用 / 拒绝访问
    /// （N6 记录的 `os error 5`）。两者都 fail-closed，测试接受两者。
    fn is_acceptable_busy_error(message: &str) -> bool {
        message.contains("正在提交")
            || message.contains("拒绝访问")
            || message.contains("os error 5")
            || message.contains("being used by another process")
    }

    /// N2：`Unknown` 静置窗口内**不对账、但仍然挡住提交**；过了窗口才去拉列表。
    ///
    /// 反例（改前）：Unknown 立刻对账，网关 504 后服务端还没写完库，列表读旧快照
    /// → 命中不了 → 判「没有入账」→ 放行重提 → 服务端两条记录。
    #[test]
    fn unknown_receipt_settles_before_reconciling() {
        let root = root();
        let id = uuid::Uuid::new_v4().to_string();
        {
            let mut g = Guard::open_in(&root, 1, 2).unwrap();
            new_attempt(&mut g, &id);
            g.unknown(&id, T0).unwrap();

            // 窗口内：不去拉列表（needs_reconcile=false），但闸门仍然拒绝。
            let inside = T0 + UNKNOWN_SETTLE_MS - 1;
            assert!(!g.needs_reconcile(inside), "静置窗口内不该触发对账");
            assert!(
                g.resolve(&[], inside).unwrap().is_empty(),
                "静置窗口内的 resolve 不该把回执定论掉"
            );
            assert_eq!(g.pending_count(), 1, "窗口内回执必须仍然未决");
            let err = g.check(1000, 601_000, inside).unwrap_err();
            assert!(err.contains("上次提交结果未知"), "{err}");
            assert!(err.contains("自动对账"), "要告诉用户多久后会核对：{err}");

            // 到点：可以拉列表；确认缺失才放行。
            let at = T0 + UNKNOWN_SETTLE_MS;
            assert!(g.needs_reconcile(at), "过了静置窗口必须去对账");
            assert!(g.check(1000, 601_000, at).is_err(), "对账之前仍然挡");
            assert!(g.resolve(&[], at).unwrap().is_empty());
            assert_eq!(g.pending_count(), 0);
            assert!(g.check(1000, 601_000, at).is_ok());
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    /// N2：窗口内命中记录时，对账（人手动看记录页）仍然可以提前定论 ——
    /// 静置只是「不自动拉列表」，不是「不许有结论」。
    #[test]
    fn unknown_receipt_can_still_be_closed_by_a_manual_lookup_inside_the_window() {
        let root = root();
        let id = uuid::Uuid::new_v4().to_string();
        let mut g = Guard::open_in(&root, 1, 2).unwrap();
        new_attempt(&mut g, &id);
        g.unknown(&id, T0).unwrap();
        // 人自己刷新记录页（`run_records` 直接调 resolve，不经过 needs_reconcile）
        let inside = T0 + 5_000;
        assert!(!g.needs_reconcile(inside));
        // 命中判定自身不看时间：调用方既然拿到了列表，就该能定论。
        assert_eq!(g.resolve(&[row(&id, 77)], inside).unwrap(), vec![77]);
        assert_eq!(g.committed_rrid(&id), Some(77));
        assert!(g.check(1001, 601_001, inside).unwrap_err().contains("#77"));
        drop(g);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// 验收 ①：Pending 回执拒提，而且**不会**因为「列表里没查到」就自动放行。
    #[test]
    fn pending_receipt_blocks_a_retry() {
        let root = root();
        let id = uuid::Uuid::new_v4().to_string();
        {
            let mut g = Guard::open_in(&root, 1, 2).unwrap();
            new_attempt(&mut g, &id);
            assert_eq!(g.pending_count(), 1);
            let err = g.check(1000, 601_000, T0 + 60_000).unwrap_err();
            assert!(err.contains("尚未闭合"), "{err}");
            assert!(err.contains("运动记录页"), "{err}");
            assert!(err.contains("unlock"), "{err}");
            // 窗口内既不自动对账，也不因「列表没有」放行。
            assert!(!g.needs_reconcile(T0 + 60_000));
            assert!(g.resolve(&[], T0 + 60_000).unwrap().is_empty());
            assert!(g.check(1000, 601_000, T0 + 60_000).is_err());
        }
        // 重启后仍然拒提：回执是 WAL，不是内存状态。
        let mut g = Guard::open_in(&root, 1, 2).unwrap();
        assert!(g.check(1000, 601_000, T0).is_err());
        // 过了对账窗口，拉一次列表确认没有，才允许重提。
        let after = T0 + RECONCILE_WINDOW_MS + 1;
        assert!(g.needs_reconcile(after));
        assert!(g.resolve(&[], after).unwrap().is_empty());
        assert_eq!(g.pending_count(), 0);
        assert!(g.check(1000, 601_000, after).is_ok());
        drop(g);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// 验收 ②：Unknown 对账命中记录 → 标 Committed，不再提交。
    ///
    /// 时间轴按 N2 的静置窗口给：`unknown` 落在 T0+5s，对账发生在 T0+95s（> 90 s）。
    #[test]
    fn unknown_receipt_closes_on_a_matching_record() {
        let root = root();
        let id = uuid::Uuid::new_v4().to_string();
        let mut g = Guard::open_in(&root, 1, 2).unwrap();
        new_attempt(&mut g, &id);
        g.unknown(&id, T0 + 5_000).unwrap();
        let after_settle = T0 + 5_000 + UNKNOWN_SETTLE_MS;
        assert!(g.needs_reconcile(after_settle));
        assert_eq!(
            g.resolve(&[row(&id.to_uppercase(), 77)], after_settle)
                .unwrap(),
            vec![77]
        );
        assert_eq!(g.pending_count(), 0);
        drop(g);
        // 终态落盘：重启后依然挡住同一时间窗。
        let g = Guard::open_in(&root, 1, 2).unwrap();
        let err = g.check(1001, 601_001, after_settle + 1_000).unwrap_err();
        assert!(err.contains("#77"), "{err}");
        assert!(err.contains("重复提交"), "{err}");
        assert!(!g.needs_reconcile(after_settle + 1_000));
        drop(g);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// 验收 ③：Unknown 对账**未**命中 → 允许重提（且只放行被确认缺失的那一条）。
    ///
    /// N2：只有在静置窗口**之后**的「没查到」才算证据；窗口内保持挡住。
    #[test]
    fn unknown_receipt_without_a_record_allows_the_retry() {
        let root = root();
        let id = uuid::Uuid::new_v4().to_string();
        let after_settle = T0 + 5_000 + UNKNOWN_SETTLE_MS;
        {
            let mut g = Guard::open_in(&root, 1, 2).unwrap();
            new_attempt(&mut g, &id);
            g.unknown(&id, T0 + 5_000).unwrap();
            // 窗口内：对账没到点，「列表里没有」不作数 → 仍然未决，仍然挡。
            assert!(!g.needs_reconcile(T0 + 5_001));
            assert!(g.resolve(&[], T0 + 5_002).unwrap().is_empty());
            assert_eq!(g.pending_count(), 1);
            assert!(g.check(1000, 601_000, T0 + 5_002).is_err());
            // 过了窗口：确认缺失 → 放行重提。
            assert!(g.needs_reconcile(after_settle));
            assert!(g.resolve(&[], after_settle).unwrap().is_empty());
            assert_eq!(g.pending_count(), 0);
            assert!(g.check(1000, 601_000, after_settle).is_ok());
            // 重提本身再落一条 Pending，仍然只挡自己。
            let retry = uuid::Uuid::new_v4().to_string();
            new_attempt(&mut g, &retry);
            assert_eq!(g.pending_count(), 1);
        }
        // 释放事件仍然留在 journal 里（只追加，可追溯）。
        let text = std::fs::read_dir(root.join("submission_receipts"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let text = std::fs::read_to_string(text).unwrap();
        assert!(text.contains("\"state\":\"unknown\""), "{text}");
        assert!(text.contains("\"state\":\"rejected\""), "{text}");
        assert!(text.contains("对账确认没有入账"), "{text}");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Unknown 命中多条不同 rrid：说不清是哪条，继续挡（不猜）。
    #[test]
    fn ambiguous_lookup_stays_blocked() {
        let root = root();
        let id = uuid::Uuid::new_v4().to_string();
        let mut g = Guard::open_in(&root, 1, 2).unwrap();
        new_attempt(&mut g, &id);
        g.unknown(&id, T0 + 1).unwrap();
        assert!(g
            .resolve(&[row(&id, 77), row(&id, 78)], T0 + 2)
            .unwrap()
            .is_empty());
        assert_eq!(g.pending_count(), 1);
        assert!(g.check(1000, 601_000, T0 + 3).is_err());
        drop(g);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// 服务端不回 uuid 时，退化成「同一开始时刻 + 同一目标距离 + 同一时长」认领。
    #[test]
    fn record_without_uuid_is_matched_by_start_and_distance() {
        let first = root();
        let id = uuid::Uuid::new_v4().to_string();
        {
            let mut g = Guard::open_in(&first, 1, 2).unwrap();
            new_attempt(&mut g, &id);
            g.unknown(&id, T0 + 1).unwrap();
            assert_eq!(
                g.resolve(&[row_at("", 91, 1000, 1003.0, 600)], T0 + 2)
                    .unwrap(),
                vec![91]
            );
        }
        std::fs::remove_dir_all(first).unwrap();
        // 另一个干净目录：距离/时刻对不上就都不是它（对账用窗口后的时间，缺失才算证据）。
        let second = root();
        let other = uuid::Uuid::new_v4().to_string();
        let after_settle = T0 + 1 + UNKNOWN_SETTLE_MS;
        let mut g = Guard::open_in(&second, 1, 2).unwrap();
        g.begin(&other, 2_000_000, 2_600_000, T0, "fp", 1000)
            .unwrap();
        g.unknown(&other, T0 + 1).unwrap();
        assert!(g
            .resolve(
                &[
                    row_at("", 92, 2_000_000, 1300.0, 600),
                    row_at("", 93, 900_000, 1003.0, 600)
                ],
                after_settle
            )
            .unwrap()
            .is_empty());
        assert!(g.check(2_000_000, 2_600_000, after_settle).is_ok());
        drop(g);
        std::fs::remove_dir_all(second).unwrap();
    }

    /// 验收 ④：回执损坏 → fail-closed（拒绝提交），而且**不覆盖**原文件。
    #[test]
    fn corrupt_or_torn_receipt_fails_closed_and_is_not_overwritten() {
        for broken in ["{\"state\":\"pending\"", "{\"state\":\"nonsense\"}\n", "\n"] {
            let root = root();
            let g = Guard::open_in(&root, 1, 2).unwrap();
            drop(g);
            let path = std::fs::read_dir(root.join("submission_receipts"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            std::fs::write(&path, broken).unwrap();
            let err = Guard::open_in(&root, 1, 2).unwrap_err();
            assert!(
                err.contains("不完整") || err.contains("损坏") || err.contains("无效"),
                "{broken:?} → {err}"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    /// 验收 ⑤：并发写入不丢 —— 同一账号同一时刻只有一个写者，任何事件都不会丢。
    #[test]
    fn concurrent_writers_are_serialised_and_lose_nothing() {
        const N: usize = 8;
        let root = root();
        let barrier = Arc::new(Barrier::new(N));
        let handles: Vec<_> = (0..N)
            .map(|_| {
                let dir = root.clone();
                let gate = barrier.clone();
                std::thread::spawn(move || {
                    let uuid = uuid::Uuid::new_v4().to_string();
                    gate.wait();
                    for _ in 0..2_000 {
                        match Guard::open_in(&dir, 1, 2) {
                            Ok(mut g) => {
                                // 直接追加事件（不走闸门）：这里只验证「写者互斥 + 不丢行」。
                                g.append(Event::Pending {
                                    uuid: uuid.clone(),
                                    start_ms: 1000,
                                    stop_ms: 601_000,
                                    at_ms: T0,
                                    plan_fp: "fp".into(),
                                    target_dist: 1000,
                                })
                                .unwrap();
                                return;
                            }
                            Err(e) => {
                                // N6：Windows 下第二次 open 偶尔拿到 ERROR_SHARING_VIOLATION
                                // （映射成 PermissionDenied / os error 5），而不是锁失败。
                                // 两者都 fail-closed、都映射成同一句可读文案；这里放宽到
                                // 「要么是设计文案，要么是系统级的占用/拒绝访问」，
                                // 免得一条环境相关的抖动把 CI 挂掉。
                                assert!(is_acceptable_busy_error(&e), "意外错误：{e}");
                                std::thread::sleep(std::time::Duration::from_millis(1));
                            }
                        }
                    }
                    panic!("没能拿到回执锁");
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let g = Guard::open_in(&root, 1, 2).unwrap();
        assert_eq!(g.pending_count(), N, "并发写入丢了事件");
        drop(g);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exclusive_lock_and_account_isolation() {
        let root = root();
        let one = Guard::open_in(&root, 1, 2).unwrap();
        assert!(Guard::open_in(&root, 1, 2).is_err());
        let other = Guard::open_in(&root, 2, 2).unwrap();
        let school = Guard::open_in(&root, 1, 3).unwrap();
        drop((one, other, school));
        assert!(Guard::open_in(&root, 1, 2).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_rejection_allows_a_later_user_retry() {
        let root = root();
        let id = uuid::Uuid::new_v4().to_string();
        let mut g = Guard::open_in(&root, 1, 2).unwrap();
        new_attempt(&mut g, &id);
        assert!(g
            .begin(
                &uuid::Uuid::new_v4().to_string(),
                1000,
                2000,
                T0,
                "fp",
                1000
            )
            .is_err());
        g.rejected(&id, "服务端明确拒绝").unwrap();
        drop(g);
        assert!(Guard::open_in(&root, 1, 2)
            .unwrap()
            .check(1000, 2000, T0)
            .is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// 人工释放：补一条 Rejected 事件，解开封锁，但历史仍然可读。
    #[test]
    fn manual_release_is_appended_not_deleted() {
        let root = root();
        let id = uuid::Uuid::new_v4().to_string();
        let mut g = Guard::open_in(&root, 1, 2).unwrap();
        new_attempt(&mut g, &id);
        assert_eq!(g.clear_pending().unwrap(), 1);
        assert_eq!(g.pending_count(), 0);
        assert!(g.check(1000, 601_000, T0).is_ok());
        assert_eq!(g.clear_pending().unwrap(), 0);
        let lines = g.describe_unresolved(T0);
        assert!(lines.is_empty());
        drop(g);
        let mut g = Guard::open_in(&root, 1, 2).unwrap();
        // 释放后仍记得这条历史：再补 Accepted 已经没有对应请求了。
        assert!(g.accepted(&id, 5).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn plan_fingerprint_is_short_and_plan_specific() {
        let a = plan_fingerprint(1_700_000_000_000, 600, 1000.0, 1200, 1000);
        let b = plan_fingerprint(1_700_000_000_000, 600, 1000.0, 1200, 1000);
        let c = plan_fingerprint(1_700_000_000_000, 600, 1500.0, 1200, 1000);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|ch| ch.is_ascii_hexdigit()), "{a}");
    }
}
