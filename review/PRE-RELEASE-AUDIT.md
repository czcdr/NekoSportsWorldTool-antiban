# 发版前检查审计清单（每版必跑）

产出本清单的原因：`t5`（gate-review）只覆盖"实现是否正确、安全面有没有新洞"，**不覆盖**依赖变更、产物审计与桌面停发的连带影响。这两层必须由 captain 自己跑，且不采信成员自述。

任何一项未过 → 不发版。

---

## 0. 前置门槛

| 检查 | 命令 | 通过标准 |
|---|---|---|
| 团队任务全部终态 | `agent_teams_status` | 6 个任务无 pending/running |
| 工作树只含预期文件 | `git status --short` | 与预期清单逐条对得上；出现陌生文件先问清楚 |
| 没有遗留调试代码 | `git diff --check` | 无输出（无行尾空白/冲突标记） |

## 1. 构建矩阵（全部 exit 0，数字要贴出来）

```bash
cargo test --release --lib          # 主 crate 全量
cargo test -p route-planner         # 路网 crate（含本轮 +4 测试）
cargo check --all-targets           # 含 bin/CLI
cargo check --lib --no-default-features   # 无 GUI 路径
cargo fmt --check
cargo clippy --all-targets 2>&1 | tail -40
```

判据：
- 测试 0 failed；`ignored` 只允许 `api::gt4::tests::live_gt4_probe`（联网探针，需 `GT4_PROBE_CAPTCHA_ID`）。
- clippy 与上一版基线逐条对比：**新增告警必须为 0**；既有告警要能逐条说出归属。

## 2. 安全面（本轮新增能力）

| 项 | 判据 |
|---|---|
| 瓦片源 SSRF | 新主机必须走 `tile_road::validate_tile_url`；白名单显式含 `autonavi.com`；`localhost`/`127.*`/`10.*`/`172.16–31.*`/`192.168.*`/`169.254.*`/`[::1]`/`*.local` 仍被拒 |
| 缓存目录 | 只接受 `{proj}/{z}/{x}/{y}.png`；z/x/y 只能是数字（不可注入 `../`）；总量 ≤ 配置上限，超限按 mtime 淘汰 |
| 并发写 | 同一瓦片并发下载不会互相截断（先写临时文件再 rename） |
| 内存 | 每帧纹理上传有上限；切源/缩放释放旧纹理；单张 2 MiB / 4096² / 32 MiB 解码上限仍生效 |
| 坐标 | BD→GCJ→瓦片像素→回 BD 误差 < 3 m；BD→WGS84 往返 < 1e-6 度；底图与矢量同点屏幕偏差 < 2 px @ z18 |
| 手机交互 | pinch 改动 scale；tap 加点不被拖拽吞掉；长按删点不误删 |
| 日志 | 新增日志不含 token/密码/坐标；高德 Key（若配）不进日志 |

## 3. 风控面（最重要，必须双重举证）

```bash
git diff v0.7.0..HEAD -- src/api src/crypto src/track/wire.rs
```

| 判据 | 说明 |
|---|---|
| `src/crypto/**` 零改动 | 加密/验签层不得被本轮任何改动碰到 |
| `src/api/**` 只允许两类改动 | ① `model.rs` 的 config 字段追加；② 锚点偏移所必需的点位/围栏请求取值处（`flow.rs`）——逐 hunk 都要能解释，字段名/字段顺序/重试/节流逻辑不得变 |
| 提交载荷字段零变化 | 用 `git diff` + 字段清单双重举证：wire/DTO 序列化字段一个不多一个不少 |
| 锚点偏移只改数值 | 只替换点位请求里的 `latitude/longitude` 数值，**不新增请求字段**；同计划复用同一偏移 |
| 提交后清计划 | 只发生在提交结束之后；提交进行中的 plan 不被清；固定 seed 不被清 |
| 新 config 字段不进提交 | `gps_spread_m` / `anchor_offset_m` / `basemap` / `map_cache_limit_mb` 只影响本地生成与显示 |

## 4. 依赖与供应链（t5 不查，captain 查）

| 项 | 判据 |
|---|---|
| `Cargo.toml` / `Cargo.lock` | 本轮无新增 crate；若有，必须能说出为什么非它不可 |
| `vendor/` | 未变（winit 等 vendored 依赖不动） |
| `.github/workflows/` | 只允许 `release.yml` 去掉 tag 触发的改动；`apk.yml` 签名与产物名不变 |

## 5. 产物审计（发版后立刻做）

| 项 | 期望 |
|---|---|
| Release 资产（Android，`apk.yml`） | `NekoSportsWorldTool-android-arm64.apk`、`.idsig`、`SHA256SUMS-android.txt` |
| Release 资产（桌面，`release.yml`） | `NekoSportsWorldTool-win-x64.zip`、`NekoSportsWorldTool-linux-x64.tar.gz`、`NekoSportsWorldTool-linux-aarch64.tar.gz`、`SHA256SUMS-desktop.txt`。**macOS 不再出现**；Linux 是 CLI-only |
| 优先级 | Android 是主交付；桌面两份只是附带，不做真机验证，只确认能构建、能解压、版本号正确 |
| `versionCode` / `versionName` | `80001` / `0.8.0-android.1`（CI 日志逐字核对） |
| 签名指纹 | 与 0.7.0 **同一把**（`2299d0e1…cbdbefe3`），否则老用户无法覆盖安装 |
| 校验和 | 下载 APK 实测 sha256 = `SHA256SUMS-android.txt` 中值；桌面同理对 `SHA256SUMS-desktop.txt` |
| `/releases/latest` | 指向本版 |
| 桌面端提示 | **macOS** 老版本检查更新时显示「自 0.8.0 起桌面端只发布 Windows x64 与 Linux，macOS 已停发…」；Windows / Linux 走正常更新路径，不出现该提示 |

## 6. 发布说明必须如实写的未验证项

- 真机触摸（pinch / tap / 长按）未在真机验证，只在无头状态机层面测过
- 高德瓦片域名在受限网络下是否可达未验证（与 Overpass 一样可能被挡）
- 高德静态地图需用户自备 Web 服务 Key，留空则整项禁用
- 桌面端自 0.8.0 起只发布 Windows x64 与 Linux（CLI-only），macOS 停发，最后可用版本 v0.7.0

## 7. 已知缺陷（本轮未修，需单开）

- **`fake_run_tests` 在 23:00–06:00 之间会红（不是缺陷，是设计后果）**：`reserve_run` 同时校验「记录开始时间」与「当前时刻」，两者任一落在深夜即拒绝（`safety.rs:371`，注释写明“只改其中一个字段仍能绕过去”）。夹具把记录钉在 12:00，但当前时刻是真实墙钟 → 夜间必然被拦。本轮在夹具的 config.json 里 `safety.enabled=false` 是**测试夹具的写法**，不是绕过生产护栏；护栏本身仍有 `safety::tests` 覆盖。
- 因此**深夜（23:00–06:00）无法代跑**，这是有意的风控选择；确需深夜提交只能 `config.json` 里 `safety.enabled=false`（UI 报错里已提示）。不要为“方便”去改这条护栏。
- 提交幂等 WAL（Pending 未闭合拒绝重提）未做：网络超时后重试仍可能产生两条记录。
- 停走窗口 / 10–30 m 野点 / `radius` 右偏 / 必经点 3–8 m 自然落位未做（参考实现 ScienseRunner 有，本轮范围内不引入）。
- `is_placeholder_tile(len < 512)` 对极高压缩率的合法单色瓦片可能误判为占位图（不缓存、不重试 → 该瓦片永久空白），未用真实瓦片验证。
- 两个 workflow 并发创建同一 Release 的 422 竞态未实测（v0.7.0 已存在此形态，非本次引入；一方失败另一方仍会创建）。
- 偏移 40–60 m 后 `geoFencesJson`（精确锚点查得的围栏）是否被判越界，需真实服务端。
