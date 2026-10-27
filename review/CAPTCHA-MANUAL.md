# 手动过码（GT4 滑块人工拖动）接入说明

分支：`fix/captcha-manual`（工作树 `E:\rea\wt-captcha`）
基线：`35298d2`（外部审计 F-01…F-12 之后的合并目标）
本分支提交：`94de1f0`（前置，GT4 拆「取图 / 交卷」）→ `feat(captcha): 手动过码接进登录流程（界面选择 + 挑战图弹窗）`

本回合**不发布新版本**，只把功能做出来 + 单测 + 本文档；合并回 `antiban-review` 由 t4 做。

---

## 1. 一句话

登录页多了一个「过码方式」开关：选**手动过码**时，后台登录线程取到 GT4 挑战图后把它交给界面，
弹窗显示背景 + 缺口切片，用户拖动 / 滑杆 / ±1px 调好位置点提交；提交后走的是和自动模式**同一条**
`/verify` → `geevalidate`（只发一次）→ `/login` 链。界面线程全程不被网络阻塞。

---

## 2. 改了哪些文件（本任务 inScope，全部改动都在这里）

| 文件 | 改了什么 | 为什么 |
|---|---|---|
| `src/api/model.rs` | `Config` 新增 `captcha_mode: String`（`#[serde(default = "default_captcha_mode")]`，默认 `"auto"`）+ `Default` 实现 + `Config::resolved_captcha_mode()`；`load_config` 里把认不出的值归一成 `"auto"` | 过码方式要能在重启后保持；`load_config` 现在走 `deserialize_lenient` 的**单键探针**，字段没有 `serde(default)` 会在每次加载时被丢掉 → 设置永远存不住，还会每次打印「字段类型不符」 |
| `src/ui/mod.rs` | `pub mod captcha;`；`App` 新增 `captcha_tx` / `captcha_rx` / `captcha` / `captcha_ctx`；`from_state` 建通道并 `ctx.clone()`；`update()` 先 `poll_captcha()`，最后在 `draw_shell` + 普通弹窗 + 更新窗口之后画 `draw_captcha_window(ctx)`；顶栏登录区加「过码方式」（桌面 + compact 两条布局），切换即落盘 | 把已存在但没被引用（死代码）的 `ui/captcha.rs` 接进实际界面；弹窗必须压在最上层、且不管停在哪个标签页都出现 |
| `src/ui/jobs.rs` | `do_login` 读 `config.resolved_captcha_mode()`；手动模式下**在 move 闭包内就地**构造 `solver_for(ctx, tx)` 调 `login_with(..., LoginOptions{captcha: Manual, solver}, ...)`；自动模式仍然调原来的 `login(...)`。取消时日志写 `[login] 已取消验证码，本次登录结束`，不写 `× 登录失败` | solver 借的是登录线程的 `&mut`，不能提前造好放在 UI 线程；自动模式的请求序列必须一个字节都不动 |
| `src/ui/msgs.rs` | `LOGIN_DONE` 失败分支改用 `captcha::failure_status(message)`（取消 → 只显示「已取消验证码」，不打「登录失败：」前缀）；分支末尾 `self.captcha.abandon()` | 取消是用户自己的选择，不是失败；登录线程收工后不能留一个永远等不到答案的弹窗 |
| `src/ui/captcha.rs` | 接入编译（补 `use super::App`）；把「等答案」抽成 `await_answer()` 便于测三条通道；新增 `CaptchaUi::abandon()`、`captcha_mode_selector()`、`mode_status()`、`is_cancelled()`、`failure_status()`、`App::set_captcha_mode()`；`1.0` → `1.0_f32` 消除 future-incompat 警告；新增 11 个测试 | 该文件此前未被 `mod.rs` 引用，属死代码；抽函数是为了能对 Submit / Retry / 断开三条通道做单测（`Gt4Challenge` 的私有字段在 `gt4.rs` 外构造不出来） |
| `review/CAPTCHA-MANUAL.md` | 本文件 | 交付说明 + 复核清单 |

## 3. **没有**碰的文件（复核时按这个表核对）

| 文件 | 状态 |
|---|---|
| `src/api/gt4.rs` | 未改动（`load_challenge` / `submit_challenge` / `solve_gt4_manual` 保持原样） |
| `src/api/login.rs` | 未改动（`CaptchaMode` / `LoginOptions` / `login_with` 保持原样） |
| `src/api/client.rs`、`src/safety.rs`、`src/ui/run.rs` | 未改动 |
| `android/`、`Cargo.toml`、`crates/`、`vendor/` | 未改动 |
| 其他 `src/ui/*`（ai/data/device/records/osm/…） | 未改动 |

核对命令（应当**没有任何输出**）：

```powershell
cd E:\rea\wt-captcha
git diff 35298d2 --name-only -- src/api/gt4.rs src/api/login.rs src/api/client.rs src/safety.rs src/ui/run.rs android Cargo.toml
```

---

## 4. 数据流 / 线程模型

```
UI 线程 (egui)                              登录线程 (spawn_job)
  │ 点「登录」(captcha_mode=manual)
  │───────────────────────────────────────▶ login_with(Manual, solver)
  │                                           checkGeeUse（问不出来就一个 GT4 请求都不发）
  │                                           load_challenge  /load + 下两张 PNG
  │                                           solve_gt4_manual:
  │                                             solver.solve(challenge)  ← 阻塞
  │  ◀── tx.send(CaptchaRequest{ bg,slice,尺寸,ypos,max_offset,responder }) ──┤
  │  poll_captcha(): 存 pending，状态栏「滑块验证：把方块拖进缺口…」
  │  draw_captcha_window(): 前景窗口，拖动/滑杆/±1px/提交/换一张/取消
  │  ──── responder.send(Slide|Retry|Cancel) ──────────────────────────────▶ 解阻塞
  │                                           Slide → submit_challenge（/verify，setLeft+passtime）
  │                                           Retry → 换下一张（最多 3 张）
  │                                           Cancel → Gt4Error::Cancelled（一个额外请求都不发）
  │                                           geevalidate（**只发一次**）→ login
  │  ◀──────────────────────── LOGIN_DONE{ok,message|session} ──────────────┤
  │ msgs: ok=false 且 message 以「已取消验证码」开头 → 状态栏只显示这句
  │       + captcha.abandon()（清掉可能残留的弹窗）
```

- 等待期间每 200ms `request_repaint()` 一次，手机前台也能看见拖动效果；5 分钟没人动 → 按取消收尾。
- `responder` 断开（界面消失 / `abandon()` / App 退出）→ 后台立刻按取消返回，不会挂满 5 分钟。
- 后台在被阻塞期间，UI 线程只做 `ctx.run` 一帧，没有任何网络请求。

---

## 5. 关键决策

1. **落盘与归一（`Config::resolved_captcha_mode`）**
   - 缺字段 / `""` / 拼错（`"hand2"`）→ `auto`。认不出就当手动会让登录卡在「等界面拖动」，
     比自动识别错一次严重得多。
   - `load_config` 解析完立刻把值归一，避免坏值被原样写回磁盘、每次启动都要重新解析。
   - 保存失败**必须说清楚**：`save_config` 在本地加密不可用时会拒绝写明文口令并返回 `Err`，
     这时状态栏是「过码方式已切换为手动过码，但设置保存失败：…（本次改动仅当前会话有效）」，
     不会出现「已保存」却重启回 auto（`mode_status()` + 单测）。
2. **取消 ≠ 失败**：状态栏只显示「已取消验证码」；日志写 `[login] 已取消验证码，本次登录结束`，
   不写 `× 登录失败`；`/verify` 之前取消 ⇒ 没有任何额外请求；手动模式判错也不进 10 分钟冷却
   （这条在 `login.rs` 里已定稿，本次只是把界面接到它上面）。
3. **自动模式零改动**：`do_login` 的 `Auto` 分支仍是 `crate::api::login::login(...)`，
   `solve_gt4` 调用点、`geevalidate` 单发、重试策略都没动。
4. **不做桌面专属 hack**：只用 `egui::Context` / `egui::Window(Order::Foreground)` / `selectable_value` /
   `Slider`，Android 走同一套代码（没有 `#[cfg(not(target_os = "android"))]`）。
5. **不新增 panic 路径**：改动里没有新增 `unwrap` / `expect` / `panic!` / 索引越界；
   日志与状态栏不含 token / 密码 / `lotNumber` / `captchaOutput` / `passToken`
   （状态栏只出现「已提交滑块位置 Npx」「已取消验证码」这类文案）。

---

## 6. 如何手测

前置：`cargo run`（或桌面 release 构建）；一个**能过极验**的账号 + 出口。手动过码不改变风控面，
但**不要**为了测这个功能反复连点登录。

### A. 落盘 / 老配置（不需要账号）

1. 启动 → 顶栏「过码方式：自动过码 / 手动过码」。
2. 点「手动过码」→ 状态栏出现「过码方式：手动过码（已保存）」。
3. 关掉程序再启动 → 开关仍是「手动过码」；`<数据目录>/config.json` 里 `"captcha_mode": "manual"`。
4. 手动把该字段改成 `"hand2"` → 重启 → 回到「自动过码」，且字段被归一成 `"auto"`。
5. 手机/窄窗口（宽度 ≤ 620）→ 顶栏「账号与登录」折叠区**最上面**就是这两个选项（不用滚动）。
   会话过期（`auth_expired`）时折叠区会自动展开，同样能改。

### B. 手动过码主链路（需要账号；日志页 `tab=6`）

1. 选「手动过码」→ 填账号密码 → 点「登录」。
2. 期望：状态栏「登录中（<账号>，手动过码）…」；日志出现 `[gee] checkGeeUse data=false`、
   `[login] 开始 GT4 滑块验证（手动过码）…`、`[gt4] 手动过码：第 1/3 张图（WxH，可移动 0..=N）,请拖动`。
3. 期望：弹出「滑块验证」窗口，背景图 + 缺口切片 + 缺口左缘细线 + 滑杆 + `◀ 1px` / `▶ 1px` + 像素数。
   - 拖动图片、拖滑杆、点 ±1px 三种方式都要能改位置；三者结果一致（同一个 `offset`）。
4. 把方块对齐缺口 → 点「提交验证」。期望日志：
   `[gt4] 手动交卷 setLeft=<n> passtime=<ms>ms`（`passtime` 在 250–9000 ms 之间被夹住），
   随后 `[geevalidate] 验证通过`、`√ 登录成功 uid=…`。
5. 点「换一张」→ 窗口内容刷新，日志出现 `[gt4] 按要求换一张` / `[gt4] 手动过码：第 2/3 张图`；
   其他状态（账号、密码、标签页）不变。
6. 点「取消」→ 弹窗立刻消失，状态栏**只**显示「已取消验证码」（不能是「登录失败：…」），
   日志里只有 `[login] 已取消验证码，本次登录结束`（没有 `× 登录失败`），
   抓包应看到：取消之后**不再有** `/verify`、`geevalidate`、`/login`。
7. 什么都不点，等 5 分钟 → 自动按取消收尾（同上文案），登录线程不会一直挂着。

### C. 自动模式回归

1. 切回「自动过码」→ 登录 → 日志应与改动前完全一致
   （`[gt4] 缺口识别 distance=… 相似度=…` → `[gt4] setLeft…` → `[geevalidate] 验证通过` → `√ 登录成功`），
   且 `setLeft` 是自动识别值而不是人工拖动值。

---

## 7. 给另一会话（复审）的复核清单

> 目的：确认「手动过码真的接上了」且「自动模式、定稿文件、其它 inScope 外的东西一个都没被带动」。

**范围**
- [ ] `git diff 35298d2 --name-only` 只出现：`src/api/model.rs`、`src/ui/captcha.rs`、`src/ui/jobs.rs`、
      `src/ui/mod.rs`、`src/ui/msgs.rs`、`review/CAPTCHA-MANUAL.md`。
- [ ] `src/api/gt4.rs`、`src/api/login.rs` 零改动（`git diff 35298d2 -- src/api/gt4.rs src/api/login.rs` 为空）。
- [ ] `android/`、`Cargo.toml` 零改动；没有 `git add -A`、没有 merge、没有新 tag、没有 push。

**接线是否真的生效**
- [ ] `ui/mod.rs` 有 `pub mod captcha;`，`App` 四个字段齐全，`from_state` 建通道 + 存 `ctx.clone()`。
- [ ] `update()` 里 `poll_captcha()` 在 `draw_shell` 之前；`draw_captcha_window(ctx)` 在 `draw_shell`
      与普通弹窗**之后**（压在它们上面），并且在 `update` 里无条件调用（不只登录页）。
- [ ] `do_login` 的 `Manual` 分支用 `login_with` + `LoginOptions{captcha: Manual, solver: Some(..)}`，
      solver 在闭包内就地构造；`Auto` 分支仍是 `login(...)`。
- [ ] `LOGIN_DONE` 失败分支走 `captcha::failure_status`，且调用了 `abandon()`。

**行为约束**
- [ ] 取消：状态栏只有「已取消验证码」；日志无 `× 登录失败`；`/verify` 之前取消不发任何请求
      （`solve_gt4_manual` 的 `Cancel` 分支直接返回 `Cancelled`，`login.rs` 里 `cancelled()` 早退）。
- [ ] `geevalidate` 仍然只发一次（`login.rs` 未改；手动分支复用同一段代码）。
- [ ] 手动模式判错不进持久冷却（`login.rs` 的 `manual` 分支，未改）。
- [ ] 状态栏 / 日志不含 token、密码、`lotNumber`、`captchaOutput`、`passToken`。
- [ ] 改动里没有新增 `unwrap` / `expect` / `panic!` / 索引越界（`git diff` 新增行自查）。
- [ ] 切过码方式用 `save_config` 的返回值决定文案；`Err` 时显示「保存失败」，不显示「已保存」。

**测试覆盖**
- [ ] `api::model::storage_tests::captcha_mode_defaults_to_auto_and_survives_a_save_load_round_trip`
      （缺字段 / 坏值 → auto；`{"captcha_mode":"manual"}` 单键探针 → Manual；save→load 往返）
- [ ] `ui::captcha::tests::{submit_path_clamps_slide_position_to_the_image_width,
      retry_path_asks_for_another_challenge, cancel_path_stops_the_login_thread,
      disappearing_interface_cancels_instead_of_blocking, unanswered_challenge_gives_up_after_patience}`
      （Submit / Retry / Cancel / 断开 / 超时 五条通道）
- [ ] `ui::captcha::tests::pending_challenge_is_dropped_when_the_login_thread_ends`（`poll_captcha` + `abandon`）
- [ ] `ui::captcha::tests::{desktop_layout_switches_captcha_mode_and_saves_it,
      compact_layout_switches_captcha_mode_and_saves_it}`（两条布局真点一下，并且真的落盘）
- [ ] `ui::msgs::login_done_tests::{cancelled_captcha_is_not_reported_as_a_login_failure,
      ordinary_login_failure_keeps_the_failure_prefix}`
- [ ] `ui::captcha::tests::save_failure_is_reported_instead_of_pretending_it_worked`

**仍需人工判断（机器测不了）**
- [ ] 真机/真账号过码一次，亲眼确认弹窗能拖动、`setLeft` 落在缺口上、`passtime` 不是常量。
      本分支未做真机联调（需要有效账号与未被降级的出口，且反复试会消耗该出口的容忍度）。

---

## 8. 本机验证记录

复用依赖产物（`CARGO_TARGET_DIR=E:\rea\upstream\target`），基线已 rebase 到 `35298d2`：

| 命令 | 结果 |
|---|---|
| `cargo check --lib --offline` | exit 0（仅剩改动前就有的 `fingerprint.rs::audit` dead_code 警告） |
| `cargo test --release --lib --offline` | `test result: ok. 300 passed; 0 failed; 1 ignored` |
| `cargo fmt --check` | exit 0 |
| `git diff --check` | 无输出（无空白错误） |
| `git diff 35298d2 --name-only` | 仅本文档 + 上述 5 个源文件 |

新增测试共 14 个：`ui::captcha::tests` 11 个（三条通道 + 超时 + 断开 + 弹窗收尾 + 文案 + 选择器 +
两条布局点击 + 保存失败文案）、`ui::msgs::login_done_tests` 2 个、`api::model::storage_tests` 1 个。

## 9. 没做 / 边界

- 没有在真机或真账号上跑完整过码链路（见 7 最后一条）。
- 没有新增任何「手动过码」的遥测/埋点，避免引入新的可观测差异。
- 弹窗里的图片按窗口宽度等比缩小（不放大），像素级对齐靠 `±1px` 与像素读数；未做缩放级别的
  子像素对齐（真机是 1:1 显示，当前实现放大率上限 1.0）。
- 手动过码仍受 `captcha_guard` 的**既有**冷却约束（那是 `login.rs` 定稿行为，本任务不改）。

---

## 10. 第 2 轮修复（t3 评审 R1–R8）

评审结论是 `needs_revision`；本轮逐条处理。除 R1/R2 的实质修复外都是小改。

| # | 结论 | 落点 |
|---|---|---|
| R1 僵尸弹窗 | **已修**：`poll_captcha` 只收「登录还活着」的请求，`if !self.login_busy { drop(request); continue; }`；丢弃即断开 responder，后台线程立刻收手 | `src/ui/captcha.rs::poll_captcha`；回归用例 `stale_challenge_is_dropped_after_the_login_ended` |
| R2 无节流 | **已修**：① 每轮 `load_challenge` 前 `sleep(200–600ms)` 随机抖动（换一张不再零间隔）；② 3 轮耗尽后记一次 `captcha_guard::record_rejection()`（单张拖歪走 `continue`，到不了这里） | `src/api/gt4.rs::solve_gt4_manual`（**只动这一个函数 + `ManualOutcome` 加一个变体**，`solve_once`/`solve_gt4`/`load_challenge`/`submit_challenge` 零改动，见 §11） |
| R3 原因被吞 | **已修**：新增 `ManualOutcome::Timeout`，映射成 `Unreliable("验证码等待超时（5 分钟未操作）")`；状态栏显示「登录失败：验证码未能完成：验证码等待超时（5 分钟未操作）」，日志不再写「已取消」；`is_cancelled` 只认「已取消验证码」 | `gt4.rs` + `captcha.rs::await_answer`；用例 `unanswered_challenge_times_out_distinctly_from_cancel`、`cancel_status_never_reads_as_a_login_failure`（超时不算取消） |
| R4 矮窗够不到按钮 | **已修**：`fit_scale()` 双轴取缩放 `min(avail_w/bg_w, avail_h/bg_h, 1.0)`，`avail_h = 屏高 - 32(边距) - 150(控件预留)`；**没有**套 `ScrollArea`（会和 `Sense::drag` 抢手势） | `captcha.rs::{fit_scale, CONTROLS_RESERVE, WINDOW_MARGIN}`；用例 `challenge_modal_keeps_every_button_on_screen`（640×360 与 360×640 下「提交验证/换一张/取消」都在屏内）、`scale_respects_both_axes_and_never_upscales` |
| R5 无上限解码 | **已修**：`image::ImageReader::with_format(Png)` + `limits(Limits{ max_image_width/height: 4096, max_alloc: 32MiB })`（该版本的上限接口是 `limits()`，`Reader::with_limits` 不存在），超限/坏图落到 `decode_error` →「换一张 / 取消」 | `captcha.rs::{decode_limits, decode_texture}`；用例 `decoding_is_bounded_and_failures_land_in_the_retry_exit`（>4096 宽的图被拒、坏字节被拒、弹窗给出两个出口） |
| R6 线程 panic 后按钮静默失效 | **已修（前半）**：三处 `send` 收敛成 `App::send_answer`，失败时 `login_busy=false` + 状态「登录线程已结束，请重新登录」。**未做** `spawn_job` 的 `catch_unwind`：`src/ui/jobs.rs` 本轮不在 inScope，且补发失败消息需要每种 job 自己的协议（另开一轮） | `captcha.rs::send_answer`；用例 `dead_login_thread_is_reported_instead_of_stuck_in_checking` |
| R7 成功分支不 `abandon` | **已满足**（无需改）：`abandon()` 原本就在 `LOGIN_DONE` 的 `if/else` 之外（成功与失败都走），R1 的 guard 再覆盖一层 | `src/ui/msgs.rs` 现状 + 用例 `pending_challenge_is_dropped_when_the_login_thread_ends` |
| R8 不释放输入焦点 | **已修**：弹窗首帧清掉「当前焦点」（`surrender_focus(当前焦点 id)`；对弹窗自己的 id 调用是空操作，因为它并不是焦点持有者） | `captcha.rs::draw_modal`（`focus_cleared` 只做一次）；用例 `opening_the_modal_releases_input_focus` |

## 11. 第 2 轮对 `src/api/gt4.rs` 的改动边界（自查用）

```
git diff 94de1f0..HEAD -- src/api/gt4.rs
```

只应有 5 个 hunk，且全部落在两处：

1. `pub enum ManualOutcome` —— 新增 `Timeout` 变体（R3 需要后台线程能区分「超时」与「用户取消」；
   若只靠 UI 侧计时，App 在后台不渲染帧时根本追不上后台线程的超时，原因仍会被吞）。
2-5. `solve_gt4_manual`（含其文档注释）—— 抖动、`Timeout` 分支、三张全废后的 `record_rejection`。

`solve_once` / `solve_gt4` / `load_challenge` / `submit_challenge` **一个字节都没动**
（上面 5 个 hunk 的上下文行都不落在它们体内），自动路径仍与 `94de1f0` 一致。

