//! CLI 子命令实现。

use super::{fmt_hms, get, jstr, logger, make_client, now_ms, parse_flags, parse_pace, print_rows};
use crate::api::ai::AiMode;
use crate::api::client::ApiClient;
use crate::api::model;

pub fn dispatch(args: Vec<String>) -> i32 {
    let cmd = args.first().map(|x| x.as_str()).unwrap_or("help");
    let rest = args.iter().skip(1).map(|x| x.as_str()).collect::<Vec<_>>();
    match cmd {
        "login" => cmd_login(&rest),
        "logout" => cmd_logout(),
        "run" => cmd_run(&rest),
        "template" => cmd_template(&rest),
        "ai" => cmd_ai(&rest),
        "ai-list" => cmd_ai_list(),
        "records" => cmd_records(),
        "records-raw" => cmd_records_raw(&rest),
        "record-info" => cmd_record_info(&rest),
        "obs-get" => cmd_obs_get(&rest),
        "obs-sample" => cmd_obs_sample(&rest),
        "ai-records" => cmd_ai_records(&rest),
        "ai-info" => cmd_ai_info(&rest),
        "semester" => cmd_semester(),
        "cheat" => cmd_cheat(&rest),
        "recover" => cmd_recover(&rest),
        "rank" => cmd_rank(&rest),
        "unlock" => cmd_unlock(&rest),
        "update" => cmd_update(&rest),
        "help" | "--help" | "-h" => {
            usage();
            0
        }
        _ => {
            eprintln!("未知命令: {cmd}");
            super::usage();
            1
        }
    }
}

fn usage() {
    super::usage();
}

fn cmd_login(rest: &[&str]) -> i32 {
    let flags = parse_flags(rest);
    let Some(user) = get(&flags, "user") else {
        eprintln!("缺少 --user");
        return 1;
    };
    let Some(pw) = read_password(&flags) else {
        eprintln!(
            "缺少 --pass：可直接给值，或用 `--pass -` 从标准输入读取，或设置环境变量 NSWT_PASS"
        );
        return 1;
    };
    let identity = model::load_identity();
    let mut client = ApiClient::new(identity, None);
    let mut log = logger();
    match crate::api::login::login(&mut client, user, &pw, &mut log) {
        Ok(s) => {
            if get(&flags, "remember").is_some() {
                let mut cfg = model::load_config();
                cfg.username = user.to_string();
                cfg.password = pw.clone();
                cfg.remember = true;
                // 保存失败必须报出来：磁盘满/只读目录下说"已保存"是谎报，
                // 用户下次定时任务会以"未登录"失败，却找不到原因。
                if let Err(e) = model::save_config(&cfg) {
                    eprintln!("× 登录成功，但凭据未能保存：{e}（本次会话有效，下次需重新登录）");
                    return 1;
                }
                println!("凭据已保存（会话失效时自动重登）");
            }
            println!("登录成功 uid={} unid={} name={}", s.uid, s.unid, s.name);
            0
        }
        Err(e) => {
            eprintln!("登录失败: {e}");
            1
        }
    }
}

/// 取登录口令。
///
/// `--pass <值>` 会进 shell 历史、进程列表（`ps` 对所有用户可见）和计划任务日志，
/// 所以额外支持 `--pass -` 从标准输入读一行，以及 `NSWT_PASS` 环境变量
/// （青龙这类改不了参数的调度场景兜底）。优先级：`--pass` > `NSWT_PASS`。
fn read_password(flags: &[(String, String)]) -> Option<String> {
    if let Some(value) = get(flags, "pass") {
        if value == "-" {
            use std::io::Write;
            eprint!("密码（从标准输入读取，输入内容会回显在终端）：");
            let _ = std::io::stderr().flush();
            let mut line = String::new();
            return match std::io::stdin().read_line(&mut line) {
                Ok(0) => None,
                Ok(_) => Some(line.trim_end_matches(['\r', '\n']).to_string()),
                Err(_) => None,
            };
        }
        return Some(value.to_string());
    }
    std::env::var("NSWT_PASS").ok().filter(|v| !v.is_empty())
}

fn cmd_logout() -> i32 {
    let identity = model::load_identity();
    let sess = model::load_session();
    let mut client = ApiClient::new(identity, sess.is_logged_in().then_some(sess));
    let mut log = logger();
    crate::api::login::logout(&mut client, &mut log);
    0
}

fn cmd_run(rest: &[&str]) -> i32 {
    let flags = parse_flags(rest);
    let mut client = match make_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let dist_km: f32 = get(&flags, "dist")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);
    let pace: f32 = get(&flags, "pace").map(parse_pace).unwrap_or(0.0);
    let ago_min: i64 = get(&flags, "ago").and_then(|v| v.parse().ok()).unwrap_or(0);
    let days_ago: i64 = get(&flags, "days-ago")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
        .clamp(0, 3);
    let time_spec = get(&flags, "time").unwrap_or("");
    let face = get(&flags, "face")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(true);
    let (manual_altitude, manual_altitude_range) = match get(&flags, "altitude") {
        Some(value) => match crate::track::altitude::parse_spec(value) {
            Ok(Some(crate::track::altitude::AltitudeSpec::Single(value))) => (Some(value), None),
            Ok(Some(crate::track::altitude::AltitudeSpec::Range(range))) => (None, Some(range)),
            Ok(None) => (None, None),
            Err(e) => {
                eprintln!("--altitude {e}");
                return 1;
            }
        },
        None => (None, None),
    };
    let seed: u64 = get(&flags, "seed")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let seed = if seed == 0 {
        (now_ms() % 2_147_483_647) as u64
    } else {
        seed
    };
    let cfg = model::load_config();
    let route_mode = crate::track::generate_road::RouteMode::from_str(
        get(&flags, "route").unwrap_or(&cfg.route_mode),
    );

    // C10：`--school-altitude` 把校区基准海拔**持久化**到 config.json（填一次即可），
    // 之后 GUI 与 CLI 都用它生成 bdA。合法范围与手动海拔一致（-500–9000 m）。
    if let Some(text) = get(&flags, "school-altitude") {
        let parsed = text.trim().parse::<f64>().ok().filter(|v| {
            v.is_finite()
                && (crate::track::altitude::ALTITUDE_MIN_M..=crate::track::altitude::ALTITUDE_MAX_M)
                    .contains(v)
        });
        match parsed {
            Some(value) => {
                let mut updated = cfg.clone();
                updated.school_base_altitude = Some(value);
                match model::save_config(&updated) {
                    Ok(()) => println!("已把学校基准海拔写入 config.json：{value:.1} m"),
                    Err(e) => {
                        eprintln!("保存学校基准海拔失败：{e}");
                        return 1;
                    }
                }
            }
            None => {
                eprintln!(
                    "--school-altitude 需要 {} 到 {} 之间的数字（米）",
                    crate::track::altitude::ALTITUDE_MIN_M,
                    crate::track::altitude::ALTITUDE_MAX_M
                );
                return 1;
            }
        }
    }

    let dist = if dist_km > 0.0 {
        dist_km as f64 * 1000.0
    } else {
        (1.0 + rand::random::<f32>() * 0.5) as f64 * 1000.0
    };
    let pace_s = if pace > 0.0 {
        pace
    } else {
        360.0 + rand::random::<f32>() * 120.0
    };
    let dur = (dist as f32 / 1000.0 * pace_s) as i64;
    let start_ms = if !time_spec.is_empty() {
        // --time "HH:MM" 配合 --days-ago（0-3）
        let (h, m) = time_spec.split_once(':').unwrap_or((time_spec, "0"));
        let (h, m) = (
            h.parse::<u32>().unwrap_or(7) % 24,
            m.parse::<u32>().unwrap_or(0),
        );
        let base = chrono::Local::now() - chrono::Duration::days(days_ago);
        use chrono::{Datelike, TimeZone};
        chrono::Local
            .with_ymd_and_hms(base.year(), base.month(), base.day(), h, m, 0)
            .single()
            .map(|x| x.timestamp_millis())
            .unwrap_or(now_ms())
    } else if ago_min > 0 {
        now_ms() - ago_min * 60_000
    } else {
        now_ms() - 30 * 60_000 - (rand::random::<f64>() * 270.0 * 60_000.0) as i64
    };
    // C1：`--time` 可能落在「现在 - 用时」之后（例如现在 12:05 却填 12:00、用时 30 分钟），
    // 那样 stopTime 就落在未来 —— 真机不可能提交还没跑完的记录。
    // flow 里还有一道最终校验（用 effective_duration 之后的时长），这里先按输入时长收紧。
    let start_ms = start_ms.min(now_ms() - dur * 1000 - 5_000);

    println!(
        "参数：{:.0}m / {}s / 配速 {}:{:02}/km / 开始 {}",
        dist,
        dur,
        pace_s as i64 / 60,
        pace_s as i64 % 60,
        fmt_hms(start_ms)
    );

    let mut log = logger();
    let params = crate::api::flow::RunParams {
        dist,
        dur,
        start_ms,
        face_check: face as i64,
        manual_altitude,
        manual_altitude_range,
        seed,
        route_mode,
    };
    match crate::api::flow::run_full_flow(&mut client, &params, &mut log) {
        Ok(out) => {
            println!(
                "记录已被服务端接受 rrid={} uuid={}；OBS 上传={}/2；详情读取={}；服务端达标判定={}",
                out.result.rrid,
                out.result.uuid,
                out.obs_ok,
                if out.verification.detail.is_some() {
                    "成功"
                } else {
                    "失败"
                },
                out.verification.label()
            );
            // 退出码区分四种结局：0 完成 / 2 OBS 不全 / 3 详情未知 / 4 未达标。
            // 定时脚本据此判断「重试有没有意义」，不再把"请求被接受"当成功。
            if out.is_success() {
                println!("结论：{}", out.summary());
            } else {
                eprintln!("结论：{}", out.summary());
            }
            out.exit_code()
        }
        Err(e) => {
            eprintln!("跑步提交失败: {e}");
            1
        }
    }
}

fn cmd_template(rest: &[&str]) -> i32 {
    let flags = parse_flags(rest);
    let Some(path) = get(&flags, "file") else {
        eprintln!("缺少 --file <GPX/JSON>");
        return 1;
    };
    match crate::template::load(path) {
        Ok(samples) => {
            let summary = crate::template::summarize(path, &samples);
            println!("本地模板分析（仅预览，不上传）");
            println!("文件：{}", summary.source);
            println!("采样点：{}", summary.samples);
            println!("海拔范围：{:.1}–{:.1} m", summary.min_m, summary.max_m);
            println!(
                "累计上升：{:.1} m；累计下降：{:.1} m",
                summary.gain_m, summary.loss_m
            );
            0
        }
        Err(e) => {
            eprintln!("模板读取失败：{e}");
            1
        }
    }
}

fn cmd_ai(rest: &[&str]) -> i32 {
    let flags = parse_flags(rest);
    let Some(sport) = get(&flags, "sport").and_then(|v| v.parse().ok()) else {
        eprintln!("缺少 --sport");
        return 1;
    };
    // 按分钟（--minutes 1-30）或按次（--count 5-1000 步长 5）
    let mode = match get(&flags, "mode").unwrap_or("min") {
        "count" => {
            let reps = get(&flags, "score")
                .or_else(|| get(&flags, "count"))
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(5)
                .clamp(5, 1000);
            AiMode::Count {
                reps: (reps / 5) * 5,
            }
        }
        _ => {
            let minutes = get(&flags, "score")
                .or_else(|| get(&flags, "minutes"))
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(1)
                .clamp(1, 30);
            AiMode::Minutes { minutes }
        }
    };
    let mut client = match make_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let mut log = logger();
    match crate::api::flow::run_ai_submit(&mut client, sport, mode, &mut log) {
        Ok(biz) => {
            println!(
                "AI 提交成功 服务器={} {}",
                biz.get("error").and_then(|e| e.as_i64()).unwrap_or(0),
                biz.get("message").and_then(|m| m.as_str()).unwrap_or("")
            );
            0
        }
        Err(e) => {
            eprintln!("AI 提交失败: {e}");
            1
        }
    }
}

fn cmd_ai_list() -> i32 {
    let mut client = match make_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let mut log = logger();
    match crate::api::flow::run_ai_list(&mut client, &mut log) {
        Ok(list) => {
            for s in list {
                println!("id={:<4} {}", s.id, s.name);
            }
            0
        }
        Err(e) => {
            eprintln!("拉取失败: {e}");
            1
        }
    }
}

/// 手动释放卡住的提交回执。
///
/// 提交响应解不开（WAF 拦截页、gzip、超时）时会被记为「结果未知」：下一次提交前会先
/// 只读对账记录列表，查到就不许再提，查不到才放行 —— 但**对账本身也可能失败**
/// （断网、列表接口被拦），那就一直卡着。这里给一个显式的、需要人确认的出口。
/// 注意：释放只是补一条「本次没有入账」的回执，不会删历史。
fn cmd_unlock(rest: &[&str]) -> i32 {
    let flags = parse_flags(rest);
    let sess = model::load_session();
    if !sess.is_logged_in() {
        eprintln!("未登录：先 login，或让 GUI 登录后再执行");
        return 1;
    }
    let unid: i64 = match sess.unid.parse() {
        Ok(v) => v,
        Err(_) => {
            eprintln!("学校编号无效，无法定位提交回执");
            return 1;
        }
    };
    let mut guard = match crate::api::submission_guard::Guard::open(sess.uid, unid) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let pending = guard.pending_count();
    if get(&flags, "yes").is_none() {
        if pending == 0 {
            println!("没有卡住的提交回执");
        } else {
            println!("有 {pending} 条未确认的提交回执（上次提交结果未知）。");
            // 列出每一条：起跑时间 / 目标距离 / 计划指纹，方便对着官方记录逐条核。
            for line in guard.describe_unresolved(crate::crypto::envelope::now_ms()) {
                println!("  · {line}");
            }
            println!("请先到官方运动记录页核对：确认确实没有入账，再执行 `unlock --yes` 释放。");
            println!("未查到不代表未入账 —— 释放后重复提交可能产生两条记录。");
        }
        return 0;
    }
    match guard.clear_pending() {
        Ok(0) => {
            println!("没有卡住的提交回执");
            0
        }
        Ok(n) => {
            println!("已释放 {n} 条未确认回执，现在可以重新提交");
            0
        }
        Err(e) => {
            eprintln!("释放失败: {e}");
            1
        }
    }
}

fn cmd_records() -> i32 {
    let mut client = match make_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let mut log = logger();
    match crate::api::flow::run_records(&mut client, &mut log) {
        Ok(rows) => {
            print_rows(&rows);
            0
        }
        Err(e) => {
            eprintln!("拉取失败: {e}");
            1
        }
    }
}

/// 记录列表原始探测：支持自定义 body，打印条数与日期范围。
fn cmd_records_raw(rest: &[&str]) -> i32 {
    let flags = parse_flags(rest);
    let body = get(&flags, "body").unwrap_or("{}").to_string();
    let mut client = match make_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    match client.call("POST", crate::api::records::RECORDS_PATH, &body, &[]) {
        Ok(biz) => {
            let data = crate::api::client::parse_data_field(&biz);
            let arr = data.as_array().cloned().unwrap_or_default();
            let n = arr.len();
            println!("条数: {n}");
            if let Some(out) = get(&flags, "out") {
                let text = match serde_json::to_string_pretty(&arr) {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!("× 序列化失败: {e}");
                        return 1;
                    }
                };
                if let Err(e) = std::fs::write(out, text) {
                    eprintln!("× 写入 {out} 失败: {e}");
                    return 1;
                }
                println!("已保存 {out}");
            } else if n > 0 {
                println!("首条: {}", preview(&arr[0], 300));
                println!("末条: {}", preview(&arr[n - 1], 300));
            } else {
                println!("data: {}", preview(&data, 500));
            }
            0
        }
        Err(e) => {
            eprintln!("拉取失败: {e}");
            1
        }
    }
}

/// 拉取 OBS 对象（签名 GET），解码 gzip+base64 字段后保存。
fn cmd_obs_get(rest: &[&str]) -> i32 {
    let flags = parse_flags(rest);
    let Some(key) = get(&flags, "key") else {
        eprintln!("缺少 --key");
        return 1;
    };
    let mut client = match make_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let mut log = |s: &str| eprintln!("{s}");
    match crate::api::obs::fetch_object(&mut client, key, &mut log) {
        Ok(v) => {
            let out = get(&flags, "out").unwrap_or("obs_real.json");
            // 解码 gzip+base64 值便于直接阅读
            let decoded = decode_gz_fields(&v);
            let text = match serde_json::to_string_pretty(&decoded) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("× 序列化失败: {e}");
                    return 1;
                }
            };
            if let Err(e) = std::fs::write(out, text) {
                eprintln!("× 写入 {out} 失败: {e}");
                return 1;
            }
            println!("已保存 {out}");
            0
        }
        Err(e) => {
            eprintln!("拉取失败: {e}");
            1
        }
    }
}

/// 解码 OBS 对象中 gzip+base64 的字段值（尽力而为）。
fn decode_gz_fields(v: &serde_json::Value) -> serde_json::Value {
    use base64::Engine;
    let mut out = v.clone();
    if let Some(obj) = out.as_object_mut() {
        for (_k, val) in obj.iter_mut() {
            if let Some(s) = val.as_str() {
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(s)
                    .ok()
                    .and_then(|raw| {
                        let mut dec = flate2::read::GzDecoder::new(raw.as_slice());
                        use std::io::Read;
                        let mut txt = String::new();
                        dec.read_to_string(&mut txt).ok()?;
                        Some(txt)
                    });
                if let Some(txt) = decoded {
                    let parsed = serde_json::from_str::<serde_json::Value>(&txt)
                        .map(|x| x.to_string())
                        .unwrap_or(txt);
                    *val = serde_json::Value::String(parsed);
                }
            }
        }
    }
    out
}

/// 本地生成一份 OBS 对象样本（不提交；结构对照用）。
fn cmd_obs_sample(rest: &[&str]) -> i32 {
    let flags = parse_flags(rest);
    let dist = get(&flags, "dist")
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(1050.0);
    let dur = get(&flags, "dur")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(480);
    let rrid = get(&flags, "rrid")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(1320000000);
    let seed = get(&flags, "seed")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(7);
    let mut client = match make_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let mut log = |s: &str| eprintln!("{s}");
    let anchor = match client.identity.anchor_coordinate() {
        Ok(value) => value,
        Err(e) => {
            eprintln!("定位锚点无效: {e}");
            return 1;
        }
    };
    let pts = match crate::api::points::fetch_points(&mut client, anchor, &mut log) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("点位拉取失败: {e}");
            return 1;
        }
    };
    let pts_bd = crate::api::points::points_bd(&pts);
    let start_ms = crate::crypto::envelope::now_ms() - dur * 1000;
    let track = crate::track::generator::build(
        dist,
        dur,
        seed,
        (anchor.latitude, anchor.longitude),
        start_ms,
        &pts_bd,
    );
    let sess = client.login.clone().unwrap_or_default();
    let uuid = uuid::Uuid::new_v4().to_string().to_uppercase();
    let obj = crate::track::wire::build_obs_object(
        &track,
        rrid,
        &uuid,
        sess.uid,
        &crate::track::wire::five_point_wrapper(&pts, &track, &Default::default()),
    );
    let out = get(&flags, "out").unwrap_or("obs_ours.json");
    let decoded = decode_gz_fields(&obj);
    let text = match serde_json::to_string_pretty(&decoded) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("× 序列化失败: {e}");
            return 1;
        }
    };
    if let Err(e) = std::fs::write(out, text) {
        eprintln!("× 写入 {out} 失败: {e}");
        return 1;
    }
    println!("已保存 {out}");
    0
}

/// 单条跑步记录原始详情。
fn cmd_record_info(rest: &[&str]) -> i32 {
    let flags = parse_flags(rest);
    let Some(rrid) = get(&flags, "rrid").and_then(|v| v.parse::<i64>().ok()) else {
        eprintln!("缺少 --rrid");
        return 1;
    };
    let mut client = match make_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    match crate::api::records::fetch_one_record(&mut client, rrid) {
        Ok(v) => println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default()),
        Err(e) => {
            eprintln!("拉取失败: {e}");
            return 1;
        }
    }
    0
}

/// 单条 AI 记录全量详情。
fn cmd_ai_info(rest: &[&str]) -> i32 {
    let flags = parse_flags(rest);
    let Some(id) = get(&flags, "id").and_then(|v| v.parse::<i64>().ok()) else {
        eprintln!("缺少 --id");
        return 1;
    };
    let mut client = match make_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    match crate::api::ai::fetch_record_detail(&mut client, id) {
        Ok(v) => println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default()),
        Err(e) => {
            eprintln!("拉取失败: {e}");
            return 1;
        }
    }
    0
}

fn cmd_ai_records(rest: &[&str]) -> i32 {
    let flags = parse_flags(rest);
    let Some(sport) = get(&flags, "sport").and_then(|v| v.parse().ok()) else {
        eprintln!("缺少 --sport");
        return 1;
    };
    let mut client = match make_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    match crate::api::ai::fetch_records(&mut client, sport, 50) {
        Ok(page) => {
            use chrono::TimeZone;
            for g in page.groups {
                let date = chrono::Local
                    .timestamp_millis_opt(g.score_date)
                    .single()
                    .map(|t| t.format("%Y-%m-%d").to_string())
                    .unwrap_or_default();
                println!("{date} ×{}：", g.frequency);
                for r in g.records {
                    let grade = if r.rtype == 2 {
                        format!("{:.1} 秒", r.score.parse::<f64>().unwrap_or(0.0) / 1000.0)
                    } else {
                        format!("{} 个", r.score)
                    };
                    let finish = chrono::Local
                        .timestamp_millis_opt(r.score_date)
                        .single()
                        .map(|t| t.format("%H:%M:%S").to_string())
                        .unwrap_or_default();
                    let video = if r.has_video { "有" } else { "-" };
                    println!("  {} {} 完成 {finish} 视频 {video}", r.name, grade);
                }
            }
            0
        }
        Err(e) => {
            eprintln!("拉取失败: {e}");
            1
        }
    }
}

fn cmd_semester() -> i32 {
    let mut client = match make_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let mut log = logger();
    match crate::api::semester::query(&mut client, &mut log) {
        Ok(r) => {
            if let Some(s) = r.summary {
                println!(
                    "学期：{}  有效次数：{}/{}  有效里程：{:.2} km（总 {:.2} km）",
                    s.sname,
                    s.semester_valid_count,
                    s.semester_count,
                    s.semester_valid_dis / 1000.0,
                    s.semester_dis / 1000.0
                );
            }
            if !r.personal_raw.is_null() {
                println!("个人完成度：{}", r.personal_raw);
            }
            0
        }
        Err(e) => {
            eprintln!("拉取失败: {e}");
            1
        }
    }
}

fn cmd_cheat(rest: &[&str]) -> i32 {
    let flags = parse_flags(rest);
    let page: i64 = get(&flags, "page")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let mut client = match make_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let mut log = logger();
    match crate::api::cheat::query(&mut client, page, &mut log) {
        Ok(r) => {
            println!("自查：{}", r.label());
            println!(
                "检查时间：{}",
                crate::track::geom::fmt_cst(r.checked_at_ms, "%Y-%m-%d %H:%M:%S 北京时间")
            );
            if r.status() == crate::api::cheat::Status::Flagged {
                println!("服务端原因：{}；发生时间：{}", r.reason(), r.occurred_at());
                if let Some(id) = r.record_id() {
                    println!("关联跑步记录：{id}");
                }
            }
            println!("本页通报 {} 条（非全校总数）", r.list.len());
            for item in r.list.iter().take(20) {
                println!(
                    "  {} | {} | {}",
                    jstr(item, &["name", "userName"]),
                    jstr(item, &["reason", "punishReason", "cause"]),
                    jstr(item, &["createTime", "time", "date"]),
                );
            }
            0
        }
        Err(e) => {
            eprintln!("检查失败: {e}");
            1
        }
    }
}

/// 异常记录修复（红转绿）。
///
/// 默认只诊断（dry-run）：先看清「是记录不达标，还是已经被记进违规台账」，
/// 再决定要不要动。`--yes` 才会真的覆盖提交，单次默认最多修 3 条。
fn cmd_recover(rest: &[&str]) -> i32 {
    use crate::api::recover::{self, FixPlan};
    let flags = parse_flags(rest);
    let yes = get(&flags, "yes").is_some();
    let verify = get(&flags, "verify").is_some();
    // 策略接口读不到时默认停手；`--force` 是知情者的显式逃生门。
    let force = get(&flags, "force").is_some();
    let only: Option<i64> = get(&flags, "only").and_then(|v| v.parse().ok());
    let limit: usize = get(&flags, "limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(3)
        .clamp(1, 10);
    let plan = FixPlan {
        dist: get(&flags, "dist")
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0)
            * 1000.0,
        pace_s: get(&flags, "pace").map(parse_pace).unwrap_or(0.0) as f64,
        face_check: get(&flags, "face")
            .map(|v| (v == "1" || v == "true") as i64)
            .unwrap_or(1),
    };
    let seed: u64 = get(&flags, "seed")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let mut client = match make_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let mut log = logger();
    let diag = match recover::diagnose(&mut client, &mut log) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("诊断失败: {e}");
            return 1;
        }
    };

    println!("—— 现状 ——");
    println!(
        "禁跑状态：{}{}",
        diag.freeze_text(),
        if diag.freeze_message.is_empty() {
            String::new()
        } else {
            format!("（服务端文案：{}）", diag.freeze_message)
        }
    );
    println!("违规榜：{}", diag.cheat.label());
    if diag.flagged() {
        println!(
            "  原因：{}；发生时间：{}",
            diag.cheat.reason(),
            diag.cheat.occurred_at()
        );
        if let Some(id) = diag.cheat.record_id() {
            println!("  关联记录：{id}");
        }
    }
    println!(
        "记录：共 {} 条，其中未达标 {} 条",
        diag.total_records,
        diag.abnormal.len()
    );
    for a in diag.abnormal.iter().take(20) {
        println!(
            "  rrid={:<12} {}  {:.0}m / {}s  原因：{}",
            a.rrid,
            crate::track::geom::fmt_cst(a.start_ms, "%Y-%m-%d %H:%M"),
            a.total_dis,
            a.total_time,
            a.reason_text()
        );
    }
    if diag.abnormal.len() > 20 {
        println!("  …其余 {} 条略", diag.abnormal.len() - 20);
    }
    println!("—— 判断 ——");
    println!("{}", diag.verdict());

    if !yes {
        println!("\n（dry-run）确认无误后加 --yes 覆盖提交；也可先 --only <rrid> 单条试一条。");
        return 0;
    }
    // 冻结期提交会被服务端独立拒绝（客户端强行提交只是白挨一次风控计数），先拦住。
    if diag.frozen() {
        eprintln!(
            "账号处于禁跑期（{}），服务端会拒绝提交。等倒计时归零后再跑本命令；\
             这几条记录不会因为等待而消失。",
            diag.freeze_text()
        );
        return 1;
    }
    // 拿不到策略 ≠ 没冻结：默认停手。旧实现在这里直接放行，策略接口抖动一次
    // 就会在禁跑期里提交，白挨一次风控计数。
    if diag.freeze_unknown() && !force {
        eprintln!(
            "冻结状态未知（策略接口本次没取到），无法确认账号没被禁跑。\
             已停止覆盖提交以免撞服务端拒绝。\n\
             · 先重跑一次 `recover` 确认能读出「禁跑状态」；\n\
             · 若确认账号可正常跑步，加 --force 跳过这道检查（风险自负）。"
        );
        return 1;
    }
    if diag.freeze_unknown() {
        eprintln!("⚠ 已按 --force 跳过冻结检查：冻结状态仍未确认，本次提交可能与服务端拒绝相撞。");
    }
    if diag.abnormal.is_empty() {
        println!("没有需要修复的记录。");
        return 0;
    }

    // 挑本轮要修的
    let batch: Vec<_> = diag
        .abnormal
        .iter()
        .filter(|a| only.map(|id| id == a.rrid).unwrap_or(true))
        .take(limit)
        .cloned()
        .collect();
    if batch.is_empty() {
        eprintln!("--only {only:?} 没有匹配到未达标记录");
        return 1;
    }

    let cfg = model::load_config();
    let route_mode = crate::track::generate_road::RouteMode::from_str(
        get(&flags, "route").unwrap_or(&cfg.route_mode),
    );
    println!("\n—— 开始修复 {} 条 ——", batch.len());
    let mut ok = 0usize;
    let mut failed = 0usize;
    for (i, a) in batch.iter().enumerate() {
        if i > 0 {
            // 逐条之间随机间隔：等距时序本身就是特征（见 antiban.md 2.3）
            let gap = 3000 + (rand::random::<u64>() % 5000);
            std::thread::sleep(std::time::Duration::from_millis(gap));
        }
        match recover::fix_one(&mut client, a, plan, route_mode, seed, &mut log) {
            Ok(out) => {
                println!(
                    "  √ rrid={} → 新 rrid={} 判定={}",
                    a.rrid,
                    out.result.rrid,
                    out.verification.label()
                );
                if out.verification.complete == Some(false) {
                    for r in out.verification.failure_reasons() {
                        println!("     仍未达标：{r}");
                    }
                }
                ok += 1;
            }
            Err(e) => {
                println!("  × rrid={} 修复失败: {e}", a.rrid);
                failed += 1;
            }
        }
    }
    println!("修复结果：成功 {ok} / 失败 {failed}");

    if verify {
        println!("\n—— 复查 ——");
        match recover::diagnose(&mut client, &mut log) {
            Ok(after) => {
                println!("违规榜：{}", after.cheat.label());
                if after.cheat.status() == crate::api::cheat::Status::Flagged {
                    println!("  仍在榜（原因：{}）", after.cheat.reason());
                }
                println!(
                    "未达标记录：{} → {}",
                    diag.abnormal.len(),
                    after.abnormal.len()
                );
                if after.abnormal.len() < diag.abnormal.len() {
                    println!("√ 记录状态已改善");
                } else if after.abnormal.len() == diag.abnormal.len() {
                    println!("⚠ 数量未变：覆盖可能未生效（服务端按键去重不是日期），或服务端把判定锁死了");
                } else {
                    println!("⚠ 未达标记录变多：先停下，别再批量覆盖");
                }
            }
            Err(e) => println!("复查失败: {e}"),
        }
    }
    if ok > 0 {
        0
    } else {
        1
    }
}

fn cmd_rank(rest: &[&str]) -> i32 {
    let kind = rest.first().copied().unwrap_or("main");
    let flags = parse_flags(&rest.iter().skip(1).copied().collect::<Vec<_>>());
    let mut client = match make_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let res = match kind {
        "indoor" => {
            let range: i64 = get(&flags, "range")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1);
            crate::api::rank::indoor_rank(&mut client, range, -1)
        }
        "history" => {
            let sort: i64 = get(&flags, "sort")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1);
            crate::api::rank::history_rank(&mut client, sort, -1)
        }
        _ => {
            let rtype: i64 = get(&flags, "type")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1);
            let sort: i64 = get(&flags, "sort")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1);
            let gender = get(&flags, "gender").and_then(|v| v.parse().ok());
            let date = get(&flags, "date").map(|v| v.to_string());
            crate::api::rank::main_rank(&mut client, rtype, sort, gender, date)
        }
    };
    match res {
        Ok(rows) => {
            for r in rows {
                println!("{:<4} {}  {:.2} km", r.sort, r.name, r.length / 1000.0);
            }
            0
        }
        Err(e) => {
            eprintln!("查询失败: {e}");
            1
        }
    }
}

fn cmd_update(rest: &[&str]) -> i32 {
    let flags = parse_flags(rest);
    let check_only = get(&flags, "check").is_some();
    crate::update::cleanup_residue();
    let mut log = logger();
    let release = match crate::update::check_latest(&mut log) {
        Ok(r) => r,
        Err(e) => {
            println!("× 检查更新失败: {e}");
            return 1;
        }
    };
    let Some(rel) = release else {
        println!("√ 已是最新版本（v{}）", crate::update::current_version());
        return 0;
    };
    println!(
        "发现新版本 {}（当前 v{}，{:.1} MB）",
        rel.tag,
        crate::update::current_version(),
        rel.asset_size as f64 / 1024.0 / 1024.0
    );
    if !rel.notes.is_empty() {
        for line in rel.notes.lines().take(8) {
            println!("  {line}");
        }
    }
    if check_only {
        println!("仅检查（--check），未下载。下载地址：{}", rel.asset_url);
        return 0;
    }
    #[cfg(target_os = "android")]
    {
        println!("Android 版请在界面（关于页）中下载并安装更新");
        0
    }
    #[cfg(not(target_os = "android"))]
    {
        println!("开始下载 {}…", rel.asset_name);
        let mut last_pct = u64::MAX;
        let bytes = match crate::update::download(&rel.asset_url, |done, total| {
            if let Some(t) = total.filter(|t| *t > 0) {
                let pct = done * 100 / t;
                if pct != last_pct && pct % 5 == 0 {
                    println!("  {pct}%");
                    last_pct = pct;
                }
            }
        }) {
            Ok(b) => b,
            Err(e) => {
                println!("× 下载失败: {e}");
                return 1;
            }
        };
        println!("下载完成（{} 字节），校验完整性…", bytes.len());
        if let Err(e) = crate::update::verify(&rel, &bytes) {
            println!("× {e}");
            return 1;
        }
        crate::update::record_pin(&rel);
        let bin = match crate::update::extract(&rel.asset_name, &bytes) {
            Ok(b) => b,
            Err(e) => {
                println!("× {e}");
                return 1;
            }
        };
        match crate::update::apply(&bin) {
            Ok(()) => {
                println!("√ 已更新到 {}，请重新运行命令", rel.tag);
                0
            }
            Err(e) => {
                println!("× 替换失败: {e}");
                1
            }
        }
    }
}

/// 控制台预览：按**字符**截断。按字节切片会在多字节字符中间截断并 panic
/// （服务端一旦返回中文或 emoji 就会触发，且发生在"只是看一眼"的调试命令里）。
fn preview(value: &serde_json::Value, chars: usize) -> String {
    crate::textlog::truncate(&value.to_string(), chars)
}

#[cfg(test)]
mod preview_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn preview_truncates_multibyte_payloads_without_panicking() {
        let value = json!({"name": "张三".repeat(200), "emoji": "🏃".repeat(50)});
        for chars in [0usize, 1, 3, 300, 5000] {
            let out = preview(&value, chars);
            assert!(
                out.chars().count() <= chars,
                "chars={chars} 时越界：{}",
                out.chars().count()
            );
        }
        // 旧实现按 300 字节切中文串必然 panic；现在按字符切。
        let text = json!("校".repeat(400));
        let out = preview(&text, 300);
        assert_eq!(out.chars().count(), 300);
        assert!(out.starts_with('"'));
    }
}
