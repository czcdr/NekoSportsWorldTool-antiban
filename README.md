<h1 style="font-size: 48px;">别倒卖了😭</h1>

# NekoSportsWorldTool

![Release](https://img.shields.io/github/v/release/czcdr/NekoSportsWorldTool-antiban?label=release)
![Build](https://github.com/czcdr/NekoSportsWorldTool-antiban/actions/workflows/release.yml/badge.svg)
![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-blue)

运动世界校园版全协议自动化工具，纯 Rust 单文件，GUI / CLI 双模式。

> ⚠️ 仅供学习研究，请勿用于违反学校规定的场景，后果自负。

## 这是什么项目（务必先读）

- **上游（原作者）**：[YanamiNeko/NekoSportsWorldTool](https://github.com/YanamiNeko/NekoSportsWorldTool)
  代码、协议实现与设计全部来自上游，功劳是原作者的。
- **本仓库**：[czcdr/NekoSportsWorldTool-antiban](https://github.com/czcdr/NekoSportsWorldTool-antiban)
  只是**接续上游继续维护与优化**（上游长期未更新）：修已知缺陷、补测试、补防异常细节、
  增加少量可用性功能（如登录的手动过码、瓦片矢量化路网）。不改协议方向、不另立门户；
  与本仓库相关的改动记录见 `review/` 下的审计与变更文档。
- **当前所有版本都是测试版（beta）**，没有稳定版承诺：功能随时可能调整或回退，
  也可能因为服务端策略变化而失效。

> 🚨 **请不要用自己的主账号登录。**
> 该服务端存在风控与事后清算机制（社区反馈过「参数异常 201」「灰色轨迹被批量清除」
> 「账号被清算」）。本仓库的改动**没有在真实账号上做过完整验证** ——
> 错误的使用方式可能让跑步记录被判为异常并清除，甚至影响账号本身。
> 想试请用**不重要的账号**，先一次只跑一条、观察几天再继续。

- 遇到问题请开 Issue。**贴日志时不要贴账号、密码、token**：日志里的敏感字段会自动打码，
  但仍请自己再检查一遍。
- 提 Issue 时请带上：版本号（关于页可见）、平台（Android/桌面）、复现步骤、日志里
  最后 10–20 行（登录问题请附 `[geevalidate]` 开头的那几行）。

## 功能

- **跑步**：一键全链（策略 → 打卡点 → 轨迹 → 提交 → OBS → 验证），轨迹按打卡点自然生成，开始时间支持随机或指定到前 3 天
- **AI 运动**：任务 / 自由练习双模式，批量补签（前 60 天 × 多项目）
- **数据**：学期完成度、违规自查、排行榜（个人/班级/院系 × 日/月、室内榜、历史榜）、个人主页
- **登录**：自动过码（缺口识别）与手动过码（GT4 滑块人工拖动，顶栏切换）
- **自动更新**：桌面端检查/下载/自替换并重启（GitHub Release 直连，无镜像）；Android 下载 APK 调起系统安装器；启动检查模式可选（静默/询问/关闭）

## 界面

![GUI](docs/screenshot.png)

## 技术特点

- 完整实现 NetSecKit 信封加密与响应解密，每个响应强制 RSA 验签（fail-closed）
- 人机验证全程本地完成，不依赖任何外部服务
- 轨迹生成器：打卡点拟合环 + 自然速度曲线 + 哨兵点 / 断崖 / 点位吸附
- 设备身份持久化（DeviceId / 安装时间 / MAC），稳定不易触发风控
- GUI 与 CLI 共用同一套协议层，数据文件互通

## 使用

双击 exe 进入 GUI；CLI 用法：

```text
NekoSportsWorldTool login --user <手机号> --pass <密码> --remember
NekoSportsWorldTool run                                  # 一键跑步（--altitude 17.2 固定海拔，或 11.6-22.8 映射到区间）
NekoSportsWorldTool ai --sport 2 --score 26000           # AI 运动
NekoSportsWorldTool rank main --type 1 --sort 1          # 排行榜
NekoSportsWorldTool update [--check]           # 自动更新（--check 仅检查不下载）
NekoSportsWorldTool template --file run.gpx   # 本地分析真实记录海拔（不会上传）
NekoSportsWorldTool help                                 # 全部命令
```

`template` 只读取用户手动选择的本地 GPX/JSON 文件，输出采样点、海拔范围、累计上升和累计下降，
不会登录、访问服务器或把模板记录接入跑步上传流程。

手动海拔支持三种状态：留空使用自动海拔；填写单个数字（例如 `17.2`）让所有轨迹点使用固定海拔；填写区间（例如 `11.6-22.8`）将生成的海拔曲线映射到该上下限内，同时保留轨迹的相对起伏。

青龙定时任务：首次手工 `login --remember` 一次，之后定时挂 `run` 即可。

## 常见问题

- **提示未登录 / 401**：先执行 `login --remember`（GUI 勾选记住密码等价），会话失效会自动重登
- **10121 设备风险**：设备身份是持久化的，删除 `identity.json` 等于换了新设备，不要频繁删
- **10603 点位限流**：点位接口 5 分钟限 3 次，程序内置 300s 缓存，正常使用不会触发
- **榜单为空**：当天还没有人产生有效里程，查询会自动回退最近 3 天
- **检查更新失败**：更新仅直连 GitHub，网络不通时到 [本仓库 Release](https://github.com/czcdr/NekoSportsWorldTool-antiban/releases) 手动下载（自动更新只查本仓库，不查上游）
- **手动更新方式**：Windows 下载新的 `nekosportsworldtool.exe` 后退出软件，用新文件替换旧文件；请保留 `config.json`、`identity.json`、`points_cache.json` 等配置文件，不要删除或修改。
- **人机验证失败（滑块过不去 / 10003）**：服务端会对同一出口连续失败逐渐降级，**不要连点重试**。
  先在顶栏把「过码方式」切成**手动过码**，弹图后把方块拖进缺口再提交；仍失败就停下，
  隔一段时间或换个网络再试。反馈时请附日志里 `[geevalidate] 10003 原始响应：…` 那一行
  （里面只有错误码与服务端消息，不含账号密码）。
- **杀软误报**：未签名编译产物可能被误报，自行判断后加白

## 构建

```bash
cargo build --release                        # GUI + CLI
cargo build --release --no-default-features  # 仅 CLI
```

推 tag 自动构建 Windows / macOS / Linux 四平台产物并发布 Release（见 `.github/workflows/release.yml`）。

## 许可证

[CC BY-NC 4.0](LICENSE) © YanamiNeko（禁止商用）

本仓库是上游的接续维护分支，遵循同一许可；上游版权归原作者所有。
