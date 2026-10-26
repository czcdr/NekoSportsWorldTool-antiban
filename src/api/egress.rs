//! 出口（代理）池：让请求来源 IP 与轨迹所在城市一致。
//!
//! 服务端拿得到我们的出口 IP。轨迹在 A 城、IP 在 B 城（或机房 IP）是最廉价、
//! 也最常被用来判定"数据非本人产生"的矛盾之一 —— 比分析轨迹本身便宜得多。
//! 这里把"选出口"做成和"选轨迹"同源的决策：城市取自设备锚点，出口必须同城。
//!
//! 没有配置代理时整体降级为直连，行为与改造前完全一致。

use std::collections::HashMap;
use std::path::Path;

/// 失败冷却档位（秒）：连续失败越多，冷却越久，避免把死代理反复拖进主流程。
pub const COOLDOWN_STEPS_SECS: [i64; 3] = [60, 180, 3600];

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProxyEntry {
    /// 稳定 id，用于回报健康度；缺省时用 url 代替。
    #[serde(default)]
    pub id: String,
    /// `socks5://host:port` 或 `http://[user:pass@]host:port`
    pub url: String,
    /// 出口所在城市（与设备锚点城市比对）。
    #[serde(default)]
    pub city: String,
    /// 运营商标签，仅做展示与优先筛选。
    #[serde(default)]
    pub carrier: String,
}

impl ProxyEntry {
    pub fn key(&self) -> &str {
        if self.id.is_empty() {
            &self.url
        } else {
            &self.id
        }
    }

    /// 是否是本机回环/内网出口（说明经用户自己的中转，而不是直接暴露家宽）。
    #[cfg(test)]
    pub fn is_local_relay(&self) -> bool {
        let lower = self.url.to_ascii_lowercase();
        ["127.0.0.1", "localhost", "192.168.", "10.", "172.16."]
            .iter()
            .any(|needle| lower.contains(needle))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Failed,
}

#[derive(Debug, Default, Clone)]
struct Health {
    consecutive_failures: u32,
    cool_until_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolError {
    /// 该城市没有配置任何出口 —— 不要跨城凑数。
    NoCandidate,
    /// 所有候选都在冷却中。
    Exhausted,
    /// 配置读取/解析失败。
    Config(String),
}

#[derive(Debug, Default)]
pub struct EgressPool {
    entries: Vec<ProxyEntry>,
    /// 进程内共享状态的键。
    ///
    /// 每个作业都会 `ApiClient::new` 并重新读盘，如果健康度和游标放在实例里，
    /// 冷却永远是"新的"、游标永远是 0 —— 结果就是死代理被反复使用、活代理从不轮到。
    /// 用「配置内容派生的稳定键」把状态放到进程级共享，同一份配置跨实例复用；
    /// 测试用独立键，避免用例互相污染。
    key: String,
}

#[derive(Debug, Default, Clone)]
struct SharedState {
    health: HashMap<String, Health>,
    cursor: usize,
}

/// 进程级共享的出口状态（键 = 池配置指纹）。
fn with_shared<T>(f: impl FnOnce(&mut HashMap<String, SharedState>) -> T) -> T {
    static SHARED: std::sync::OnceLock<std::sync::Mutex<HashMap<String, SharedState>>> =
        std::sync::OnceLock::new();
    let map = SHARED.get_or_init(Default::default);
    // 锁被 panic 污染时继续用内部值，不让健康度记录把主流程带崩。
    let mut guard = map.lock().unwrap_or_else(|poison| poison.into_inner());
    f(&mut guard)
}

/// 由配置内容派生稳定键：同一份 proxies.json 在任何实例上得到同一个键。
fn pool_key(entries: &[ProxyEntry]) -> String {
    let mut urls: Vec<&str> = entries.iter().map(|e| e.url.as_str()).collect();
    urls.sort_unstable();
    urls.join("\n")
}

impl EgressPool {
    /// 从 JSON 文件加载。文件不存在视为"未配置"（直连），不是错误。
    pub fn load(path: &Path) -> Result<Self, PoolError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .map_err(|e| PoolError::Config(format!("读取出口配置失败: {e}")))?;
        let entries: Vec<ProxyEntry> = serde_json::from_str(&text)
            .map_err(|e| PoolError::Config(format!("出口配置不是合法 JSON 数组: {e}")))?;
        for entry in &entries {
            let scheme = entry.url.split("://").next().unwrap_or("");
            if !matches!(scheme, "socks5" | "socks5h" | "http" | "https") {
                return Err(PoolError::Config(format!(
                    "不支持的代理协议 {scheme:?}（{}），只支持 socks5/http",
                    crate::textlog::redact_url(&entry.url)
                )));
            }
            if entry.url.contains(char::is_whitespace) {
                return Err(PoolError::Config(format!(
                    "代理地址含空白字符: {}",
                    crate::textlog::redact_url(&entry.url)
                )));
            }
        }
        let key = pool_key(&entries);
        Ok(Self { entries, key })
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 城市规范化：去空白、去"市"后缀后比较，避免"大连"与"大连市"错配。
    fn city_matches(entry_city: &str, want: &str) -> bool {
        fn norm(s: &str) -> String {
            s.trim()
                .trim_end_matches('市')
                .trim_end_matches(" City")
                .to_ascii_lowercase()
        }
        let want = norm(want);
        if want.is_empty() {
            // 没配置轨迹城市时不能"随便挑一个"：那等于静默跨城，
            // 与模块的核心约束（出口城市必须与轨迹城市一致）直接冲突。
            return false;
        }
        let got = norm(entry_city);
        // 空城市的出口视为"未标注"，不参与同城匹配（宁可不发，也不发错城）。
        !got.is_empty() && got == want
    }

    fn is_cooling(&self, key: &str, now_ms: i64) -> bool {
        let pool_key = self.key.clone();
        with_shared(|m| {
            m.get(&pool_key)
                .and_then(|state| state.health.get(key))
                .map(|h| h.cool_until_ms > now_ms)
                .unwrap_or(false)
        })
    }

    /// 取一个同城且不在冷却中的出口。绝不返回异地出口。
    pub fn acquire(&mut self, city: &str) -> Result<ProxyEntry, PoolError> {
        self.acquire_at(city, crate::crypto::envelope::now_ms())
    }

    pub fn acquire_at(&mut self, city: &str, now_ms: i64) -> Result<ProxyEntry, PoolError> {
        if self.entries.is_empty() {
            return Err(PoolError::NoCandidate);
        }
        if city.trim().is_empty() {
            // fail-closed：没有城市就没有"同城"可言，宁可不发也不跨城。
            return Err(PoolError::NoCandidate);
        }
        let candidates: Vec<&ProxyEntry> = self
            .entries
            .iter()
            .filter(|e| Self::city_matches(&e.city, city))
            .collect();
        if candidates.is_empty() {
            return Err(PoolError::NoCandidate);
        }
        let cooling: Vec<String> = candidates
            .iter()
            .filter(|e| self.is_cooling(e.key(), now_ms))
            .map(|e| e.key().to_string())
            .collect();
        let alive: Vec<&ProxyEntry> = candidates
            .into_iter()
            .filter(|e| !cooling.iter().any(|c| c == e.key()))
            .collect();
        if alive.is_empty() {
            return Err(PoolError::Exhausted);
        }
        let pool_key = self.key.clone();
        let cursor = with_shared(|m| {
            let state = m.entry(pool_key).or_default();
            let cursor = state.cursor;
            state.cursor = state.cursor.wrapping_add(1);
            cursor
        });
        Ok(alive[cursor % alive.len()].clone())
    }

    /// 回报一次使用的成败：成功清零，失败按连续次数升档冷却。
    pub fn report(&mut self, key: &str, outcome: Outcome) {
        self.report_at(key, outcome, crate::crypto::envelope::now_ms());
    }

    pub fn report_at(&mut self, key: &str, outcome: Outcome, now_ms: i64) {
        let pool_key = self.key.clone();
        with_shared(|m| {
            let health = m
                .entry(pool_key)
                .or_default()
                .health
                .entry(key.to_string())
                .or_default();
            match outcome {
                Outcome::Ok => {
                    health.consecutive_failures = 0;
                    health.cool_until_ms = 0;
                }
                Outcome::Failed => {
                    health.consecutive_failures = health.consecutive_failures.saturating_add(1);
                    let step = (health.consecutive_failures as usize - 1)
                        .min(COOLDOWN_STEPS_SECS.len() - 1);
                    health.cool_until_ms = now_ms + COOLDOWN_STEPS_SECS[step] * 1000;
                }
            }
        })
    }

    /// 当前可用（未冷却）出口数量。
    #[cfg(test)]
    pub fn available_count(&self, city: &str, now_ms: i64) -> usize {
        self.entries
            .iter()
            .filter(|e| Self::city_matches(&e.city, city) && !self.is_cooling(e.key(), now_ms))
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(urls: &[(&str, &str)]) -> EgressPool {
        EgressPool {
            entries: urls
                .iter()
                .map(|(url, city)| ProxyEntry {
                    id: String::new(),
                    url: url.to_string(),
                    city: city.to_string(),
                    carrier: String::new(),
                })
                .collect(),
            // 每个用例独立键：共享健康度是进程级的，同键会让断言互相污染。
            key: format!("test-{}", uuid::Uuid::new_v4()),
        }
    }

    #[test]
    fn never_crosses_city() {
        let mut p = pool(&[
            ("socks5://1.1.1.1:1080", "大连市"),
            ("socks5://2.2.2.2:1080", "沈阳市"),
        ]);
        assert_eq!(
            p.acquire_at("大连", 1000).unwrap().url,
            "socks5://1.1.1.1:1080"
        );
        assert_eq!(
            p.acquire_at("大连市", 1000).unwrap().url,
            "socks5://1.1.1.1:1080"
        );
        // 杭州没有任何出口：宁可 NoCandidate，也不拿大连的顶上
        assert_eq!(p.acquire_at("杭州", 1000), Err(PoolError::NoCandidate));
    }

    #[test]
    fn unlabelled_city_is_not_a_match() {
        let mut p = pool(&[("socks5://1.1.1.1:1080", "")]);
        assert_eq!(p.acquire_at("大连", 1000), Err(PoolError::NoCandidate));
        // 请求侧没有城市时同样拒绝：空城市 = 无法确认同城，
        // 此时"任何出口都可用"就是静默跨城（旧行为，已按 fail-closed 改掉）。
        assert_eq!(p.acquire_at("", 1000), Err(PoolError::NoCandidate));
        assert_eq!(p.acquire_at("   ", 1000), Err(PoolError::NoCandidate));
    }

    #[test]
    fn failures_escalate_cooldown_then_recover() {
        let mut p = pool(&[("socks5://1.1.1.1:1080", "大连")]);
        let key = "socks5://1.1.1.1:1080";
        // 第一次失败：60s
        p.report_at(key, Outcome::Failed, 1_000_000);
        assert_eq!(
            p.acquire_at("大连", 1_000_000 + 59_999),
            Err(PoolError::Exhausted)
        );
        assert!(p.acquire_at("大连", 1_000_000 + 60_001).is_ok());
        // 第二次：180s
        p.report_at(key, Outcome::Failed, 2_000_000);
        assert_eq!(
            p.acquire_at("大连", 2_000_000 + 179_999),
            Err(PoolError::Exhausted)
        );
        // 第三次：3600s
        p.report_at(key, Outcome::Failed, 3_000_000);
        assert_eq!(
            p.acquire_at("大连", 3_000_000 + 3_599_999),
            Err(PoolError::Exhausted)
        );
        // 成功清零
        p.report_at(key, Outcome::Ok, 4_000_000);
        assert!(p.acquire_at("大连", 4_000_000).is_ok());
        p.report_at(key, Outcome::Failed, 5_000_000);
        assert!(
            p.acquire_at("大连", 5_000_000 + 60_001).is_ok(),
            "清零后应回到 60s 档"
        );
    }

    #[test]
    fn rotates_through_alive_entries() {
        let mut p = pool(&[
            ("socks5://1.1.1.1:1080", "大连"),
            ("socks5://2.2.2.2:1080", "大连"),
        ]);
        let a = p.acquire_at("大连", 1000).unwrap().url;
        let b = p.acquire_at("大连", 1000).unwrap().url;
        let c = p.acquire_at("大连", 1000).unwrap().url;
        assert_ne!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn empty_pool_means_direct_connect() {
        let p = EgressPool::default();
        assert!(p.is_empty());
        assert_eq!(
            EgressPool::load(Path::new("definitely-missing.json"))
                .unwrap()
                .len(),
            0
        );
    }

    /// 同一份配置的两个实例必须共享健康度与游标 —— 否则每个作业都从"全新"开始，
    /// 冷却不生效、游标恒为 0（永远挑同一条出口）。
    #[test]
    fn health_and_cursor_are_shared_across_instances_of_the_same_config() {
        let dir = std::env::temp_dir().join(format!("egress-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("proxies.json");
        std::fs::write(
            &path,
            r#"[{"url":"socks5://10.9.9.1:1080","city":"大连"},{"url":"socks5://10.9.9.2:1080","city":"大连"}]"#,
        )
        .unwrap();
        let mut a = EgressPool::load(&path).unwrap();
        let mut b = EgressPool::load(&path).unwrap();
        // 轮转跨实例进行（同一份配置）
        let first = a.acquire_at("大连", 1_000).unwrap().url;
        let second = b.acquire_at("大连", 1_000).unwrap().url;
        assert_ne!(first, second, "两个实例应轮流用不同出口");
        // 冷却跨实例生效
        a.report_at(&first, Outcome::Failed, 1_000);
        let mut c = EgressPool::load(&path).unwrap();
        let picked = c.acquire_at("大连", 1_000).unwrap().url;
        assert_ne!(picked, first, "冷却中的出口不应再被任何实例选中");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_bad_scheme_and_whitespace() {
        let dir = std::env::temp_dir().join(format!("egress-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("proxies.json");
        std::fs::write(&path, r#"[{"url":"ftp://1.1.1.1:21","city":"大连"}]"#).unwrap();
        assert!(matches!(EgressPool::load(&path), Err(PoolError::Config(_))));
        std::fs::write(&path, r#"[{"url":"socks5://1.1.1.1:1080 ","city":"大连"}]"#).unwrap();
        assert!(matches!(EgressPool::load(&path), Err(PoolError::Config(_))));
        std::fs::write(&path, r#"[{"url":"socks5://1.1.1.1:1080","city":"大连"}]"#).unwrap();
        assert_eq!(EgressPool::load(&path).unwrap().len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn local_relay_detection() {
        let e = ProxyEntry {
            id: String::new(),
            url: "socks5://127.0.0.1:1080".into(),
            city: "大连".into(),
            carrier: String::new(),
        };
        assert!(e.is_local_relay());
        let e = ProxyEntry {
            id: String::new(),
            url: "socks5://8.8.8.8:1080".into(),
            city: "大连".into(),
            carrier: String::new(),
        };
        assert!(!e.is_local_relay());
    }

    #[test]
    fn available_count_excludes_cooling_entries() {
        let mut p = pool(&[
            ("socks5://1.1.1.1:1080", "大连"),
            ("socks5://2.2.2.2:1080", "大连"),
        ]);
        assert_eq!(p.available_count("大连", 1000), 2);
        p.report_at("socks5://1.1.1.1:1080", Outcome::Failed, 1000);
        assert_eq!(p.available_count("大连", 1001), 1);
        assert_eq!(p.available_count("大连", 1000 + 60_001), 2);
        // 异城不计入候选
        assert_eq!(p.available_count("沈阳", 1000), 0);
    }
}
