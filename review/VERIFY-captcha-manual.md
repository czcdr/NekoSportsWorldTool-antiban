# 手动过码接登录 · 独立验收记录（t2）

- 验收人：gate-review（不参与实现）
- 目录：`E:\rea\wt-captcha`（worktree），分支 `fix/captcha-manual`
- HEAD：`a8e626f docs(captcha)`，其父 `85c0037 feat(captcha)`
- rebase 基：`35298d2 fix(audit)`；实现者自述的旧基线 `94de1f0 wip(captcha)`
- 图形/无网络前提：全部命令 `--offline`，未联网、未改主树、未改 tag、未 push
- 验收结论：**PASS**（4 条 verify 命令全 0 退出；6 条验收标准逐条有证据；3 条低危观察与 3 项未覆盖如实登记，均不阻塞合并）

---

## 1. 实际改动面核对

```
$ git status
On branch fix/captcha-manual
nothing to commit, working tree clean

$ git diff 35298d2..HEAD --name-status
A       review/CAPTCHA-MANUAL.md
M       src/api/model.rs
M       src/ui/captcha.rs
M       src/ui/jobs.rs
M       src/ui/mod.rs
M       src/ui/msgs.rs
```

6 个文件，**未超出 inScope**（实现 5 文件 + 复核文档 1 文件）。

说明：任务描述里的 `git diff 94de1f0 --stat` 会把中间的审计提交 `35298d2` 一起算进来（20 文件 / +1893 −213），那不是本次改动。以 rebase 基 `35298d2` 为准的实际面是 `6 files changed, 926 insertions(+), 53 deletions(-)`。已确认 `35298d2` 是 HEAD 的祖先：

```
$ git merge-base --is-ancestor 35298d2 HEAD   → exit 0
```

---

## 2. verify 命令复跑（全部本机重跑，不引用实现者自述）

| # | 命令 | 退出码 | 输出摘要 |
|---|---|---|---|
| 1 | `$env:CARGO_TARGET_DIR="E:\rea\upstream\target"; cargo check --lib --offline` | **0** | `Finished dev profile … in 1.77s`；仅 1 条既有警告 `src\api\fingerprint.rs:352 associated function audit is never used`（该文件本次未改动） |
| 2 | `$env:CARGO_TARGET_DIR="E:\rea\upstream\target"; cargo test --release --lib --offline` | **0** | `test result: ok. 300 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 9.46s` |
| 3 | `cargo fmt --check` | **0** | 无输出 |
| 4 | `git diff --check` | **0** | 无输出（无空白错误/冲突标记） |

补充定向复跑（用于验收标准 2 的证据）：

```
$ cargo test --release --lib --offline -- ui::captcha ui::msgs::login_done_tests
running 13 tests
test ui::captcha::tests::submit_path_clamps_slide_position_to_the_image_width ... ok
test ui::captcha::tests::retry_path_asks_for_another_challenge ... ok
test ui::captcha::tests::cancel_path_stops_the_login_thread ... ok
test ui::captcha::tests::disappearing_interface_cancels_instead_of_blocking ... ok
test ui::captcha::tests::unanswered_challenge_gives_up_after_patience ... ok
test ui::captcha::tests::pending_challenge_is_dropped_when_the_login_thread_ends ... ok
test ui::captcha::tests::cancel_status_never_reads_as_a_login_failure ... ok
test ui::captcha::tests::save_failure_is_reported_instead_of_pretending_it_worked ... ok
test ui::captcha::tests::selector_reports_only_real_changes ... ok
test ui::captcha::tests::desktop_layout_switches_captcha_mode_and_saves_it ... ok
test ui::captcha::tests::compact_layout_switches_captcha_mode_and_saves_it ... ok
test ui::msgs::login_done_tests::cancelled_captcha_is_not_reported_as_a_login_failure ... ok
test ui::msgs::login_done_tests::ordinary_login_failure_keeps_the_failure_prefix ... ok
test result: ok. 13 passed; 0 failed; 0 ignored; 288 filtered out
```

---

## 3. 验收标准逐条判定

### 3.1 手动过码四条通道（Submit / Retry / 取消 / 断开）

| 情形 | 测试 | 断言要点 | 判定 |
|---|---|---|---|
| Submit | `ui::captcha::tests::submit_path_clamps_slide_position_to_the_image_width`（`src/ui/captcha.rs:539`） | 界面故意交 `set_left=9999`（max=250）→ 后台二次夹到 `250`；`passtime_ms=1234` 原样透传 | PASS |
| Retry | `retry_path_asks_for_another_challenge`（`:573`） | 收到 `Retry` → `ManualOutcome::Retry` | PASS |
| 取消 | `cancel_path_stops_the_login_thread`（`:594`） | 收到 `Cancel` → `ManualOutcome::Cancel` | PASS |
| 断开 | `disappearing_interface_cancels_instead_of_blocking`（`:617`） | UI 侧丢弃请求（responder 断开）→ 立刻 `Cancel`，且 `elapsed < 5s`（patience 设成 300s 也不等） | PASS |

补强证据（我逐条读了测试体，断言没有被放宽）：

- 四条用例都把 `await_answer` 直接当被测对象（`src/ui/captcha.rs:91`），用真实 `mpsc` 通道 + 真实 200ms 轮询，不是桩。
- 超时分支另有 `unanswered_challenge_gives_up_after_patience`（`:645`）：patience=10ms、无人回答 → `Cancel`。
- 收尾分支 `pending_challenge_is_dropped_when_the_login_thread_ends`（`:668`）：`abandon()` 后 `pending.is_none()`，且后台 `recv_timeout(50ms)` 返回 `Err`（证明通道确实断开、线程不会被挂住）。

### 3.2 自动过码路径未被改动（与 94de1f0 对比举证）

三条关键文件在 `94de1f0..HEAD` 范围内 **零差异**：

```
$ git diff 94de1f0..HEAD --stat -- src/api/login.rs src/api/gt4.rs src/api/captcha_guard.rs
(空)
```

即以下三者字节级一致：

1. **solve_gt4 调用点**：`src/api/login.rs:169` `(CaptchaMode::Auto, _) => super::gt4::solve_gt4(captcha_id, client, log)`；
2. **geevalidate 单次发送**：`src/api/login.rs:252` 只调用一次 `client.envelope_request("POST", &url, &body, &ua, &[])`，无重发分支；
3. **captcha_guard 行为**：`src/api/login.rs:146` 冷却前置、`:186`/`:270` 自动模式 `record_rejection`、`:261` `record_success`。

再对全 `src/api/` 做特征行扫描，确认本次 diff 没有碰过这三个符号：

```
$ git diff 94de1f0..HEAD -- src/api/ | Select-String '^[+-].*(solve_gt4|geevalidate|captcha_guard|record_rejection|record_success)'
(无匹配)
```

分支接线本身（`src/ui/jobs.rs:436-459`）：自动模式仍然原样调用 `crate::api::login::login(&mut client, &username, &password, &mut log)`（`:439`），**没有**改成 `login_with + solver: None` 之类的新写法；只有 `CaptchaMode::Manual` 分支（`:444-459`）才用 `login_with(LoginOptions { captcha: Manual, solver: Some(&mut solver) })`。

> 归因说明：`src/api/flow.rs` 在 `94de1f0..HEAD` 内有变化（+197/−…），但那全部来自审计提交 `35298d2`，不是本次改动；且 flow.rs 不含 `solve_gt4 / geevalidate / captcha_guard` 任何调用（已 grep 确认），不影响登录链的自动过码路径。

判定：**PASS**

### 3.3 UI 线程无阻塞

- UI 侧收挑战图：`src/ui/captcha.rs:246` `while let Ok(request) = self.captcha_rx.try_recv()` —— 非阻塞。
- UI 消息泵：`src/ui/msgs.rs:100`、`:182`、`:565` 均为 `try_recv`。
- 阻塞等待只发生在**后台登录线程**：`src/ui/captcha.rs:106` `answers.recv_timeout(POLL_INTERVAL=200ms)`，循环内 `ctx.request_repaint()`（`:122`）保证手机前台持续重绘。
- 全 `src/ui/` 的 `sleep` 只有 `src/ui/jobs.rs:673`（AI 记录拉取的 worker 线程内限速，既有代码，非登录路径）。
- `set_captcha_mode`（`src/ui/captcha.rs:253`）在 UI 线程同步调 `save_config`：落盘走 AES-256-CBC + HMAC-SHA256（`src/crypto/local_seal.rs`，无慢 KDF），且「UI 线程保存配置」是本仓库既有模式（`src/ui/about.rs:144`、`src/ui/device.rs:414`、`src/ui/osm.rs:374` 等），不构成新增阻塞面。

判定：**PASS**

### 3.4 取消路径：不显示「登录失败」、不发额外请求

- 状态文案：`src/ui/msgs.rs:349` 改走 `captcha::failure_status`，`src/ui/captcha.rs:222-228` 对以 `CANCELLED_STATUS`（`"已取消验证码"`，`:21`）开头的消息**原样返回**，不加「登录失败：」前缀。
- 日志：`src/ui/jobs.rs:479` 取消分支写 `"[login] 已取消验证码，本次登录结束"`，不写 `× 登录失败`。
- 不发请求：`src/api/gt4.rs:469-471` `ManualOutcome::Cancel => return Err(Gt4Error::Cancelled("已取消验证码"))`，在 `submit_challenge`（`:492`）之前返回；`src/api/login.rs:175-177` 对 `error.cancelled()` 直接 `return Err(...)`，**不走 geevalidate、不记判错、不进冷却**。
- 单测双保险：`cancelled_captcha_is_not_reported_as_a_login_failure`（`src/ui/msgs.rs:773`）断言 `status == CANCELLED_STATUS`、`login_busy == false`、`session.is_none()`；`ordinary_login_failure_keeps_the_failure_prefix`（`:787`）断言普通失败仍带「登录失败：」前缀（防止把失败话术一起改掉）。

判定：**PASS**

### 3.5 无新增 unwrap / expect / panic 路径

两条口径都查了，**且发现一个容易漏的前提**：`src/ui/captcha.rs` 在 `35298d2` 时已存在（317 行）但 **没有被 `mod` 引用**，即整份文件当时根本没参与编译；本次 `src/ui/mod.rs:12` 新增 `pub mod captcha;` 把它变成生效代码。所以「新增生效面 = 整份 captcha.rs（317→774 行）」，只查 diff 新增行会漏审。

- diff 新增行扫描（`git diff 35298d2..HEAD -U0 -- src/`，代码行 675 行）：`unwrap(` / `expect(` / `panic!` / `unreachable!` / `todo!` / `unimplemented!` 命中 **0**（唯一命中是复核文档里的两行说明文字）。
- 新增行索引访问：仅 `src/ui/msgs.rs:348` `val["message"]`（`serde_json::Value` 的 `Index<&str>`，缺键返回 `Null`，不 panic；且该行原有写法就是 `val["message"]`，本次只是提取成局部变量）。
- 整份 `src/ui/captcha.rs` 非测试区（1–432 行）复查：`unwrap(`/`expect(`/`panic!`/`unreachable!` **0 处**；索引表达式 **0 处**。
- 数值转换：`pending.offset.round().max(0.0) as usize`（`:413`）是浮点→整数**饱和转换**（Rust 1.45+，NaN/负数不 panic），且前面已 `.max(0.0)`；`offset.drag_delta().x / scale`（`:349`）配合 `.clamp(0.0, max_offset)`；`background` 侧 `set_left.min(max_offset)`（`:114`）二次夹取。

判定：**PASS**

### 3.6 config 向后兼容（旧 config.json 无 captcha_mode）

运行时证据（真实文件、真实 `load_config`）：

```
$ cargo test --release --lib --offline -- --exact \
    api::model::storage_tests::captcha_mode_defaults_to_auto_and_survives_a_save_load_round_trip --nocapture
test api::model::storage_tests::captcha_mode_defaults_to_auto_and_survives_a_save_load_round_trip ... ok
test result: ok. 1 passed; 0 failed   (exit 0)
```

该用例确实是在临时目录里**手写旧格式 JSON 到 `config.json`** 后调真实 `load_config()`（`src/api/model.rs:723-790`），覆盖三种旧/坏形态：

| 写入的原始 JSON | 期望 | 结果 |
|---|---|---|
| `{"username":"alice","remember":true}`（无 captcha_mode，老配置） | `captcha_mode == "auto"`、`resolved == Auto` | OK |
| `{"username":"alice","captcha_mode":"hand2"}`（值被手改坏） | 归一为 `"auto"` | OK |
| `{"username":"alice","captcha_mode":""}` | 归一为 `"auto"` | OK |
| `{"captcha_mode":"manual"}`（单键探针路径） | 保留 `"manual"` | OK |

「不被写坏」的证据：

- 静态：`load_config`（`src/api/model.rs:499-511`）只做 `read_json` → `deserialize_lenient` → 内存里归一 → `return`，**没有任何写盘调用**；唯一的写入方是 `save_config`（`:513-517`，`write_json`），只在用户显式保存时触发。
- 运行时：上面用例在同一目录先 `save_config` 再 `load_config`，之后又连续对多份手写 JSON 调 `load_config`，均无报错、无字段丢失（`username`/`remember`/`captcha_mode` 均按预期读回）。
- 回滚兼容（静态判断）：`Config` 无 `#[serde(deny_unknown_fields)]`，旧版程序读到带 `captcha_mode` 的新配置会按 serde 默认**忽略未知字段**，不会解析失败。

字段本身：`src/api/model.rs:61` `#[serde(default = "default_captcha_mode")]`（`default_captcha_mode` 在 `:100` 返回 `"auto"`），默认 `"auto"`；`Config::default()`（`:130`）同样给 `"auto"`。语义取向正确——认不出的值一律回落自动，避免老用户升级后被卡在「等界面拖动」。

判定：**PASS**

---

## 4. 低危观察（不阻塞，但建议后续修）

> 均为本次新增 UI 面（`src/ui/captcha.rs`）引入，非既有代码问题。

**L1 — 弹窗只按宽度缩放，横屏/小屏时底部按钮可能被裁掉（触摸不可达）**
`src/ui/captcha.rs:333` `let scale = (avail / bg_w).min(1.0);` 只用了可用**宽度**；`:310` 的 `set_max_width((screen_width - 24).min(460))` 也只约束宽度。弹窗 `egui::Window`（`:303-309`）内部没有 `ScrollArea`。
后果：横屏手机（如 640×360）或异常高的挑战图下，内容高度会超过一屏，「提交验证 / 换一张 / 取消」这一行被裁到屏幕外——`取消` 是唯一逃生口，此时用户只能等 300s patience 超时。
最省事的修法：`scale = (avail_w / bg_w).min(avail_h / bg_h).min(1.0)`，或给弹窗内容套 `egui::ScrollArea::vertical().max_height(...)`。

**L2 — 服务端图片在 UI 线程无上限解码**
`src/ui/captcha.rs:235` `image::load_from_memory(png)` 在 UI 线程跑，且没有尺寸/分配上限。正常 GT4 图（约 600×300）没问题，但一张超大图会直接卡帧（甚至 OOM）。建议用 `image::io::Reader::with_limits(...)` 限制像素数，超限走已有的 `decode_error` 分支（该分支已有「换一张 / 取消」出口，体验是完整的）。

**L3 — 成功分支不调 `abandon()`，两侧收尾不对称**
`src/ui/msgs.rs:351-353` 只在 `LOGIN_DONE` 的**失败**分支 `captcha.abandon()`；成功分支不调。当前不会出问题（挑战图只在后台线程阻塞等答案时存在，成功时必然已被回答），但这是个隐含依赖：将来若 solver 改成非阻塞，成功路径会留下一个「点了没反应」的弹窗。一行成本：把 `abandon()` 移到 `if/else` 之外。

## 5. 未覆盖项（如实登记）

1. **真机 + 真账号过码联调没做**：本次验收全程 `--offline`，没有有效账号、没有未降级的出口 IP，无法验证「拖对了服务端真给 lot_number」以及 Android 触摸手感。实现者文档已标为人工判断项，本次验收同样无法替代。**这是本回合最大的未覆盖风险。**
2. **「加载后按 auto 工作」的端到端可执行证据只能到 crate 内单测**：`api` 模块在 `src/lib.rs` 里是 `mod api;`（私有），外部二进制/集成测试读不到 `captcha_mode`，CLI 也没有任何「打印当前配置」的子命令。所以兼容性验证用的是「真实临时目录 + 手写旧 JSON + 真实 `load_config()`」的单测 + 源码静态证据，而非外部进程黑盒复现。
3. **弹窗渲染本身没有测试**：`draw_modal`（`src/ui/captcha.rs:288`）和 `draw_captcha_window`（`:259`）未被任何用例执行——现有测试覆盖的是通道层（`await_answer`）、选择器（`captcha_mode_selector`）与消息处理（`LOGIN_DONE`）。也就是说「真实点击『提交验证』按钮 → responder 收到 Slide」这一跳只有代码阅读证据，没有运行时证据（Submit 的夹取逻辑本身在 `await_answer` 侧已被覆盖）。同样，选择器测试只验证了「桌面 900×700 / 手机 360×600 能点到并落盘」，没有覆盖 L1 里的横屏场景。

## 6. 结论

**PASS** —— 6 条验收标准全部满足，4 条 verify 命令全部 0 退出；自动过码路径与 `94de1f0` 字节级一致（login.rs / gt4.rs / captcha_guard.rs 零 diff）；取消语义、无阻塞、无新增 panic 路径、旧配置兼容均有可复跑证据。

3 条低危观察（L1 触摸可达性、L2 无界解码、L3 收尾不对称）与 3 项未覆盖项（真机联调、黑盒配置复现、弹窗渲染测试）已登记，不构成合并阻塞，建议在下一个版本里顺手处理 L1/L3。
