# 应用内更新装不上 + 数据页把 10000 当次数（t17）

## ① 应用内更新：点完「下载并安装」，版本没变

### 代码核查结论（现在这条链路的真实状态）

| 检查项 | 结论 |
|---|---|
| FileProvider authority | `AndroidManifest.xml` 的 `.ApkProvider` = `org.nekosportsworld.tool.apk`，与 `ApkProvider.AUTHORITY` **一致** ✓ |
| 授权标志 | `Intent` 同时带 `FLAG_ACTIVITY_NEW_TASK` + **`FLAG_GRANT_READ_URI_PERMISSION`** ✓ |
| 权限 | manifest 已声明 `android.permission.REQUEST_INSTALL_PACKAGES` ✓ |
| Provider 暴露面 | `exported="false"` + `grantUriPermissions="true"`，只允许读 `update.apk`（`openFile` 校验文件名与 `mode` 以 `r` 开头）✓ |
| 包体校验（Java） | `verifyUpdatePackage`：包名、signer、`versionCode >= 当前` ✓（`<` 才拒） |
| 完整性（Rust） | 下载后 `update::verify` 已比对 `asset_size` + **SHA-256**（GitHub `assets[].digest` / TOFU pin）✓ |
| **静默失败点** | **Rust `install_apk` 丢弃 JNI 返回值** —— 调用失败（Activity 未就绪、方法签名不匹配）时界面什么都不发生 |
| **语义漏洞** | Java 只在 `incoming < current` 时拒绝；`incoming == current` 会放行 → 系统安装器「未安装」一句带过，用户看到的就是「装了但版本没变」（**与用户实测完全吻合**） |
| 缺步骤 | 没检查「安装未知应用」授权 → 未授权时 `startActivity` 成功但系统直接吞掉，表现同样是静默 |
| 缺出口 | 只有「下载并安装」一次机会，失败后只能去 GitHub |

### 改了什么

1. **Rust 不再吞错误**：`platform::install_apk(&str) -> Result<(), String>`（桌面返回明确的「桌面版没有系统安装器」）。
   `about.rs` 在 `Err` 时**弹 `PopupInfo`**：原因 + 安装包绝对路径 + 手动安装指引 + 去授权的说明；成功时状态栏提示「若提示未安装/被拦截，请允许本应用安装未知应用」。
2. **Java 三道前置拦截**（`MainActivity.installApk`）：
   - `versionCode` **相同或更旧** → 弹窗「这个更新包与已安装版本相同或更旧…系统不会覆盖安装，所以点多少次都不会变」+ 指向 Release 页核对 tag；
   - 未授予「安装未知应用」（`canRequestPackageInstalls()`）→ 弹窗 + **「去授权」按钮**（`ACTION_MANAGE_UNKNOWN_APP_SOURCES`）+ 安装包路径；
   - `intent.resolveActivity == null`（设备没有可用安装器）→ 弹窗 + 路径；
   - `startActivity` 抛异常 → 弹窗带**异常类名与 message**（此前只有一句「无法启动安装器」）。
3. **「再次安装」入口**：更新区在有 `update.apk` 时显示「再次安装已下载的更新包」+ 包大小与完整路径（`Android/data/<pkg>/files/update.apk` 实际路径由 API 给出，不写死），失败可反复点，无需重新下载。
4. 新增 Java 辅助：`incomingVersionCode()` / `currentVersionCode()` / `showInstallBlockedDialog()`（统一出口，标题「这次没能启动安装」）。

**版本一致性**：下载完整性继续由 `update::verify`（size + SHA-256）把关；`versionCode` 与 tag 的对应关系由 Java 侧在安装前用「包内真实 versionCode vs 已安装 versionCode」判定 —— 比读 `rel.tag` 字符串更可靠（tag 可写成 `v0.6.1`、`0.6.1-antiban` 等）。

### Android 上仍然需要用户手动做的（写清给用户）

1. **首次更新要先授权**：设置 → 应用 → 本应用 → **安装未知应用** → 允许。部分 ROM 在「安全中心/应用管理」里。
2. 如果系统弹「未安装 / 已被拦截」：
   - 先确认是不是**同版本**（本版会直接拦下并说明，不再让你白点）；
   - 用文件管理器打开：`Android/data/org.nekosportsworld.tool/files/update.apk`（界面上「再次安装」旁会显示真实路径，可直接照着找）；
   - 仍装不上且是**换签名发布**的版本：设置里卸载旧版，再从 Release 页装新版（卸载会丢本地会话，需要重新登录；签名变更时应用会弹专门的说明框）。
3. 精简 ROM / 安全软件会拦「未知来源安装」，需要在对应安全软件里放行。

## ② 数据页把 10000 当成了「有效次数」

### 核查结论

- 日志来自 `ui/jobs.rs::refresh_semester`：`"√ [semester] {} 有效 {}/{} 次，有效里程 {:.2} km"`，参数是 `s.semester_valid_count / s.semester_count`。
- 这些字段在 `api/semester.rs` 里由**三处** fallback 填充：汇总接口 → 个人完成度（`semesterValid`）→ 完成状态（`semesterGoalCount` / `semesterValidCount`）。
- `10000` 是本项目的**业务成功码**（`error: 10000`）。它出现在计数位置，说明某个 fallback 把「成功码对象」里的字段当成了次数（服务端字段布局变化 / 取到了错误层级）。
- 修法不猜服务端：**加一道「次数可信性」闸**，`0..=1000` 之外一律当没拿到，并留一条日志说明为什么丢。

### 改了什么（`src/api/semester.rs`）

- 新增 `pub fn plausible_count(v: i64) -> Option<i64>`：`0..=1000` 才算次数（10000 必然被判掉）；
- 新增 `count_field(source, key, log)`：取数后过这道闸，被丢时记 `[semester] {key}=10000 不像次数（10000 是业务成功码），已按 0 处理`；
- 三处 fallback（`semesterValid` / `semesterGoalCount` / `semesterValidCount`）全部改走 `count_field`；
- 顶层汇总的 `semesterCount` / `semesterValidCount` 也过同一道闸（`plausible_count(...).unwrap_or(0)`）。

### 测试

`api::semester::count_sanity_tests`：

- `business_success_code_is_never_shown_as_a_count`：`plausible_count(10000) == None`、边界 `0/1000/1001/-1`；`{"error":10000,"semesterValid":10000}` → 取数返回 `None` **且留有说明日志**；正常 `{"error":10000,"semesterValid":7}` → 7。
- `summary_counts_use_the_same_gate`：`10000→0`、`0→0`、`15→15`、`999→999`。

即：**给一份含 `error:10000` 的假响应，显示的次数不会是 10000**。

## 验证

`cargo check --lib --offline` exit 0 ｜ `cargo test --release --lib --offline` 全绿 ｜ `cargo fmt --check` exit 0 ｜ `git diff --check` exit 0。

**Java 侧限制**：本机没有 Android SDK，`android/java/**` 与 `AndroidManifest.xml` 不参与 `cargo check`；改动只做了结构自检（括号配平 90/90）与逐行复核，真机效果需要一次 Android 构建验证。manifest 未改（authority/权限原本就对）。
