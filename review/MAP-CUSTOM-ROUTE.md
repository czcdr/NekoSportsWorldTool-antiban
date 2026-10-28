# MAP-CUSTOM-ROUTE 独立验收（t5 初审 → t9 复审 → t10 收口 / gate-review）

- 验收人：`gate-review`（只读验收；不采信实现者自述）
- worktree：`E:\rea\upstream`（branch `antiban-review`，HEAD `fcbef9e` = 标签 `v0.7.0` 的树）
- 基线：`git rev-parse v0.7.0^{commit}` = `fcbef9e63ac12548de52f79dfc4b73a58481ffd3`；基线干净副本 `E:\rea\tmp\t4-base`（`git archive HEAD`）用于 fmt / clippy 对照
- 三轮都未修改任何源码、未 `git commit`；产物只有本文件；命令日志在仓库外（`E:\rea\t5logs\`、`E:\rea\t9logs\`、`E:\rea\t10logs\`）
- 轮次：§1 = t5 初审（needs_revision）；§2 = t9 复审（needs_revision，F1）；§3 = **t10 收口（pass）**；§4/§5 保留历史 findings 与遗留

---

## 0. 最终结论

**verdict: pass**

| 六条门禁 | 状态 | 实测来源 |
|----------|------|----------|
| `cargo fmt --all -- --check` | **exit 0**（无输出） | t10 复跑 |
| `cargo check --all-targets` | exit 0 | t9 实测（已在格式化后的树上） |
| `cargo test --release --lib` | **exit 0；running 396 → 395 passed / 0 failed / 1 ignored** | t9 + t10 两次复跑结果一致 |
| `cargo test -p route-planner` | exit 0；22 + 7 passed | t9 实测 |
| `cargo check --lib --no-default-features` | exit 0 | t9 实测 |
| `cargo clippy --all-targets` | exit 0；新增仅 style 级（与基线逐条比对） | t9 实测 |

| 上轮 findings | 最终状态 |
|---------------|----------|
| F1 fmt 红 | **已关闭** —— t9 的 F1 是**时序假阳性**（我方 fmt 检查 22:36:40 结束，captain 的统一格式化 22:36:41 落盘，见 §3） |
| F2 默认锚点偏移 50 m | 已关闭（t9 确认） |
| F3 close 换 seed 预览≠提交 | 已关闭（t9 确认） |
| F4 字段顺序断言 | 已关闭（t9 确认） |
| F5 拖动地图模式误拖顶点 | 已关闭（t9 确认） |

风控面（三轮一致）：`src/crypto/**`、`src/api/submit.rs`、`src/api/obs.rs`、`src/api/records.rs`、`src/track/wire.rs` 相对 `v0.7.0` **零 diff**；点位请求 `request_body` 字段与顺序未变（`sportType` 首、`runAreaId` 末）；清计划仍是唯一清理点，在飞提交不受影响。

---

## 3. 复审收口（t10）

### 3.1 我这一轮亲自跑的两条命令

```
$ cargo fmt --all -- --check
FMT_EXIT=0 ELAPSED=0.6s
(no output = 无 diff)

$ cargo test --release --lib
TEST_EXIT=0 ELAPSED=46.8s
running 396 tests
test api::gt4::tests::live_gt4_probe ... ignored, live network probe（会真实请求极验，需显式给 captcha_id）
test result: ok. 395 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 19.72s
```

两条均符合收口条件：fmt **exit 0**；测试 **395 passed / 0 failed / 1 ignored**，ignored 就是既有的 `api::gt4::tests::live_gt4_probe`（真实联网探针）。

### 3.2 F1 为什么是时序假阳性（用时间戳自证，不是采信自述）

| 时刻 | 事件 | 证据 |
|------|------|------|
| 22:36:40 | 我 t9 的 `cargo fmt --all -- --check` 跑完并写日志（exit 1，12 处 diff） | `E:\rea\t9logs\g1_fmt_check.log` 的 Creation/LastWrite = 22:36:40 |
| **22:36:41** | **`cargo fmt --all` 落盘，恰好重写了 t9 列出的那 7 个文件** | `load.rs`、`fake_run_tests.rs`、`model.rs`、`points.rs`、`generate_road.rs`、`generator.rs`、`jobs.rs` 的 mtime 全部 = 22:36:41（与 t9 F1 的文件清单**逐一对应**；`src/ui/mod.rs` 22:31:48、`src/ui/run.rs` 22:33:09 未被这次格式化触碰，因为它们本来就已经是格式化的） |
| 22:37:07 | 我 t9 的 `cargo check --all-targets` 才开始 | `g2_check_all_targets.log` 的 Creation = 22:37:07 |
| 22:38:46 | t9 的 clippy 才开始 | `g6_clippy_current.log` 的 Creation = 22:38:46 |

即：t9 的 fmt 门禁**在格式化落盘前一秒**读完了工作树，而同一轮的 check / test / clippy 已经在格式化后的树上跑（所以它们当时就是绿的）。这与「同一批 hunk 行号与内容一字未变」的观察一致 —— 不是我读错文件，是树在复审过程中被改动了。t9 把 F1 判为「未关闭」在当时是正确判断（那一刻树确实是红的），只是结论被随后发生的格式化覆盖。

### 3.3 收口后的最终状态

- F1 关闭（`cargo fmt --all -- --check` exit 0，且 t10 复跑测试仍 395 passed / 0 failed / 1 ignored，说明格式化没有改变任何语义）。
- F2–F5 于 t9 逐条关闭（证据见 §2 的对应小节）。
- 六条门禁全部通过（状态一览见 §0）。
- 上轮 t9 的 findings 表原样保留在 §4 作为历史记录，不作删除。

---

## 1. 初审（t5）结论摘要（历史）

上轮判定 **needs_revision**，findings：

| id | severity | 问题 | 最终状态 |
|----|----------|------|----------|
| F1 | low | `cargo fmt --all -- --check` exit 1（12 处 / 8 文件；基线 exit 0） | 已关闭（t10 复跑 exit 0；t9 的重现属时序假阳性） |
| F2 | medium | 默认 `anchor_offset_m = 50.0` → 升级后默认上传坐标整体偏移 40–60 m，与「不动代跑提交载荷」冲突 | 已关闭（t9） |
| F3 | low | 换 seed（换一版/改距离配速）后预览点位与提交点位分叉 | 已关闭（t9） |
| F4 | low | `points.rs` 用 `sorted_keys` 比较，证明不了字段**顺序** | 已关闭（t9） |
| F5 | low | `draw_track` 拖动地图模式下从顶点起手会误拖顶点 | 已关闭（t9） |

t5 门禁记录（供对照）：`check --all-targets` 0；`test --release --lib` 389 passed / 0 failed / 1 ignored；`test -p route-planner` 22 + 7；`check --lib --no-default-features` 0；`clippy` 两侧 0；`fmt --check` 1（基线 0）。
t5 已确认的事实（后续复核仍成立）：负数 id 全链路 i64、瓦片底图默认 off 零请求 + 100 MiB LRU + z≤18 + 每帧 ≤4 上传、手绘正数 id + 保存前 `load_osm` 预验、GPS 散布 0 值与旧版逐位一致、提交后清计划唯一清理点。

---

## 2. 复审（t9）：F1–F5 逐条核查（历史）

### F1 —— t9 当时未关闭（后由 t10 确认关闭）

t9 实测 `cargo fmt --all -- --check` **exit 1**，12 处 diff，7 个文件：

| # | 位置 | rustfmt 要求（当时） |
|---|------|----------------------|
| 1 | `crates/route-planner/src/load.rs:562` | `let p_neg = parse_osm(...)` 单行超宽 → 断成两行 |
| 2 | `crates/route-planner/src/load.rs:572` | `assert_eq!(p_neg.buildings.len(), …)` 应压成单行 |
| 3 | `src/api/fake_run_tests.rs:225` | `drive_with_config(platform, r#"…"#)` 应压成单行 |
| 4 | `src/api/fake_run_tests.rs:530` | 两个 `assert_eq!(…to_bits(), …)` 应展开 |
| 5 | `src/api/fake_run_tests.rs:538` | `plan_anchor(exact, 0.0, 7).latitude.to_bits()` 应折行 |
| 6 | `src/api/model.rs:1793` | `default_anchor_offset_is_the_exact_anchor_bit_for_bit` 里两条 `assert_eq!` 应展开 |
| 7 | `src/api/model.rs:1807` | `assert_eq!(…resolved_gps_spread_m(), 0.0, …)` 应展开 |
| 8 | `src/api/model.rs:1816` | `assert_eq!(…resolved_anchor_offset_m(), 0.0, …)` 应展开 |
| 9 | `src/api/points.rs:308` | `request_body_key_order_is_stable` 里 `order.first()/last()` 应展开 |
| 10 | `src/track/generate_road.rs:1258` | `a["locations"][0].as_object().unwrap().keys().cloned()…` 应折行 |
| 11 | `src/track/generator.rs:446` | `assert!(mean_delta > 1.0, "…")` 应展开 |
| 12 | `src/ui/jobs.rs:133` | `let plan_seed = …map(\|p\| p.seed).unwrap_or(0);` 应压成单行 |

当时我给出的判定依据（仍成立于那一刻）：基线副本同命令 exit 0、仓库无 `rustfmt.toml`、12 处全是宽度问题、且上轮同批 hunk 行号与内容一字未变。**补充事实（t10 才拿到）**：文件 mtime = 22:36:41，比我那次检查晚 1 秒 —— 树在复审期间被格式化，故上述 hunk 随后全部消失，`cargo fmt --all -- --check` 现为 exit 0。

### F2 —— 已关闭

| 检查点 | 证据 |
|--------|------|
| `default_anchor_offset_m()` 是否 0.0 | `src/api/model.rs:237-243`：`fn default_anchor_offset_m() -> f64 { … 0.0 }`，注释写明「0 = 精确锚点，与 v0.7.0 一致」 |
| 默认配置 → `resolved_anchor_offset_m()` == 0 | `src/api/model.rs:1780`、`:1782`；`:1848` 序列化断言 `json["anchor_offset_m"] == 0.0` |
| 默认值喂 `plan_anchor` 是否**逐位**等于精确锚点 | `src/api/model.rs:1790-1808`：`default_anchor_offset_is_the_exact_anchor_bit_for_bit`，`seed ∈ {0, 7, 42, u64::MAX}` 下 `latitude/longitude/accuracy` 三者 `to_bits()` 全等（实跑通过） |
| 显式 50 才偏移 | `src/api/points.rs:225,245-248`（50 m → 实际位移落 `39.5..=60.5` m）；`src/api/model.rs:1839-1840`；`api::flow::fake_run_tests::anchor_offset_adds_no_request_field_and_keeps_the_device_anchor` 显式传 50 |
| 上限/兜底 | `src/api/model.rs:1826-1837`：5000 → 200、-1 → 0、`∞` → 0、0 → 0 |
| UI 文案 | `src/ui/run.rs:585`：「默认 0（关闭）；开启后每份计划偏移约 40–60 m，可能让点位更贴近围栏边缘」 |

### F3 —— 已关闭

1. **预览与提交读同一个偏移值（UI 值为唯一真值）**：`src/ui/run.rs:351-358` `effective_anchor_offset_m()`（有限值 `clamp(0,200)`，非有限 → 0）；预览侧 `src/ui/jobs.rs:140/241`；提交侧 `src/ui/run.rs:1336` 写入 config，`flow.rs` 读 `resolved_anchor_offset_m()`。测试 `preview_and_submit_read_the_same_anchor_offset`（含越界/NaN 兜底）。
2. **换 seed 确实重取点位**：`src/ui/run.rs:332-341` `if fixed_seed.is_none() && effective_anchor_offset_m() > 0.0 { anchor_offset_dirty = true }`；消费点 `:813-815` 松手后 `refresh_points()`（`plan_seed` 取当前 plan，`ensure_plan` 已在同帧 `:744` 跑过）。测试 `new_plan_marks_points_for_refetch_only_when_the_offset_is_on`。
3. **顺序不变量未破**：`on_route_mode_changed` → `refresh_points` → `ensure_plan` → 松手消费 dirty（727 → 731 → 744 → 813）。

残余（非 finding）：若在「seed 刚变、dirty 未消费」的一两帧内点提交，预览仍是旧锚点点位；提交本身自洽，且默认 offset=0 时不会发生。

### F4 —— 已关闭

- `src/api/points.rs:197-202`：`keys_in_order` 按 `keys()` **原始迭代顺序**（`serde_json` 已开 `preserve_order`）。
- `:293-341` `request_body_key_order_is_stable`：完整 8 键序列 `["sportType","longitude","latitude","sign","uuid","selectedUnid","runec","runAreaId"]` + `first()/last()` + 无区域时去掉末位 + 50 m 偏移后顺序不变。
- 生产代码 `request_body`（`:42-63`）未变，仍与 v0.7.0 逐键同序。

### F5 —— 已关闭

- 代码 `src/ui/draw_track.rs:619-626`：`drag = match (state.mode, hit) { (DrawMode::Tap, Some(index)) => Some(index), _ => None };` —— Pan 模式恒平移；Tap 模式命中顶点才拖顶点；长按/右键删点不受影响。
- 测试（无头 egui 直接跑真实 `draw_track_body`）：`pan_mode_pans_view_even_when_the_gesture_starts_on_a_vertex`（顶点逐值不变、`view.offset` >10 px、`drag.is_none()`）、`tap_mode_adds_on_tap_and_drags_vertex_without_adding`（tap 4→5 点、拖顶点改坐标不增点）。

### 2.1 t9 的 clippy 基线比对（定性）

按「文件 :: 告警文案」归一后比对，两侧 exit 0，**无 correctness / perf 级新增**：
新增仅 style 级 —— `src/ui/tiles.rs` 的 `from_*` self 约定、`.is_multiple_of()`、`map_or`、测试里的 `vec!`，以及测试构造代码的 `field assignment outside of initializer`（`src/api/model.rs` +1、`src/ui/run.rs` +2）；
另**修掉**基线两条：`src/ui/map.rs` 的冗余 `f32` cast、`src/track/generate_road.rs` 的 `net` 重定义。

---

## 4. 历史 findings 表（t9 原文，保留不删）

| id | severity | 位置 | problem | requiredFix | 最终 |
|----|----------|------|---------|-------------|------|
| **F1** | low | `crates/route-planner/src/load.rs:562`、`:572`；`src/api/fake_run_tests.rs:225`、`:530`、`:538`；`src/api/model.rs:1793`、`:1807`、`:1816`；`src/api/points.rs:308`；`src/track/generate_road.rs:1258`；`src/track/generator.rs:446`；`src/ui/jobs.rs:133` | `cargo fmt --all -- --check` **exit 1**：12 处折行/并行建议未应用（7 个文件）。任务书要求该命令 exit 0。全部为宽度问题，无语义差异。 | 在 `E:\rea\upstream` 执行一次 `cargo fmt --all`，随后 `cargo fmt --all -- --check` 确认 0，再跑 `cargo test --release --lib` 确认仍 395 passed / 1 ignored。 | **已关闭**：captain 于 22:36:41 统一格式化；t10 复跑 fmt exit 0、测试 395/0/1 |

F2/F3/F4/F5 的历史 findings 与关闭证据见 §1、§2（t5 提出的原表在 §1，t9 的逐条核查在 §2）。

---

## 5. 已知遗留（单列，不计入 findings）

1. **真机触摸未验证**：pinch / 长按 600 ms 删点 / Pan 模式「一律平移」都只在无头 egui（状态机 + 真实 `draw_track_body`）里跑过；Android 上 egui 对多点触摸的合成、与系统长按/滚动的冲突仍需真机手测。
2. **高德域名可达性未验证**：`webrd01..04.is.autonavi.com` 只做离线白名单/模板校验；不可达时只写状态栏，不影响矢量绘制。
3. **静态地图 Key 需自备**：高德 Web 服务静态地图未实现（不内置凭据）。

---

## 6. 未覆盖 / 诚实声明

- 我**没有**真机、没有真账号：所有网络面（瓦片、点位接口）均为离线单测 + 代码读，凡涉及「服务端返回什么」的推断都在正文标注。
- t10 只复跑了 fmt 与 `test --release --lib` 两条（按收口任务要求）；`check --all-targets`、`-p route-planner`、`--no-default-features`、`clippy` 采用 t9 在同一棵（已格式化）树上的实测结果 —— t9 的 check 起跑于 22:37:07、clippy 于 22:38:46，均在 22:36:41 格式化之后，故与当前树等价。
- F3 的「残余窗口」是静态推演，未做时序注入测试。
- 共享 worktree 里 `src/update.rs`、`.github/workflows/release.yml`、`tools/` 是他人并发改动（发版流程），不碰 `src/crypto/**`、`wire.rs`、`submit.rs`，不影响本报告结论；发版前需各自责任人确认。

---

## 7. 复核命令清单（可原样重跑）

```powershell
cd E:\rea\upstream
cargo fmt --all -- --check                                  # 0（t10 实测）
cargo check --all-targets                                   # 0（t9，格式化后）
cargo test --release --lib                                  # 396 → 395 passed / 0 failed / 1 ignored（t10 复跑 19.72s）
cargo test -p route-planner                                 # 22 + 7 passed（t9）
cargo check --lib --no-default-features                     # 0（t9）
cargo clippy --all-targets                                  # 0（新增仅 style 级；t9）
cargo test --release --lib anchor_offset                    # 8  （F2/F3/F4 证据）
cargo test --release --lib ui::draw_track                   # 11 （F5 证据）
cargo test --release --lib request_body_key_order           # 1  （F4 证据）
cargo test --release --lib plan                             # 11 （F3 + 清计划/在飞证据）
# 风控面
git diff v0.7.0 -- src/crypto | Measure-Object -Line         # 0
git diff v0.7.0 --stat -- src/api/submit.rs src/api/obs.rs src/api/records.rs src/track/wire.rs   # 空
# 基线对照
cd E:\rea\tmp\t4-base; cargo fmt --all -- --check            # 0（基线干净）；cargo clippy --all-targets  # 0
```
