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

fn save(state: &State) {
    if let Ok(text) = serde_json::to_string(state) {
        let _ = std::fs::write(state_path(), text);
    }
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
    let mut state = load();
    state.reject_streak = 0;
    state.cooldown_until_ms = 0;
    save(&state);
}

/// 一次判定未通过。返回本次触发的冷却秒数（0 表示还没到冷却门槛）。
///
/// 门槛：连续 2 次 → 10 分钟；3 次 → 30 分钟；4 次及以上 → 60 分钟。
pub fn record_rejection() -> u64 {
    let mut state = load();
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
    save(&state);
    secs
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
