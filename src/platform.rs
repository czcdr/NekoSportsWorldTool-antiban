//! Platform services shared by the existing UI and the Android host.

#[derive(Clone, Copy, Debug)]
pub enum InputKind {
    Text,
    Password,
    Integer,
    Decimal,
}

pub fn data_dir() -> std::path::PathBuf {
    #[cfg(test)]
    if let Some(path) = TEST_DATA_DIR.with(|p| p.borrow().clone()) {
        return path;
    }
    #[cfg(target_os = "android")]
    {
        crate::android::data_dir()
    }
    #[cfg(not(target_os = "android"))]
    {
        desktop_data_dir()
    }
}

/// 桌面的数据目录。
///
/// - Windows：**保持便携语义**（exe 同目录）——解压即用、可放 U 盘，这是既有行为。
/// - Linux / macOS：走用户数据目录（`$XDG_DATA_HOME` / `~/.local/share` /
///   `~/Library/Application Support`）。装在 `/usr/local/bin`、`/opt`、AppImage
///   这类只读位置时，exe 同目录写不进去，登录态/身份/防重放状态会全部静默失效，
///   而且违反 FHS。
/// - `NSWT_DATA_DIR` 可强制指定（便携部署、多账号隔离、测试）。
#[cfg(not(target_os = "android"))]
fn desktop_data_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("NSWT_DATA_DIR") {
        let dir = dir.trim();
        if !dir.is_empty() {
            let path = std::path::PathBuf::from(dir);
            if std::fs::create_dir_all(&path).is_ok() {
                return path;
            }
        }
    }
    #[cfg(windows)]
    {
        exe_dir()
    }
    #[cfg(not(windows))]
    {
        let exe = exe_dir();
        let Some(root) = user_data_root() else {
            return exe;
        };
        let target = root.join("nekosportsworldtool");
        if std::fs::create_dir_all(&target).is_err() {
            // 连用户目录都建不出来（无 HOME 的精简容器）：退回旧行为。
            return exe;
        }
        // 搬运只在**进程内第一次**走：`data_dir()` 是热路径（每条配置/状态读写都会调），
        // 每次重新扫 exe 目录既费 IO，又意味着「谁往安装目录丢一个 json，下次就被搬走」。
        static MIGRATED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        MIGRATED.get_or_init(|| migrate_legacy_data(&exe, &target));
        target
    }
}

#[cfg(not(target_os = "android"))]
pub fn exe_dir() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// 用户数据根目录；无 HOME 时返回 None。
#[cfg(not(any(windows, target_os = "android")))]
fn user_data_root() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
        Some(home.join("Library/Application Support"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
            let path = std::path::PathBuf::from(&xdg);
            // XDG 规范要求绝对路径；相对值视为未设置。
            if path.is_absolute() {
                return Some(path);
            }
        }
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
        Some(home.join(".local/share"))
    }
}

/// 旧版本把数据写在 exe 同目录；切到用户数据目录后**一次性搬运**已有文件。
///
/// 不搬 `identity.json` 的代价最大：它一丢，`device_id` 重新生成，服务端会把
/// 同一部手机当成新设备（10121 风控）。所以这里按「缺什么补什么」复制，
/// 且不删除原文件（回退旧版本仍能用）。
///
/// 只搬**本程序自己的**数据文件（白名单）：安装目录里放着的其它 json（用户自己的
/// 配置、测试夹具、别的工具的数据）一律不碰 —— 之前按 `*.json` 通配搬，
/// 只要有人往安装目录丢一个 json，下次启动就被复制进用户数据目录。
#[cfg(not(any(windows, target_os = "android")))]
fn migrate_legacy_data(from: &std::path::Path, to: &std::path::Path) {
    if from == to {
        return;
    }
    const DATA_FILES: &[&str] = &[
        "config.json",
        "session.json",
        "identity.json",
        "proxies.json",
        "points_cache.json",
        "ai_sports.json",
        "captcha_state.json",
        "update_pins.json",
    ];
    for name in DATA_FILES {
        let source = from.join(name);
        let target = to.join(name);
        if !source.is_file() || target.exists() {
            continue;
        }
        let _ = std::fs::copy(&source, &target);
    }
    // 学校路网缓存是目录
    let source = from.join("school_roads");
    let target = to.join("school_roads");
    if source.is_dir() && !target.exists() {
        let _ = copy_tree(&source, &target);
    }
}

#[cfg(not(any(windows, target_os = "android")))]
fn copy_tree(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)?.flatten() {
        let source = entry.path();
        let Some(name) = source.file_name() else {
            continue;
        };
        let target = to.join(name);
        if source.is_dir() {
            copy_tree(&source, &target)?;
        } else {
            std::fs::copy(&source, &target)?;
        }
    }
    Ok(())
}

pub fn edit_text(id: u64, value: &str, kind: InputKind) {
    #[cfg(target_os = "android")]
    crate::android::edit_text(id, value, kind);
    #[cfg(not(target_os = "android"))]
    let _ = (id, value, kind);
}

pub fn take_edited_text(id: u64) -> Option<String> {
    #[cfg(target_os = "android")]
    {
        crate::android::take_edited_text(id)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = id;
        None
    }
}

pub fn set_keep_screen_on(enabled: bool) {
    #[cfg(target_os = "android")]
    crate::android::set_keep_screen_on(enabled);
    #[cfg(not(target_os = "android"))]
    let _ = enabled;
}

/// 当前程序版本：桌面读 Cargo 版本，Android 读 manifest versionName
/// （形如 0.2.5-android.3，与 Release tag 对齐）。
pub fn version_name() -> String {
    #[cfg(target_os = "android")]
    {
        crate::android::version_name()
    }
    #[cfg(not(target_os = "android"))]
    {
        env!("CARGO_PKG_VERSION").to_string()
    }
}

/// Android：把已下载的 APK 交给系统安装器（用户在系统弹窗确认安装）。
pub fn install_apk(path: &str) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        crate::android::install_apk(path)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = path;
        Err("桌面版没有系统安装器：请直接运行下载好的新版本".into())
    }
}

/// 让用户挑一个本地文件（Android：系统文件选择器；桌面：由调用方用 rfd）。
///
/// 选中后文件内容会被复制进应用私有目录 —— Android 的 `content://` URI 只有
/// 授权瞬间可读，交给后台线程解析会失效，先落盘再解析最省事。
///
/// 返回 false 表示选择器没能唤起（桌面端恒为 false，因为没有实现）。
pub fn pick_file() -> bool {
    #[cfg(target_os = "android")]
    {
        crate::android::pick_file()
    }
    #[cfg(not(target_os = "android"))]
    {
        false
    }
}

/// 取一次选择结果（取走即清空）。`Some(Ok(路径))` / `Some(Err(原因))`。
pub fn take_picked_file() -> Option<Result<String, String>> {
    #[cfg(target_os = "android")]
    {
        crate::android::take_picked_file()
    }
    #[cfg(not(target_os = "android"))]
    {
        None
    }
}

#[cfg(feature = "gui")]
pub fn sync_clipboard(context: &egui::Context) {
    #[cfg(target_os = "android")]
    {
        let text = context.output(|output| output.copied_text.clone());
        if !text.is_empty() {
            crate::android::copy_text(&text);
        }
    }
    #[cfg(not(target_os = "android"))]
    let _ = context;
}

#[cfg(feature = "gui")]
pub fn apply_safe_area(context: &egui::Context, input: &mut egui::RawInput) {
    #[cfg(target_os = "android")]
    if let Some(rect) = input.screen_rect.as_mut() {
        let [left, top, right, bottom] = crate::android::safe_insets();
        let scale = context.pixels_per_point();
        rect.min.x += left as f32 / scale;
        rect.min.y += top as f32 / scale;
        rect.max.x -= right as f32 / scale;
        rect.max.y -= bottom as f32 / scale;
    }
    #[cfg(not(target_os = "android"))]
    let _ = (context, input);
}

#[cfg(test)]
thread_local! {
    pub(crate) static TEST_DATA_DIR: std::cell::RefCell<Option<std::path::PathBuf>> = const { std::cell::RefCell::new(None) };
}
