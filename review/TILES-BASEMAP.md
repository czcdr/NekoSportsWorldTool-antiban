# 瓦片底图实施报告（t2）

目标：地图从「白底矢量」升级为「在线瓦片底图（本地渲染）」，拖拽/缩放即时跟手。
面向 Android；默认**关闭**，首次启动绝不联网。

## 一、改动面

| 文件 | 性质 | 内容 |
| --- | --- | --- |
| `src/ui/tiles.rs` | **新增**（~1600 行，含 15 条单测） | 瓦片源枚举、slippy 编号、视口→包围盒、四角贴图、磁盘缓存 + LRU、后台抓取线程、纹理上传、工具条 |
| `src/ui/map.rs` | 改 | `MapState::interact` 支持 `zoom_delta()`（双指）+ 滚轮 + 以指针为不动点缩放；新增 `Gestures` 手势状态机（pinch/drag/tap）；`MapState` 增 `gesture` 字段 |
| `src/track/tile_road.rs` | 改（+39 行，纯抽取） | 抽出 `pub fn fetch_tile_bytes`（原 `fetch_one` 的抓取语义：白名单校验/2 MiB/占位图判定/重试 1 次）与 `pub fn decode_tile_rgba`（4096² 解码限额）；`fetch_one`/`blit` 行为不变 |
| `src/ui/mod.rs` | 改（+1 行） | `pub mod tiles;`（紧接 `pub mod map;`） |
| `src/ui/osm.rs` | 改（+21 行） | `OsmPage::tiles`、工具条、底图绘制（矢量图之下）、失败提示进 `osm_page.status` |
| `src/ui/run.rs` | 改（+35 行，属本任务） | `RunPage::tiles`、两处（预览图 / 基础图）工具条 + 底图绘制、失败提示进 `status` |
| `src/api/model.rs` | 改（+20 行，仅配置字段） | `basemap: String`（默认 `off`）、`map_cache_limit_mb: u32`（默认 100） |

禁改面：`src/api/**` 只动了 `model.rs` 的两条配置字段；`src/crypto/**`、`src/track/generate_road.rs`、提交载荷（wire/DTO）零改动。

## 二、设计落实（逐条）

1. **坐标系**：内部统一 BD-09。高德 = GCJ-02 网格（BD→GCJ 复用 `track::wire::bd09_to_gcj02`），
   OSM = WGS84 网格（BD→WGS 复用 `api::school_roads::to_wgs`，即 `wgs84_to_bd09` 的迭代反解，
   与手绘路网写 `.osm` 同口径，全仓只留一份）。
2. **贴图对齐**：`MapState::to_screen` 一行未改。瓦片四角各自过它，取轴对齐矩形（`tile_screen_rect`）。
3. **缩放级别**：`zoom_float = log2(scale × 2·SHIFT·cosφ / 256)`，`zoom_level` 四舍五入并截断到 18；
   被截断时 `tile_scale_factor = 2^(zoom_float − 18) ∈ (1, 8]`，用 z18 瓦片按比例放大（最近邻采样）。
4. **瓦片源**：高德 `https://webrd0{host}.is.autonavi.com/appmaptile?...&x={x}&y={y}&z={z}`，
   `{host}` 轮换 `1..4`（`AMAP_HOSTS`）；`allowed_hosts = ["autonavi.com"]` 显式进白名单。
   OSM 用 `tile_road::DEFAULT_URL_TEMPLATE`。抓取走 `tile_road::fetch_tile_bytes`
   （`validate_tile_url` + 2 MiB + 4096² + `is_placeholder_tile` + 失败重试 1 次），**没有第二套 fetch**。
5. **缓存**：`data_dir()/map_cache/{amap|osm}/{z}/{x}/{y}.png`，总上限 100 MiB（`map_cache_limit_mb`），
   超限按 mtime LRU 淘汰（`plan_eviction` / `evict_to`）；只缓存成功且非占位图；写入用唯一临时名 + rename。
6. **渲染**：`src/ui/tiles.rs`，后台线程抓取/解码，主线程只上传纹理，每帧上限 `MAX_UPLOADS_PER_FRAME = 4`；
   派发上限 8 张/帧；失败重试 1 次后写状态栏，底图任何失败都只 return（不 `?`、不 panic），矢量图照画。
7. **开关**：`Config::basemap`（`off|amap|osm`，默认 `off`），地图上方工具条切换并 `save_config` 落盘；
   工具条同时显示「缓存 X MB / 100 MB，N 张」与「清理缓存」。
8. **手机**：`interact` 同时读 `ui.input(|i| i.zoom_delta())`（双指）与 `raw_scroll_delta`（滚轮），
   缩放以指针为不动点；手势状态机保证「捏合 > 拖拽」优先级，且按下→抬起全程未移动才算 tap
   （tap 不会被误判为拖拽）。

## 三、数值举证

### 3.1 对齐误差（线性投影 vs 真 Mercator）

测法（`linear_projection_deviation_is_subpixel`）：真 Mercator 曲线与「两端点连线」的最大偏差，
按该 z 的瓦片像素计。只量曲率——全局比例常数不产生相对错位（底图与矢量共用同一条线性映射，
瓦片逐块按自身经纬度边界贴图）。

| 场景 | 偏差 |
| --- | --- |
| 单张瓦片 z18（256 px 屏） | **0.0005 px** |
| 手机竖屏一侧 z18（1920 px ≈ 880 m 地面） | **0.0278 px** |
| 平板 z18（3000 px） | **0.0678 px** |
| 3 km 校园在 z15（800 px 屏） | **0.0386 px** |
| 3 km 硬塞进 z18（需 ≈6566 px 宽屏，真实设备放不下） | **0.3230 px** |

结论：所有真实屏幕配置下 < 0.2 px；极端组合仍亚像素。

另一条（`tile_rect_is_built_from_projected_corners`）：瓦片矩形 = 四个角点过 `to_screen` 的外接框
（误差 < 1e-3 px），每个角都贴在矩形边上（< 0.05 px）。矩形尺寸与 `256 px × tile_scale_factor`
的相对残差 ~0.9%，来源是 BD↔WGS/GCJ 换算的经纬交叉项；后果仅为相邻瓦片重叠 ≤3 px（上下边取同一组
角点的 min/max，只会重叠、不会留白），肉眼不可见。

已知的既有行为（不在本任务改动范围）：`map.rs` 横向用 `MET_PER_DEG_LNG = 86600`（≈北纬 39° 真值
85 400 m/deg 的 1.4% 偏大），底图与矢量共用它，故二者对齐一致，只是影像横向被同比例拉伸。

### 3.2 z 反算与超限放大

`zoom_matches_meters_per_pixel`：对 z = 1..18、lat ∈ {22.5, 31.2, 39.9, 45} 逐级构造「1 瓦片像素
= 1 屏幕像素」的 scale（用 `tile_road::meters_per_pixel` 反推），`zoom_level` 必须还原出该 z
（误差 < 1e-3），且 `tile_scale_factor ≈ 1`；scale 继续放大时 z 停在 18、倍率 > 1（超限放大）。

### 3.3 缓存上限

- `lru_plan_deletes_oldest_first`：4000 B / 限 2500 → 删最旧两张；不超限一张不删；同 mtime 按路径稳定排序。
- `cache_evicts_files_down_to_limit`：真写 5 张 × 1000 B → `evict_to(3000)` 后总量 ≤ 3000、只剩 3 张，
  最新的在、最旧的没了。

### 3.4 默认 off 不发起网络请求（举证）

`default_off_never_requests_network`：
- `Config::default().basemap == "off"`，`BasemapSource::parse("")` / `parse("bogus")` 均回落到 `Off`；
- `BasemapSource::Off.url_template() == None`（拼不出 URL）、`cache_key() == None`（不落盘）；
- `plan_requests(Off, 10×10 的 span, …) == 空`，`tile_url(Off, …) == Err`；
- 对照：同一 span 用 `Osm` 时 `plan_requests` 返回 64 条（上限 64），证明「空」不是 span 空；
- 新建 `TileLayer` 在 `Off` 下 `jobs_sent() == 0`、`texture_count() == 0`。

代码侧第二道闸：`TileLayer::draw` 在 `src == Off` 时直接 `return 0`（在任何 `ensure_worker`/channel 之前），
后台线程**懒创建**（`ensure_worker` 只在真的有计划任务时调用）。所以「off」= 没有 URL、没有 job、
没有线程、没有 socket。

### 3.5 主机白名单 / SSRF

`amap_url_rotates_hosts_and_passes_whitelist`：host 轮换 webrd01..04 且 `validate_tile_url` 通过；
把 `autonavi.com` 从白名单拿掉即被拒；`http://127.0.0.1`、`https://192.168.1.9`、`https://localhost`
一律被拒。缓存路径只由枚举 + 整数（z/x/y）拼成，不接受外部字符串，无路径注入面。

## 四、测试与命令

```
cargo test --release --lib
→ test result: ok. 384 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out

cargo check --all-targets     → 通过
rustfmt --check src/ui/tiles.rs src/ui/map.rs → 干净
```

（过程中出现过 2 条 `api::flow::fake_run_tests` 失败，属 t4 的 anchor_offset 进行中改动，
与本任务无关；t4 修复后全量已全绿。）

本任务单测（`ui::tiles::tests`，15 条，全绿）：
`bd09_wgs84_display_path_is_the_shared_inverse`、`bd09_to_gcj02_reuses_wire_implementation`、
`tile_bounds_are_contiguous_and_one_tile_tall`、`zoom_matches_meters_per_pixel`、
`viewport_span_covers_center_tile`、`tile_rect_is_built_from_projected_corners`、
`linear_projection_deviation_is_subpixel`、`amap_url_rotates_hosts_and_passes_whitelist`、
`default_off_never_requests_network`、`cache_layout_is_data_dir_map_cache`、
`lru_plan_deletes_oldest_first`、`cache_evicts_files_down_to_limit`、
`placeholder_tiles_are_not_cached`、`pinch_gesture_state_machine`、
`zoom_about_keeps_anchor_fixed_and_clamps`。

## 五、已知限制 / 不在范围

1. **未做「可选静态地图 / 高德 Web 服务 Key」**：任务书未列，且内置 Key 属于凭据面，留给后续任务；
   需要时 `basemap=amap` 的静态图接口应另做开关，Key 留空整项禁用。
2. **瓦片请求不走代理池**：底图是公开 CDN，走出口代理既浪费也不稳定；`tile_road` 的代理路径
   （`generate`）不受影响。
3. **真机触摸未验**：pinch 由 `ui.input(|i| i.zoom_delta())` 驱动，逻辑层已由状态机单测覆盖；
   Android 实机双指手势需实机点验（桌面端 ctrl+滚轮走同一路径）。
4. **横向 1.4% 拉伸**（§3.1 末段）是 `map.rs` 的既有常数，改它会影响全部矢量图层，按任务书不动。
5. 高德域名可达性依赖网络环境（部分网络对 autonavi.com 有拦截），失败时按设计只提示、不挡矢量图。
