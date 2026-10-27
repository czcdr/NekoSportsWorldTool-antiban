# 瓦片矢量化路网（第 4 种路由模式，实验性）

新增模块 `src/track/tile_road.rs`（自包含）+ 三处薄接线（`RouteMode::Tiles`、`flow.rs` 一个分支、
跑步页一个按钮）。**`Legacy` / `Road` / `Manual` 的算法一行没改**：本模块产出的仍是
`route_planner::RoadGraph`，所以预览、`build_road`、地图渲染全部复用既有链路。

## 一、流程

```
围栏（没有就用锚点 ±700m）
   ↓  tile_range()      算需要哪些瓦片
百度栅格瓦片 pl 风格        fetch_tiles()  四域名轮换 + 限速 + 单张重试一次 + 预算上限
   ↓  拼接成一张 RGB 大图
classify_roads()        彩度 + 亮度 + 自适应阈值 → 掩码 → 3×3 开运算
   ↓
thin()                  Zhang-Suen 细化 → 1px 骨架
   ↓
trace_polylines()       从端点出发、交点按最直方向继续 → 折线
   ↓
simplify()              Douglas-Peucker（容差按米配置）
   ↓
build_graph()           像素→BD-09（局部仿射）→ 按容差合并端点 → 双向图
   ↓
save_cache()            JSON（bbox / z / 生成时间 / 折线 / 统计）
```

## 二、关键参数（`TileConfig`，都在 `Default` 里）

| 字段 | 默认 | 说明 |
|---|---|---|
| `z` | 18 | 墨卡托尺度 ≈0.597 m/px；北京地面 ≈0.458 m/px（乘 cos φ） |
| `max_tiles` | 64 | 8×8；超了**直接报错**，不硬拼（见 `check_budget`） |
| `simplify_m` | 2.5 | 折线简化容差（米） |
| `merge_m` | 3.0 | 端点合并容差（米）：只有端点这么近才共用一个节点 |
| `road_min_luma` | 150 | 亮度下限（排除深色文字/图标） |
| `road_max_luma` | 255 | 亮度上限（默认不设） |
| `max_chroma` | 18 | RGB 极差上限（排除水系蓝、绿地绿、彩色图标） |
| `contrast_min` | 8 | 比「整幅图背景均值」亮多少才算路 |
| `min_polyline_px` | 12 | 骨架折线最短像素长度（噪点过滤） |

## 三、瓦片编号推导（`baidu_tile_xy`）

1. `baidu_mercator`：BD-09 经纬度 → 球面墨卡托米，x 向东、y 向北，原点在经纬度 0：
   `mx = lon × SHIFT/180`，`my = ln(tan((90+lat)·π/360)) × 180/π × SHIFT/180`，`SHIFT = 20037726.37`。
2. 平移到 `[0, 2×SHIFT]`：`px = mx + SHIFT`；瓦片行号**向南**增大 → `py = SHIFT − my`。
3. 一张瓦片覆盖 `span = 2×SHIFT / 2^z` 米 → `x = ⌊px/span⌋`、`y = ⌊py/span⌋`。

天安门（BD-09 39.9087, 116.3975）：`mx = 12 957 451.418`、`my = 4 852 736.471`、
`span(z18) = 152.8757` → **tile (215830, 99328)**；z17 正好折半 → (107915, 49664)。
测试 `baidu_tiles_match_the_documented_derivation` 逐项核对，并检查「该瓦片四角确实包住这个点」
「向东 x 增、向北 y 减」「尺度 0.597 / 0.458 m/px」。

## 四、为什么用「整幅图均值」而不是绝对阈值

百度 `pl` 底图里路面近白、背景是浅灰，两者亮度差只有 10–20；瓦片的明暗还随地区/样式浮动。
所以判定是「比**整幅图的亮度均值**亮 `contrast_min` 以上 + 彩度低」，再夹在
`[road_min_luma, road_max_luma]` 里 —— 背景占绝大多数像素，它的均值就是可靠的自适应基准。
先做过「9×9 局部对比」，实测会**漏掉路口内部**（那一圈全是路面，局部均值也高），已放弃。

## 五、已知局限（都是这条模式的固有代价）

1. **文字遮挡**（POI 名、路名、小区名）：笔画会切断路面 → 骨架断线。开运算只能去掉细笔画，
   粗体路名仍会断路。缓解：`min_polyline_px` 调大过滤碎线，或换 z 级别。
2. **双层高架 / 立交**：栅格图上投影重叠，矢量化后变成"一个十字"或"一团"，无法还原上下层。
   走出来的轨迹会在路口抄近道。
3. **隧道**：底图上没有路面像素（被山体/建筑覆盖），隧道段直接缺失，路由只能绕行。
4. **瓦片接缝**：相邻瓦片的路面抗锯齿差异会让接缝处出现 1–2px 错位，形成小折角；
   靠 `merge_m`（3m）+ Douglas-Peucker（2.5m）吸收，跨接缝的长直路仍可能有轻微抖动。
5. **颜色规则依赖百度样式**：`styles=pl&scaler=1` 一旦改版（比如换成深色底图），
   阈值要重新标定；`TileConfig` 全部可配就是为这个留的口子。
6. **不连通/认不出**：整片空地、水域、纯色图会得到 `road_ratio < 0.4%` → 直接报错
   「认不出道路像素」，不会产出空图骗人。
7. **坐标系**：节点直接放在 BD-09（`align_applied = true`），所以 `align_network` 会自己跳过；
   局部仿射在校区尺度误差亚米级，跨城市不适用。

## 六、失败时的退路

每条失败都会明说原因，并点出下一步：

| 失败 | 文案要点 |
|---|---|
| 瓦片 403/401 | 「瓦片被拒绝（HTTP 403）…可换网络或稍后重试；也可以先切回『真实道路』，或到路网页人工导入 OSM 路网」 |
| 超过 `max_tiles` | 「范围需要 N 张瓦片（a×b），超过上限 64。请缩小半径，或把缩放级别从 z18 降一档」 |
| 认不出道路 | 「整张图只识别出 x% 的道路像素…可换 z 级别重试，或退回『真实道路』/人工导入 OSM」 |
| 折线为空/不连通 | 「骨架里没有可用的折线…可把 min_polyline_px 调小后重试」 |
| 提交时没有缓存 | 「瓦片路网不可用（…）。请到跑步页点『生成瓦片路网』，或切回『真实道路 / 经典』模式；人工导入 OSM 路网也仍然可用」 |

**人工导入 OSM 这条路完全没动**：`Tiles` 只是多了一个数据来源，切换模式即可回到旧链路。

## 七、测试（全部离线，不联网、不落任何抓到的瓦片）

| 用例 | 验证什么 |
|---|---|
| `baidu_tiles_match_the_documented_derivation` | 瓦片编号 vs 手算、z 折半、瓦片包住点、方向、尺度 |
| `cross_image_is_classified_thinned_and_traced` | 合成「白十字 + 灰底」：灰底不被误判、路口判为路、骨架 ≈1px（150–260 像素 vs 原始 ~1295）、追出 1–2 条折线、简化容差 0 原样返回 |
| `parallel_roads_stay_two_components` | 两条平行路 → 图里 2 个连通分量（不允许被 `merge_m` 并成一条） |
| `cache_round_trip_and_version_guard` | 写缓存 → 读回统计/图；版本不符**拒绝**而不是读出半对的图 |
| `too_many_tiles_is_refused_before_fetching` | 超限在抓图前被拒（文案含「超过上限」「张瓦片」）；`max_tiles=0` 也明确报错 |
| `route_mode_strings_and_capabilities_round_trip`（扩展） | `tiles`/`tile`/`瓦片` 都认；`requires_network`/`needs_school_route` 语义 |

## 八、接线（三处，都很薄）

1. `RouteMode::Tiles`（`generate_road.rs`）：`as_str = "tiles"`、`from_str` 认三个别名、
   `requires_network = true`、`needs_school_route = true`。
2. `flow.rs` 的 `route_mode` 分支：读 `tile_road::load(default_cache_path())` →
   `align_network`（自动跳过）→ `apply_fences` → `build_road`，与 Road 同一条路。
3. 跑步页：多一个「瓦片矢量化（实验）」选项 + 「生成瓦片路网」按钮
   （`App::generate_tile_network`）→ 后台抓图建图 → 结果放进 `self.network`，
   状态栏汇报「瓦片 N 张 / 道路像素占比 x% / 折线 M 条 / 节点 K / 耗时 ms」。

## 九、联网冒烟结论（2026-10-27，一次性，≤16 张瓦片）

在**天安门（BD-09 39.9087, 116.3975）z18、半径 200m、4×4=16 张**的范围内实测：

| 观测 | 结果 |
|---|---|
| HTTP | 200（不是 403，也不是网络不通） |
| 图内容 | **256×256、197 字节的单色空白 PNG**（纯色 256×256 压完就是这个量级） |
| 坐标形式 | 绝对编号 (215830, 99328) 与百度常用的「偏移」形式 (84758, −31744) **返回完全相同的 197 字节** |
| `udt` / `Referer` | 加上 `&udt=…` 与 `Referer: map.baidu.com` 后**仍是同一张空白图** |
| 道路像素占比 | 空白图无从谈起；`fetch_one` 的「<512 字节 = 占位图」守卫直接整体失败，不会产出垃圾图 |

**结论：数据获取当前不可用（不是坐标错）。** 服务端对上述所有组合都回空白瓦片，
说明 `styles=pl&scaler=1` 这个匿名取图口子已经不能直接用了（要么改成需要 `ak`/签名，
要么只对带特定 Referer/登录态的请求出图）。**这不影响瓦片编号公式的正确性**——编号只由
公开公式与内部一致性（z 折半、四角包点、方向）验证，与能否取到图是两件事。

下一步（需要另开任务，属于「抓一次真实浏览器请求看参数」的逆向工作）：

1. 用真实浏览器在 map.baidu.com 打开同一位置，抓 1 个瓦片请求的完整 URL（含 `ak`/`udt`/签名），
   对比现在的 URL 差哪些参数；
2. 或者换源：`https://tile.openstreetmap.org/{z}/{x}/{y}.png`（免鉴权、WGS84 墨卡托），
   改动集中在 `fetch_one` 的 URL 与 `pixel_to_bd09` 的投影（Web 墨卡托 → WGS84 → BD-09）。

**阈值标定暂时做不了**（没有一张真实瓦片）；`TileConfig` 的 5 个阈值保持默认，
文档 §五.5 的重标定流程等有真实图之后再走。

### 顺带修掉的真缺陷

冒烟还暴露了一个逻辑漏洞并已修：**空白/纯色瓦片会被整片判成路面**。
纯白图里每个像素都比「整幅图均值」亮、彩度又是 0，`classify_roads` 会给 ≈100% 的路面占比，
而原来只有 `MIN_ROAD_RATIO`（下限）一道闸 → 会一路走到建图、产出一张荒唐的「全是路」的图。
现在加了 `MAX_ROAD_RATIO = 0.55` 与 `road_ratio_is_plausible()`，两端都卡，
并有用例 `blank_or_uniform_tiles_are_rejected_by_the_ratio_guard` 覆盖（全白→拒、纯灰→拒、正常十字→放行）。

## 十、瓦片源可配置（t15）

### 默认换成 OSM

`TileConfig.url_template` 默认 `https://tile.openstreetmap.org/{z}/{x}/{y}.png`（免鉴权，WGS84）。
百度匿名口子已封（见第九节），所以默认不再指向百度 —— **管线一行没改**，只换了「从哪儿取图」：

- `TileProjection::WebMercator`（默认）：标准 XYZ，`web_mercator_tile_xy`；产物是 **WGS84**，
  建图时 `align_applied = false`，由 `align_network` 做一次 WGS84→BD-09。
- `TileProjection::Baidu`：保留原有 `baidu_tile_xy` 与 BD-09 局部仿射（`align_applied = true`）。
  只有粘到可用的百度 URL 时才用；`Config::tile_config()` 看到模板含 `bdimg.com` 会自动切过去。

### 怎么粘自定义源

1. 浏览器打开底图 → F12 → Network → 过滤 `tile`，点一张瓦片 → 复制完整 URL；
2. 把 `x`/`y`/`z` 的数值替换成占位符，例如
   `https://maponline0.bdimg.com/tile/?qt=tile&x={x}&y={y}&z={z}&styles=pl&scaler=1&udt=20240801`；
3. 填进跑步页的「URL 模板」（或 `config.json` 的 `tile_url_template`）；
4. 需要的请求头写进 `config.json` 的 `tile_headers`，每行一条，例如
   `Referer: https://map.baidu.com/`；
5. 主机不在白名单时，把主机名加进 `tile_allowed_hosts`（逗号分隔，`foo.com` 同时覆盖 `*.foo.com`）。

### 源白名单与限额（F1）

`url_template` 是**用户可输入**的，不做校验等于给自己留了个 SSRF 入口。现在的约束：

| 检查 | 规则 |
|---|---|
| 协议 | 只允许 `https://` 开头的绝对地址；`http:` / `file:` / `ftp:` / 相对路径一律拒绝并明确报错（**不静默回退默认源**） |
| 主机白名单 | 内置 `tile.openstreetmap.org` / `bdimg.com`（含 `*.bdimg.com`）/ `tile.openstreetmap.de`，其余需写进 `allowed_hosts`；比对大小写不敏感、去端口、去 userinfo |
| 私网/回环 | `localhost`、`127.*`、`10.*`、`172.16–31.*`、`192.168.*`、`169.254.*`、`0.0.0.0`、`[::1]`、`*.local`、`*.internal` 一律拒绝（即使加进白名单） |
| 单张读取 | `take(2 MiB + 1)`；超限按坏图处理 |
| 单张解码 | `ImageReader::with_format(Png).limits(4096×4096 / 32 MiB)`（与 `ui/captcha.rs` 同款） |
| 拼接总量 | 保留 `MAX_PIXELS`（8 Mi 像素）与 `max_tiles`（默认 64） |
| 占位图 | `<512 字节` 视为占位图；`<0.4%` 与 `>55%` 道路像素占比两端卡死 |

用例：`tile_source_whitelist_rejects_unsafe_urls`（http/file/相对路径、私网字面量、非白名单公网主机、
显式加白名单后放行）。

### 同批的另外三条（F2/F3/F5）

- **F2 解码失败不静默吞**：`blit` 返回原因，`fetch_tiles` 计数 `skipped`，`Stats::summary()`
  会显示「（N 张解码失败已跳过）」，UI 日志另有一条告警 —— 缺块处骨架会断线，必须让人看见。
- **F3 缓存归属**：缓存里带 `owner`（学校 unid）与 `bbox`；`load_checked(path, owner, bbox)`
  在「学校不符」或「范围不重叠」时拒绝并提示重新生成。用例 `cache_is_scoped_to_school_and_area`。
- **F5 可取消**：`fetch_tiles`/`generate` 接受 `cancel: &dyn Fn() -> Result<(), String>`，
  每张瓦片之间检查一次；UI 取消后不再继续发请求。

### 换源后阈值要重新标定

不同底图的「背景 vs 路面」亮度差不一样，换源后若道路像素占比异常（太小/太大），按这个顺序调：

1. 先看 `Stats::summary()` 里的占比：正常校园底图约 **2%–25%**；
2. 占比过小（<1%）→ 降 `contrast_min`（默认 8）到 4–6，让「比背景略亮」的路面也能入选；
3. 占比过大（>55%）→ 升 `contrast_min` 或降 `road_max_luma`，把接近纯白的背景排除；
4. 彩色底图（水系/绿地占比高）→ 收紧 `max_chroma`（默认 18 → 12）；
5. 每次只改一个参数，改完重新生成看折线条数与连通分量是否合理（碎线多说明阈值太松，
   骨架断得多说明太紧）。
