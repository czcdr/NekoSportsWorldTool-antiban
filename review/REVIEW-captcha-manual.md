# 手动过码接线 · 对抗评审（t3）

- 评审人：gate-review（未参与实现；t2 的验收结论只作为输入，不重复跑命令）
- 评审对象：`fix/captcha-manual` 的 `35298d2..HEAD`（6 文件：`A review/CAPTCHA-MANUAL.md`、`M src/api/model.rs`、`M src/ui/{captcha,jobs,mod,msgs}.rs`）
- 方法：逐类对抗审代码（不看实现者自述），每个重点问题给出**可判定的代码依据**或**复现路径**
- 结论：**needs_revision** —— 未发现凭证泄露或自动模式回归，但抓到一个新引入的「僵尸弹窗」缺陷（R1，medium）与一处反检测语义回归（R2，medium），两处都是小改

> 口径提醒（沿用 t2）：`captcha.rs` 在 `35298d2` 时已存在但**未被 `mod` 引用＝没编译**，本次 `ui/mod.rs:12` 让它整份生效（317→774 行）。所以「本次引入的新问题」包括整份 captcha.rs 里此前从未运行过的逻辑，不只是 diff 新增行。

---

## Findings

### R1（medium，新引入的 bug）消息顺序把「已超时的挑战图」复活成僵尸弹窗

- 位置：`src/ui/mod.rs:163-165`（`poll_messages()` 在 `poll_captcha()` 之前）、`src/ui/captcha.rs:245-250`（`poll_captcha` 无条件收图）
- 问题：挑战请求与 `LOGIN_DONE` 可能同时留在队列里。`poll_messages` 先把登录线程判死（`msgs.rs:305` `login_busy=false`、`msgs.rs:353` `abandon()`），紧接着 `poll_captcha` 又把那张**已经没人会回答**的挑战图收成 `pending`，弹窗重新出现。
- 为什么出事（复现路径）：
  1. 竖屏手机切「手动过码」，点登录（弱网，挑战图 1s 后才到）；
  2. 图到达前后立刻按 Home 把 App 切后台 —— Android 暂停渲染，`update()` 不再执行，队列只进不出；
  3. 干等 5 分钟：后台线程 `recv_timeout` 到 `PATIENCE`（`captcha.rs:18`/`:124`）→ `ManualOutcome::Cancel` → 发送 `LOGIN_DONE`；
  4. 回到前台：`update()` 先跑 `poll_messages`（状态栏「已取消验证码」、`login_busy=false`、`abandon()` 空转，因为此刻 `pending` 还是 `None`），再跑 `poll_captcha` → 弹窗出现在一个**已经结束**的登录上；
  5. 用户拖完点「提交」→ `captcha.rs:278` `let _ = pending.request.responder.send(...)` 失败被吞掉 → 弹窗消失、状态栏永久停在「已提交滑块位置 Npx，正在校验…」（`captcha.rs:282`）。
- 桌面最小化理论上同源；Android 后台这条路径是确定的（Activity pause → 无帧）。
- 最小修法：`poll_captcha` 只收「登录还活着」的请求 ——
  ```rust
  while let Ok(request) = self.captcha_rx.try_recv() {
      if !self.login_busy { continue; }   // 登录线程已收工，这张图没人会回答
      ...
  }
  ```
  （或把 `poll_captcha()` 挪到 `poll_messages()` 之前让同帧 `abandon()` 把它清掉；guard 写法更稳，且能顺带覆盖 R7。）

### R2（medium，反检测语义回归）手动模式没有任何客户端节流，且「换一张」是零间隔

- 位置：`src/api/login.rs:183-192`（手动判错不调 `captcha_guard::record_rejection`）、`:267-276`（手动 10003 同样不记）；`src/api/gt4.rs:458-474`（`Retry` → `continue` → 下一轮 `load_challenge` 无延时）
- 问题：自动模式的判定是「第 2 次判错 → 冷却 10 分钟、第 3 次 → 30 分钟、第 4 次起 → 60 分钟」（`src/api/captcha_guard.rs:127-132`）；手动模式**完全不进 `captcha_guard`**，因此 3 轮耗尽后重新点「登录」立刻又能拿 3 张图，无上限。
- 为什么出事：`captcha_guard` 的存在理由写在 `src/api/login.rs:144-145` 自己的注释里 ——「用户看到滑块过不去就会连点，而连点正是最坏的动作：每点一次都在加深极验对该会话的降级」。现在这条唯一的客户端节流被整段绕开，且 t1 之前 `Manual` 分支从 UI 根本不可达（`35298d2` 的 `do_login` 恒调 `login()`），是这次接线把它变成了「顶栏一按就能用」。
  可观察面还有一个硬证据：自动路径在取图后 `sleep(900..1900ms)`、拖动前 `sleep(120..380ms)`（`gt4.rs:524`/`:536`），手动路径的 `Retry` 分支是**立即** `continue` 再 `load_challenge`，两次 `/load` 之间只有网络 RTT —— 一个脚本驱动的手动会话会呈现固定节奏的请求串。
- 最小修法（只改 `solve_gt4_manual`，别碰 `solve_once`/`solve_gt4`，否则自动路径不再与 `94de1f0` 字节级一致，要重跑 t2 的对比）：
  1. 每轮 `load_challenge` 之前加 200–600ms 随机抖动；
  2. 3 轮耗尽后调一次 `captcha_guard::record_rejection()`（保留「拖歪一次不锁十分钟」的原意，但让**反复整轮失败**能升级成冷却）。

### R3（low，错误路径吞掉真实原因）三种不同原因都显示成「已取消验证码」

- 位置：`src/ui/captcha.rs:21`（`CANCELLED_STATUS`）、`:120-129`（超时与通道断开都 `return ManualOutcome::Cancel`）、`src/api/gt4.rs:469-471`（Cancel → `Gt4Error::Cancelled("已取消验证码")`）、`src/ui/jobs.rs:479`
- 问题：用户点「取消」、5 分钟不操作超时、界面通道断开（App 退出/`abandon`），三条不同原因全部映射到同一句「已取消验证码」，日志也是「[login] 已取消验证码，本次登录结束」。
- 为什么出事：复现＝什么都不做等 5 分钟 → 状态栏「已取消验证码」+ 日志说「已取消」，而真实原因是**等待超时**。这正是本轮要查的「把真实原因吞掉」：用户回来会以为自己误触了取消，排障时也拿不到线索。
- 最小修法：`await_answer` 的超时/断开分支返回可区分的结果（如 `ManualOutcome::Cancel` 加一个 reason，或新增 `Timeout`），映射为「验证码等待超时（5 分钟未操作）」；`is_cancelled` 的判定同步收窄。

### R4（low，可达性）弹窗只按宽度缩放、没有 ScrollArea，窗口一矮就够不到「提交/取消」

- 位置：`src/ui/captcha.rs:310`（`set_max_width((screen_w-24).min(460))`）、`:331-335`（`scale = (avail / bg_w).min(1.0)` 只除宽）、`:303-309`（`egui::Window` 无 `ScrollArea`）
- 问题：窗口高度不足时，底部那行「提交验证／换一张／取消」被裁到屏外；`取消` 是唯一逃生口（见下条：没有 X / 没有 Esc）。
- 为什么出事：桌面把窗口拉到 600×320 再点登录即可复现（内容高 ≈ 标题 28 + 图 230 + 控件 90 > 320）。Android 已锁竖屏（`android/AndroidManifest.xml:14 android:screenOrientation="portrait"`），所以横屏不成立；但同文件 `:13-14` 的 `configChanges` 全接管 + `adjustResize` 意味着**分屏/弹出键盘缩窗**时 App 视口会变矮，`resizeableActivity` 未显式关闭（targetSdk 36 默认 true），分屏下半屏同样会裁。
- 最小修法：`scale` 再取一次高度约束 —— `let scale = (avail_w/bg_w).min(avail_h/bg_h).min(1.0)`（`avail_h` 用 `ui.available_height()` 扣掉控件预留）。**不要**简单套 `ScrollArea`：图上的 `Sense::drag`（`captcha.rs:335`）会和滚动抢手势，得先把拖动区排除在滚动之外。

### R5（low）UI 线程无上限解码服务端 PNG

- 位置：`src/ui/captcha.rs:235` `image::load_from_memory(png)`
- 为什么出事：解码发生在 `draw_modal`（首帧），没有像素/分配上限。一张超大图（服务端或中间人可控）会在这帧直接吃掉几百 MB 并卡住 UI。正常 GT4 图（≈600×300）无影响，所以是 low。
- 最小修法：`image::io::Reader::with_limits(Limits { max_image_width/height, max_alloc: 32 << 20, .. })`，超限落到已有的 `decode_error` 分支（该分支已有「换一张/取消」出口，体验完整）。

### R6（low）后台线程 panic 后，弹窗按钮静默失效、状态栏永久「正在校验…」

- 位置：`src/ui/captcha.rs:267/271/278`（三处 `let _ = ...send(...)` 吞掉发送失败）、`src/ui/mod.rs:570-579`（`spawn_job` 直接 `std::thread::spawn`，无 `catch_unwind`）
- 问题：登录线程若 panic（当前新代码里没有 panic 路径，但依赖 `image`/egui/native 不出事），responder 的接收端随栈解开被丢弃；用户再点提交，`send` 失败被静默吞掉，弹窗消失但状态栏永远停在「正在校验…」，`login_busy` 保持 `true`（登录按钮一直灰）。
- 为什么出事：「login_busy 卡死」这半边是既有问题（任何 job panic 都会），但**弹窗把它变成「点了没反应 + 状态栏撒谎」**，用户完全无法判断发生了什么。
- 最小修法：三处 `send` 改成检查结果，失败时 `self.login_busy = false; self.status = "登录线程已结束，请重新登录".into()`（一行）；更彻底的做法是给 `spawn_job` 包 `catch_unwind` 并在 panic 时补发一条失败消息。

### R7（low，收尾不对称）成功分支不 `abandon()`

- 位置：`src/ui/msgs.rs:351-353`（只在 `else` 失败分支调 `abandon`）
- 说明：当前不会出问题——挑战图只在后台线程阻塞等答案时存在，成功时必然已被回答。但这是个隐含依赖：将来 solver 改成非阻塞，成功路径会留下一个点了没反应的弹窗。R1 的 guard 修法顺带覆盖它；只改 R1 时也建议把 `abandon()` 提到 `if/else` 外面，一行成本。

### R8（low，Android 手感，需真机确认）打开弹窗时不主动释放输入焦点

- 位置：`src/ui/captcha.rs:303-309`（`egui::Window` 创建处；全文件无 `surrender_focus`/`request_focus`，已 grep 确认零命中）
- 为什么出事：用户在密码框里输完直接点「登录」，焦点仍在 `TextEdit`，Android 输入法可能不收；`adjustResize`（`AndroidManifest.xml:14`）会让窗口变矮，与 R4 叠加就可能把「取消」挤出屏幕。
- 判定依据是代码事实（无焦点处理）+ manifest 的 `adjustResize`；真实表现需真机验证，所以标 low。最小修法：弹窗首次出现那一帧 `ctx.memory_mut(|m| m.surrender_focus(egui::Id::new("captcha_modal")))`。

---

## 六类逐条结论

**1. UI 线程 —— 没有新的阻塞/死锁，但有一个消息顺序 bug（R1）**
等待期间 UI 线程只做 `try_recv`（`captcha.rs:246`、`msgs.rs:100/182/565`），唯一的 `recv_timeout` 在后台登录线程（`captcha.rs:106`）；没有锁被跨线程持有。重绘不是新问题：`update()` 本来就有全局 `request_repaint_after(200ms)`（`mod.rs:169`），弹窗只是复用节拍。弹窗在 `update()` 顶层无条件绘制（`mod.rs:199`），任何页面/标签页都会出现。`egui::Window` 未使用 `.open()`，所以**没有 X 按钮，Escape 也不会关**（见 R4：好处是后台线程不会因「弹窗被关掉」而永挂，坏处是没有逃生口）。触摸拖动与 Slider 共用 `Sense::drag` + `Slider`（`captcha.rs:335`/`:384`），在 egui 里都吃触摸事件；弹窗内无文本输入，不触发 IME 竞争。

**2. 错误处理 —— 取图失败/解码失败按预期降级，但「超时 vs 取消 vs 断开」被压成同一句文案（R3），panic 路径会静默吞掉（R6）**
取图失败：`load_challenge` 的 Err 直接冒到 `LOGIN_DONE`，状态栏「登录失败：验证码未能完成：…」，真实原因保留。解码失败：`decode_error` 显示具体错误并给「换一张/取消」，恢复路径完整（`captcha.rs:312-323`）。5 分钟不操作：变成「已取消验证码」——原因被吞（R3）。channel 断开：立即返回不挂死（`captcha.rs:128`，测试 `disappearing_interface_cancels_instead_of_blocking` 有 5s 上限断言），但同样被叫成「已取消」。后台 panic：R6。**没有**把真实原因统一换成「滑块失败」——失败话术仍是 `failure_status` 之外的原文，这点是干净的。

**3. 凭证保密 —— 没有发现新增泄露面**
新增的日志只有三类：取消中性文案（`jobs.rs:479`）、模式名称（`captcha.rs:208-209`）、`setLeft/passtime`（`gt4.rs:489-491`，属于 94de1f0 就有的手动交卷日志）。`captchaOutput/passToken` 仍只以**长度**出现（`login.rs:207-223`），10003 原始响应仍走 `textlog::redact_json`（`login.rs:277`/`:287`）；弹窗里只有图片和位置，不含任何 token；`CaptchaRequest`（`captcha.rs:24-38`）只有 PNG 与几何信息。唯一可讨论的是 `setLeft`（本次答案）进了日志与状态栏（`captcha.rs:282`）——它是一次性 proof-of-work、非可复用凭证，且截图上本来就看得见，判定为**不构成新泄露**；要更保守可以只记长度。

**4. 自动模式回归 —— 没有改动**
t1 的 diff 在 `src/api/` 只碰 `model.rs`（新增 config 字段），`login.rs`/`gt4.rs`/`captcha_guard.rs` 与 `94de1f0` 零 diff（t2 已用 `git diff` 举证）。`solve_gt4` 重试语义、`geevalidate` 单次发送、`captcha_guard` 冷却阈值、自动 `passtime`（`gt4.rs:540` 用 `challenge.elapsed_ms()`）均未变；手动分支的 `passtime` 是真人拖动时长经 `clamp(250, 9000)`（`gt4.rs:488`），也不影响自动路径。

**5. Android 可达性 —— 没有桌面专属 API；小屏/缩窗的裁切是真的（R4），焦点未处理（R8）**
`captcha.rs` 只用 `egui` + `image`，无 `rfd`/文件对话框/桌面窗口 API；`cfg(target_os="android")` 分支不涉及手动过码。竖屏已锁死（manifest `screenOrientation="portrait"`），所以 L1 的横屏场景不成立，剩下分屏与键盘缩窗两条真实路径。触摸拖动、Slider、±1px 按钮三件套都能用（无 ScrollArea 干扰手势）。

**6. 反检测语义 —— 有回归（R2）**
手动模式是**唯一完全不进 `captcha_guard` 的路径**，且 3 轮耗尽后可无限重开；「换一张」零延时（对比自动模式 900–1900ms + 120–380ms 的两次 sleep）。取消/换图不会产生固定间隔的是**人点**的那一侧（人手上有时延），但脚本驱动时这条路径的节奏是代码决定的，等于把「连点加深降级」的保护整段关掉。

---

## 明确回答：这次改动有没有引入新漏洞 / 新 bug

- **新 bug：有 1 个（R1，medium）** —— `poll_messages` 先于 `poll_captcha` 的消息顺序，会让「已超时的挑战图」在下次渲染时复活成僵尸弹窗，并留下一条永远不结束的「正在校验…」状态。属于本次接线引入（两个队列、这个顺序都是新代码）。
- **新漏洞：没有。** 凭证/密码/token 未进日志、状态栏或弹窗；无新增网络请求路径、无新增文件写入、无新增 panic 路径（整份 captcha.rs 非测试区已逐个核对）；自动过码路径与 `94de1f0` 字节级一致；配置向后兼容。
- **语义回归：有 1 个（R2，medium）** —— 手动模式把 `captcha_guard` 整段绕开，且这条路径是本次从「不可达」变成「顶栏一键可达」的。

## 结论

**needs_revision**：先修 **R1 + R2**（各自 1–5 行，均不碰自动路径），R3/R4 建议同批带上；R5–R8 可留到后续版本。修完只需重跑 `cargo check/test --lib` 与 `ui::captcha` / `ui::msgs::login_done_tests` 定向用例，并补一条「队列里同时有 LOGIN_DONE + 旧挑战图」的回归用例（R1 可直接用测试锁死：先入队请求、置 `login_busy=false`、跑 `poll_captcha`，断言 `pending.is_none()`）。
