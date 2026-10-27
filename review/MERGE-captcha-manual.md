# 手动过码（GT4 人工拖动）合并交接

- 合并目标：`E:\rea\upstream`，分支 `antiban-review`
- 合并来源：`fix/captcha-manual`（隔离工作树 `E:\rea\wt-captcha`）
- 合并方式：`git merge --ff-only fix/captcha-manual`（**fast-forward，无合并提交、无冲突**）
- 合并前主树：`35298d2`，`git status --short` 为空（硬门槛已满足）
- 合并后 HEAD：`73f805f`
- 本回合**未 push、未打 tag、未改任何版本号、未触发 CI/workflow**——版本号 / tag / push / 产物核对由 captain 统一执行

## 1. 合并的提交（4 个，线性于 35298d2）

| 提交 | 主题 |
|---|---|
| `85c0037` | feat(captcha): 手动过码接进登录流程（界面选择 + 挑战图弹窗） |
| `a8e626f` | docs(captcha): 手动过码的改动清单、手测步骤与复核清单 |
| `cee6335` | fix(captcha): 落实 t3 评审 R1-R8（僵尸弹窗/节流/超时文案/矮窗可达/解码上限/线程已结束） |
| `73f805f` | fix(captcha): R9 仅 Rejected 计入冷却、R10 Timeout 改 Fatal，并补同帧顺序集成用例 |

## 2. diff 摘要（`git diff --stat 35298d2..73f805f`）

```
 review/CAPTCHA-MANUAL.md        | 257 ++++++++++++
 review/REVIEW-captcha-manual.md | 114 ++++++
 review/VERIFY-captcha-manual.md | 202 +++++++++
 src/api/gt4.rs                  |  96 ++++-
 src/api/model.rs                | 102 +++++-
 src/ui/captcha.rs               | 882 +++++++++++++++++++++++++++++++++++++---
 src/ui/jobs.rs                  |  40 +-
 src/ui/mod.rs                   |  47 +++
 src/ui/msgs.rs                  |  64 ++-
 9 files changed, 1736 insertions(+), 68 deletions(-)
```

- 未触碰：`src/api/login.rs`、`src/api/client.rs`、`src/safety.rs`、`src/ui/run.rs`、`android/`、`Cargo.toml`、`Cargo.lock`（`git diff --name-only 35298d2..73f805f` 可核对）。
- 自动过码路径：`src/api/gt4.rs` 相对 `94de1f0` 的改动全在 `solve_gt4_manual`（+`ManualOutcome::Timeout` 变体 + `record_if_rejected` 小函数 + 其单测）；
  `solve_once` / `solve_gt4` / `load_challenge` / `submit_challenge` 的 `+/-` 行数为 0；`login.rs`、`captcha_guard.rs` 与 `94de1f0` 零 diff。

## 3. 合并后验证（主树默认 `target/`，四项全绿）

| 命令 | 结果 |
|---|---|
| `cargo check --lib --offline` | exit 0（仅历史遗留 `src/api/fingerprint.rs:352 IosProfile::audit` dead_code 警告） |
| `cargo test --release --lib --offline` | `test result: ok. 308 passed; 0 failed; 1 ignored`（9.46s） |
| `cargo test -p route-planner --offline` | `ok. 16 passed` + `ok. 7 passed` + `ok. 0 passed`，全部 0 failed |
| `cargo fmt --check` | exit 0（无输出） |

> 注：任务描述里「预期 300 passed」是合并前的旧基线数字；本特性新增 8 条用例（6 条新写 + 2 条加严/改名）后为 **308 passed**，`1 ignored` 是既有的联网探针 `live_gt4_probe`。

## 4. 回滚

合并是 fast-forward，回滚只需把分支指回合并前：

```powershell
cd E:\rea\upstream
git reset --hard 35298d2      # 主树回到合并前（会丢弃本文件所在的 docs 提交）
```

如需保留合并、只回滚本交接文档：`git reset --hard 73f805f`（即合并落点）或单独 `git rm review/MERGE-captcha-manual.md`。

> 工作树 `E:\rea\wt-captcha` 不受影响；`fix/captcha-manual` 分支仍指向 `73f805f`。

## 5. 给另一会话复审的要点

1. **确认主树历史**：`git log --oneline -5` 应为 `73f805f → cee6335 → a8e626f → 85c0037 → 35298d2`，且**没有** merge commit（纯 ff）。
2. **确认没碰版本号**：`git diff 35298d2..HEAD -- Cargo.toml android/` 应为空输出（本特性只改 9 个文件，见 §2）。
3. **自动模式未变**：`git diff 94de1f0..HEAD -- src/api/gt4.rs` 只应出现 manual 相关 hunk；`git diff 94de1f0..HEAD -- src/api/login.rs src/api/captcha_guard.rs` 应为空。
4. **行为口径**：手动模式 3 张图全废时**只有** `Gt4Error::Rejected` 才 `record_rejection()`（R9）；等待 5 分钟超时走 `ManualOutcome::Timeout → Fatal`（R10），文案「验证码等待超时（5 分钟未操作）」，不进冷却、不被 `is_cancelled` 认成取消。
5. **凭据面**：新增日志/状态栏不含 token/密码/lotNumber/captchaOutput/passToken；状态栏文案只有「过码方式…」「已提交滑块位置 Npx」「已取消验证码」「登录线程已结束，请重新登录」这类。
6. **测试锚点**：`cargo test --release --lib --offline captcha` 与 `cargo test --release --lib --offline gt4` 应分别命中本特性用例（清单见 `review/CAPTCHA-MANUAL.md` §10 的两张表），共 21 条。

## 6. 已知遗留（不阻塞本次合并，均不在本特性 inScope 内）

1. **R6 的 `spawn_job` `catch_unwind` 半**：`src/ui/jobs.rs` 不在任何 inScope；现有兜底是「responder 断开 → `send_answer` 报『登录线程已结束，请重新登录』并放开 `login_busy`」（已有用例覆盖），但没有「job panic 时补发失败消息」的通用机制。
2. **手动过码真机/真账号联调**：合并环境无有效账号与未降级出口，未跑（反复试会消耗该出口容忍度）。手测步骤见 `review/CAPTCHA-MANUAL.md` §6。
3. **R9 的保守取舍**：按 `last` 判定，会漏掉「2 次判错 + 1 次传输失败」的混合情形（t3/gate-review 判定「保守，可接受」）；若需覆盖，加一个 `saw_rejected: bool` 即可（+3 行 + 1 条测试）。
