//! OSM `.osm`（XML）解析：节点 / 道路 way / 建筑 way+multipolygon relation。
//!
//! 输入为 openstreetmap.org 导出的标准 OSM XML（`<node>`/`<way>`/`<relation>`）。
//! 只抽取：`highway` 路径（剔除机动车道）、`building` 轮廓。

use std::collections::HashMap;

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::graph::{Coord, EdgeData, RoadGraph};

/// 一条道路 way。
///
/// `id` 与节点引用统一用 **i64**：负数 id 是 OSM 的合法约定（JOSM / iD 里
/// 新建但还没上传的对象一律是负数），按 u64 解析会让整份手绘图里的 way
/// 静默失效 — 表现为 `load_osm` 报「OSM 中未找到可用的 highway 道路」。
pub struct OsmRoad {
    pub id: i64,
    pub node_refs: Vec<i64>,
    pub oneway: bool,
    pub oneway_reversed: bool,
    pub highway: String,
    pub maxspeed_kmh: Option<f64>,
}

/// 一个建筑：外环为节点 id 序列。
pub struct OsmBuilding {
    pub outer: Vec<Vec<i64>>,
}

pub struct ParsedOsm {
    pub nodes: HashMap<i64, Coord>,
    pub roads: Vec<OsmRoad>,
    pub buildings: Vec<OsmBuilding>,
}

/// 不应被当作可跑步道路的 highway 值。
fn is_motorized(h: &str) -> bool {
    matches!(
        h,
        "motorway"
            | "motorway_link"
            | "trunk"
            | "trunk_link"
            | "primary"
            | "primary_link"
            | "secondary"
            | "secondary_link"
            | "construction"
            | "proposed"
    )
}

fn parse_maxspeed(v: &str) -> Option<f64> {
    let digits: String = v.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let n: f64 = digits.parse().ok()?;
    if v.to_ascii_lowercase().contains("mph") {
        Some(n * 1.60934)
    } else {
        Some(n)
    }
}

struct Attrs {
    map: HashMap<String, String>,
}

impl Attrs {
    fn new() -> Self {
        Attrs {
            map: HashMap::new(),
        }
    }
    fn insert(&mut self, k: &[u8], v: &[u8]) {
        self.map.insert(
            String::from_utf8_lossy(k).into_owned(),
            String::from_utf8_lossy(v).into_owned(),
        );
    }
    fn get(&self, k: &str) -> Option<&str> {
        self.map.get(k).map(|s| s.as_str())
    }
    /// 解析 `id` 属性。
    ///
    /// 按 **i64** 解析：负数 id 是 JOSM/iD 未上传对象的正常形态。
    fn id(&self) -> Option<i64> {
        self.get("id").and_then(|v| v.trim().parse().ok())
    }
}

/// 解析 OSM XML 字节流。
pub fn parse_osm(bytes: &[u8]) -> Result<ParsedOsm, String> {
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);

    let mut nodes: HashMap<i64, Coord> = HashMap::new();
    let mut roads: Vec<OsmRoad> = Vec::new();
    let mut buildings: Vec<OsmBuilding> = Vec::new();

    // 所有 way 的节点序列（供 relation 建筑外环回填）。
    let mut way_nodes: HashMap<i64, Vec<i64>> = HashMap::new();
    // relation 建筑：收集 outer way id。
    let mut rel_buildings: Vec<Vec<i64>> = Vec::new();

    let mut cur_way: Option<(i64, Vec<i64>, Vec<(String, String)>)> = None;
    let mut cur_rel: Option<Vec<(String, String, String)>> = None;
    let mut cur_rel_tags: Vec<(String, String)> = Vec::new();

    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let local = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                let mut attrs = Attrs::new();
                for a in e.attributes().with_checks(false) {
                    let a = a.map_err(|e| format!("属性解析失败: {e}"))?;
                    attrs.insert(a.key.as_ref(), a.value.as_ref());
                }
                match local.as_str() {
                    "node" => {
                        if let (Some(id), Some(lat), Some(lon)) = (
                            attrs.id(),
                            attrs.get("lat").and_then(|v| v.parse::<f64>().ok()),
                            attrs.get("lon").and_then(|v| v.parse::<f64>().ok()),
                        ) {
                            // "NaN"/"inf" 都能被 f64 成功解析：不拦就会带着 NaN 建图，
                            // 边长变 NaN、`length_m` 也变 NaN，最后以「无法生成路线」
                            // 之类更难查的方式失败。越界坐标同理。
                            if lat.is_finite()
                                && lon.is_finite()
                                && lat.abs() <= 90.0
                                && lon.abs() <= 180.0
                            {
                                nodes.insert(id, Coord::new(lon, lat));
                            }
                        }
                    }
                    "way" => {
                        cur_way = Some((attrs.id().unwrap_or(0), Vec::new(), Vec::new()));
                    }
                    "relation" => {
                        cur_rel = Some(Vec::new());
                        cur_rel_tags = Vec::new();
                    }
                    "nd" => {
                        if let Some((_, refs, _)) = cur_way.as_mut() {
                            if let Some(r) = attrs.get("ref").and_then(|v| v.trim().parse().ok()) {
                                refs.push(r);
                            }
                        }
                    }
                    "tag" => {
                        let k = attrs.get("k").unwrap_or("").to_string();
                        let v = attrs.get("v").unwrap_or("").to_string();
                        if let Some((_, _, tags)) = cur_way.as_mut() {
                            tags.push((k.clone(), v.clone()));
                        }
                        if cur_rel.is_some() {
                            cur_rel_tags.push((k, v));
                        }
                    }
                    "member" => {
                        if let Some(members) = cur_rel.as_mut() {
                            let mtype = attrs.get("type").unwrap_or("").to_string();
                            let mref = attrs.get("ref").unwrap_or("").to_string();
                            let role = attrs.get("role").unwrap_or("").to_string();
                            members.push((mtype, mref, role));
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                let local = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                match local.as_str() {
                    "way" => {
                        if let Some((id, refs, tags)) = cur_way.take() {
                            // 缺 `id` 的 way 会全部落到 0：进 way_nodes 后互相覆盖，
                            // 在 route.rs 的重复惩罚 Map 里也会撞同一个键。OSM 规范要求
                            // way 必有 id（本地未上传对象是负数），所以 id=0 一定是坏了。
                            if id != 0 {
                                way_nodes.insert(id, refs.clone());
                                let is_building =
                                    tags.iter().any(|(k, v)| k == "building" && v != "no");
                                if is_building && refs.len() >= 3 {
                                    buildings.push(OsmBuilding { outer: vec![refs] });
                                } else if let Some(h) = tags
                                    .iter()
                                    .find(|(k, _)| k == "highway")
                                    .map(|(_, v)| v.clone())
                                {
                                    if !is_motorized(&h) && refs.len() >= 2 {
                                        let oneway = tags.iter().any(|(k, v)| {
                                            k == "oneway"
                                                && matches!(v.as_str(), "yes" | "true" | "1")
                                        });
                                        let oneway_reversed =
                                            tags.iter().any(|(k, v)| k == "oneway" && v == "-1");
                                        let maxspeed = tags
                                            .iter()
                                            .find(|(k, _)| k == "maxspeed")
                                            .and_then(|(_, v)| parse_maxspeed(v));
                                        roads.push(OsmRoad {
                                            id,
                                            node_refs: refs,
                                            oneway,
                                            oneway_reversed,
                                            highway: h,
                                            maxspeed_kmh: maxspeed,
                                        });
                                    }
                                }
                            }
                        }
                    }
                    "relation" => {
                        if let Some(members) = cur_rel.take() {
                            let is_building = cur_rel_tags
                                .iter()
                                .any(|(k, v)| k == "building" && v != "no");
                            if is_building {
                                let outer: Vec<i64> = members
                                    .iter()
                                    .filter(|(mtype, _r, role)| mtype == "way" && role == "outer")
                                    .filter_map(|(_, r, _)| r.trim().parse().ok())
                                    .collect();
                                if !outer.is_empty() {
                                    rel_buildings.push(outer);
                                }
                            }
                        }
                        cur_rel_tags = Vec::new();
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("OSM 解析失败: {e}")),
            _ => {}
        }
    }

    // 回填 relation 建筑外环（outer way 的节点序列）。
    for outer_ways in rel_buildings {
        let mut rings: Vec<Vec<i64>> = Vec::new();
        for wid in outer_ways {
            if let Some(refs) = way_nodes.get(&wid) {
                if refs.len() >= 3 {
                    rings.push(refs.clone());
                }
            }
        }
        if !rings.is_empty() {
            buildings.push(OsmBuilding { outer: rings });
        }
    }

    Ok(ParsedOsm {
        nodes,
        roads,
        buildings,
    })
}

/// 节点焊接精度：1e-7 度（约 1.1 cm）。
///
/// OSM 里同一个路口可能被不同 way 用**不同 node id** 表达（合并数据源、手绘、
/// AI 生成、导出工具重编号都会这样）。只按 id 建图时它们「看着连着、图上断开」，
/// 表现为闭环搜索报「道路不连通」或被迫绕远 —— 这是「路网残缺」最常见的一类假断点。
/// 按坐标量化合并可以消除它，且因为容差只有 1 cm，不会误并相距正常的两个路口。
const WELD_QUANT: f64 = 1e7;

fn weld_key(c: Coord) -> (i64, i64) {
    (
        (c.lon * WELD_QUANT).round() as i64,
        (c.lat * WELD_QUANT).round() as i64,
    )
}

/// 用道路 way 构建路网图。
///
/// 节点按**量化坐标**去重（焊接），而不是只按 OSM node id：
/// 同坐标不同 id 的节点共用同一个图节点，交叉口因此真正连通。
///
/// way 里引用了不存在的 node id 时（手绘删点、导出残缺）只跳过这一段，
/// 不 panic：`filter_map` 会把这个 id 从链里剔掉，若剩下的锚点数不足 2 该 way
/// 自然不产生边。
pub fn build_graph(parsed: &ParsedOsm) -> RoadGraph {
    let mut g = RoadGraph::new();
    let mut node_map: HashMap<(i64, i64), petgraph::graph::NodeIndex> = HashMap::new();

    for r in &parsed.roads {
        // way_id 在 `EdgeData` 里是 u64（历史原因，被 route.rs 用作重复惩罚的
        // HashMap 键）。i64 → u64 是**单射**：负数 id 会落到高位区间，与任何
        // 正数 id 都不会撞车，分组语义（同一 way 的段共享一个键）完全保持。
        let way_id = r.id as u64;
        // 先解析节点 id → NodeIndex（避免闭包跨迭代借用）。
        let idxs: Vec<petgraph::graph::NodeIndex> = r
            .node_refs
            .iter()
            .filter_map(|id| {
                let c = *parsed.nodes.get(id)?;
                let key = weld_key(c);
                if let Some(&n) = node_map.get(&key) {
                    Some(n)
                } else {
                    let n = g.add_node(c);
                    node_map.insert(key, n);
                    Some(n)
                }
            })
            .collect();
        for w in idxs.windows(2) {
            let (p, next) = (w[0], w[1]);
            let a = g.node_coord(p);
            let b = g.node_coord(next);
            let len = geo_haversine(a, b);
            if !r.oneway_reversed {
                g.add_edge(
                    p,
                    next,
                    EdgeData {
                        way_id,
                        oneway: r.oneway,
                        highway: r.highway.clone(),
                        maxspeed_kmh: r.maxspeed_kmh,
                        length_m: len,
                    },
                );
            }
            if !r.oneway {
                g.add_edge(
                    next,
                    p,
                    EdgeData {
                        way_id,
                        oneway: r.oneway,
                        highway: r.highway.clone(),
                        maxspeed_kmh: r.maxspeed_kmh,
                        length_m: len,
                    },
                );
            }
        }
    }

    let mut bld: Vec<Vec<Coord>> = Vec::new();
    for b in &parsed.buildings {
        for ring in &b.outer {
            let coords: Vec<Coord> = ring
                .iter()
                .filter_map(|id| parsed.nodes.get(id).copied())
                .collect();
            if coords.len() >= 3 {
                bld.push(coords);
            }
        }
    }
    g.buildings = bld;

    g.compute_anchor();
    g.rebuild_index();
    g
}

/// 完整流程：解析 OSM → 构建路网。
pub fn load_osm(bytes: &[u8]) -> Result<RoadGraph, String> {
    let parsed = parse_osm(bytes)?;
    if parsed.roads.is_empty() {
        return Err("OSM 中未找到可用的 highway 道路".into());
    }
    Ok(build_graph(&parsed))
}

/// 球面近似距离（米）。
fn geo_haversine(a: Coord, b: Coord) -> f64 {
    use geo::HaversineDistance;
    let pa = geo::Point::new(a.lon, a.lat);
    let pb = geo::Point::new(b.lon, b.lat);
    pa.haversine_distance(&pb)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn road(id: i64, refs: &[i64]) -> OsmRoad {
        OsmRoad {
            id,
            node_refs: refs.to_vec(),
            oneway: false,
            oneway_reversed: false,
            highway: "footway".into(),
            maxspeed_kmh: None,
        }
    }

    fn parsed(nodes: &[(i64, f64, f64)], roads: Vec<OsmRoad>) -> ParsedOsm {
        ParsedOsm {
            nodes: nodes
                .iter()
                .map(|&(id, lat, lon)| (id, Coord::new(lon, lat)))
                .collect(),
            roads,
            buildings: Vec::new(),
        }
    }

    #[test]
    fn same_coordinate_different_ids_are_welded_into_one_node() {
        // 节点 2 与 3 完全同坐标但 id 不同：两条 way 应在此真正连通。
        let p = parsed(
            &[
                (1, 39.9000, 116.4000),
                (2, 39.9005, 116.4000),
                (3, 39.9005, 116.4000),
                (4, 39.9010, 116.4000),
            ],
            vec![road(10, &[1, 2]), road(11, &[3, 4])],
        );
        let g = build_graph(&p);
        assert_eq!(g.graph.node_count(), 3, "2/3 应合并成一个节点");
        assert_eq!(g.graph.edge_count(), 4, "两条双向 way 各 2 条有向边");
        // 焊接点应同时归属两条 way：一条 way 在这里收尾（入边）、另一条从这里出发（出边）
        use petgraph::Direction;
        let welded = g
            .graph
            .node_indices()
            .find(|n| (g.node_coord(*n).lat - 39.9005).abs() < 1e-9)
            .expect("应存在焊接点");
        let out = g.graph.edges_directed(welded, Direction::Outgoing).count();
        let inc = g.graph.edges_directed(welded, Direction::Incoming).count();
        // 两条 way 都是双向：焊接点对各自的另一端各有一条出边和一条入边。
        assert_eq!(
            (out, inc),
            (2, 2),
            "焊接点应连到两条 way 的另一端（各双向）"
        );
    }

    #[test]
    fn distinct_coordinates_one_metre_apart_stay_separate() {
        // 1e-5 度纬度约 1.1 m，远超 1 cm 焊接容差：不应合并。
        let p = parsed(
            &[
                (1, 39.90000, 116.4000),
                (2, 39.90050, 116.4000),
                (3, 39.90051, 116.4000),
                (4, 39.90100, 116.4000),
            ],
            vec![road(10, &[1, 2]), road(11, &[3, 4])],
        );
        let g = build_graph(&p);
        assert_eq!(g.graph.node_count(), 4, "相邻 1 m 的两个点不该被焊接");
        assert_eq!(g.graph.edge_count(), 4);
    }

    #[test]
    fn welded_network_is_a_single_connected_component() {
        let p = parsed(
            &[
                (1, 39.9000, 116.4000),
                (2, 39.9005, 116.4000),
                (3, 39.9005, 116.4000),
                (4, 39.9010, 116.4000),
            ],
            vec![road(10, &[1, 2]), road(11, &[3, 4])],
        );
        let g = build_graph(&p);
        let comps = petgraph::algo::connected_components(&g.graph);
        assert_eq!(comps, 1, "焊接后应只有一个连通分量，否则闭环路由会失败");
    }

    // ---------------------------------------------------------------
    // 负数 id（JOSM/iD 未上传对象）回归测试
    // ---------------------------------------------------------------

    /// 生成**同一份几何**的 OSM XML；`negate = true` 时把所有 id 取负数，
    /// 模拟 JOSM 存盘后的本地未上传对象。
    ///
    /// 两份 fixture 都带 JOSM 常写的 `version` / `action` / `timestamp` /
    /// `uid` / `user` / `visible` 额外属性（需求 #2：这些属性不该干扰解析）。
    fn fixture(negate: bool) -> String {
        let f = |id: i64| if negate { -id } else { id };
        let node = |id: i64, lat: f64, lon: f64| {
            format!(
                "  <node id=\"{}\" version=\"2\" action=\"modify\" \
                 timestamp=\"2024-05-01T10:00:00Z\" uid=\"42\" user=\"mapper\" \
                 visible=\"true\" lat=\"{lat:.6}\" lon=\"{lon:.6}\"/>\n",
                f(id)
            )
        };
        let way = |id: i64, refs: &[i64], tags: &[(&str, &str)]| {
            let mut s = format!("  <way id=\"{}\" version=\"1\" action=\"modify\">\n", f(id));
            for r in refs {
                s.push_str(&format!("    <nd ref=\"{}\"/>\n", f(*r)));
            }
            for (k, v) in tags {
                s.push_str(&format!("    <tag k=\"{k}\" v=\"{v}\"/>\n"));
            }
            s.push_str("  </way>\n");
            s
        };

        let mut xml =
            String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<osm version=\"0.6\" generator=\"JOSM\">\n");
        // 道路节点 1..6：一条主路 + 一条单向支路 + 一条回头弯，构成闭环。
        xml.push_str(&node(1, 39.9000, 116.4000));
        xml.push_str(&node(2, 39.9005, 116.4000));
        xml.push_str(&node(3, 39.9010, 116.4000));
        xml.push_str(&node(4, 39.9010, 116.4006));
        xml.push_str(&node(5, 39.9005, 116.4006));
        xml.push_str(&node(6, 39.9000, 116.4006));
        xml.push_str(&way(
            100,
            &[1, 2, 3],
            &[("highway", "footway"), ("surface", "paving_stones")],
        ));
        xml.push_str(&way(
            101,
            &[3, 4],
            &[("highway", "residential"), ("oneway", "yes")],
        ));
        xml.push_str(&way(102, &[4, 5, 6, 1], &[("highway", "footway")]));

        // 独立建筑 way（4 点闭合环）。
        xml.push_str(&node(10, 39.9020, 116.4000));
        xml.push_str(&node(11, 39.9020, 116.4002));
        xml.push_str(&node(12, 39.9022, 116.4002));
        xml.push_str(&node(13, 39.9022, 116.4000));
        xml.push_str(&way(
            200,
            &[10, 11, 12, 13, 10],
            &[("building", "dormitory")],
        ));

        // multipolygon 建筑：无 tag 的 outer way + building 关系。
        xml.push_str(&node(20, 39.9030, 116.4010));
        xml.push_str(&node(21, 39.9030, 116.4012));
        xml.push_str(&node(22, 39.9032, 116.4012));
        xml.push_str(&node(23, 39.9032, 116.4010));
        xml.push_str(&way(201, &[20, 21, 22, 23, 20], &[]));
        xml.push_str(&format!(
            "  <relation id=\"{}\" version=\"1\" action=\"modify\">\n    \
             <member type=\"way\" ref=\"{}\" role=\"outer\"/>\n    \
             <tag k=\"type\" v=\"multipolygon\"/>\n    <tag k=\"building\" v=\"yes\"/>\n  </relation>\n",
            f(300),
            f(201)
        ));
        xml.push_str("</osm>\n");
        xml
    }

    #[test]
    fn negative_ids_parse_and_build_identically_to_positive_ids() {
        let pos = fixture(false);
        let neg = fixture(true);

        let p_pos = parse_osm(pos.as_bytes()).expect("正数 id fixture 应解析成功");
        let p_neg =
            parse_osm(neg.as_bytes()).expect("负数 id fixture（JOSM 未上传形态）应解析成功");

        // 解析层：一个都不能少。
        assert_eq!(
            p_neg.roads.len(),
            p_pos.roads.len(),
            "负数 id 不该让任何 way 被丢弃"
        );
        assert_eq!(p_neg.roads.len(), 3, "fixture 含 3 条 highway way");
        assert_eq!(p_neg.nodes.len(), p_pos.nodes.len(), "节点数应一致");
        assert_eq!(p_neg.buildings.len(), p_pos.buildings.len(), "建筑数应一致");

        // 负号必须原样保留（不是被截断/取绝对值）。
        assert_eq!(p_neg.roads[0].id, -100);
        assert_eq!(p_neg.roads[0].node_refs, vec![-1, -2, -3]);

        // 建图层：节点 / 边 / 建筑全等。
        let g_pos = build_graph(&p_pos);
        let g_neg = build_graph(&p_neg);
        assert_eq!(
            g_neg.graph.node_count(),
            g_pos.graph.node_count(),
            "node_count 应完全一致"
        );
        assert_eq!(
            g_neg.graph.edge_count(),
            g_pos.graph.edge_count(),
            "edge_count 应完全一致"
        );
        assert_eq!(
            g_neg.buildings.len(),
            g_pos.buildings.len(),
            "buildings 应完全一致"
        );

        // 具体数字，避免两边一起退化成 0 也算「一致」。
        assert_eq!(g_pos.graph.node_count(), 6, "6 个道路节点");
        assert_eq!(
            g_pos.graph.edge_count(),
            11,
            "way100=2段×2 + way101=1段×1(单向) + way102=3段×2"
        );
        assert_eq!(g_pos.buildings.len(), 2, "1 个建筑 way + 1 个 multipolygon");

        // way_id 的 i64 → u64 转换必须无损：负数 way 各自成键、互不相等，
        // 且同一 way 的多段共享同一个键（route.rs 的重复惩罚靠这个分组）。
        let mut pos_ids: Vec<u64> = g_pos.graph.edge_weights().map(|e| e.way_id).collect();
        pos_ids.sort_unstable();
        pos_ids.dedup();
        assert_eq!(pos_ids, vec![100u64, 101, 102], "正数 way 各自成键");

        let mut neg_ids: Vec<u64> = g_neg.graph.edge_weights().map(|e| e.way_id).collect();
        neg_ids.sort_unstable();
        neg_ids.dedup();
        assert_eq!(
            neg_ids,
            vec![(-102i64) as u64, (-101i64) as u64, (-100i64) as u64],
            "负数 way 转 u64 后仍互不相等，分组语义保持"
        );

        // 端到端：这就是「JOSM 存盘即可导入」的直接回归点。
        let lg_pos = load_osm(pos.as_bytes()).expect("正数 id 应导入成功");
        let lg_neg = load_osm(neg.as_bytes())
            .expect("负数 id 应导入成功（修复前这里报 'OSM 中未找到可用的 highway 道路'）");
        assert_eq!(lg_neg.graph.node_count(), lg_pos.graph.node_count());
        assert_eq!(lg_neg.graph.edge_count(), lg_pos.graph.edge_count());
        assert_eq!(lg_neg.buildings.len(), lg_pos.buildings.len());
        assert_eq!(lg_neg.graph.edge_count(), 11);
    }

    #[test]
    fn josm_extra_attributes_are_tolerated() {
        // JOSM 存盘会写 version / action / timestamp / uid / user / visible 等
        // 额外属性；解析器用 `attributes().with_checks(false)` 逐条读取，不应
        // 因为这些属性报错或漏读 id/lat/lon。
        let xml = fixture(false);
        for needle in [
            "action=\"modify\"",
            "timestamp=\"2024-05-01T10:00:00Z\"",
            "visible=\"true\"",
            "uid=\"42\"",
        ] {
            assert!(xml.contains(needle), "fixture 应包含 JOSM 属性 {needle}");
        }
        let p = parse_osm(xml.as_bytes()).expect("JOSM 额外属性不该导致解析失败");
        assert_eq!(p.roads.len(), 3, "3 条 highway way 应全部解析出来");
        assert_eq!(p.nodes.len(), 14, "6 道路 + 4 建筑 + 4 multipolygon 节点");
    }

    #[test]
    fn josm_single_quoted_serialization_imports() {
        // JOSM 真实存盘形态：单引号属性、`upload='never'`、自闭合标签前带空格、
        // CRLF 换行、negative id。这些都不该把导入搞挂。
        let xml = "<?xml version='1.0' encoding='UTF-8'?>\r\n\
<osm version='0.6' upload='never' generator='JOSM'>\r\n  \
  <node id='-1' action='modify' visible='true' lat='39.9000' lon='116.4000' />\r\n  \
  <node id='-2' action='modify' visible='true' lat='39.9005' lon='116.4000' />\r\n  \
  <way id='-10' action='modify' visible='true'>\r\n    \
    <nd ref='-1' />\r\n    <nd ref='-2' />\r\n    \
    <tag k='highway' v='footway' />\r\n  </way>\r\n</osm>\r\n";
        let g = load_osm(xml.as_bytes()).expect("JOSM 单引号存盘应可直接导入");
        assert_eq!(g.graph.node_count(), 2);
        assert_eq!(g.graph.edge_count(), 2, "双向 footway");
    }

    #[test]
    fn way_referencing_missing_node_is_skipped_without_panic() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<osm version="0.6">
  <node id="1" lat="39.9000" lon="116.4000"/>
  <node id="2" lat="39.9005" lon="116.4000"/>
  <way id="10"><nd ref="1"/><nd ref="2"/><tag k="highway" v="footway"/></way>
  <!-- 整条 way 的 nd 都指向不存在的节点：应被静默跳过 -->
  <way id="11"><nd ref="3"/><nd ref="4"/><tag k="highway" v="footway"/></way>
  <!-- 中间夹一个不存在的节点：只跳过该点，两端仍应连通 -->
  <way id="12"><nd ref="1"/><nd ref="99"/><nd ref="2"/><tag k="highway" v="footway"/></way>
  <!-- 只剩 1 个可解析锚点：不产生边 -->
  <way id="13"><nd ref="1"/><nd ref="77"/><tag k="highway" v="footway"/></way>
  <!-- 只有 1 个 ref：连 way 都不算 -->
  <way id="14"><nd ref="99"/><tag k="highway" v="footway"/></way>
</osm>"#;
        let g = load_osm(xml.as_bytes()).expect("缺节点不该让整份 OSM 导入失败");
        assert_eq!(g.graph.node_count(), 2, "只有节点 1/2 能落到图上");
        assert_eq!(
            g.graph.edge_count(),
            4,
            "way 10 与 way 12 各产生一对双向边（平行边不合并）"
        );
        // 被丢掉的 way 不应留下任何边。
        assert!(
            g.graph
                .edge_weights()
                .all(|e| e.way_id != 11 && e.way_id != 13 && e.way_id != 14),
            "引用不存在节点的 way 不该建出边"
        );
    }

    /// `lat="NaN"` 能被 f64 解析成功；不拦就会带着 NaN 建图，`length_m` 变 NaN，
    /// 最后以「无法生成路线」这种难查的方式失败。
    #[test]
    fn non_finite_coordinates_are_dropped() {
        let xml = r#"<osm version="0.6">
  <node id="1" lat="39.9000" lon="116.4000"/>
  <node id="2" lat="NaN" lon="116.4005"/>
  <node id="3" lat="39.9010" lon="116.4010"/>
  <node id="4" lat="39.9020" lon="1e12"/>
  <way id="10"><nd ref="1"/><nd ref="2"/><nd ref="3"/><nd ref="4"/><tag k="highway" v="footway"/></way>
</osm>"#;
        let g = load_osm(xml.as_bytes()).expect("非有限坐标应被丢弃，但整体仍可导入");
        assert_eq!(g.graph.node_count(), 2, "只有节点 1/3 坐标合法");
        assert!(
            g.graph.edge_weights().all(|e| e.length_m.is_finite()),
            "任一 basis 为 NaN/越界时都不该产生 NaN 边长"
        );
        assert!(g.graph.edge_count() >= 2, "1 与 3 之间仍应连通（双向）");
    }

    /// 缺 `id` 的 way 全部落到 0：会互相覆盖 way_nodes、并在重复惩罚 Map 里撞键。
    /// OSM 规范要求 way 必有 id（本地未上传对象是负数），所以这类 way 直接不建图。
    #[test]
    fn ways_without_id_are_skipped() {
        let xml = r#"<osm version="0.6">
  <node id="1" lat="39.9000" lon="116.4000"/>
  <node id="2" lat="39.9005" lon="116.4000"/>
  <way><nd ref="1"/><nd ref="2"/><tag k="highway" v="footway"/></way>
  <way><nd ref="1"/><nd ref="2"/><tag k="highway" v="service"/></way>
</osm>"#;
        let err = match load_osm(xml.as_bytes()) {
            Ok(_) => panic!("两条无 id 的 way 不该被当成同一条建图"),
            Err(e) => e,
        };
        assert!(
            err.contains("highway"),
            "应回落到「未找到可用的 highway 道路」，实际：{err}"
        );
    }
}
