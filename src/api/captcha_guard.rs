//! 验证码冷却。
//!
//! 实测：同一出口连续答错会把极验对该会话/出口的容忍度打掉（12 轮里 11 对，
//! 之后同一出口 2/16、0/8），而且换了出口也没恢复。用户看到「滑块过不去」后
//! 的第一反应就是连点重试 —— 那正好是最坏的动作：每点一次都在加深降级。
//!
//! 这里做**进程级持久**的冷却，界面上表现为「现在点也没用，等一会儿」，
//! 把用户从「反复点 → 越点越差」里拉出来。

use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
struct State {
    /// 连续判错次数（一次成功就清零）。
    reject_streak: u32,
    /// 最近一次判错的时间戳（毫秒）。
    last_reject_ms: u64,
    /// 冷却截止时间戳（毫秒）。
    cooldown_until_ms: u64,
}

fn state_path() -> std::path::PathBuf {
    crate::platform::data_dir().join("captcha_state.json")
}

fn now_ms() -> u64 {
    crate::crypto::envelope::now_ms().max(0) as u64
}

fn load() -> State {
    std::fs::read_to_string(state_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// 原子写 + 报错不再静默。
///
/// 旧实现直接 `fs::write`：并发登录时两个线程会先后截断同一个文件（写坏的 JSON
/// 下次读取会回退默认值，冷却计数直接丢失），写失败也只被 `let _ =` 吞掉 ——
/// 用户看到「说了要等 10 分钟」却又立刻能点。现在复用凭据文件的原子写
/// （同目录唯一临时文件 + fsync + rename），失败至少打到 stderr。
fn save(state: &State) -> Result<(), String> {
    let text = serde_json::to_string(state).map_err(|e| format!("序列化冷却状态失败: {e}"))?;
    crate::api::model::write_secret_file(&state_path(), text.as_bytes())
        .map_err(|e| format!("写入冷却状态失败: {e}"))
}

/// 进程内串行化 + 跨进程尽力而为的文件锁（读-改-写不能交错）。
static STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 跨进程锁：`create_new` 占位，用完删除。
///
/// 拿不到（别的进程正在写 / 崩溃留下僵尸锁）时**不阻塞业务流程**：
/// 最多等约 500 ms 后继续。宁可极偶发地丢一次计数，也不能让登录卡在锁上。
struct FileLock(Option<std::path::PathBuf>);

impl FileLock {
    fn acquire(path: &std::path::Path) -> Self {
        let lock = path.with_extension("json.lock");
        for _ in 0..50 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock)
            {
                Ok(_) => return Self(Some(lock)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(_) => return Self(None),
            }
        }
        Self(None)
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// 读-改-写的唯一入口：全程持锁，写完把失败报出来。
fn update<T>(f: impl FnOnce(&mut State) -> T) -> T {
    let _guard = STATE_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let _file_lock = FileLock::acquire(&state_path());
    let mut state = load();
    let out = f(&mut state);
    if let Err(e) = save(&state) {
        eprintln!("[captcha] {e}（本次冷却可能未被记录）");
    }
    out
}

/// 冷却中还剩多少秒。
pub fn cooldown_remaining_secs() -> Option<u64> {
    let state = load();
    let now = now_ms();
    if state.cooldown_until_ms > now {
        Some((state.cooldown_until_ms - now) / 1000 + 1)
    } else {
        None
    }
}

/// 一次验证成功（服务端确认 10000）——重置所有计数。
pub fn record_success() {
    update(|state| {
        state.reject_streak = 0;
        state.cooldown_until_ms = 0;
    });
}

/// 一次判定未通过。返回本次触发的冷却秒数（0 表示还没到冷却门槛）。
///
/// 门槛：连续 2 次 → 10 分钟；3 次 → 30 分钟；4 次及以上 → 60 分钟。
pub fn record_rejection() -> u64 {
    update(|state| {
        let now = now_ms();
        state.reject_streak = state.reject_streak.saturating_add(1);
        state.last_reject_ms = now;
        let secs = match state.reject_streak {
            0 | 1 => 0,
            2 => 600,
            3 => 1_800,
            _ => 3_600,
        };
        if secs > 0 {
            state.cooldown_until_ms = now + secs * 1_000;
        }
        secs
    })
}

/// 给用户看的等待文案。
pub fn cooldown_hint(secs: u64) -> String {
    if secs >= 60 {
        format!("约 {} 分钟", secs.div_ceil(60))
    } else {
        format!("约 {secs} 秒")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试独立目录：`platform::data_dir` 在测试期读 TEST_DATA_DIR。
    fn with_temp_dir<T>(name: &str, f: impl FnOnce() -> T) -> T {
        let dir = std::env::temp_dir().join(format!(
            "neko-captcha-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        crate::platform::TEST_DATA_DIR.with(|cell| *cell.borrow_mut() = Some(dir.clone()));
        let out = f();
        crate::platform::TEST_DATA_DIR.with(|cell| *cell.borrow_mut() = None);
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    #[test]
    fn second_rejection_starts_a_cooldown_and_success_clears_it() {
        with_temp_dir("cooldown", || {
            assert_eq!(cooldown_remaining_secs(), None, "初始不该冷却");
            assert_eq!(record_rejection(), 0, "第一次判错不触发冷却");
            assert_eq!(cooldown_remaining_secs(), None);
            let secs = record_rejection();
            assert_eq!(secs, 600, "连续第二次判错应冷却 10 分钟");
            let remaining = cooldown_remaining_secs().expect("应处于冷却中");
            assert!(remaining > 500 && remaining <= 601, "剩余 {remaining}s 不合理");

            record_success();
            assert_eq!(cooldown_remaining_secs(), None, "成功后必须清零");
            assert_eq!(record_rejection(), 0, "成功后重新从第一次算起");
        });
    }

    #[test]
    fn repeated_rejections_escalate_the_cooldown() {
        with_temp_dir("escalate", || {
            record_rejection();
            assert_eq!(record_rejection(), 600);
            assert_eq!(record_rejection(), 1_800);
            assert_eq!(record_rejection(), 3_600);
            assert_eq!(record_rejection(), 3_600, "四分钟以上封顶一小时");
        });
    }
}
