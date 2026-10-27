//! School-scoped public OSM download. No account credentials are sent to Overpass.
use route_planner::RoadGraph;
use sha2::{Digest, Sha256};
use std::{io::Read, path::Path, time::Duration};

/// 路网体积上限（下载与手动导入共用同一个额度）。
///
/// 两条入口给不同数值只会让用户遇到「自动下载能跑、手动导入闪退」这种自相矛盾的
/// 失败，所以这里作为唯一真值：`generate_road::load_network_path` 与 Android 侧
/// 的 `MAX_OSM_BYTES` 都必须跟着这个数字走。
pub(crate) const MAX_BYTES: u64 = 24 * 1024 * 1024;
const TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;
const ENDPOINT: &str = "https://overpass-api.de/api/interpreter";
/// Overpass 公开镜像，按顺序回退。
///
/// 主站在不少网络下不可达（用户实测：`error sending request for url
/// (https://overpass-api.de/api/interpreter)`），而路网拿不到就等于 Road/Tiles
/// 全废。这里给三个常用实例，逐个试，全部失败才报错。
pub const OVERPASS_MIRRORS: [&str; 3] = [
    "https://overpass-api.de/api/interpreter",
    "https://overpass.kumi.systems/api/interpreter",
    "https://overpass.private.coffee/api/interpreter",
];
/// 单个镜像的超时：25s 是 Overpass 对 [timeout:25] 查询的常见上限，再长就是干等。
const MIRROR_TIMEOUT: Duration = Duration::from_secs(25);

/// 本次要试的端点顺序：自定义端点（非空）优先，其余按内置顺序，去重。
pub fn endpoint_order(custom: &str) -> Vec<String> {
    let mut out = Vec::new();
    let custom = custom.trim();
    if !custom.is_empty() {
        out.push(custom.to_string());
    }
    for mirror in OVERPASS_MIRRORS {
        if !out.iter().any(|e| e == mirror) {
            out.push(mirror.to_string());
        }
    }
    out
}

/// 配置里的自定义 Overpass 端点（空 = 用内置镜像）。
fn custom_endpoint() -> String {
    crate::api::model::load_config().osm_endpoint
}

/// 逐个镜像尝试，全部失败才报错。
///
/// `per_endpoint` 是可注入的单次请求（离线测试用假实现），`log` 写每个镜像的
/// 结果与耗时 —— 用户的日志里就能看出是"某个镜像慢"还是"整片网络都不通"。
pub fn download_with(
    query: &str,
    custom: &str,
    per_endpoint: &mut dyn FnMut(&str, &str) -> Result<Vec<u8>, String>,
    control: &dyn Fn() -> Result<(), String>,
    log: &mut dyn FnMut(&str),
) -> Result<Vec<u8>, String> {
    let endpoints = endpoint_order(custom);
    let mut failures: Vec<String> = Vec::new();
    for (index, endpoint) in endpoints.iter().enumerate() {
        control()?;
        let host = endpoint
            .split("//")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .unwrap_or(endpoint);
        if index > 0 {
            log(&format!("[osm] 回退到第 {} 个镜像：{host}", index + 1));
        }
        let started = std::time::Instant::now();
        match per_endpoint(endpoint, query) {
            Ok(bytes) => {
                log(&format!(
                    "√ [osm] {host} 成功（{} ms，{} KiB）",
                    started.elapsed().as_millis(),
                    bytes.len() / 1024
                ));
                return Ok(bytes);
            }
            Err(error) => {
                let elapsed = started.elapsed().as_millis();
                eprintln!("[osm] 镜像 {host} 失败（{elapsed} ms）：{error}");
                log(&format!("⚠ [osm] {host} 失败（{elapsed} ms）"));
                failures.push(format!("{host}：{}", shorten(&error, 120)));
            }
        }
    }
    // 面向用户的文案只说人话：底层报错（error sending request for url (...)）对用户没有意义；
    // 逐镜像结果与耗时已写进日志/eprintln，排查时看那里。
    for failure in &failures {
        eprintln!("[osm] 逐镜像结果：{failure}");
    }
    Err(format!(
        "无法连接 OpenStreetMap 路网服务（已试 {} 个镜像）：可能是网络限制，\
         可到「路网」页用『选择 OSM 文件』人工导入本地 .osm 文件，或稍后重试。\
         各镜像的结果与耗时见日志。",
        endpoints.len()
    ))
}

fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

pub fn validate_fences(fences: &[Vec<(f64, f64)>]) -> Result<(), String> {
    if fences.is_empty() {
        return Err("当前学校没有有效围栏，无法保证校内路线；请先确认学校围栏".into());
    }
    for ring in fences {
        if ring.len() < 3
            || ring.iter().any(|&(lat, lon)| {
                !lat.is_finite() || !lon.is_finite() || lat.abs() > 85.0 || lon.abs() > 180.0
            })
        {
            return Err("学校围栏坐标无效".into());
        }
        let (oy, ox) = ring[0];
        let area: f64 = ring
            .iter()
            .zip(ring.iter().cycle().skip(1))
            .map(|(&(y, x), &(v, u))| (x - ox) * (v - oy) - (u - ox) * (y - oy))
            .sum();
        if area.abs() < 1e-12 {
            return Err("学校围栏面积无效".into());
        }
    }
    Ok(())
}

// Invert the same forward transform used to align roads, avoiding mixed coordinate systems.
pub fn to_wgs(lat: f64, lon: f64) -> (f64, f64) {
    let (mut y, mut x) = (lat, lon);
    for _ in 0..16 {
        let (by, bx) = crate::track::geom::wgs84_to_bd09(y, x);
        y += lat - by;
        x += lon - bx;
    }
    (y, x)
}

fn query(fences: &[Vec<(f64, f64)>]) -> Result<String, String> {
    validate_fences(fences)?;
    query_coordinates(&fences.iter().flatten().copied().collect::<Vec<_>>(), 0.001)
}

fn query_coordinates(points: &[(f64, f64)], margin: f64) -> Result<String, String> {
    if points.is_empty()
        || points
            .iter()
            .any(|&(a, b)| !a.is_finite() || !b.is_finite() || a.abs() > 85. || b.abs() > 180.)
    {
        return Err("缺少有效点位，不能确定路网下载范围".into());
    }
    let (mut south, mut west, mut north, mut east) = (90f64, 180f64, -90f64, -180f64);
    for &(lat, lon) in points {
        let (y, x) = to_wgs(lat, lon);
        south = south.min(y);
        north = north.max(y);
        west = west.min(x);
        east = east.max(x);
    }
    // Reject unexpectedly huge / geographically separate school areas before downloading.
    if north - south > 0.15 || east - west > 0.2 {
        return Err("路网下载范围跨度过大，请确认当前学校和点位后重试".into());
    }
    let bbox = format!(
        "{:.7},{:.7},{:.7},{:.7}",
        south - margin,
        west - margin,
        north + margin,
        east + margin
    );
    Ok(format!("[out:xml][timeout:25];(way[highway]({bbox});way[building]({bbox});relation[building]({bbox}););(._;>;);out body;"))
}

fn parse(bytes: &[u8]) -> Result<RoadGraph, String> {
    if bytes.len() as u64 > MAX_BYTES {
        return Err("学校路网响应超过 24 MiB".into());
    }
    let xml = std::str::from_utf8(bytes).map_err(|_| "学校路网不是有效 XML")?;
    if !xml.contains("<osm") || !xml.contains("</osm>") || xml.contains("<remark") {
        return Err("路网服务返回错误或不完整数据，请稍后重试".into());
    }
    let net = route_planner::load_osm(bytes)?;
    if net.graph.edge_count() == 0
        || net.graph.node_weights().any(|n| {
            !n.coord.lon.is_finite()
                || !n.coord.lat.is_finite()
                || n.coord.lon.abs() > 180.0
                || n.coord.lat.abs() > 90.0
        })
    {
        return Err("当前学校没有可用的 OSM 道路数据，可手动导入完整路网".into());
    }
    Ok(net)
}

/// 向单个端点发一次 Overpass 查询（超时 25s，响应上限 24 MiB）。
fn download_from(endpoint: &str, query: &str) -> Result<Vec<u8>, String> {
    let agent = crate::http::Agent::with_timeout(MIRROR_TIMEOUT);
    let response = agent
        .post(endpoint)
        .set("User-Agent", "NekoSportsWorldTool/0.3.0 (school-road-map)")
        .send_form(&[("data", query)])
        .map_err(|e| format!("{e}"))?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("读取学校路网失败：{e}"))?;
    Ok(bytes)
}

/// 按镜像顺序下载（自定义端点优先）。`control` 在每次尝试前检查，可取消。
fn download_mirrors(
    query: &str,
    control: &dyn Fn() -> Result<(), String>,
    log: &mut dyn FnMut(&str),
) -> Result<Vec<u8>, String> {
    let custom = custom_endpoint();
    download_with(query, &custom, &mut download_from, control, log)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Cache {
    key: String,
    loaded_ms: i64,
    xml: String,
    #[serde(default)]
    school: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    fences: Vec<Vec<(f64, f64)>>,
    #[serde(default)]
    points: Vec<(f64, f64)>,
}

fn cached_network(
    dir: &Path,
    school: &str,
    fences: &[Vec<(f64, f64)>],
    refresh: bool,
    now: i64,
    fetch: impl FnOnce(&str, &mut dyn FnMut(&str)) -> Result<Vec<u8>, String>,
    log: &mut dyn FnMut(&str),
) -> Result<RoadGraph, String> {
    cached_route_network(dir, school, fences, &[], refresh, now, fetch, log)
}

/// 读缓存文件（**忽略 TTL**）：网络失败时用它兜底，返回（缓存岁数 ms, 路网）。
///
/// 只认 key 相同的缓存 —— 换学校/换围栏范围后旧缓存本来就不该用。
fn stale_cache(file: &Path, key: &str, now: i64) -> Option<(i64, RoadGraph)> {
    let text = std::fs::read_to_string(file).ok()?;
    let cache: Cache = serde_json::from_str(&text).ok()?;
    if cache.key != key {
        return None;
    }
    let net = parse(cache.xml.as_bytes()).ok()?;
    let age = now - cache.loaded_ms;
    if age < 0 {
        // 缓存时间在「未来」（时钟回拨）：不能当可用数据。
        return None;
    }
    Some((age, net))
}

fn cached_route_network(
    dir: &Path,
    school: &str,
    fences: &[Vec<(f64, f64)>],
    points: &[(f64, f64)],
    refresh: bool,
    now: i64,
    fetch: impl FnOnce(&str, &mut dyn FnMut(&str)) -> Result<Vec<u8>, String>,
    log: &mut dyn FnMut(&str),
) -> Result<RoadGraph, String> {
    if school.trim().is_empty() {
        return Err("登录信息缺少学校编号，请重新登录".into());
    }
    let q = if fences.is_empty() {
        query_coordinates(points, 0.002)?
    } else {
        query(fences)?
    };
    let key = route_scope_key(school, fences, points);
    let file = dir.join(format!("{key}.json"));
    if !refresh
        && std::fs::metadata(&file)
            .map(|m| m.len() < MAX_BYTES * 2)
            .unwrap_or(false)
    {
        if let Ok(text) = std::fs::read_to_string(&file) {
            if let Ok(cache) = serde_json::from_str::<Cache>(&text) {
                if cache.key == key && (0..TTL_MS).contains(&(now - cache.loaded_ms)) {
                    if let Ok(net) = parse(cache.xml.as_bytes()) {
                        log("[road] 使用当前学校及下载范围对应的路网缓存");
                        return Ok(net);
                    }
                }
            }
        }
    }
    log("[road] 正在下载当前学校区域的 OpenStreetMap 路网…");
    // 缓存优先 + 镜像回退：网络失败时不丢弃可用数据，只提示「未刷新」。
    let bytes = match fetch(&q, &mut *log) {
        Ok(bytes) => bytes,
        Err(error) => {
            if let Some((age_ms, cached)) = stale_cache(&file, &key, now) {
                let hours = (age_ms as f64 / 3_600_000.0).max(0.1);
                log(&format!(
                    "⚠ [road] 本次未刷新，仍在使用 {hours:.1} 小时前的缓存路网（{error}）"
                ));
                return Ok(cached);
            }
            return Err(error);
        }
    };
    let net = parse(&bytes)?;
    // Reject empty in-school coverage before accepting a download into the cache.
    let mut aligned = net.clone();
    crate::track::generate_road::align_network(&mut aligned);
    if crate::track::generate_road::apply_fences(&aligned, fences)
        .graph
        .edge_count()
        == 0
    {
        return Err("下载的路网在学校围栏内没有可用道路，不能向校外补足距离".into());
    }
    let cache = Cache {
        key,
        loaded_ms: now,
        xml: String::from_utf8(bytes).map_err(|e| e.to_string())?,
        school: school.into(),
        name: String::new(),
        fences: fences.to_vec(),
        points: points.to_vec(),
    };
    // Cache is optional: read-only installation directories must not block a valid download.
    let saved = (|| -> Result<(), String> {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        // Interrupted/concurrent writes are rejected by the parser on the next read.
        std::fs::write(
            &file,
            serde_json::to_vec(&cache).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })();
    if let Err(e) = saved {
        log(&format!("[road] 本次路网可用，但缓存未保存：{e}"));
    }
    Ok(net)
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub key: String,
    pub school: String,
    pub name: String,
    pub loaded_ms: i64,
    pub bytes: u64,
    pub fences: Vec<Vec<(f64, f64)>>,
    pub points: Vec<(f64, f64)>,
}
pub fn scope_key(school: &str, fences: &[Vec<(f64, f64)>]) -> String {
    // About centimetre precision: stable across JSON floating-point round trips.
    let normalized: Vec<Vec<(i64, i64)>> = fences
        .iter()
        .map(|f| {
            f.iter()
                .map(|&(a, b)| ((a * 1e7).round() as i64, (b * 1e7).round() as i64))
                .collect()
        })
        .collect();
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(school, normalized)).unwrap())
    )
}

fn route_scope_key(school: &str, fences: &[Vec<(f64, f64)>], points: &[(f64, f64)]) -> String {
    if !fences.is_empty() {
        return scope_key(school, fences);
    }
    let normalized: Vec<_> = points
        .iter()
        .map(|&(a, b)| ((a * 1e7).round() as i64, (b * 1e7).round() as i64))
        .collect();
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&("points-v1", school, normalized)).unwrap())
    )
}
fn cache_dir() -> std::path::PathBuf {
    crate::platform::data_dir().join("school_roads")
}
fn cache_path(key: &str) -> Result<std::path::PathBuf, String> {
    if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("无效缓存标识".into());
    }
    Ok(cache_dir().join(format!("{key}.json")))
}
pub fn entries() -> Vec<Entry> {
    let Ok(files) = std::fs::read_dir(cache_dir()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for f in files.flatten() {
        let Some(key) = f
            .path()
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_owned)
        else {
            continue;
        };
        if let Ok((cache, bytes)) = read_cache_record(&key) {
            out.push(cache_entry(&cache, bytes));
        }
    }
    out.sort_by(|a, b| b.loaded_ms.cmp(&a.loaded_ms));
    out
}
fn read_cache_record(key: &str) -> Result<(Cache, u64), String> {
    let path = cache_path(key)?;
    let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
    if meta.len() > MAX_BYTES * 2 {
        return Err("缓存过大".into());
    }
    let cache: Cache = serde_json::from_slice(&std::fs::read(&path).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    if cache.key != key {
        return Err("缓存标识不一致".into());
    }
    Ok((cache, meta.len()))
}
fn cache_entry(c: &Cache, bytes: u64) -> Entry {
    Entry {
        key: c.key.clone(),
        school: c.school.clone(),
        name: c.name.clone(),
        loaded_ms: c.loaded_ms,
        bytes,
        fences: c.fences.clone(),
        points: c.points.clone(),
    }
}
pub fn read_cache(key: &str) -> Result<(RoadGraph, Entry), String> {
    let (c, bytes) = read_cache_record(key)?;
    Ok((parse(c.xml.as_bytes())?, cache_entry(&c, bytes)))
}
pub fn clear_cache(key: &str) -> Result<(), String> {
    std::fs::remove_file(cache_path(key)?).map_err(|e| e.to_string())
}
pub fn save_context(school: &str, name: &str, fences: &[Vec<(f64, f64)>], points: &[(f64, f64)]) {
    let key = route_scope_key(school, fences, points);
    let Ok(path) = cache_path(&key) else {
        return;
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return;
    };
    if let Ok(mut cache) = serde_json::from_slice::<Cache>(&bytes) {
        cache.name = name.into();
        cache.points = points.to_vec();
        if let Ok(bytes) = serde_json::to_vec(&cache) {
            let _ = std::fs::write(path, bytes);
        }
    }
}

pub fn load_for_school_cancellable(
    path: &str,
    school: &str,
    fences: &[Vec<(f64, f64)>],
    refresh: bool,
    control: &crate::task_control::Control,
    log: &mut dyn FnMut(&str),
) -> Result<RoadGraph, String> {
    control.check()?;
    if !path.trim().is_empty() {
        let net = crate::track::generate_road::load_network_path(path)?;
        control.check()?;
        return Ok(net);
    }
    validate_fences(fences)?;
    cached_network(
        &cache_dir(),
        school,
        fences,
        refresh,
        crate::crypto::envelope::now_ms(),
        |q, log| {
            control.check()?;
            let bytes = download_mirrors(q, &|| control.check(), log)?;
            control.check()?;
            Ok(bytes)
        },
        log,
    )
}

/// With no server boundary, download near real school checkpoints. This extent
/// is only a public-map query, never a campus fence or a route constraint.
pub fn load_for_route_cancellable(
    path: &str,
    school: &str,
    fences: &[Vec<(f64, f64)>],
    points: &[(f64, f64)],
    refresh: bool,
    control: &crate::task_control::Control,
    log: &mut dyn FnMut(&str),
) -> Result<RoadGraph, String> {
    if !path.trim().is_empty() || !fences.is_empty() {
        return load_for_school_cancellable(path, school, fences, refresh, control, log);
    }
    control.check()?;
    log("[road] 无学校围栏：按当前学校真实点位获取附近道路；下载范围不是学校边界");
    cached_route_network(
        &cache_dir(),
        school,
        fences,
        points,
        refresh,
        crate::crypto::envelope::now_ms(),
        |q, log| {
            control.check()?;
            let bytes = download_mirrors(q, &|| control.check(), log)?;
            control.check()?;
            Ok(bytes)
        },
        log,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Vec<Vec<(f64, f64)>>, Vec<u8>) {
        let fences = vec![vec![
            (39.899, 116.399),
            (39.899, 116.403),
            (39.903, 116.403),
            (39.903, 116.399),
        ]
        .into_iter()
        .map(|(a, b)| crate::track::geom::wgs84_to_bd09(a, b))
        .collect()];
        let xml=br#"<osm version="0.6"><node id="1" lat="39.9" lon="116.4"/><node id="2" lat="39.901" lon="116.401"/><way id="1"><nd ref="1"/><nd ref="2"/><tag k="highway" v="footway"/></way></osm>"#.to_vec();
        (fences, xml)
    }
    #[test]
    fn coordinate_round_trip_and_query_scope() {
        for p in [(39.9, 116.4), (22.5, 113.9), (31.2, 121.5)] {
            let bd = crate::track::geom::wgs84_to_bd09(p.0, p.1);
            let w = to_wgs(bd.0, bd.1);
            assert!((w.0 - p.0).abs() < 1e-8 && (w.1 - p.1).abs() < 1e-8);
        }
        let q = query(&fixture().0).unwrap();
        assert!(q.contains("39.8980000,116.3980000,39.9040000,116.4040000"));
        assert!(query(&[]).is_err());
        assert!(validate_fences(&[vec![(0., 0.); 3]]).is_err());
    }
    #[test]
    fn cache_is_school_and_fence_scoped_expires_and_propagates_failure() {
        let dir = std::env::temp_dir().join(format!("school-roads-{}", uuid::Uuid::new_v4()));
        let (f, xml) = fixture();
        let get =
            |school: &str, fences: &[Vec<(f64, f64)>], refresh, now, bytes: Option<Vec<u8>>| {
                cached_network(
                    &dir,
                    school,
                    fences,
                    refresh,
                    now,
                    |_, _| bytes.ok_or("offline".into()),
                    &mut |_| {},
                )
            };
        assert!(get("a", &f, false, 1000, Some(xml.clone())).is_ok());
        assert!(get("a", &f, false, 1001, None).is_ok());
        assert!(get("b", &f, false, 1001, None).is_err());
        let mut changed = f.clone();
        changed[0][0].0 -= 0.00001;
        assert!(get("a", &changed, false, 1001, None).is_err());
        // 缓存优先：TTL 过了但网络失败 → 仍返回旧缓存（只提示「未刷新」）
        assert!(get("a", &f, false, 1000 + TTL_MS, None).is_ok());
        assert!(get("a", &f, false, 999, None).is_err());
        // 用户点「刷新」但网络失败：仍旧用缓存 + 提示「本次未刷新」（不清空可用数据）
        assert!(get("a", &f, true, 1001, None).is_ok());
        assert!(get("a", &f, true, 1002, Some(xml)).is_ok());
        assert!(get("a", &f, false, 1002 + TTL_MS - 1, None).is_ok());
        assert!(parse(b"<osm></osm>").is_err());
        assert!(parse(b"<osm><remark>timeout</remark></osm>").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 镜像回退：按顺序试，成功即停；全部失败才报错，且文案是「人话」。
    #[test]
    fn mirrors_are_tried_in_order_and_all_failures_produce_a_human_message() {
        // ① 前两个失败、第三个成功 → 依次访问三个端点，最终拿到数据
        let mut visited = Vec::new();
        let mut logs = Vec::new();
        let mut per = |endpoint: &str, _q: &str| -> Result<Vec<u8>, String> {
            visited.push(endpoint.to_string());
            if endpoint.contains("overpass-api.de") {
                Err("error sending request for url (…)".into())
            } else if endpoint.contains("kumi") {
                Err("connection reset".into())
            } else {
                Ok(b"<osm/>".to_vec())
            }
        };
        let got = download_with("q", "", &mut per, &|| Ok(()), &mut |line: &str| {
            logs.push(line.to_string())
        })
        .expect("第三个镜像应成功");
        assert_eq!(got, b"<osm/>".to_vec());
        assert_eq!(visited, OVERPASS_MIRRORS.to_vec(), "必须按内置顺序逐个试");
        assert!(logs.iter().any(|l| l.contains("回退到第 2 个镜像")));
        assert!(logs.iter().any(|l| l.contains("成功")));

        // ② 全部失败 → 只报一次错，且说明试了几个镜像 + 给人工导入的出路
        let mut visited = Vec::new();
        let mut per = |endpoint: &str, _q: &str| -> Result<Vec<u8>, String> {
            visited.push(endpoint.to_string());
            Err("error sending request for url (…) ".to_string())
        };
        let err = download_with("q", "", &mut per, &|| Ok(()), &mut |_| {}).unwrap_err();
        assert_eq!(visited.len(), OVERPASS_MIRRORS.len(), "每个镜像都要试一次");
        assert!(err.contains("已试 3 个镜像"), "{err}");
        assert!(err.contains("网络限制"), "{err}");
        assert!(err.contains("人工导入"), "必须给下一步：{err}");
        assert!(
            !err.contains("error sending request for url ("),
            "不要把底层报错原样抛给用户：{err}"
        );

        // ③ 取消：第一次尝试前就取消 → 立刻返回、一个请求都不发
        let mut visited = 0usize;
        let mut per = |_e: &str, _q: &str| -> Result<Vec<u8>, String> {
            visited += 1;
            Ok(vec![1u8; 1024])
        };
        let err =
            download_with("q", "", &mut per, &|| Err("已取消".into()), &mut |_| {}).unwrap_err();
        assert!(err.contains("已取消"), "{err}");
        assert_eq!(visited, 0);
    }

    /// 自定义端点排在内置镜像之前。
    #[test]
    fn custom_endpoint_takes_priority() {
        let order = endpoint_order("https://my.overpass.example/api/interpreter");
        assert_eq!(order[0], "https://my.overpass.example/api/interpreter");
        assert_eq!(order.len(), OVERPASS_MIRRORS.len() + 1);
        assert_eq!(endpoint_order("  "), OVERPASS_MIRRORS.to_vec());
        // 与内置重复时去重
        let dedup = endpoint_order(OVERPASS_MIRRORS[1]);
        assert_eq!(dedup.len(), OVERPASS_MIRRORS.len());

        let mut visited = Vec::new();
        let mut per = |endpoint: &str, _q: &str| -> Result<Vec<u8>, String> {
            visited.push(endpoint.to_string());
            Ok(vec![7u8; 1024])
        };
        download_with(
            "q",
            "https://my.overpass.example/api/interpreter",
            &mut per,
            &|| Ok(()),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(
            visited,
            vec!["https://my.overpass.example/api/interpreter".to_string()],
            "自定义端点成功就不该再试内置镜像"
        );
    }

    /// 缓存优先：网络全挂时不清空可用数据，只提示「仍在用 X 小时前的缓存」。
    #[test]
    fn expired_cache_survives_a_total_network_failure_with_an_age_notice() {
        let dir = std::env::temp_dir().join(format!("school-stale-{}", uuid::Uuid::new_v4()));
        let (fences, xml) = fixture();
        cached_network(
            &dir,
            "s",
            &fences,
            false,
            1_000,
            |_, _| Ok(xml.clone()),
            &mut |_| {},
        )
        .unwrap();
        // 8 天后（TTL 过期）用户点刷新，但网络全挂
        let mut logs: Vec<String> = Vec::new();
        let net = cached_network(
            &dir,
            "s",
            &fences,
            true,
            1_000 + TTL_MS + 3_600_000,
            |_, _| Err("无法连接 OpenStreetMap 路网服务（已试 3 个镜像）".into()),
            &mut |line: &str| logs.push(line.to_string()),
        )
        .expect("有缓存时不该因为网络失败而报错");
        assert!(net.graph.edge_count() > 0, "旧缓存必须仍然可用");
        let notice = logs
            .iter()
            .find(|l| l.contains("未刷新"))
            .unwrap_or_else(|| panic!("缺少『未刷新』提示：{logs:?}"));
        assert!(notice.contains("小时前的缓存"), "{notice}");
        assert!(
            notice.contains("已试 3 个镜像"),
            "要把失败原因带上：{notice}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn no_boundary_download_is_point_scoped_without_inventing_fences() {
        let dir = std::env::temp_dir().join(format!("point-roads-{}", uuid::Uuid::new_v4()));
        let (_, xml) = fixture();
        let points = [(39.9, 116.4), (39.901, 116.401)]
            .map(|(a, b)| crate::track::geom::wgs84_to_bd09(a, b));
        let net = cached_route_network(
            &dir,
            "a",
            &[],
            &points,
            false,
            1000,
            |q, _log| {
                assert!(q.contains("39.8980000,116.3980000,39.9030000,116.4030000"));
                Ok(xml)
            },
            &mut |_| {},
        )
        .unwrap();
        assert!(net.graph.edge_count() > 0);
        assert!(net.boundaries.is_empty());
        let key = route_scope_key("a", &[], &points);
        let cache: Cache =
            serde_json::from_slice(&std::fs::read(dir.join(format!("{key}.json"))).unwrap())
                .unwrap();
        assert!(cache.fences.is_empty());
        assert_eq!(cache.points.len(), 2);
        let cached = |school, pts: &[(f64, f64)], now| {
            cached_route_network(
                &dir,
                school,
                &[],
                pts,
                false,
                now,
                |_, _| Err("offline".into()),
                &mut |_| {},
            )
        };
        assert!(cached("a", &points, 1001).is_ok());
        assert!(cached("b", &points, 1001).is_err());
        assert!(cached("a", &points[..1], 1001).is_err());
        // 缓存优先：TTL 过了但网络失败 → 仍返回旧缓存（只是提示「未刷新」）
        assert!(cached("a", &points, 1000 + TTL_MS).is_ok());
        assert!(query_coordinates(&[], 0.002).is_err());
        assert!(query_coordinates(&[(f64::NAN, 116.4)], 0.002).is_err());
        assert!(query_coordinates(&[(39., 116.), (40., 116.)], 0.002).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn manual_map_loading_needs_neither_boundary_nor_school_id() {
        let path = std::env::temp_dir().join(format!("manual-road-{}.osm", uuid::Uuid::new_v4()));
        std::fs::write(&path, fixture().1).unwrap();
        let net = load_for_route_cancellable(
            path.to_str().unwrap(),
            "",
            &[],
            &[],
            false,
            &crate::task_control::Control::default(),
            &mut |_| {},
        )
        .unwrap();
        assert!(net.graph.edge_count() > 0);
        assert!(net.boundaries.is_empty());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn saved_maps_can_be_listed_loaded_offline_and_deleted_safely() {
        let root = std::env::temp_dir().join(format!("school-manager-{}", uuid::Uuid::new_v4()));
        crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = Some(root.clone()));
        let (fences, xml) = fixture();
        cached_network(
            &root.join("school_roads"),
            "test-school",
            &fences,
            false,
            1000,
            |_, _| Ok(xml),
            &mut |_| {},
        )
        .unwrap();
        save_context("test-school", "接口校区名", &fences, &[(39.9, 116.4)]);
        let list = entries();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "接口校区名");
        assert!(list[0].bytes > 0);
        let (_, entry) = read_cache(&list[0].key).unwrap();
        assert_eq!(scope_key("", &entry.fences), scope_key("", &fences));
        assert_eq!(entry.points.len(), 1);
        assert!(clear_cache("../other").is_err());
        clear_cache(&list[0].key).unwrap();
        assert!(entries().is_empty());
        crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = None);
        std::fs::remove_dir_all(root).unwrap();
    }
}
