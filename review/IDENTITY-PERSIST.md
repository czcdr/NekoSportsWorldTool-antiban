# 设备身份在「应用内更新后」丢失的排查与恢复机制

## 一、现象

用户在手机上**应用内更新并安装**之后，原本已保存（点过保存、杀进程重开仍在）的城市
「包头市」变回默认「大连市」。

## 二、排查结论（代码侧无删改，问题在文件层）

| 检查项 | 证据 |
|---|---|
| 全仓删除数据文件的调用 | 非测试代码里只有 `update.rs:572` 删 `.old`（更新残留）；Android 分支不调 `apply`，只把 APK 写进私有目录交给系统安装器 |
| `load_identity_for_platform`（`api/model.rs`） | 只补 device_id / 安装时间 / MAC / 机型 / 指纹，**不碰 city 与锚点** |
| `crypto/header.rs` | v0.5.0..HEAD 零 diff（默认城市常量没变） |
| 结论 | 代码不会删/覆盖 `identity.json`；它「没了」或「被换成默认值」只可能发生在系统/文件层（应用内更新走 install，私有目录被清/被替换） |

**代码改变不了文件层的事，能做的是「丢了能恢复」+「不用想起来再点一次保存」。**

## 三、修复（三层备份 + 容错 + 自动落盘）

### 1. 冗余备份（`save_identity`）

保存成功时同时写三处：

| 位置 | 内容 | 作用 |
|---|---|---|
| `identity.json` | 完整身份 | 主文件 |
| `identity.json.bak` | 完整身份（同一次写入的副本） | 主文件被写成半截时救回 |
| `config.json` 的 `identity_backup` | 城市 / 锚点经纬度 / device_id / 备份时间 | 另一份文件里的最小字段集 |

- 主文件写失败 → 整体 `Err`（调用方弹窗）；后两层失败只记日志，不误报「保存失败」。
- 城市为空时**不写**备份（没有可恢复内容），也不会把已有的有效备份冲成空值。

### 2. 双文件容错（`load_identity_for_platform`）

读取顺序：`identity.json` → `identity.json.bak` → `identity.json.corrupt` → config 备份 → 默认值。

- 主文件解析失败会先被改名成 `.corrupt` 留档并告警（沿用老行为），然后继续往后试；
- **`device_id` 优先从备份恢复**：换 device_id 等于让服务端把这台手机当新设备，
  直接撞 10121（设备风险）—— 这比城市错了严重得多；
- 城市判据比「文件是否存在」更宽：**城市为空或等于系统默认值**且备份里有别的城市 → 按备份恢复，
  并打日志「已从 config.json 备份恢复城市=包头市」。用户报的正是「文件在、城市变回默认」这种形态。

### 3. 城市/锚点自动落盘（`ui/device.rs`）

- 输入停手 **500ms**（`AUTOSAVE_IDLE`）且值合法才写盘；
- 每个按键都重置计时（快照比较）；打字中间态（「包」「包头」）**绝不写盘**；
- 「保存」按钮保留；自动保存与按钮走同一套判据（`location_value_problem`：城市非空、经纬度在范围内）；
- 写失败**弹 `PopupInfo`**（不只写状态栏），且不每帧重试（否则会连续弹窗）。

### 4. 启动可诊断

启动打一行：`[identity] 城市=包头市（来源：identity.json / config 备份 / 默认值）`；
界面设备页也显示「身份来源：…」，并在「备份城市 ≠ 当前城市」时提示会以备份为准。

## 四、测试（全离线）

`api::model::identity_backup_tests`：

| 用例 | 验证 |
|---|---|
| `config_backup_restores_a_defaulted_city` | 主文件被换成默认值时，从 config 备份恢复城市+锚点+device_id，来源标为「config 备份」 |
| `truncated_identity_json_recovers_from_rolling_backup` | 半截 `identity.json` → 从 `.bak` 恢复，**device_id 不变**，坏文件留档为 `.corrupt` |
| `corrupt_identity_and_bak_fall_back_to_config_backup` | 主文件与 `.bak` 都坏 → 仍能用 config 备份救回 |
| `missing_everything_falls_back_to_defaults_with_a_warning` | 全丢 → 回落默认值，`identity_warning()` 给出可操作文案（含「默认」「设备页」） |
| `empty_city_does_not_overwrite_a_useful_backup` | 空城市不覆盖已有有效备份 |

`ui::device::autosave_tests`：

| 用例 | 验证 |
|---|---|
| `autosave_waits_for_idle_then_persists_and_reads_back` | 120ms 只等不写；到 500ms 写盘；读回城市/锚点一致，device_id 未被动 |
| `invalid_location_values_never_reach_the_disk` | 空城市、纬度 200 → 判为非法，磁盘字节完全没变 |
| `unchanged_location_is_a_noop` | 没有改动时 Skip（不会每帧判定/写盘） |

## 五、已知边界

- 三层备份都在**同一个数据目录**里：如果系统把整个私有目录清掉（而不是只换 `identity.json`），
  三层会一起没 —— 那种情况只能靠用户重填（界面会明确提示）。
- 自动落盘只覆盖「城市/锚点」；机型、device_id 等仍需手动「保存」（改动它们要连带重置派生指纹）。
