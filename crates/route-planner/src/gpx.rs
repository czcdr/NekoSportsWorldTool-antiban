//! GPX 轨迹点解析（带边界限制）；轨迹段之间不连线。
//!
//! 除坐标外还保留**海拔**与**已解析的时间**：
//! - 海拔来自 `<trkpt><ele>`，供 `totalAscent` 用真值而不是重新估；
//! - 时间同时保留原始文本与 RFC3339 解析结果（毫秒），配速曲线可以按点还原。
//!
//! 另外提供两种「非 XML 坐标」入口，覆盖从别的 App 复制的常见格式：
//! - [`parse_point_text`]：每行 `lat,lon[,ele]`（逗号/空白分隔均可）；
//! - [`parse_point_json`]：`[{"lat":..,"lon":..},..]` 或 `[[lat,lon],..]`。

use crate::Coord;
use quick_xml::{events::Event, Reader};

/// 文件大小上限（24 MiB），与界面选择器的校验保持一致。
pub const MAX_BYTES: usize = 24 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Sample {
    pub coordinate: Coord,
    /// 原始时间文本（RFC3339）；保留给需要原文的调用方。
    pub time: Option<String>,
    /// 解析后的时间（毫秒，UTC）。解析失败为 `None`，**不表示没有时间**。
    pub time_ms: Option<i64>,
    /// 海拔（米）。GPX 没有 `<ele>` 时为 `None`。
    pub elevation: Option<f64>,
    pub segment: usize,
}

/// 解析 RFC3339 时间戳为毫秒（UTC）。失败返回 `None`。
pub fn parse_time_ms(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text.trim())
        .ok()
        .map(|t| t.timestamp_millis())
}

/// 解析 GPX 文本。
pub fn parse(text: &str) -> Result<Vec<Sample>, String> {
    if text.len() > MAX_BYTES {
        return Err("GPX 超过 24 MiB".into());
    }
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);
    reader.config_mut().expand_empty_elements = true;
    let (mut points, mut current, mut segment, mut in_time, mut in_ele, mut depth, mut root) =
        (Vec::new(), None::<Sample>, 0, false, false, 0usize, false);
    loop {
        match reader
            .read_event()
            .map_err(|e| format!("GPX XML 无效：{e}"))?
        {
            Event::Start(e) => {
                let name = e.local_name();
                let name = name.as_ref();
                if name == b"gpx" {
                    root = true;
                }
                if name == b"trkseg" {
                    segment += 1;
                }
                if name == b"trkpt" {
                    if current.is_some() {
                        return Err("GPX 点嵌套无效".into());
                    }
                    let (mut lat, mut lon) = (None, None);
                    for a in e.attributes() {
                        let a = a.map_err(|e| e.to_string())?;
                        let v = std::str::from_utf8(&a.value).map_err(|e| e.to_string())?;
                        if a.key.as_ref() == b"lat" {
                            lat = v.parse::<f64>().ok();
                        }
                        if a.key.as_ref() == b"lon" {
                            lon = v.parse::<f64>().ok();
                        }
                    }
                    let (lat, lon) = (lat.ok_or("GPX 点缺少纬度")?, lon.ok_or("GPX 点缺少经度")?);
                    if !lat.is_finite() || !lon.is_finite() || lat.abs() > 90. || lon.abs() > 180. {
                        return Err("GPX 坐标无效".into());
                    }
                    current = Some(Sample {
                        coordinate: Coord::new(lon, lat),
                        time: None,
                        time_ms: None,
                        elevation: None,
                        segment,
                    });
                }
                if name == b"time" && current.is_some() {
                    in_time = true;
                }
                if name == b"ele" && current.is_some() {
                    in_ele = true;
                }
                depth += 1;
            }
            Event::End(e) => {
                depth = depth.saturating_sub(1);
                match e.local_name().as_ref() {
                    b"time" => in_time = false,
                    b"ele" => in_ele = false,
                    b"trkpt" => {
                        if let Some(p) = current.take() {
                            points.push(p);
                        }
                    }
                    _ => {}
                }
            }
            Event::Text(e) if in_time => {
                if let Some(p) = &mut current {
                    let text = e.decode().map_err(|e| e.to_string())?.to_string();
                    p.time_ms = parse_time_ms(&text);
                    p.time = Some(text);
                }
            }
            Event::Text(e) if in_ele => {
                if let Some(p) = &mut current {
                    let text = e.decode().map_err(|e| e.to_string())?;
                    // 海拔非法值（NaN/inf/超出地表范围）按缺失处理，不让整条轨迹失败。
                    if let Ok(v) = text.trim().parse::<f64>() {
                        if v.is_finite() && (-500.0..=9000.0).contains(&v) {
                            p.elevation = Some(v);
                        }
                    }
                }
            }
            Event::DocType(_) => return Err("不支持带 DTD 的 GPX".into()),
            Event::Eof => break,
            _ => {}
        }
    }
    if !root || depth != 0 || current.is_some() || points.len() < 2 {
        return Err("GPX 不完整或少于两个轨迹点".into());
    }
    Ok(points)
}

/// 解析「每行一个坐标」的文本：`lat,lon` 或 `lat lon`，可带第三列海拔。
///
/// 空行、`#` 开头与 `//` 开头的注释行、以及 `lat,lon` 表头行会被跳过。
pub fn parse_point_text(text: &str) -> Result<Vec<Sample>, String> {
    if text.len() > MAX_BYTES {
        return Err("坐标文本超过 24 MiB".into());
    }
    let mut out = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
            continue;
        }
        let cleaned = line.replace(',', " ").replace(';', " ");
        let mut it = cleaned.split_whitespace();
        let lat = match it.next().and_then(|v| v.parse::<f64>().ok()) {
            Some(v) => v,
            None => continue, // 表头/非数字行直接跳过
        };
        let lon = match it.next().and_then(|v| v.parse::<f64>().ok()) {
            Some(v) => v,
            None => {
                return Err(format!("第 {} 行缺少经度", idx + 1));
            }
        };
        if !lat.is_finite() || !lon.is_finite() || lat.abs() > 90. || lon.abs() > 180. {
            return Err(format!("第 {} 行坐标超出范围", idx + 1));
        }
        let elevation = it
            .next()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite() && (-500.0..=9000.0).contains(v));
        out.push(Sample {
            coordinate: Coord::new(lon, lat),
            time: None,
            time_ms: None,
            elevation,
            segment: 0,
        });
    }
    if out.len() < 2 {
        return Err("坐标文本少于两个点".into());
    }
    Ok(out)
}

/// 解析坐标 JSON 数组。接受 `[{"lat":..,"lon":..},..]`、`[[lat,lon],..]`
/// 以及只含 `points`/`track` 键的外层对象。
pub fn parse_point_json(text: &str) -> Result<Vec<Sample>, String> {
    if text.len() > MAX_BYTES {
        return Err("坐标 JSON 超过 24 MiB".into());
    }
    let root: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("坐标 JSON 无效：{e}"))?;
    let arr = root
        .as_array()
        .or_else(|| root.get("points").and_then(|v| v.as_array()))
        .or_else(|| root.get("track").and_then(|v| v.as_array()))
        .ok_or("坐标 JSON 里没有数组（期望顶层数组或 points/track 键）")?;
    let num = |v: Option<&serde_json::Value>| -> Option<f64> {
        v.and_then(|x| x.as_f64().or_else(|| x.as_str()?.parse().ok()))
    };
    let mut out = Vec::new();
    for (idx, item) in arr.iter().enumerate() {
        let (lat, lon, ele) = if let Some(pair) = item.as_array() {
            (num(pair.first()), num(pair.get(1)), num(pair.get(2)))
        } else {
            let obj = item
                .as_object()
                .ok_or_else(|| format!("第 {} 个元素既不是数组也不是对象", idx + 1))?;
            let pick = |keys: &[&str]| keys.iter().find_map(|k| num(obj.get(*k)));
            (
                pick(&["lat", "latitude", "Lat"]),
                pick(&["lon", "lng", "longitude", "Lon"]),
                pick(&["ele", "elevation", "alt", "altitude"]),
            )
        };
        let (lat, lon) = (
            lat.ok_or_else(|| format!("第 {} 个元素缺少纬度", idx + 1))?,
            lon.ok_or_else(|| format!("第 {} 个元素缺少经度", idx + 1))?,
        );
        if !lat.is_finite() || !lon.is_finite() || lat.abs() > 90. || lon.abs() > 180. {
            return Err(format!("第 {} 个元素坐标超出范围", idx + 1));
        }
        out.push(Sample {
            coordinate: Coord::new(lon, lat),
            time: None,
            time_ms: None,
            elevation: ele.filter(|v| v.is_finite() && (-500.0..=9000.0).contains(v)),
            segment: 0,
        });
    }
    if out.len() < 2 {
        return Err("坐标 JSON 少于两个点".into());
    }
    Ok(out)
}

/// 按分段汇总海拔：返回（累计爬升米，累计下降米，有效点数）。
///
/// 用 3 米阈值过滤 GPS 海拔抖动 —— 否则平路也能算出几百米爬升。
pub fn ascent_descent(samples: &[Sample]) -> (f64, f64, usize) {
    const HYSTERESIS_M: f64 = 3.0;
    let (mut up, mut down, mut n) = (0.0, 0.0, 0usize);
    let mut prev: Option<f64> = None;
    for s in samples {
        let Some(e) = s.elevation else { continue };
        n += 1;
        match prev {
            Some(p) => {
                let d = e - p;
                if d >= HYSTERESIS_M {
                    up += d;
                    prev = Some(e);
                } else if d <= -HYSTERESIS_M {
                    down += -d;
                    prev = Some(e);
                }
                // 阈值内的抖动不更新基准，避免逐点累加噪声
            }
            None => prev = Some(e),
        }
    }
    (up, down, n)
}

/// `parse_time_ms` 在整条轨迹上的结果：任一相邻点时间倒退即整条不可信。
pub fn times_monotonic(samples: &[Sample]) -> bool {
    samples
        .windows(2)
        .filter(|w| w[0].segment == w[1].segment)
        .all(|w| match (w[0].time_ms, w[1].time_ms) {
            (Some(a), Some(b)) => b >= a,
            _ => true,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elevation_and_time_are_parsed_per_point() {
        let xml = r#"<gpx><trk><trkseg>
            <trkpt lat="39.9" lon="116.4"><ele>52.5</ele><time>2026-01-01T00:00:00Z</time></trkpt>
            <trkpt lat="39.901" lon="116.4"><ele>bad</ele><time>2026-01-01T00:01:00Z</time></trkpt>
        </trkseg></trk></gpx>"#;
        let s = parse(xml).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].elevation, Some(52.5));
        assert_eq!(s[0].time_ms, parse_time_ms("2026-01-01T00:00:00Z"));
        // 非法海拔按缺失处理，不影响该点其余信息
        assert_eq!(s[1].elevation, None);
        assert!(s[1].time_ms.unwrap() > s[0].time_ms.unwrap());
        assert!(times_monotonic(&s));
    }

    #[test]
    fn ascent_ignores_sub_threshold_jitter() {
        let mk = |ele: Option<f64>| Sample {
            coordinate: Coord::new(0., 0.),
            time: None,
            time_ms: None,
            elevation: ele,
            segment: 0,
        };
        let flat = vec![mk(Some(50.)), mk(Some(51.)), mk(Some(49.)), mk(Some(50.5))];
        let (up, down, n) = ascent_descent(&flat);
        assert_eq!(n, 4);
        assert!(
            up < 1e-9 && down < 1e-9,
            "抖动不该计入：up={up} down={down}"
        );
        let hill = vec![mk(Some(50.)), mk(Some(60.)), mk(Some(45.))];
        let (up, down, _) = ascent_descent(&hill);
        assert!((up - 10.0).abs() < 1e-9);
        assert!((down - 15.0).abs() < 1e-9);
        // 没有海拔的点不参与统计
        assert_eq!(ascent_descent(&[mk(None), mk(None)]).2, 0);
    }

    #[test]
    fn point_text_accepts_comma_space_and_skips_headers() {
        let text = "lat,lon\n39.9,116.4\n39.901 116.4 55\n# 注释\n\n39.902;116.401";
        let s = parse_point_text(text).unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!(s[1].elevation, Some(55.0));
        assert!((s[0].coordinate.lon - 116.4).abs() < 1e-9);
        assert!(parse_point_text("39.9,116.4").is_err(), "单点应报错");
        assert!(parse_point_text("39.9,999").is_err(), "经度越界应报错");
    }

    #[test]
    fn point_json_handles_objects_pairs_and_wrappers() {
        let a = parse_point_json(
            r#"[{"lat":39.9,"lon":116.4,"ele":50},{"latitude":39.91,"longitude":116.4}]"#,
        )
        .unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].elevation, Some(50.0));
        let b = parse_point_json(r#"{"points":[[39.9,116.4],[39.91,116.4,61]]}"#).unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!(b[1].elevation, Some(61.0));
        assert!(parse_point_json(r#"{"nope":1}"#).is_err());
        assert!(
            parse_point_json(r#"[{"lat":39.9}]"#).is_err(),
            "缺经度应报错"
        );
    }

    #[test]
    fn dtd_and_truncated_inputs_still_rejected() {
        assert!(parse(r#"<!DOCTYPE gpx><gpx><trkpt lat="1" lon="1"/></gpx>"#).is_err());
        assert!(parse(r#"<gpx><trkpt lat="1" lon="1">"#).is_err());
    }
}
