# 最终全量复查（t14）

- 对象：`E:\rea\upstream`，HEAD = `7d0c4fa`，工作树干净；对比基线 `v0.6.0` = `378660a`
- 范围：`v0.6.0..HEAD` 全部 19 个提交（t9 路由模式 / t13 防异常 A1–A13 / t10 瓦片路网 / t12 身份恢复 / README / 10003 诊断 / 0.6.1）
- 方式：**只读**（本文件是唯一新增），本机复跑构建矩阵 + 代码审查，未联网、未抓包
- 结论：**needs_revision**（1 条 medium：瓦片解码/读取无上限；3 条 low + 1 条 low 回归；其余无问题）

---

## 一、构建 / 测试矩阵（本机复跑）

| 命令 | 退出码 | 输出摘要 |
|---|---|---|
| `cargo check --all-targets --offline` | **0** | 无 error |
| `cargo test --release --lib --offline` | **0** | `test result: ok. 334 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 8.33s` |
| `cargo test -p route-planner --offline` | **0** | `16 passed` + `7 passed` + `0 passed`（三个 target，全 0 failed） |
| `cargo check --lib --no-default-features --offline` | **0** | 78 条 `dead_code` 警告（GUI 关掉后大量 UI 代码未用，属预期，无 error） |
| `cargo fmt --check` | **0** | 无输出 |
| `git diff --check` | **0** | 无输出 |
| `cargo clippy --lib --offline` | **0** | `nekosportsworldtool (lib) generated 20 warnings`；`route-planner (lib) generated 5 warnings` |

### clippy 20 条逐条归属（`git blame` 定到提交，判断是否来自本轮）

| # | file:line | 警告 | 引入提交 | 是否本轮 |
|---|---|---|---|---|
| 1 | `src/api/fingerprint.rs:352` | `audit` never used | `cb6ba3d` | 否 |
| 2 | `src/safety.rs:309` | field `now_ms` never read | `728ef35`(A13) | **是** |
| 3 | `src/api/model.rs:529` | doc list indent | `7d0c4fa`(t12) | **是** |
| 4 | `src/api/model.rs:530` | doc list indent | `7d0c4fa`(t12) | **是** |
| 5 | `src/api/school_roads.rs:151` | too many arguments | `f416472` | 否 |
| 6 | `src/api/school_roads.rs:292` | sort_by_key | `f416472` | 否 |
| 7 | `src/crypto/sign.rs:90` | sort_by_key | `9fa7657` | 否 |
| 8 | `src/safety.rs:268` | create without truncate | `693321f` | 否 |
| 9 | `src/track/generate_road.rs:573` | map_or simplify | `4064331`(A6) | **是** |
| 10 | `src/track/generate_road.rs:591` | clamp-like | `e437bf1` | 否 |
| 11 | `src/track/generate_road.rs:502` | late init | `e437bf1` | 否 |
| 12 | `src/track/geom.rs:148` | excessive precision | `e437bf1` | 否 |
| 13 | `src/track/tile_road.rs:590` | while let | `a528efc`(t10) | **是** |
| 14 | `src/track/tile_road.rs:663` | map_or simplify | `a528efc`(t10) | **是** |
| 15 | `src/track/wire.rs:226` | unnecessary cast | `693321f` | 否 |
| 16 | `src/update.rs:512` | for loop | `b63c77d` | 否 |
| 17 | `src/ui/about.rs:104` | collapsible if | `b63c77d` | 否 |
| 18 | `src/ui/jobs.rs:761` | sort_by_key | `9fa7657` | 否 |
| 19 | `src/ui/map.rs:113` | unnecessary cast | `cdf772e` | 否 |
| 20 | `src/ui/run.rs:605` | collapsible if | `f416472` | 否 |

**本轮新增 6 条，全部是风格 / 死字段，无一条涉及正确性**：#2 是 A13 留下的未读字段（`RunReservation.now_ms`），#3/#4 是 t12 的文档缩进，#9 是 A6 的 `map_or`，#13/#14 是 t10 的写法建议。`route-planner` 的 5 条为既有。

---

## 二、逐项提交复查（`v0.6.0..HEAD`）

| 提交 | 内容 | 结论 | 证据 |
|---|---|---|---|
| `ab81d28` | t9：Manual 也画地图；切模式标脏+清旧预览 | **无问题** | 提前返回改为 `== Legacy`；`on_route_mode_changed` 只在 `route_mode != prev_route` 时触发（跑一次，不是每帧）；4 条新测试含「真实点击切模式后 preview/preview_key 已清」 |
| `953f2f6` | A1：iOS 身份提交前拒绝 | **无问题**（见三-7 的误伤评估） | 拒绝点在 `submit_record` 最前、`egress_guard`/写回执之前（`submit.rs:88-98`）；默认身份有一次性迁移（`model.rs:437-450`）把「未被改动过的旧 iOS 默认」自动转 Android，绝大多数老用户不受影响 |
| `a4ef64d` | A3：步差统计由窗内采样推导 | **无问题** | 只改 `stepsPerTenSec.*Diff`，不动 `stepsNum`/汇总；新增用例断言由采样点反算 |
| `5e40ba6` | A5：Road 海拔 20–260 抽样 | **无问题** | 初值与 OU 目标同源；只影响 `bdA`/`totalAscent` |
| `5d820d9` | A7：指定时刻补真秒/毫秒 | **引入 1 个 low 回归**（F4） | 见下 |
| `c0e561f` | A4 围栏只读校验 + A8 核验警告 | **无问题** | 只写 `warnings`/日志，不拦提交；`RunOutcome::summary()` 追加，UI/CLI 共用；测试断言「轨迹点没少、距离没变」 |
| `4064331` | A6：围栏内保留 GPS 抖动 | **无问题** | 逐点+逐线段判内、失败折半重试、最终回退中心线；测试用笔直走廊断言纬度标准差 >0.3m |
| `728ef35` | A13：两次提交最小间隔 180s | **无问题** | 校验在每日配额之后、计数写入之前（`safety.rs:411-431`）；失败由 `Drop` 还原 `last_submit_ms`（`:337`）；`0`=关闭；5 组测试覆盖「不足拒绝/失败不占间隔/0 关闭」 |
| `8e3893c` `76d20e2` | 10003 变体复问 + 0.6.1 | **无问题** | 登录侧改动，未触提交链；版本号 0.6.0→0.6.1 与 manifest 一致 |
| `a528efc` `c2962fb` | t10：瓦片矢量化 + 空白瓦片占比上限 | **引入 1 个 medium**（F1）+ 2 low（F2/F3/F5） | 见三 |
| `7d0c4fa` | t12：身份三层备份 + 自动落盘 | **无问题** | 备份只写 city/anchor/device_id；`config.json` 仍走 `write_secret_file`（0600 + 临时文件改名的原子写）；空城市不覆盖已有备份；`device_id` 只在本地两个文件里 |
| `abe5e3b` `1fabd1d` `c02bafd` `1bc8595` | README / 审计文档 / fmt | **无问题** | 纯文档与格式化；README 注明上游来源与「测试版、勿用主账号」 |

---

## 三、安全面专项

### 1. 新增网络出口（瓦片抓取）—— **SSRF 不可达，但读取无上限**

- **URL 不是配置来的**：`tile_road.rs:310-312` 硬编码 `https://maponline{0..3}.bdimg.com/tile/?qt=tile&x={x}&y={y}&z={z}&styles=pl&scaler=1`，`x/y` 是 `i64`、`z` 是 `u8`，全部由 bbox 数学推导。**没有 scheme/host 白名单的需求，因为没有可注入的 URL 字段** —— 这条设计是对的。
- **重定向**：`http.rs:207` `redirect::Policy::limited(10)`，未校验跳转后的 host。初始 host 固定，只有 MITM/恶意代理才能把请求拐走；且请求不带 token/密码（只带 UA）。**low，仅记录**（F7）。
- **超时**：走 `Agent` 的统一超时（`http.rs:203-205`）。**总字节上限**：**没有** —— `fetch_one` 用 `read_to_end`（`tile_road.rs:325`），只有 `into_string` 才受 `MAX_BODY=512MiB` 约束。**瓦片数/像素上限真生效**：`check_budget` 在发请求**之前**跑（`:281`，`:227-243`），`max_tiles=64`、`MAX_PIXELS=8Mi`；`fetch_tiles` 的 `vec![255; w*h*3]` 也在此预算内。
- **结论**：SSRF **不可达**；真正的缺口是「单张瓦片可读入最多 512MB 且解码无上限」→ 见 F1。

### 2. 图片解码 —— **总像素有上限，单张解码没有**（F1）

- 拼接后的总量由 `MAX_PIXELS` 兜住（`tile_road.rs:241-243`，且在建 `rgb` 之前）。
- 但 `blit`（`:347-350`）用 `image::load_from_memory(png)` —— **无 `Limits`**。这与 `src/ui/captcha.rs:282-284` 已经修好的写法不一致（那边用 `ImageReader::with_limits`）。一张「小文件、大声明」的 PNG 可以在后台线程里直接吃掉几 GB。
- 这是一次**同类问题的漏修**，不是新设计缺陷：中等风险、修法现成。

### 3. 文件写入 —— **无路径穿越；并发写不截断**

- 缓存路径 `platform::data_dir().join(CACHE_FILE)`（`tile_road.rs:766-767`），文件名是常量，bbox/参数**不参与**路径拼接 → 无穿越。
- 写入走 `write_secret_file`（`model.rs:250-263`）：`{name}.{pid}.{tid}.{seq}.tmp` + 同目录改名 → 并发写不会互相截断（最后改名者胜出）；unix 下 0600。
- 唯一问题：**缓存是全局单文件**，`flow.rs:560-578` 加载时**不校验学校/bbox** → 见 F3。

### 4. 凭据与日志 —— **未发现泄露**

- 扫描本轮新增的所有 `log(...)` / `eprintln!` / `status =` 行，匹配 `token|password|secret|lat|lon|坐标|anchor`：**0 命中**。
- 瓦片统计日志只有「张数 / 占比 / 折线数 / 节点数 / 耗时」（`tile_road.rs:873-882`），不含 bbox 坐标。
- 身份恢复日志只打印**城市名**（`model.rs:378-380`），不含锚点经纬度。
- `NetworkLoad.scope` 里确实带了 `session.token`（`jobs.rs:342-347`），但只用于「结果是否属于当前会话」的比较（`msgs.rs:186-194`），不落盘、不打日志；`NetworkLoad` 也没有 `Debug` 派生。**可接受**，但这是一份凭据副本待在 UI 结构体里，建议后续换成 session epoch（不是本轮必修）。
- `redact_*` 覆盖情况：本轮新增日志没有输出服务端响应体，不需要脱敏；既有 `redact_json` 路径未变。

### 5. 并发与阻塞 —— **无 UI 阻塞，但取消不打断抓取**（F5）

- 瓦片生成在 `spawn_session_job` 后台线程（`jobs.rs:352`），UI 线程只收 `net_tx` 消息 → **不阻塞**。
- UI 侧 `net_rx.try_recv()`（`msgs.rs:182`），非阻塞。
- **取消后仍会跑完**：`control.check()` 只在 `generate` **前后**（`jobs.rs:360,368`），`fetch_tiles` 内部没有取消点 → 点「取消」后最多还会抓 64 张（含重试、180ms 限速），约 10–60s，并把缓存写完；结果本身会因 `request_id` 不匹配被丢弃（`msgs.rs:183-185`），**不会串到界面上**。
- panic：本轮新增生产代码 **0 个** `unwrap/expect/panic!`（新增命中全部在 `#[cfg(test)]` 的缓存用例里）。`spawn_session_job` 仍无 `catch_unwind` —— 这是既有遗留（见第五节），不是本轮引入。

### 6. Android 侧 —— **无桌面专属 API，权限未变**

- `tile_road.rs` 只用 `std::thread/fs` + `crate::http::Agent` + `image`，无 `rfd`/窗口/文件选择器等桌面 API；`platform::data_dir()` 双端可用。
- `android/AndroidManifest.xml` 在 `v0.6.0..HEAD` 只改了 `versionCode 60001→60101`、`versionName 0.6.0→0.6.1`，**权限集零变化**（仍只有 INTERNET + REQUEST_INSTALL_PACKAGES）。
- 新增 UI（第 4 个模式选项 + 生成按钮）在 `src/ui/run.rs`，无 `cfg` 门控需求（两端口径一致）；`spawn_session_job` 在 Android 上同样可用。
- 未在 Android 上构建/运行（本机无工具链）—— 这条按代码审查结论登记。

### 7. 回归面 —— **A1/A13 不会误伤正常用户；身份备份不越界**

- **A1**：老用户若从未动过默认身份，`load_identity_for_platform` 的一次性迁移会把它转成 Android（`model.rs:437-450`）→ 不受影响；只有**主动选过 iOS**的用户会被拒，提示明确指向「设备页切 Android」。这是设计取舍，不是 bug。唯一可改进：报错时给一个「一键切回 Android」的按钮（现在要用户自己找）。
- **A13**：默认 180s，且在 `daily_*` 之后判定 —— 「今天已经跑过」会先报，不会把「每日上限」误报成「等 3 分钟」；失败不占间隔（`Drop` 还原），失败后的立即重试不受影响。**正常单次使用零影响**。
- **t12 备份**：`identity_backup` 只含 city / anchor_lat / anchor_lon / device_id，**不含 token、不含 android_id、不含 MAC**；写进 `config.json`（0600）与 `identity.json` 同目录，权限一致。没有把 device_id 写进日志或提交报文以外的地方。

---

## 四、结论

**verdict: needs_revision**（只卡 F1；其余四条建议本轮一并处理，都是几行）

| id | severity | file:line | 问题 | 最小修法 |
|---|---|---|---|---|
| **F1** | **medium** | `src/track/tile_road.rs:325`、`:347-350` | 瓦片**读取无单张上限**（`read_to_end` 可到 512MB）、**解码无 `Limits`**（`image::load_from_memory`）。拼接总量有 `MAX_PIXELS` 兜底，但单张就能打穿内存。与 `captcha.rs:282-284` 已修的同类问题不一致。 | `fetch_one` 里 `take(TILE_MAX_BYTES /*2MiB*/ + 1)` 并在超限时报错；`blit` 改用 `ImageReader::with_format(...).limits(Limits{ max_image_width/height: 4096, max_alloc: 32MiB })`，失败按 F2 处理。 |
| **F2** | low | `tile_road.rs:348-350`、`:325-326` | 解码失败被 `let Ok(...) else { return }` **静默吞掉** → 该瓦片留成纯白，可能被分类成路面；另外 `read_to_end` 的 `?` 直接冒泡，**跳过了「重试一次」**。 | `blit` 返回 `Result`，失败让 `fetch_tiles` 报「第 N 张瓦片无法解码」；把读失败也纳入重试。 |
| **F3** | low | `src/api/flow.rs:560-578`、`tile_road.rs:766` | 瓦片缓存是**全局单文件**，提交时 `load` 不校验学校/bbox。换校区后用旧缓存：有围栏时会被 `apply_fences` 裁空而报错（安全），**围栏为空时则直接按旧校区路线跑**。 | 缓存里已有 `bbox`；`flow` 加载后比对当前 `anchor`/围栏中心（容差 ~100m），不符就报「请重新生成瓦片路网」。 |
| **F4** | low（本轮回归） | `src/api/flow.rs:241-246,333` | A7 的 `second_precision_start` 在「指定时刻=当前分钟」时，能把 `start_ms` 推到 `now` **之后最多 ~60s**（原来是 `+0..4s`）→ `startTime` 落在未来。 | 加一个上限：`(base + draw%60000).min(now_ms)`。 |
| **F5** | low | `src/ui/jobs.rs:360-368` | 取消不打断抓取：`fetch_tiles` 内部无取消点，取消后仍会抓完 64 张并写缓存（结果会被 `request_id` 丢弃，不串界面）。 | 把 `&Control` 传进 `fetch_tiles`，每张瓦片前 `check()`；取消时跳过 `save_cache`。 |

> F7（重定向不校验 host）**不单列**：初始 host 固定、请求不带凭据，风险低于上面五条，记在这里即可。

---

## 五、本轮已知遗留（不算 findings，不阻塞）

1. `spawn_job` / `spawn_session_job` 仍无 `catch_unwind`（`ui/mod.rs`）—— 后台线程 panic 时 UI 只会在下次交互时才发现（R6 的收尾只补了「发送失败」这一半）。
2. A9（10s 窗严格对齐）与 A10（满圈 1000.0）、A11（`dtr` 语义）、A12（窗 id 口径）仍**需真机抓包**才能定性，代码里已按现有样本保留原状。
3. 真机 / 真账号未验证：本机无 Android 工具链、无账号、无未降级出口；瓦片抓取的真实底图质量、iOS 身份拒绝后的实际体验都未在真机确认。
4. t10 的瓦片管线是实验特性：文字遮挡/隧道/高架处会断线（文档与 UI 都已标注），`MAX_ROAD_RATIO` 只能挡住「整片空白」，挡不住「部分瓦片被占位图污染」。
5. `RunReservation.now_ms` 是 A13 留下的未读字段（clippy #2），属清理项。
6. 默认路线模式仍是 **Legacy（不避让围栏/建筑）**，A4 只把它变成「可见的警告」，没有改选线 —— 这是审计当时的取舍，仍未变。

---

## 六、工作树状态（**不属于本审查范围**，但下一个人必须知道）

本报告的全部结论钉在 **HEAD `7d0c4fa`**。审查期间（19:52:51，晚于本报告生成）工作树里 `src/track/tile_road.rs` 出现了一份**未提交**的重写（+282/−55，非本审查产出），内容是：新增 `TileProjection`（WebMercator/Baidu）、把瓦片源改成**可配置的 `url_template`**、默认源换成 `https://tile.openstreetmap.org/{z}/{x}/{y}.png`（理由：百度匿名口子只回 197 字节占位图）。

它会让第三节的结论**部分失效**，合入前必须补两项：

- **URL 模板没有任何 scheme / host 校验**（`url_template` 只校验非空与 `{x}{y}{z}` 占位符，见工作树版 `tile_road.rs:110-112,473-492`）→ 上一版「SSRF 不可达」的前提是「URL 硬编码」，现在不成立了。至少要求 `https://` 前缀，并对 loopback/私网/`file:` 直接拒绝（否则配置文件被改坏或被诱导粘贴模板时，本工具会去打本机/内网端口）。
- **解码仍未加 `Limits`**（工作树版 `tile_road.rs:555` 还是 `image::load_from_memory`）→ F1 在下一提交里依然存在，别以为换了源就自动变安全。

行号对不上属正常：本报告的行号按 HEAD 标注，工作树版本行号已整体位移。
