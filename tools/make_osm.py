#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""生成（或修正）可被 NekoSportsWorldTool 直接导入的路网文件（OSM XML，.osm）。

三种用法
--------
1) 在操场/校园里画几圈同心环（最稳，适合「就在操场跑」）：

    python tools/make_osm.py --mode ring \
        --center 40.6574,109.8404 \
        --rx 90 --ry 60 --rings 3 --step 25 --segments 96 \
        --out school.osm

2) 手上有学校给的打卡点位（WGS84），让路网串着它们走：

    # points.txt 每行：lat,lon   （逗号或空白分隔，可用 # 注释）
    python tools/make_osm.py --mode loop --from-points points.txt --out school.osm

3) 用 JOSM 手绘后**必须**跑一次修正（JOSM 给新对象分配的是负数 id，
   而本项目的解析器把 id 当 u64 读，负数会解析失败 → 所有 way 被静默丢弃，
   导入时报「OSM 中未找到可用的 highway 道路」。这不是路网画错了）：

    python tools/make_osm.py --renumber josm-out.osm --out school.osm

生成完在软件「路网」页 →「手动 OSM 文件」→ 选中文件 →「加载路网」。
手机端点「选择文件…」，系统选择器选文件后会被复制到应用私有目录。

为什么需要这个脚本
------------------
软件只认 OSM XML（UTF-8，`<osm>…</osm>`），而且对路网有硬约束；不符合时不会
「勉强跑」，直接报错。脚本生成后会做同样的自检，把问题在电脑上就暴露出来。

硬约束（来自 src/track/generate_road.rs、crates/route-planner/src/{load,check}.rs）
----------------------------------------------------------------------------------
* **id 正负都能用**：`crates/route-planner/src/load.rs` 自 2026-10 起按 i64 解析
  `id` 与 `nd ref`，所以 JOSM/iD 未上传对象的负数 id 可以直接导入。
  `--renumber` 只剩「把负 id 规整成正数」的可选用途（不再必需）。
* 坐标系必须 WGS84（OSM 原生）。软件内部逐节点转 BD-09 与打卡点同系，
  所以**不要**手填 BD-09 坐标，否则整条轨迹偏 500 米左右。
* 每条 way 必须有 `highway` 标签，且不能是
  motorway/trunk/primary/secondary 及其 _link、construction、proposed
  —— 这些被当作机动车道整条丢弃，路网可能是空的。
* 每条 way ≥ 2 个节点，被引用的节点必须在同一文件里有坐标。
  节点按坐标焊接（1e-7 度 ≈ 1.1 cm），所以「同位置不同 id」也能连上。
* 文件 ≤ 24 MiB（Android 选择器与 Rust 侧共用这一个上限）。
* 全部策略点/实时点位距最近道路 ≤ 40 米，否则报
  「路网无法到达第 N 个点位（最近 xx 米，超过 40 米）」。
* 点位之间必须**双向可达**：单向（oneway）或断头路会报
  「点位 N 到 M 的道路不连通或方向受限」。
* 有学校围栏时，围栏外的节点会被裁掉、跨围栏的边会被删；所以路网要落在围栏内。
* 目标距离必须 ≥ 按点位顺序闭环的最短路总长，否则报「目标 X 米不足」。
"""

import argparse
import math
import os
import re
import sys
import xml.etree.ElementTree as ET

# 与 Rust 侧 school_roads::MAX_BYTES / MainActivity.MAX_OSM_BYTES 一致
MAX_BYTES = 24 * 1024 * 1024

# 被软件当作机动车道丢弃的 highway 值（load.rs: is_motorized）
MOTORIZED = {
    "motorway", "motorway_link",
    "trunk", "trunk_link",
    "primary", "primary_link",
    "secondary", "secondary_link",
    "construction", "proposed",
}

M_PER_DEG_LAT = 111_320.0


def _attr(value):
    """XML 属性值转义（含引号），避免非法字符让整份文件非良构。"""
    text = str(value)
    return (
        text.replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
        .replace('"', "&quot;")
        .replace("'", "&apos;")
    )


def m_per_deg_lon(lat_deg):
    return M_PER_DEG_LAT * max(0.05, math.cos(math.radians(lat_deg)))


class Builder:
    """按 OSM XML 约定累加节点/道路。

    id 一律用**正整数**：本项目的解析器把 id 当 u64，负 id 会导致整条 way 被丢弃。
    """

    def __init__(self):
        self.nodes = []          # [(id, lat, lon)]
        self.ways = []           # [(id, [node ids], [(k, v), ...])]
        self._next_node = 1
        self._next_way = 1

    def node(self, lat, lon):
        nid = self._next_node
        self._next_node += 1
        self.nodes.append((nid, lat, lon))
        return nid

    def way(self, refs, tags):
        if len(refs) < 2:
            raise ValueError("way 至少要 2 个节点，否则会被丢弃")
        wid = self._next_way
        self._next_way += 1
        self.ways.append((wid, list(refs), list(tags)))
        return wid

    def to_xml(self, generator="NekoSportsWorldTool make_osm.py"):
        out = ["<?xml version='1.0' encoding='UTF-8'?>",
               '<osm version="0.6" generator="%s">' % _attr(generator)]
        for nid, lat, lon in self.nodes:
            out.append('  <node id="%d" lat="%.7f" lon="%.7f"/>' % (nid, lat, lon))
        for wid, refs, tags in self.ways:
            out.append('  <way id="%d">' % wid)
            for r in refs:
                out.append('    <nd ref="%d"/>' % r)
            for k, v in tags:
                # 属性必须转义：`--highway 'path&x'` 这类值会让整份 XML 非良构，
                # 而解析器只会回一句「文件不是完整的 OSM XML」，很难查。
                out.append('    <tag k="%s" v="%s"/>' % (_attr(k), _attr(v)))
            out.append("  </way>")
        out.append("</osm>")
        return "\n".join(out) + "\n"


def ring_nodes(builder, lat0, lon0, rx, ry, segments, phase=0.0):
    """以 (lat0, lon0) 为中心生成一个闭合椭圆环，返回节点 id 序列（首尾不重复）。"""
    kx = m_per_deg_lon(lat0)
    ids = []
    for i in range(segments):
        t = phase + 2.0 * math.pi * i / segments
        lat = lat0 + (ry * math.sin(t)) / M_PER_DEG_LAT
        lon = lon0 + (rx * math.cos(t)) / kx
        ids.append(builder.node(lat, lon))
    return ids


def mode_ring(args):
    lat0, lon0 = args.center
    b = Builder()
    prev_anchor = None
    ring_ids = []
    for r in range(args.rings):
        rx = args.rx + r * args.step
        ry = args.ry + r * args.step
        ids = ring_nodes(b, lat0, lon0, rx, ry, args.segments, phase=r * 0.13)
        b.way(ids + [ids[0]], [("highway", args.highway), ("name", "训练环 %d" % (r + 1))])
        ring_ids.append(ids)
        if prev_anchor is not None:
            # 径向连接线：把相邻两环连起来，保证整张图连通
            b.way([prev_anchor, ids[0]], [("highway", "service"), ("name", "连接道 %d" % r)])
        prev_anchor = ids[0]

    if args.building:
        # 可选建筑轮廓：只影响 GPS 抖动的建筑遮挡衰减（SDF），不给也能跑
        kx = m_per_deg_lon(lat0)
        half_lat = 8.0 / M_PER_DEG_LAT
        half_lon = 12.0 / kx
        pts = [
            b.node(lat0 - half_lat, lon0 - half_lon),
            b.node(lat0 - half_lat, lon0 + half_lon),
            b.node(lat0 + half_lat, lon0 + half_lon),
            b.node(lat0 + half_lat, lon0 - half_lon),
        ]
        b.way(pts + [pts[0]], [("building", "yes")])

    # 最内环周长：判断「一圈够不够目标里程」
    kx = m_per_deg_lon(lat0)
    ids = ring_ids[0]
    coord = {nid: (lat, lon) for nid, lat, lon in b.nodes}
    circ = 0.0
    for i in range(len(ids)):
        la1, lo1 = coord[ids[i]]
        la2, lo2 = coord[ids[(i + 1) % len(ids)]]
        circ += math.hypot((lo2 - lo1) * kx, (la2 - la1) * M_PER_DEG_LAT)
    return b, {"rings": args.rings, "ring0_circumference_m": circ}, [coord[i] for i in ids]


def read_points(path):
    pts = []
    with open(path, "r", encoding="utf-8-sig") as fh:
        for lineno, raw in enumerate(fh, 1):
            line = raw.strip()
            if not line or line.startswith("#"):
                continue
            parts = line.replace(",", " ").split()
            if len(parts) < 2:
                raise ValueError("第 %d 行不是 lat lon：%r" % (lineno, raw))
            lat, lon = float(parts[0]), float(parts[1])
            if not (-90.0 <= lat <= 90.0) or not (-180.0 <= lon <= 180.0):
                raise ValueError("第 %d 行坐标越界（需要 WGS84 的 lat,lon）：%r" % (lineno, raw))
            pts.append((lat, lon))
    if len(pts) < 2:
        raise ValueError("点位少于 2 个，构不成闭环")
    return pts


def densify(a, b, step_m):
    """在 a→b 之间按间距插入中间节点，让路径上有足够顶点（弯道/转弯更自然）。"""
    (la1, lo1), (la2, lo2) = a, b
    dist = math.hypot((lo2 - lo1) * m_per_deg_lon(la1), (la2 - la1) * M_PER_DEG_LAT)
    n = max(1, int(math.ceil(dist / max(1.0, step_m))))
    return [(la1 + (la2 - la1) * i / n, lo1 + (lo2 - lo1) * i / n) for i in range(1, n)]


def mode_loop(args):
    pts = read_points(args.from_points)
    b = Builder()
    ids = []
    anchor_ids = []
    for i, p in enumerate(pts):
        nid = b.node(*p)
        anchor_ids.append(nid)
        ids.append(nid)
        q = pts[(i + 1) % len(pts)]
        for mid in densify(p, q, args.step_m):
            ids.append(b.node(*mid))
    b.way(ids + [ids[0]], [("highway", args.highway), ("name", "点位闭环")])
    if args.branch:
        # 支线必须从**环上已有的节点**出发（复用同一个 node id）。
        # 旧写法在这里新建了一个中点节点，它与环上的点坐标不同、焊不上，
        # 结果整张图被切成「1 个环 + N 条浮空支线」（独立审计实测 components=5）。
        for i, p in enumerate(pts):
            tip = (p[0] + 60.0 / M_PER_DEG_LAT, p[1])
            b.way([anchor_ids[i], b.node(*tip)],
                  [("highway", "service"), ("name", "支线 %d" % (i + 1))])
    return b, {"points": len(pts)}, pts


def polyline_length_m(poly):
    kx = m_per_deg_lon(poly[0][0])
    total = 0.0
    for i in range(len(poly)):
        a, b = poly[i], poly[(i + 1) % len(poly)]
        total += math.hypot((b[1] - a[1]) * kx, (b[0] - a[0]) * M_PER_DEG_LAT)
    return total


def sample_closed(poly, target_m, step_m):
    """沿闭合折线按弧长等距采样；跑完一圈继续绕，直到 target_m。"""
    kx = m_per_deg_lon(poly[0][0])
    segs = []
    total = 0.0
    for i in range(len(poly)):
        a, b = poly[i], poly[(i + 1) % len(poly)]
        d = math.hypot((b[1] - a[1]) * kx, (b[0] - a[0]) * M_PER_DEG_LAT)
        segs.append((a, b, d, total))
        total += d
    if total <= 1.0:
        raise ValueError("闭环周长不足 1 米，无法生成轨迹")
    out = []
    s = 0.0
    while s <= target_m:
        t = s % total
        for a, b, d, start in segs:
            if t <= start + d or (a, b, d, start) == segs[-1]:
                f = 0.0 if d <= 1e-9 else (t - start) / d
                f = min(max(f, 0.0), 1.0)
                out.append((a[0] + (b[0] - a[0]) * f, a[1] + (b[1] - a[1]) * f, s))
                break
        s += step_m
    return out, total


def build_gpx_text(samples, pace_s_per_km, alt_base, start=None):
    """生成 GPX 1.1：<trkpt> 带 <ele> 与 <time>，可直接进「导入真实 GPX 并分析」。"""
    import datetime

    if start is None:
        start = datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0)
    speed = 1000.0 / max(60.0, pace_s_per_km)  # m/s
    lines = ['<?xml version="1.0" encoding="UTF-8"?>',
             '<gpx version="1.1" creator="NekoSportsWorldTool make_osm.py"'
             ' xmlns="http://www.topografix.com/GPX/1/1">',
             '  <trk><name>本地校验轨迹</name>',
             '    <trkseg>']
    for lat, lon, s in samples:
        # 平缓起伏：两个不同周期的正弦叠加，避免「海拔完全恒定」这种一眼假的形态
        ele = alt_base + 6.0 * math.sin(s / 700.0 * 2 * math.pi) + 1.5 * math.sin(s / 130.0 * 2 * math.pi)
        ts = start + datetime.timedelta(seconds=s / speed)
        lines.append('      <trkpt lat="%.7f" lon="%.7f"><ele>%.1f</ele><time>%s</time></trkpt>'
                     % (lat, lon, ele, ts.strftime("%Y-%m-%dT%H:%M:%SZ")))
    lines.append('    </trkseg>')
    lines.append('  </trk>')
    lines.append('</gpx>')
    return "\n".join(lines) + "\n"


def read_gpx_points(path):
    """读 GPX 里的 <trkpt>（按文件顺序；WGS84）。"""
    root = ET.parse(path).getroot()
    pts = []
    for el in root.iter():
        tag = el.tag.split("}")[-1]
        if tag != "trkpt":
            continue
        try:
            pts.append((float(el.get("lat")), float(el.get("lon"))))
        except (TypeError, ValueError):
            raise ValueError("GPX 里有缺少 lat/lon 的 trkpt")
    if len(pts) < 2:
        raise ValueError("GPX 里少于两个轨迹点")
    return pts


def downsample(pts, step_m):
    """按累计距离降采样：默认 15 米一个顶点，避免把每条轨迹点都写进路网。"""
    out = [pts[0]]
    acc = 0.0
    for i in range(1, len(pts)):
        a, b = pts[i - 1], pts[i]
        acc += math.hypot((b[1] - a[1]) * m_per_deg_lon(a[0]), (b[0] - a[0]) * M_PER_DEG_LAT)
        if acc >= step_m:
            out.append(b)
            acc = 0.0
    if out[-1] != pts[-1]:
        out.append(pts[-1])
    return out


def dist_m(a, b):
    return math.hypot((b[1] - a[1]) * m_per_deg_lon(a[0]), (b[0] - a[0]) * M_PER_DEG_LAT)


def keep_one_lap(pts, tol_m=25.0, min_pts=12):
    """多圈轨迹只保留第一圈。

    为什么必须截断：Rust 侧按 1e-7 度（约 1 cm）焊接节点，而每圈的采样相位
    不同（一圈 476 m ÷ 5 m 不是整数），相邻两圈的点会相差 1 m 左右、**焊不上**。
    结果是一叠几乎重合的平行环，最短路被迫在层间来回跳，`check` 报的
    「最小闭环」会翻十几倍，提交时直接「目标 X 米不足」。
    """
    if len(pts) < min_pts:
        return pts, False
    for i in range(min_pts, len(pts)):
        if dist_m(pts[i], pts[0]) < tol_m:
            return pts[: i + 1], True
    return pts, False


def mode_gpx(args):
    samples = read_gpx_points(args.from_gpx)
    pts = downsample(samples, args.step_m)
    pts, truncated = keep_one_lap(pts, args.lap_tol)
    if len(pts) < 3:
        raise ValueError("降采样后不足 3 个顶点，请把 --step-m 调小")
    # 去掉首尾重复点（闭合由 way 自己补第一点）
    if len(pts) > 3 and dist_m(pts[0], pts[-1]) < 3.0:
        pts = pts[:-1]
    b = Builder()
    ids = [b.node(*p) for p in pts]
    b.way(ids + [ids[0]], [("highway", args.highway), ("name", "GPX 反推路网")])
    info = {"gpx_points": len(samples), "vertices": len(pts)}
    if truncated:
        info["note"] = "多圈已截断为1圈"
    return b, info, pts


def renumber(src, dst):
    """把任意 .osm 里的负数 id 换成唯一正整数（JOSM 手绘输出必做）。"""
    tree = ET.parse(src)
    root = tree.getroot()
    node_map = {}
    way_map = {}
    next_id = 1
    for el in root.findall("node"):
        old = el.get("id")
        node_map[old] = str(next_id)
        el.set("id", str(next_id))
        next_id += 1
    for el in root.findall("way"):
        old = el.get("id")
        way_map[old] = str(next_id)
        el.set("id", str(next_id))
        next_id += 1
        for nd in el.findall("nd"):
            ref = nd.get("ref")
            if ref in node_map:
                nd.set("ref", node_map[ref])
    for rel in root.findall("relation"):
        old = rel.get("id")
        rel.set("id", str(next_id))
        next_id += 1
        for m in rel.findall("member"):
            t, ref = m.get("type"), m.get("ref")
            if t == "node" and ref in node_map:
                m.set("ref", node_map[ref])
            elif t == "way" and ref in way_map:
                m.set("ref", way_map[ref])
    tree.write(dst, encoding="UTF-8", xml_declaration=True)
    return {"nodes": len(node_map), "ways": len(way_map)}


def parse_osm_file(path):
    """按本项目的规则统计：可用的 highway way、驾驶道路、节点、建筑。"""
    tree = ET.parse(path)
    root = tree.getroot()
    nodes = set()
    negative_ids = 0
    for n in root.findall("node"):
        nid = n.get("id", "")
        if nid.startswith("-"):
            negative_ids += 1
        nodes.add(nid)
    usable, motorized, missing_ref, buildings = 0, 0, 0, 0
    for w in root.findall("way"):
        tags = {t.get("k"): t.get("v") for t in w.findall("tag")}
        refs = [nd.get("ref") for nd in w.findall("nd")]
        if any(r not in nodes for r in refs) or len(refs) < 2:
            missing_ref += 1
        if "building" in tags and tags.get("building") != "no":
            buildings += 1
            continue
        hw = tags.get("highway")
        if hw is None:
            continue
        if hw in MOTORIZED:
            motorized += 1
        elif len(refs) >= 2:
            usable += 1
    return {
        "nodes": len(nodes),
        "usable_ways": usable,
        "motorized_ways_dropped": motorized,
        "broken_ways": missing_ref,
        "buildings": buildings,
        "negative_ids": negative_ids,
    }


def self_check(builder, xml_bytes):
    problems = []
    if not builder.ways:
        problems.append("没有任何 way —— 软件会报「OSM 中未找到可用的 highway 道路」")
    usable = 0
    for wid, refs, tags in builder.ways:
        tag = dict(tags)
        if "building" in tag:
            continue
        hw = tag.get("highway")
        if hw is None:
            problems.append("way %d 没有 highway 标签，会被静默忽略" % wid)
            continue
        if hw in MOTORIZED:
            problems.append("way %d 的 highway=%s 属机动车道，会被整条丢弃" % (wid, hw))
            continue
        if len(refs) < 2:
            problems.append("way %d 节点数 < 2，会被丢弃" % wid)
            continue
        usable += 1
    if usable == 0:
        problems.append("没有一条可用的 highway 道路 —— 导入后会直接失败")
    if len(xml_bytes) > MAX_BYTES:
        problems.append("文件 %.1f MiB 超过 24 MiB 上限" % (len(xml_bytes) / 1048576.0))
    return problems


def main(argv=None):
    ap = argparse.ArgumentParser(
        description="生成 / 修正 NekoSportsWorldTool 可导入的 OSM XML 路网（WGS84）",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    ap.add_argument("--renumber", metavar="SRC.osm",
                    help="修正已有文件（JOSM 手绘后必跑）：负数 id → 正整数")
    ap.add_argument("--mode", choices=["ring", "loop", "gpx"], default="ring")
    ap.add_argument("--center", default="40.6574,109.8404",
                    help="ring 模式中心：lat,lon（WGS84），默认包头市中心")
    ap.add_argument("--rx", type=float, default=90.0, help="最内环东西半径（米）")
    ap.add_argument("--ry", type=float, default=60.0, help="最内环南北半径（米）")
    ap.add_argument("--rings", type=int, default=3, help="同心环数量")
    ap.add_argument("--step", type=float, default=25.0, help="每外扩一环半径增加（米）")
    ap.add_argument("--segments", type=int, default=96, help="每环顶点数（越多越圆滑）")
    ap.add_argument("--building", action="store_true", help="附带一个建筑轮廓（SDF 遮挡）")
    ap.add_argument("--from-points", help="loop 模式：点位文件，每行 lat,lon（WGS84）")
    ap.add_argument("--from-gpx", help="gpx 模式：从一条真实 GPX 轨迹反推路网（WGS84）")
    ap.add_argument("--lap-tol", type=float, default=25.0,
                    help="gpx 模式：判定「回到起点」的容差（米），多圈轨迹会截断为第一圈")
    ap.add_argument("--step-m", type=float, default=8.0, help="loop 模式插点间距（米）")
    ap.add_argument("--branch", action="store_true", help="loop 模式：额外拉断头支线")
    ap.add_argument("--highway", default="footway",
                    help="道路类型（footway/service/path/residential/tertiary…）")
    ap.add_argument("--emit-gpx", metavar="OUT.gpx",
                    help="同时生成一段 GPX 轨迹（用同一几何重复跑满 --gpx-dist），供「导入真实 GPX 并分析」校验")
    ap.add_argument("--gpx-dist", type=float, default=3000.0, help="GPX 目标里程（米）")
    ap.add_argument("--gpx-step", type=float, default=5.0, help="GPX 采样间距（米）")
    ap.add_argument("--pace", type=float, default=360.0, help="配速（秒/公里），默认 6'00\"")
    ap.add_argument("--alt", type=float, default=60.0, help="海拔基线（米）")
    ap.add_argument("--out", default="school.osm", help="输出文件名")
    args = ap.parse_args(argv)

    if args.renumber:
        out = args.out if args.out != "school.osm" else os.path.splitext(args.renumber)[0] + ".fixed.osm"
        info = renumber(args.renumber, out)
        stats = parse_osm_file(out)
        print("已写出：%s" % os.path.abspath(out))
        print("节点 %d · way %d" % (info["nodes"], info["ways"]))
        print("复查：可用道路 %d 条，建筑 %d，被丢弃的机动车道 %d，引用缺失节点的 way %d，残负数 id %d"
              % (stats["usable_ways"], stats["buildings"], stats["motorized_ways_dropped"],
                 stats["broken_ways"], stats["negative_ids"]))
        if stats["usable_ways"] == 0:
            print("\n仍然没有可用道路：请确认每条线都有 highway 标签（JOSM 里画完要选预设「路径/人行道」）")
            return 1
        if stats["negative_ids"]:
            print("\n仍有负数 id，修正未完成")
            return 1
        return 0

    if args.highway in MOTORIZED:
        print("错误：--highway %s 是机动车道，会被软件丢弃" % args.highway, file=sys.stderr)
        return 2
    if not re.fullmatch(r"[a-z][a-z_]*", args.highway or ""):
        print(
            "错误：--highway 只能是 highway 标签值（小写字母/下划线，如 footway、path、service）",
            file=sys.stderr,
        )
        return 2
    if args.mode == "ring" and (args.rings < 1 or args.segments < 3):
        print("错误：--rings 至少 1，--segments 至少 3", file=sys.stderr)
        return 2

    if args.mode == "ring":
        try:
            lat0, lon0 = [float(v) for v in args.center.replace(" ", "").split(",")]
        except ValueError:
            print("错误：--center 需要 lat,lon", file=sys.stderr)
            return 2
        if not (-90 <= lat0 <= 90 and -180 <= lon0 <= 180):
            print("错误：--center 需要 WGS84 的 lat,lon", file=sys.stderr)
            return 2
        args.center = (lat0, lon0)
        builder, info, polyline = mode_ring(args)
    elif args.mode == "gpx":
        if not args.from_gpx:
            print("错误：gpx 模式需要 --from-gpx 轨迹文件", file=sys.stderr)
            return 2
        builder, info, polyline = mode_gpx(args)
    else:
        if not args.from_points:
            print("错误：loop 模式需要 --from-points 点位文件", file=sys.stderr)
            return 2
        builder, info, polyline = mode_loop(args)

    xml = builder.to_xml().encode("utf-8")
    out_path = os.path.abspath(args.out)
    with open(out_path, "wb") as fh:
        fh.write(xml)

    problems = self_check(builder, xml)
    print("已写出：%s（%.1f KiB）" % (out_path, len(xml) / 1024.0))
    print("节点 %d · way %d · %s" % (
        len(builder.nodes), len(builder.ways),
        ", ".join("%s=%s" % (k, ("%.0f" % v) if isinstance(v, float) else v)
                  for k, v in info.items()),
    ))
    if problems:
        print("\n自检发现 %d 个问题（软件会因此拒绝或报错）：" % len(problems))
        for p in problems:
            print("  - " + p)
        return 1
    print("\n自检通过。导入后请确认：")
    print("  1) 每个策略点/实时点位距这条路 ≤ 40 米（超出会报「路网无法到达第 N 个点位」）")
    print("  2) 路网落在学校围栏内（围栏外的道路会被裁掉）")
    print("  3) 目标里程 ≥ 按点位顺序闭环一圈的最短总长（否则报「目标 X 米不足」）")

    if args.emit_gpx:
        samples, lap = sample_closed(polyline, args.gpx_dist, args.gpx_step)
        gpx = build_gpx_text(samples, args.pace, args.alt)
        gpx_path = os.path.abspath(args.emit_gpx)
        with open(gpx_path, "w", encoding="utf-8") as fh:
            fh.write(gpx)
        speed = 1000.0 / max(60.0, args.pace)
        print("\n已写出 GPX：%s" % gpx_path)
        print("  一圈 %.0f 米 · 目标 %.0f 米（绕 %.1f 圈）· 采样 %d 点 · 预计 %d 分 %02d 秒"
              % (lap, args.gpx_dist, args.gpx_dist / lap, len(samples),
                 int((args.gpx_dist / speed) // 60), int((args.gpx_dist / speed) % 60)))
        print("  用途：「路网」页 →「导入真实 GPX 并分析」，看里程/越界是否与预期一致（本地分析，不上传）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
