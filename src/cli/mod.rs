//! 命令行入口：子命令分发 + 共享助手。
//! 子命令实现见 ；数据文件与 GUI 共用（exe 同目录）。
//!
//! 青龙定时任务：首次 `login --user .. --pass .. --remember`，之后定时 `run`
//! 即可（会话失效且已记住凭据时自动重登）。

mod cmds;

use crate::api::client::ApiClient;
use crate::api::model::{self, Session};
use crate::api::records::RecordRow;

pub fn main(args: Vec<String>) -> i32 {
    cmds::dispatch(args)
}

fn usage() {
    println!(
        r#"NekoSportsWorldTool CLI（full 构建无参数启动 GUI）

  login   --user <手机号> --pass <密码|-> [--remember]
                                           登录并保存会话；--pass - 从标准输入读密码（避免留在 shell 历史与进程列表）
                                           --remember 同时保存凭据供自动重登；也可用环境变量 NSWT_PASS
  logout                                   登出并清理本地会话
  run    [--dist km] [--pace 秒/km] [--altitude 米或min-max] [--ago 分钟] [--days-ago 0-3 --time HH:MM] [--face 0|1] [--seed n] [--route legacy|road|manual]
                                           跑步全链：策略-点位-轨迹-提交-OBS-验证（--route 选择路线算法；
                                           manual=只按实时点位串联、不要求策略必经点，无路网时退回按点位连线）
  template --file <GPX/JSON>                本地读取真实记录，分析海拔（不会上传）
  ai-list                                  AI 运动项目列表
  ai     --sport <id> [--mode min|count] [--score n]
                                           AI 运动：min 按分钟 1-30；count 按次 5-1000 步长 5
  records                                  跑步记录列表
  ai-records --sport <id>                  AI 运动记录（按天分组）
  semester                                 学期完成度
  cheat  [--page n]                        违规自查
  recover [--yes] [--verify] [--only rrid] [--limit n] [--dist km] [--pace 秒/km] [--force]
                                           异常记录修复：默认只诊断（违规榜 + 未达标记录 +
                                           原因）；--yes 才用原开始时刻覆盖提交，默认一次最多 3 条；
                                           --verify 修完复查（记录是否转绿、违规榜是否清）；
                                           冻结状态读不到时默认停手，--force 才跳过该检查
  rank   main   --type 1|2|3 --sort 1|2 [--gender 0|1] [--date YYYY-MM-DD]
                                           排行榜（1个人 2班级 3院系；1日 2月）
  rank   indoor --range 1|2|3 [--gender 0|1]
                                           室内榜（1日 2周 3月）
  rank   history --sort 1|2 [--gender 0|1] 历史榜
  update [--check]                        检查更新；默认下载并自替换（--check 仅检查）
  unlock [--yes]                          查看/释放卡住的提交回执（结果未知时不阻塞后续提交）

当天榜单通常在有效里程产生后才有数据；run 默认随机 1.0~1.5 km / 6~8 分配速 / 30-300 分钟前。"#
    );
}

pub(crate) fn parse_flags(rest: &[&str]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        let k = rest[i].trim_start_matches('-').to_string();
        // 单独一个 `-` 是「值从标准输入读」的约定写法（`login --pass -`）：
        // 它必须以**值**的身份被取走，否则会被当成一个空名开关吞掉，
        // `--pass` 只能拿到兜底值 "1"，安全输入路径整条不可达。
        // 其余以 `-` 开头的一律当开关，不吃下一个 token。
        let v = rest
            .get(i + 1)
            .filter(|n| !n.is_empty() && (*n == &"-" || !n.starts_with('-')))
            .map(|s| s.to_string());
        match v {
            Some(v) => {
                if !k.is_empty() {
                    out.push((k, v));
                }
                i += 2;
            }
            None => {
                if !k.is_empty() {
                    out.push((k, "1".into()));
                }
                i += 1;
            }
        }
    }
    out
}

pub(crate) fn get<'a>(flags: &'a [(String, String)], name: &str) -> Option<&'a str> {
    flags
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

pub(crate) fn logger() -> impl FnMut(&str) {
    |s: &str| println!("{}", crate::textlog::clean(s))
}

pub(crate) fn silent() -> impl FnMut(&str) {
    |_s: &str| {}
}

/// 自动登录（GUI 记住密码 / login --remember 落盘的凭据）。
pub(crate) fn auto_login(identity: &model::HeaderIdentity) -> Result<Session, String> {
    let cfg = model::load_config();
    if cfg.remember && !cfg.username.is_empty() && !cfg.password.is_empty() {
        println!("[login] 使用已保存的账号自动登录…");
        let mut client = ApiClient::new(identity.clone(), None);
        return crate::api::login::login(&mut client, &cfg.username, &cfg.password, &mut silent());
    }
    Err("未登录：先执行 login 子命令，或在 GUI 勾选记住密码".into())
}

/// 构造已登录客户端；本地会话失效时自动重登一次。
pub(crate) fn make_client() -> Result<ApiClient, String> {
    let identity = model::load_identity();
    let client = {
        let sess = model::load_session();
        if sess.is_logged_in() {
            ApiClient::new(identity, Some(sess))
        } else {
            let s = auto_login(&identity)?;
            ApiClient::new(identity, Some(s))
        }
    };
    // 不做前置健康检查：业务错误（如新账号无学期数据）不等于会话过期，
    // 贸然重登会把好 session 删掉并触发 10121。会话真正过期时业务接口
    // 自然返回 401，由调用方处理。
    Ok(client)
}

pub(crate) fn now_ms() -> i64 {
    crate::crypto::envelope::now_ms()
}

pub(crate) fn fmt_hms(ms: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

/// 只要 "YYYY-MM-DD HH:MM:SS" 的 "MM-DD HH:MM:SS" 部分。
///
/// `fmt_hms` 在时间戳无法格式化时返回**空串**，直接 `[5..]` 会 panic
/// （伪造/异常的 `startTime` 就能让整个 `records` 崩掉）。按字符边界取，取不到就原样返回。
pub(crate) fn short_stamp(stamp: &str) -> &str {
    stamp.get(5..).unwrap_or(stamp)
}

pub(crate) fn print_rows(rows: &[RecordRow]) {
    println!(
        "{:<16} {:>8} {:>8} {:>6} {:>6} {:>4} rrid",
        "时间", "距离m", "时长", "配速", "步频", "达标"
    );
    for r in rows {
        let stamp = fmt_hms(r.start_time);
        let t = short_stamp(&stamp);
        let pace = if r.total_dis > 0.0 && r.total_time > 0 {
            let p = r.total_time as f64 / (r.total_dis / 1000.0);
            format!("{}:{:02}", (p / 60.0) as i64, (p as i64) % 60)
        } else {
            "-".into()
        };
        println!(
            "{:<16} {:>8.0} {:>8} {:>6} {:>6} {:>4} {}",
            t,
            r.total_dis,
            format!(
                "{}:{:02}:{:02}",
                r.total_time / 3600,
                r.total_time % 3600 / 60,
                r.total_time % 60
            ),
            pace,
            r.avg_step_freq,
            if r.complete { "是" } else { "否" },
            r.rrid
        );
    }
}

pub(crate) fn jstr(v: &serde_json::Value, keys: &[&str]) -> String {
    for k in keys {
        match v.get(k) {
            Some(serde_json::Value::String(s)) => return s.clone(),
            Some(serde_json::Value::Number(n)) => return n.to_string(),
            _ => {}
        }
    }
    "-".into()
}

pub(crate) fn parse_pace(v: &str) -> f32 {
    if let Some((m, s)) = v.split_once(':') {
        m.parse::<f32>().unwrap_or(0.0) * 60.0 + s.parse::<f32>().unwrap_or(0.0)
    } else {
        v.parse().unwrap_or(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(rest: &[&str]) -> Vec<(String, String)> {
        parse_flags(rest)
    }

    /// 回归：`--pass -` 必须把 "-" 当作**值**取走，而不是兜底成 "1"。
    #[test]
    fn dash_is_consumed_as_a_value() {
        assert_eq!(
            pairs(&["--user", "13800000000", "--pass", "-", "--remember"]),
            vec![
                ("user".to_string(), "13800000000".to_string()),
                ("pass".to_string(), "-".to_string()),
                ("remember".to_string(), "1".to_string()),
            ]
        );
        // 前面没有 flag 的裸 "-" 不再产生空名开关（也不会把后面的 token 当值留下）
        assert!(pairs(&["-", "Secret"]).is_empty());
        assert_eq!(pairs(&["--pass", "-"]), vec![("pass".to_string(), "-".to_string())]);
    }

    #[test]
    fn ordinary_flags_and_values_still_parse() {
        assert_eq!(
            pairs(&["--dist", "1.2", "--pace", "6:30", "--face", "0"]),
            vec![
                ("dist".to_string(), "1.2".to_string()),
                ("pace".to_string(), "6:30".to_string()),
                ("face".to_string(), "0".to_string()),
            ]
        );
        // 空字符串不是值；后续 token 仍是开关
        assert_eq!(
            pairs(&["--route", "", "--seed", "7"]),
            vec![
                ("route".to_string(), "1".to_string()),
                ("seed".to_string(), "7".to_string()),
            ]
        );
        // 以 '-' 开头的长选项不会被当成上一个选项的值
        assert_eq!(
            pairs(&["--only", "--yes"]),
            vec![
                ("only".to_string(), "1".to_string()),
                ("yes".to_string(), "1".to_string()),
            ]
        );
    }

    #[test]
    fn get_returns_first_match() {
        let f = pairs(&["--user", "a", "--user", "b"]);
        assert_eq!(get(&f, "user"), Some("a"));
        assert_eq!(get(&f, "missing"), None);
    }

    /// 回归：时间戳无效时 `fmt_hms` 返回空串，短格式不能 panic。
    #[test]
    fn short_stamp_survives_invalid_timestamp() {
        assert_eq!(short_stamp(""), "");
        assert_eq!(short_stamp("2026-03-05 07:30:00"), "03-05 07:30:00");
        assert_eq!(short_stamp("abc"), "abc");
        // 非法毫秒值走 fmt_hms 的真实路径
        assert_eq!(fmt_hms(i64::MAX), "");
        assert_eq!(short_stamp(&fmt_hms(i64::MAX)), "");
    }
}
