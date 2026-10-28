# ScienseRunner 参考实现摘录（用于 NekoSportsWorldTool 借鉴）

分析对象与方式：

| 对象 | 值 |
|---|---|
| `D:\lenovo\xiaz\ScienseRunner.exe` | SHA256 `7b7fc9c4ba9046e3c89e24e4dde35fcb07dd0491c3510efda740ab8edfe6fb77`，PE32/.NET Framework 4.8/anycpu/IL-only，AssemblyName `ScienseRunner` 1.0.12.0，MVID `02cd69af-b9bf-4952-a02d-94fb4c119d2e`，428 TypeDef / 2376 MethodDef / 2571 条 #US 字符串 |
| `D:\lenovo\xiaz\ScienseRunner-v1.0.12.zip` | 仅含 `README.md`、`docs/UPDATE-1.0.10.md`、5 张截图；**无源码**（README 原文：“为保证安全，代码不再公开”） |

方法：静态分析，**未执行目标程序**。exe 用 REA 读元数据（`inspect_managed_artifact` / `inspect_managed_members`），并用脚本提取 `#US`/ASCII 字符串；文档部分直接读 zip 内的 README 与更新公告。以下每条都标了来源，避免把推断当实现。

---

## 一、底图瓦片（exe 字符串原文）

| 项 | 值 | 来源 |
|---|---|---|
| 高德瓦片 | `https://webrd01.is.autonavi.com/appmaptile?lang=zh_cn&size=1&scale=1&style=8&x={x}&y={y}&z={z}` | exe #US |
| OSM 瓦片 | `https://tile.openstreetmap.org/{z}/{x}/{y}.png` | exe #US |
| 缓存目录 | `temp/map_cache`（程序目录相对） | exe #US |
| 缓存上限配置键 | `map_cache_limit_mb`（公告写默认 100 MB） | exe #US + 公告 |
| 高德 Web 服务 Key | `amap_key`；用于静态地图 `restapi.amap.com/v3/staticmap`、逆地理 `restapi.amap.com/v3/geocode/regeo`；设置页有「测试高德 Key」 | exe #US |
| 相关类型 | `Services.AmapBasemapController`、`Services.AmapService`、`Services.TileCacheService`、`Controls.TrackMap` | exe 类型表 |
| 相关方法/字段 | `TrackMap.LoadTileAsync` `DrawTiles` `TileLayerEnabled` `TileUrlTemplate` `TileCacheNamespace` `MaxTileZoom` `TileAttribution` `StaticMapUrl` `ClearTileMemory` `EnsureTile` `DecodeTile`；`AmapBasemapController.LoadAsync`；`AmapService.GetStaticMapAsync` | exe 元数据 |
| 行为 | z 最高请求 18；超过后放大现有瓦片（避免 19/20 空白） | 公告原文 |

## 二、自定义标点（exe 字符串原文）

- 两种模式：「拖动地图模式」（拖拽只移视野，不加点）/「点击标点模式」（左键单击加点）
- 「按住左键画轨迹，松开后自动简化为标点」
- 右键删除最近标点；判定半径 **120 米**（提示「未找到 120 米内的标点」）
- 「从打卡点初始化」：当前校区打卡点直接填成标点（「N 个打卡点初始化标点」）
- 支持导入 `.gpx` →「转为标点」
- 保存后提示「可在跑步页用自定义标点模式」；说明「有路网时会沿道路路由」
- 标点顺序 = 行进顺序
- 落盘两种格式都认：「标点格式应为 lat,lon」、「JSON 标点格式无效」、「JSON 标点应为点数组」
- 按钮组：保存标点 / 撤销 / 清空 / 从打卡点初始化；面板显示「N 个标点」
- 运行时日志有一句 `[track] 自定义轨迹已沿真实道路路由`，否则「使用标点连线」

## 三、轨迹真实性（`docs/UPDATE-1.0.10.md` 原文）

- **上传锚点按计划随机偏移**：锚点本身不变，每次新计划对上传锚点施加 40–60 m 随机方向偏移（默认 50 m，可配 0–200 m，0 = 精确）；预览与提交复用同一份；理由是逐请求漂移会让点位与围栏自相矛盾
- 海拔：学校基准海拔 + 3–15 m/km 目标爬升；未填回退 82 m；`api.open-meteo.com/v1/elevation` 查 DEM
- GPS 噪声：单轴 σ 约 2.8–4.2 m；AR(1) 系数每次运行随机（不再固定 0.72）；1–2 点短时漂移；少量 10–30 m 野点；起点/终点/哨兵/必经点不注入野点
- `radius/accuracy` 右偏分布：普通 4–15 m，type=3 4–20 m，异常 10–30 m
- 零速点：1–3% 短停走窗口（speed/avgSpeed=0，totalDis/validDis 与步数不增长），优先落在起末段
- 步频 OU 序列 + 步幅独立噪声；Classic 取消 100/200 硬钳
- 必经点 40 m 内落位加 3–8 m 自然偏差（避免精确零距离与 40 m 瞬移）
- **提交结束后清空计划缓存**：修「两次记录开始时间/轨迹/步频/步数完全一样」
- 提交幂等 WAL：`submission_receipts/*.jsonl`，Pending 未闭合拒绝重提，超时不当失败
- 点位缓存 300 s（对应 10603「5 分钟 3 次」）

## 四、本项目采纳对照

| 项 | 状态 | 落点 |
|---|---|---|
| 瓦片底图（高德/OSM 可切换、缓存上限、z≤18 放大、手机 pinch） | 本轮采纳 | 团队 `map-custom-route` / t2 |
| 手绘 → 自定义路网（走既有 `load_osm_async`） | 本轮采纳 | t3 |
| GPS 散布可调 0–20 m | 本轮采纳 | t4 |
| 上传锚点按计划偏移 40–60 m | 本轮采纳 | t4 |
| 提交后清空计划缓存 | 本轮采纳 | t6 |
| 负数 id 导入（我方解析器缺陷） | 本轮采纳 | t1 |
| 提交幂等 WAL | 未采纳（需单独一轮 + 专门评审） | 后续 |
| 停走窗口 / 野点 / radius 右偏 / 必经点 3–8 m | 未采纳（改生成器面大，等验收完单开） | 后续 |
| 地图全要素底图（绿地/水域/POI/铁路） | 未采纳（Overpass 负载与解析面） | 后续 |
| 预设导出（路网 gzip 内嵌）、多账号目录 | 未采纳（与本次目标无关） | 后续 |

> 说明：以上“采纳”指进入任务书，不等于已经实现；实现与验收状态以团队任务列表与 `review/MAP-CUSTOM-ROUTE.md` 为准。
