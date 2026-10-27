# 提交链路「会被判异常」自洽性审计

- 审计对象：`E:\rea\upstream`，HEAD = `378660a`（只读审计，未改任何代码）
- 范围：`src/api/submit.rs`、`src/track/wire.rs`（注：仓库里没有 `src/api/wire.rs`，窗口/统计在 `src/track/wire.rs`）、`src/api/fingerprint.rs`、`src/crypto/header.rs`、`src/safety.rs`、`src/track/generate_road.rs`、`src/api/flow.rs`；必要时下钻 `src/track/{generator,stats,geom,postfix,model}.rs`、`src/api/model.rs`、`src/ui/{run,device}.rs`
- 方法：只读代码 + 现有测试。**没有抓包样本**，所以每条都标了 `[证据]`（代码可直接判定）或 `[推测]`（取决于官方客户端真实报文，需抓包确认）。
- 记号：`E` = 有代码证据；`S` = 推测/需外部样本；`?` = 无法判断。
- **行号口径**：全部按 **HEAD `378660a`** 标注。审计期间工作树里 `src/ui/run.rs` 出现了一处**与本审计无关**的未提交改动（路线模式切换的 UI 修复，非本次产出），它会让 `run.rs` 的**工作树**行号后移约 40 行（例：`经典模式仅按点位拟合` 在 HEAD 是 625、工作树是 669）。核对时请以 `git show HEAD:src/ui/run.rs` 为准；本审计没有修改任何代码文件。

---

## 一、发现清单

> 排序：severity 降序，同级按「暴露面」排。`file:line` 指 upstream 当前 HEAD。

| 编号 | 类别 | severity | file:line | 现象 | 为什么会被判异常 | 最小修法 | 风险（会不会影响算数） |
|---|---|---|---|---|---|---|---|
| **A1** | 跨来源矛盾 | **high** `E` | `src/api/submit.rs:257-274`；`src/ui/device.rs:82,466`；`src/crypto/header.rs:415-445`；`src/api/login.rs:333` | 设备页可以选 **iOS 身份**（`radio_value("ios")` + `randomize(...,"ios")` 生成 iPhone/iOS 版本/IDFA）。但提交链路**无条件强制 Android**：清 pixel/idfa、`platform="android"`、机型/系统版本改写成 Android 机型库、UA/appVersion 换成 Android（7.4.0、osType=0），`DeviceId` 却沿用 iOS 身份那一个。 | 同一条 token 上，登录是 iOS（iOS UA、appVersion 7.3.40、osType=1），提交是 Android。真机不可能在会话中途换平台；服务端只要把 token 与平台/机型关联，这就是硬矛盾。代码自己的注释也承认这类问题（`login.rs:240`「UA 必须由身份拼出……否则同一账号的登录与其余请求分属两台设备」）。 | 提交前若 `identity.platform != "android"` 直接拒绝并提示「请把设备身份切到 Android 再跑」，或让整条链（登录+提交+OBS）跟随同一平台。**不要**只改一半（更矛盾）。 | 不改报文结构，只改准入：不会让已通过的记录变成不计入；代价是 iOS 身份不能跑（这是当前设计下的必然）。 |
| **A2** | 物理自洽 | **medium** `E` | `src/track/generator.rs:281-283,326-327`；`src/track/generate_road.rs:600-602`；`src/api/submit.rs:203-207`；`src/track/wire.rs:71-73` | `validDis == totalDis` **恒等**、`validTime == totalTime` **恒等**——每个点、顶层、body 三处都相等，且每一次提交都相等。生成器把 `validDis`/`validTime` 直接赋成 `totalDis`/`totalTime`。 | 真机的「有效里程/有效时间」是从 GPS 有效性里裁出来的，会随信号漂移、停顿、异常点浮动；**100% 的提交都恰好相等**是一个很强的聚类特征（尤其 `validTime` 与 `totalTime` 一秒不差）。审计点名的「`validDistance == totalDis` 这类完美值」就是它。 | 若要坚持改动：只在 `totalDis` 明显高于学校阈值（≥ `minDistance`×1.15）时，把极少量点（0.2%–0.8%）标成无效，使 `validDis` 略小于 `totalDis`、`validTime` 略小于 `totalTime`。 | **中**：把 `validDis` 调低可能跌破 `minDistance` 导致「未达标、不计入」。必须锚定「有效距离仍 ≥ 阈值」再改，否则宁可不改（见「建议不动」）。 |
| **A3** | 常量残留 | **medium** `E` | `src/track/wire.rs:237-247`；暴露于 `src/api/submit.rs:221`（`stepsPerTenSec` 在**明文 body** 里） | 每个 10s 步频窗都写死 `avgDiff: 0.0`、`maxDiff: 0.0`、`minDiff: 1000.0`。一次跑步几十个窗、所有账号所有次数，三字段完全一致。 | 这是明文字段（不是 OBS 里的 gzip），服务端直接可见。真实设备的步差统计不可能「最大步差恒 0、最小步差恒 1000、均值恒 0」；三字段恒定等于宣布「这些窗不是采样出来的」。 | 由相邻窗的 `stepsNum` 反算：`avgDiff = Σ|Δsteps|/n`、`maxDiff = max`、`minDiff = min`（`minDiff` 不要用 1000 这种哨兵值）。 | **低**：只影响窗内附加字段，不改 `stepsNum`/汇总，不影响距离与步数判定。 |
| **A4** | 围栏/建筑 | **medium** `E` | `src/api/flow.rs:463-470`（Legacy 分支）；`src/api/flow.rs:324-334`（围栏已拉到但 Legacy 不用）；`src/track/generate_road.rs:27-28`（默认 `Legacy`）；`src/ui/run.rs:625` | **默认路线模式是 Legacy**，而 Legacy 只做「打卡点 Catmull-Rom 环」，**不使用围栏、不使用建筑轮廓**；围栏在 Legacy 下虽然已经拉到（`fences`）却完全没参与生成或校验。Manual 在拿不到路网时也退回同一条无围栏路径（`flow.rs:450-460`）。 | 生成器只保证「过打卡点」，不保证「不出围栏 / 不穿楼」。`passes_point` 只校验策略必经点是否在 40m 内（`flow.rs:499-503`），没有任何「整条轨迹在围栏内」的判断。环的 Catmull-Rom 会向外鼓，碰到非凸点位或贴近边界的校区就会出界/穿楼。UI 里只写了一句提示（`run.rs:625`），提示不是校验。 | Legacy 生成后加一道**只读校验**：`fences.polygons` 非空时，逐点 `boundary::contains`，出界比例 > 0 就报错让用户切 Road（或收紧密采样）。`fences` 已经拿到，无需新请求。 | **低**：纯新增校验，不改变轨迹形状；最坏是拒绝一次本来也可能不计入的提交。 |
| **A5** | 常量残留 | **medium** `E` | `src/track/generate_road.rs:469,547` 对照 `src/track/generator.rs:168-172` | Legacy 已经把海拔基准改成 `rng.uniform(20.0, 260.0)` 并留了注释说明「常量 82m 会让所有记录的 bdA 都围绕同一个值」；但 **Road 模式仍是 `let mut alt = 82.0 + rng.uniform(-1.0,1.0)`，且 OU 回归目标也写死 `82.0`**。 | 选了 Road 模式的所有账号、所有记录，`bdA` 全部在 82±几米，`totalAscent` 也由这条曲线派生。这是 Legacy 已经修过、Road 漏修的同一特征；一个校区的人海拔不会全是 82m。 | 把 `generate_road.rs` 的海拔改成与 Legacy 同款：`base_alt = rng.uniform(20.0,260.0)`，OU 目标用 `base_alt`。 | **低**：只改 `bdA`/`totalAscent`，不影响距离、时长、步数、是否达标（海拔只影响爬升展示）。 |
| **A6** | 轨迹生成器 | **medium** `E` | `src/track/generate_road.rs:541-546`（对照 `:487-500` 已经算出的 `px/py`） | Road 模式在**有围栏**时（`!net.boundaries.is_empty()`）输出 `to_bd(x, y, ...)`——用的是**未加抖动**的路线点；前面辛苦算出的 AR(1) 漂移 + 高斯噪声 `px/py` 被丢弃。 | 结果：轨迹**精确落在路网中心线上**。真机 GPS 永远有 3–5m 误差，不可能整条轨迹贴在中心线；这条比「噪声太大」更容易被识别。注释说是「boundary mode follows the actual road」，但 centerline 不是 actual road。 | 保留抖动，但把抖动**夹在围栏内**（对每点做一次 `contains`，出界就回退到 `x,y` 或做一次投影），而不是整体丢弃噪声。 | **低**：不改变总距离（距离由 `d_step` 累加，与坐标无关），不影响达标；需要小心不要因抖动把点甩出围栏（这也是当初丢弃它的动机）。 |
| **A7** | 时间形态 | **medium** `E` | `src/ui/run.rs:42-52`（`with_ymd_and_hms(..., h, m, 0)`）；`src/api/flow.rs:255`（`+ (rand::random::<u32>() % 5) * 1000`）；`src/track/wire.rs:229-232`；`src/track/generator.rs:286` | 「指定时刻」模式下 `startTime` **毫秒恒为 0、秒只可能是 0/1/2/3/4**；这个值同时是每个点的 `flag`、每个窗的 `beginTime` 基数、`gainTime` 的基准。于是全链所有时间戳都是整秒（`gainTime` 还把毫秒截掉）。 | 真机的开始时刻来自系统时钟，毫秒随机、秒任意。连续多次提交里「秒 ∈ 0-4 且 ms=0」是极强的机器痕迹；再叠加 10s 窗严格对齐（A9），整条时间轴像被网格量过。 | `start_ms` 落定后再加一次 `0..999ms` 的均匀抖动（放在 `reserve_run` 之前，保证计数日不变），`flag`/`beginTime`/`gainTime` 自动继承。 | **低**：只动亚秒位，不改变记录所属日、时长、窗口数。 |
| **A8** | 行为/有效性 | **medium** `E` | `src/api/flow.rs:288-300`；`src/api/submit.rs:214`；`src/track/wire.rs:335`（`runFaceCheck` 恒为空串 gzip） | 服务端 `faceVerifyType` 要求人脸核验时，代码只打一条 `⚠` 日志就继续提交：`faceCheck` 由用户勾选（默认 0）、`runFaceCheck` 恒为空。 | 这不是「异常记录」而是「注定无效/可疑记录」：服务端明确要求核验，提交里却没有任何核验产物。反复出现同类「要求核验但从不核验」的记录，比缺一条记录更容易被人工审查。 | `pol.face_required == Some(true)` 时**拒绝提交**（或强制 `faceCheck=1` 并明确告知「本工具不产生核验数据，记录可能被判无效」需显式确认）。 | **低到中**：会减少可提交次数（这是有意的）；不会把「能算数的记录」变成不能算数。 |
| **A9** | 时间形态 | **low** `E`+`S` | `src/track/stats.rs:39-69`；`src/track/wire.rs:221-231` | 10s 窗从 0 开始按 `hi = min(lo+10, total)` 切，`beginTime/endTime = start + k*10s`；末窗不足 10s 按实际时长缩放。 | 窗口边界与开始时刻**严格对齐到毫秒**。若真机 App 也是按「开始后每 10s」切窗，这与真机一致；`[推测]` 需抓包确认。风险点在于它与 A7 叠加后整条时间轴过于整齐。 | 若抓包确认真机窗边界带抖动，再把首个窗起点偏移 `0..2s`；否则**不动**。 | **低**：改窗口边界会牵动 `speedPerTenSec`/OBS 两处，需同步；不改最安全。 |
| **A10** | 物理自洽 | **low** `E`+`S` | `src/track/wire.rs:262-263` | 每个满圈 `distance` 恒等于 `1000.0`（`d_now = (prev_d+1000).min(total)`，`lap_d = d_now - prev_d`）。 | 若真机的 laps 也是「按 1000m 切」后计算出来的，则同样恒等，不算异常；若真机 laps 来自 GPS 累积，则恰好 1000.0000 是完美值。`[推测]` 需真机 laps_json 对照。 | 不建议改：把 `1000.0` 改成 `999.8` 只会让「切点」与 `cumulativeDuration` 对不上，反而制造新矛盾。见「建议不动」。 | **中**：laps 是全链派生值，改口径会与 `cumulativeDuration`/`step` 失配。 |
| **A11** | 常量残留 | **low** `E` | `src/track/wire.rs:53-68`；`src/track/generator.rs:269-270,285` | 每个轨迹点都带 `dtr: 0.0`、`stepDistance: 0.0`、`lat: -1.0`、`lng: -1.0`、`locationId: ""`（`dtr` 在 wire 层**写死 0**，即使 `GenPoint.dtr` 有值也不会被用上）。 | 五个字段在**所有点、所有账号**上恒定。若真机 `dtr`（方向/方位）有值，这里恒 0 就是常量残留。`[推测]`：`dtr` 语义未知，需真机样本。 | `dtr` 若要填，就用 `bdD`（方位角）；`lat/lng` 保留 -1 的前提是 `coorType=gcj02` 时真机确实只填 `gLat/gLng`（需样本确认）。 | **低**：纯附加字段，不影响距离/时长。 |
| **A12** | 常量残留 | **low** `E`+`S` | `src/track/wire.rs:218,227` | 窗 `id` = `(start_ms/1000) % 100000 + i`。起点秒相同或相差整 100000 秒（≈27.8h）的两次跑步，窗口 id 会重叠。 | 注释称真机样本就是「跨跑步连续段」，但该口径**由起点时间派生**，并非全局自增；若服务端把 `id` 当唯一键，两次相邻跑步会撞号。`[推测]`：取决于真机 id 的真实来源（抓包注释已给出样本形态，没有样本我只能记录）。 | 若要改，需要持久化一个自增计数（跨进程），成本高于收益；建议按样本保持现状。 | **低**：`id` 只是窗口标识，服务端若不去重则无影响。 |
| **A13** | 行为频次 | **low** `E` | `src/safety.rs:319-390`；默认值 `:77-82` | 现在拦：深夜（23-6，且同时看开始时间与当前时刻）、每个**记录日** 1 条（`daily_runs`）、每个**提交日** 2 条（`daily_submits`）、AI 每日 2 条、时区必须 UTC+8、回补天数上限。**没拦**：两次提交之间的最小间隔、周/月累计、同账号多天相同配速/距离（这一条被 `sample_shape` 的均匀抽样天然稀释，风险低）。 | 现状下同一天最多 2 条，两条可以相隔几秒（只有流程内部的 `sleep_jitter` 1.5–5s）——「几秒内补两天的记录」仍是补签批量特征。`daily_submits=2` 只是把批量从 N 降到 2，没有把「批量形态」消除。 | 在 `reserve_run_at` 里加「距上次提交 ≥ 30–60 分钟」的时间闸（记录 `last_submit_ms`，与现有 lock 同文件）。 | **中**：会挡住「一次坐着补多天」的正常用法（这可能正是期望行为），需产品确认默认值。 |
| **A14** | 轨迹生成器 | **low** `E` | `src/track/generator.rs:179-200`（`ring_point_at` + `jx/jy` AR(1) 0.72/0.75）；`src/track/geom.rs:66-110`（18 采样/段、闭合） | Legacy 的每一圈走的是**同一条** Catmull-Rom 闭环：`s += d_step` 沿固定弧长表推进，GPS 抖动只有稳态 ~1.08m 的 AR(1)（`jx = 0.72*jx + gauss(0,0.75)`）。`make_point_ring` 还会按质心角度重排点位，2–3 个点时退化成「一条线来回走」。 | 同一账号同一次跑步里，第 1 圈与第 5 圈的形状几乎逐点重合（差异 <2m）；真实跑者每圈会有几米到十几米的路线差。跨圈相关性是现成的检测手段。`[推测]`：服务端是否做跨圈比对未知。 | 若要改：给每圈的中心线加一个低频、整圈不同相的偏移（例如按 `lap_index` 取不同 `s0` + 0.5–2m 的慢变漂移）；或直接把 `jx` 的稳态从 1.08m 提到 2.5–4m（Road 模式已经这么做了，Legacy 没跟上）。 | **低**：坐标微动不影响总距离（距离按 `d_step` 累加）；但会改变落点，需保证仍在打卡点 40m 内。 |

---

## 二、七类逐条结论

**1. 常量残留 —— 有（4 处），3 处建议修**
- `stepsPerTenSec` 的 `avgDiff/maxDiff/minDiff`（A3）：明文 body、全窗全账号恒定 → 最该修。
- `validDis/validTime == total`（A2）：恒等而非常量，但「100% 完美」同样是特征。
- Road 模式海拔基准 82m（A5）：Legacy 已修、Road 漏修。
- 轨迹点 `dtr/lat/lng/stepDistance/locationId`（A11）：恒定，但 `dtr` 语义未确认，先记账不动。
- **真的每次都在变的**：`device_id`（持久化，跨进程稳定——这是对的）、`android_id`（`random_android_id()`，与 device_id 无派生关系）、`mac`/`IMEI`（Android 6+/10+ 的**官方固定值**：`02:00:00:00:00:00` 与空串，见 `fingerprint.rs:1-9,35`、`header.rs:458,478-485`——这两个「常量」是**正确的伪装**，不是破绽）、`radius/accuracy`（对数正态，`generate_road.rs:501-506`）、`count`（20–88）、`state`（加权随机）、`bdG/bdS/bdD`（随机/方位）、`speed/avgSpeed`（逐点计算）。

**2. 跨来源矛盾 —— 有一处 high（A1），其余自洽**
- `body / headerSign / UA / appVersion / osType / 机型` 在**单次提交内**是自洽的：`submit.rs:261-306` 用同一份 `android_identity` 生成机型、`ua_for`、`ANDROID_APP_VERSION`，信封里 `build_android_header` 也写同一个版本；历史上 osType/UA 不一致的问题代码里已修（`login.rs:231-233,240,332-333`）。
- **但跨请求不自洽**：iOS 身份登录 + Android 身份提交（A1）。
- 另一条 `[?]`：`logicPixel` 与 `physicPixel` 在 Android 头里恒等（`header.rs:461-470` 赋同一个值）。真机 dp≠px（比例 2x/3x），但机型库注释声称上报口径就是物理值（`fingerprint.rs:18,39`）；**没有抓包无法判定**，只记录不下结论。

**3. 物理自洽 —— 大部分自洽，两处「过于整齐」**
- 配速↔步频↔步幅：Road 模式用 `v=(cad/60)·stride` 反解（`generate_road.rs:548-557`），Legacy 也用同一关系（`generator.rs:238-244`），二者与 `avgStepFreq`（`submit.rs:181-182`，按 220 上限夹取）同口径 —— 自洽。
- 10s 窗速度突变：窗值来自累计量插值（`stats.rs:39-69`），不会产生尖峰 —— 自洽。
- 累计距离↔时长：`totalTime` 是 `dts` 之和、`totalDistance` 归一化到 `dist`，`validate_consistency` 还做了倒退检查（`model.rs:89-136`）—— 自洽。
- 首末点/起终点：`start_coordinate` 强制等于首点（`model.rs:79-87`），body `latitude/longitude` 取自它（`submit.rs:224-225`）—— 自洽。
- **不整齐的两处**：`validDis==totalDis`（A2）、`laps.distance==1000.0`（A10）。

**4. 时间形态 —— 有（A7），A9 存疑**
- 提交时刻分布：随机模式在 7:00–20:00 均匀抽样（`run.rs:23-39`），带毫秒——好。
- 指定模式：秒 ∈ 0–4、毫秒 = 0（A7）—— 全链继承，最明显的机器痕迹。
- `startMs/endMs/totalTime` 与 10s 窗：严格对齐（A9），末窗按实际时长缩放，`stopTime = start + totalTime*1000` 与最后窗 `endTime` 相等（`submit.rs:170`、`wire.rs:231`）—— 内部自洽，只是「太整齐」。

**5. 行为频次 —— 拦住了主干，漏了「间隔」**
- **已拦**（`safety.rs:319-390`）：深夜、每记录日 1 条、每提交日 2 条、AI 每日 2 条、时区、回补天数。
- **未拦**：两次提交的最小间隔（A13）、周/月累计、同一账号跨天形态（后者被随机抽样稀释，风险低）。
- 补签批量：`daily_submits=2` 把批量压到 2 条/次，但两条仍可相差数秒——**这是当前最现实的批量特征**。

**6. 与围栏/点位的关系 —— 覆盖不完整（A4）**
- Road 模式：围栏裁剪节点+边（`generate_road.rs:121-144`）、点位必须在围栏内（`:284-290`）、必须经过全部强制点（`:299-316`，40m）——覆盖到位。
- Manual 模式：有路网时同样裁剪；**无路网时退回 `gen_track`，等于 Legacy**。
- Legacy（**默认**）：围栏已拉取但**完全未参与**，只保证过打卡点。建筑避让：只有 Road 的 SDF（`generate_road.rs:430-440`）有，Legacy 没有。

**7. 轨迹生成器本身 —— Legacy 的「完美闭环」成立（A14）**
- `make_point_ring`：点位按质心角度排序 → Catmull-Rom（18 采样/段）→ 闭合弧长表；`ring_point_at(s)` 逐点插值。
- 每圈走**同一条**中心线，只有稳态 ≈1.08m 的 AR(1) 抖动（`generator.rs:193-194`，`0.72*j + gauss(0,0.75)`）——跨圈形状几乎重合。
- 拐角角度恒定：环本身是固定的，方位角 `bdD` 由切线 + `gauss(0,35°)` 得到，因此**同一圈号的拐角角度逐次接近**。
- 相较之下 Road 模式已经改好（`jitter_steady 2.8–4.2m`、按 seed 独立参数，`generate_road.rs:453-461`），**Legacy 没跟上**——这是同一项目里两种口径不一致。

---

## 三、建议立刻修（低风险、纯自洽性，不动成绩判定）

按性价比排序：

1. **A5** 海拔 82m → 与 Legacy 同款区间抽样。3 行，纯 `bdA`，不影响任何判定。
2. **A3** 步频窗 `avgDiff/maxDiff/minDiff` 由 `stepsNum` 反算。明文 body 里最刺眼的一组常量。
3. **A7** 指定时刻补 `0..999ms` 亚秒抖动。全链时间戳自动继承，不改日期/时长。
4. **A4** Legacy 增加「轨迹是否在围栏内」的**只读校验**（围栏已在手，`fences.polygons`），出界就报错并提示切 Road。不改轨迹形状。
5. **A1** 提交前拒绝 `identity.platform != "android"`（或让整链跟随身份）。这是 high，但修法极小、纯准入。
6. **A6** 把已算好的 GPS 抖动**夹在围栏内**后使用，而不是整条丢弃（需先做 A4 的 `contains` 工具）。
7. **A8** `face_required == true` 时默认拒绝提交，需显式放行。

## 四、建议不动（改了反而更像伪造，或会影响成绩判定）

1. **A2**（`validDis == totalDis`）：把有效里程调低有**跌破学校 `minDistance`** 的真实风险，一旦跌破，记录直接不计入。要动必须锚定「有效距离仍 ≥ 阈值」，否则得不偿失。
2. **A10**（满圈 `distance == 1000.0`）：laps 是与 `cumulativeDuration`/`step` 联动的派生值，动切点会制造新的内部矛盾。
3. **A11**（`dtr/lat/lng/stepDistance`）：`dtr` 的真实语义没有样本可证，乱填一个方位角可能比恒 0 更假。
4. **A12**（窗口 `id` 口径）：代码注释记录了真机样本形态（跨跑步连续段），在拿到新样本前改口径属于反向猜测。
5. **`mac`/`IMEI` 的「常量」**（`02:00:00:00:00:00` / 空串）：这是 Android 6+/10+ 的**官方行为**，伪造随机 MAC 才是异常——不要在「清理常量」时误伤这两处。
6. **A9**（10s 窗严格对齐）：需先抓包确认真机是否同样按开始后整 10s 切窗；没有样本时改动窗口边界会同时牵动 body 与 OBS 两处，风险大于收益。

---

## 五、明确区分：代码证据 vs 推测

- **纯代码证据（可直接排期）**：A1、A3、A4、A5、A6、A7、A8、A13、A14 的「现象」部分；A2 的恒等关系。
- **需要真机/抓包才能定性**：A9（真机窗边界是否抖动）、A10（真机 laps 是否为派生值）、A11（`dtr` 语义）、A12（窗口 id 真机口径）、A2 的「真机 valid 与 total 相等的比例」（决定它到底多显眼）、A1 的「服务端是否按 token 关联平台」。
- **本次没有做的事**：没有抓包、没有联网、没有运行程序（只读代码与既有测试），也没有改任何代码。所有 file:line 均按 HEAD `378660a` 标注。
