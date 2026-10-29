# 「学校基准海拔」的 App 内出口（t10）

> 只改 `src/ui/**`。未 commit、未跑 `cargo fmt`、未碰 `src/track/**` 与 `src/api/**`。

## 结论

flow.rs 的警告「未配置学校基准海拔…出口：跑步页「手动海拔」/ `run --school-altitude`」现在在 App 内有了真正的落点：

**跑步页 →「高级生成参数（不填即用默认值）」折叠区 →「学校基准海拔（米）」**，留空 = 未配置，
填一次写进 `config.school_base_altitude`，之后所有记录按它生成（优先级低于手动海拔）。

## 改了什么

| 位置 | 改动 |
|---|---|
| `ui/run.rs::RunPage` | 新增输入缓冲 `school_base_altitude: String`（空白 = `None`） |
| `ui/run.rs::RunSettings` | `from_page/apply_to` 带上它；新增 `parse_school_altitude()` 做校验 |
| `ui/run.rs::draw_run_content` | 折叠区内新增一行：标签 + 输入框 + 「未配置/已配置/未生效」状态；下方是优先级 + 当前 origin；非法时行内红字 |
| `ui/mod.rs::from_state` | 启动时从 `config.school_base_altitude` 回填输入框 |
| `ui/msgs.rs::run_popup` | 提交结论里出现「未配置学校基准海拔」时，追加一行指向该输入框 |

### 校验规则（与 `track::altitude` 完全一致）

- 空白 → `Ok(None)`（清掉配置，回落默认 82 m）；
- 数字且在 `ALTITUDE_MIN_M..=ALTITUDE_MAX_M`（-500–9000）→ `Some(v)`；
- 非数字 / 负数越界 / >9000 / NaN / 带空格分隔（"1 050"）→ 返回错误文本，**配置保留原值**，
  同时显示在行内（`学校基准海拔未生效：…`）与状态栏（`persist_run_settings` 已把这个 `Err` 写进状态栏）。

## 为什么改了 `ui/msgs.rs` 而不是 `api/flow.rs`

任务第 2 条要求把 `default_altitude_warning` 的文案改成同时指向新输入框，但硬约束写着
「不要碰 `src/api/**`」。因此本轮的处理是：

- `flow.rs` 原文一字未改（它自己的测试 `unset_school_base_altitude_is_warned_with_an_exit` 仍绿）；
- 在 **UI 侧**把这两处文案对齐：输入框下方常显「提交结论里的「未配置学校基准海拔」警告，就是让你在这里填。」，
  提交结论弹窗（`run_popup`）在检测到该警告时追加「海拔出口（App 内）：跑步页 →「高级生成参数」→「学校基准海拔（米）」…」。

用户看到的两端（警告 + 出口）指向同一个控件；如果 captain 希望 `flow.rs` 的**源字符串**本身也带上这句，
那是一行 api 改动，留给 flow.rs 的 owner（t9）更稳妥。

## 优先级与 origin（避免自相矛盾）

输入框下方的说明与 `altitude::resolve_base_altitude` 逐条一致：

```
留空 = 未配置。优先级：手动海拔（单值/区间）> 学校基准 > 默认 82 m。当前生效：1050.0 m（学校基准海拔）
```

`当前生效` 直接来自 `describe_base_altitude(value, origin)`，所以：

- 只填学校基准 → `School`；
- 学校基准 + 手动海拔（或手动区间）并存 → 显示 `Manual`/`ManualRange`，学校基准仍在配置里但**不生效**
  （有专门测试 `manual_altitude_wins_over_school_base_but_school_stays_configured` 锁住这个语义）；
- 都没填 → `Default`（82 m），此时提交结论会出现那条警告。

## 对 t7 布局的影响

新行在**默认收起的折叠区内部**，收起时渲染树里根本不存在 → 主操作位置不变：

- `layout_tests::run_primary_action_is_visible_without_scrolling_on_a_phone`（360×640 + 412×720）**仍绿**；
- 展开后才多 3 行（输入行 + 优先级 + 警告出口），此时页面本来就在滚动位置，不影响主操作。

## 测试

新增 4 条（`ui::run::tests`）：

| 测试 | 覆盖 |
|---|---|
| `school_base_altitude_writes_config_and_blank_clears_it` | 填 `" 1050 "` → `Some(1050.0)`、origin=School；改空白 → `None`、回落 Default |
| `invalid_school_base_altitude_keeps_previous_value_and_reports` | `abc`/`-600`/`9001`/`NaN`/`1 050` 一律报错且**不覆盖**已存的 1050 |
| `manual_altitude_wins_over_school_base_but_school_stays_configured` | origin 顺序 + 学校基准不被抹掉 + 手动清空后学校基准接管 |
| `school_base_altitude_row_shows_priority_and_current_origin` | 无头：点开折叠区后，标签 / 优先级句 / `当前生效：1050.0 m（学校基准海拔）` 三者真的画出来 |

全量：`cargo test --release --lib` → **468 passed / 0 failed / 1 ignored**（19.64s）。

## 遗留

- `flow.rs` 源文案未改（见上）；UI 侧已双向对齐。
- 折叠区默认收起（t7 的决定）：警告的出口需要用户点开一次折叠区。若觉得还是太难找，
  下一步可以在提交结论弹窗里加「打开配置」按钮（需要 App 级跳转 + 展开状态，属 B 类改动）。
