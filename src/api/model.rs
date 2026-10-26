//! 数据模型与本地持久化。
//!
//! 持久化文件位于桌面 exe 目录或 Android 应用私有目录：identity.json（设备身份，device_id 固定复用
//! ——严禁每次随机，会触发 10121 风控）、session.json（登录态）、config.json
//! （账号/参数）、points_cache.json（点位缓存）。

pub use crate::crypto::header::HeaderIdentity;

pub const HOST: &str = "https://run.gxapp.iydsj.com";
/// 排行榜 / 违规名单域名（信封链与 RUN 相同）。
pub const DISCOVERY: &str = "https://discovery.gxapp.iydsj.com";

/// 登录态（session.json）。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Session {
    #[serde(default)]
    pub uid: i64,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub unid: String,
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_weight")]
    pub weight: f64,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub device_id: String,
    /// 登录响应完整业务数据（我的页展示）
    #[serde(default)]
    pub profile: serde_json::Value,
}

fn default_weight() -> f64 {
    68.0
}

impl Session {
    pub fn is_logged_in(&self) -> bool {
        self.uid >= 1 && !self.token.is_empty()
    }
}

/// UI 参数（config.json）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Config {
    #[serde(default)]
    pub school_campus: Option<super::campus::Selection>,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub remember: bool,
    #[serde(default = "default_f32")]
    pub dist_min: f32,
    #[serde(default = "default_f32")]
    pub dist_max: f32,
    #[serde(default = "default_pace_min")]
    pub pace_min: f32,
    #[serde(default = "default_pace_max")]
    pub pace_max: f32,
    #[serde(default)]
    pub face_check: bool,
    /// 跑步时覆盖轨迹点 bdA；为空表示使用生成器的海拔曲线。
    #[serde(default)]
    pub manual_altitude: Option<f64>,
    /// 跑步时将生成器海拔曲线映射到此范围；为空表示不使用范围覆盖。
    #[serde(default)]
    pub manual_altitude_range: Option<crate::track::altitude::AltitudeRange>,
    #[serde(default = "default_ai_minutes")]
    pub ai_minutes: i64,
    #[serde(default = "default_ai_reps")]
    pub ai_reps: i64,
    /// 启动时检查更新：silent（静默，发现新版才弹窗）/ ask（每次询问）/ off
    #[serde(default = "default_update_check")]
    pub update_check: String,
    /// OSM 路网文件路径（真实道路路由用）。
    #[serde(default)]
    pub osm_path: String,
    /// 路线算法：legacy | road。
    #[serde(default = "default_route_mode")]
    pub route_mode: String,
    /// 提交行为护栏（深夜/每日次数/批量补签）。缺省即默认开启。
    #[serde(default)]
    pub safety: crate::safety::SafetyPolicy,
}

fn default_route_mode() -> String {
    "legacy".into()
}

fn default_f32() -> f32 {
    1.0
}
fn default_pace_min() -> f32 {
    360.0
}
fn default_pace_max() -> f32 {
    480.0
}
fn default_ai_minutes() -> i64 {
    1
}
fn default_ai_reps() -> i64 {
    5
}
fn default_update_check() -> String {
    "silent".into()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            school_campus: None,
            username: String::new(),
            password: String::new(),
            remember: false,
            dist_min: 1.0,
            dist_max: 1.5,
            pace_min: default_pace_min(),
            pace_max: default_pace_max(),
            face_check: true,
            manual_altitude: None,
            manual_altitude_range: None,
            ai_minutes: default_ai_minutes(),
            ai_reps: default_ai_reps(),
            update_check: default_update_check(),
            osm_path: String::new(),
            route_mode: default_route_mode(),
            safety: crate::safety::SafetyPolicy::default(),
        }
    }
}

// ── 持久化（平台数据目录）──────────────────────────────────────

fn exe_dir() -> std::path::PathBuf {
    crate::platform::data_dir()
}

fn read_json<T: serde::de::DeserializeOwned>(name: &str) -> Option<T> {
    let path = exe_dir().join(name);
    let text = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str(&text) {
        Ok(value) => Some(value),
        Err(error) => {
            // 静默回退默认值会让用户以为"账号/设置被重置了"。把坏文件留档再告警，
            // 至少在控制台说清发生了什么、备份在哪里。
            let bad = path.with_extension("json.corrupt");
            let _ = std::fs::rename(&path, &bad);
            eprintln!(
                "[data] {name} 解析失败（{error}），已备份为 {} 并使用默认值",
                bad.display()
            );
            None
        }
    }
}

/// 本程序的本地数据文件都含账号、口令或会话令牌，写入时只给当前用户读写。
fn write_json<T: serde::Serialize>(name: &str, value: &T) -> Result<(), String> {
    let path = exe_dir().join(name);
    let json =
        serde_json::to_string_pretty(value).map_err(|e| format!("序列化 {name} 失败: {e}"))?;
    write_secret_file(&path, json.as_bytes())
        .map_err(|e| format!("写入 {} 失败: {e}", path.display()))
}

/// 原子写：先写同目录临时文件并落盘，再 rename 覆盖。
///
/// 直接 `fs::write` 会在崩溃/断电时留下半截 JSON，而读取端只能解析失败；
/// 凭据与身份文件一旦这样坏掉，用户看到的是"设置被清空"。
/// Unix 上把权限收紧到 0600（默认 umask 往往给同机其他用户留了读权限）。
///
/// 临时文件名必须**全局唯一**：只用 `文件名.PID.tmp` 时，同一进程里两个线程
/// 同时保存同一目标会共同截断/写入同一个临时文件，再互相 rename —— 原子写承诺
/// 就不成立了（写出来的可能是两段 JSON 的交错残片）。这里再拼上线程 id 与
/// 进程内单调计数器，把这条竞争路径堵死。
pub(crate) fn write_secret_file(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("neko-data");
    let tmp = path.with_file_name(format!(
        "{file_name}.{}.{:?}.{}.tmp",
        std::process::id(),
        std::thread::current().id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let write = || -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        use std::io::Write;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // 已存在的临时文件可能带着旧权限，显式收紧一次。
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, path)
    };
    let result = write();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// 加载设备身份；device_id / app_install_time 缺失时生成一次并立即落盘，
/// 此后同一设备全生命周期复用（逐请求漂移会影响设备一致性）。
pub fn load_identity() -> HeaderIdentity {
    // 官方只发布 Android 包：7.4.0 的 `native-code` 只有 arm64-v8a / armeabi-v7a /
    // armeabi，dex 里也没有任何 iOS 客户端痕迹（`Scale/`、`CFNetwork` 均 0 命中）。
    // 桌面端过去按 `cfg!(target_os="android")` 退到 iOS，结果是同一个账号
    // 「提交走 Android、其余链路走 iOS」——一次运行两套身份，比机型不像真机更容易被关联。
    // 现在统一按 Android 身份走；用户在设备页显式切成 iOS 仍然尊重其选择。
    load_identity_for_platform("android")
}

fn load_identity_for_platform(platform: &str) -> HeaderIdentity {
    let mut id: HeaderIdentity = read_json("identity.json").unwrap_or_else(|| {
        let mut identity = HeaderIdentity::default();
        if platform == "android" {
            identity.platform = "android".into();
            // 机型/系统版本留空，交给下面的统一派生填成机型库里的真机组合。
            // 写死 "Android"/"16" 这类占位值会让 header 的 deviceName/osVersion
            // 与 UA 各说各话（占位值本身也不是任何一台真机的 Build.MODEL）。
            identity.device_name.clear();
            identity.os_version.clear();
        }
        identity
    });
    let mut dirty = false;
    if id.device_id.is_empty() {
        id.device_id = uuid::Uuid::new_v4().to_string().to_uppercase();
        dirty = true;
    }
    if id.app_install_time <= 0 {
        id.app_install_time =
            crate::crypto::header::HeaderIdentity::fresh_install_time(&id.platform);
        dirty = true;
    }
    // MAC 一律归一到 Android 6+ 的占位值。请求里从来不用 `identity.mac_address`
    // ——登录体的 `mac_address` 与信封的 `blMac`/`wifiMac` 都直接用
    // `ANDROID_PLACEHOLDER_MAC`。磁盘上留一个随机 MAC 只会在导出/复制
    // identity.json 时变成一个假的「设备指纹」，而且一旦将来哪条链路读它，
    // 就是一处凭空造出来的异常值（Android 6+ 普通应用读不到真实 MAC）。
    if id.mac_address != crate::api::fingerprint::ANDROID_PLACEHOLDER_MAC {
        id.mac_address = crate::api::fingerprint::ANDROID_PLACEHOLDER_MAC.to_string();
        dirty = true;
    }
    // 一次性迁移：老版本桌面端的默认身份是 iOS（`iPhone` + `26.5.2`），
    // 与"用户主动选了 iOS"在磁盘上无法区分，所以只迁移**完全没被动过**的旧默认值，
    // 迁完打标记，不再反复改写用户的选择。
    if !id.platform_migrated {
        let pristine_legacy_ios = id.platform != "android"
            && id.device_name == crate::crypto::header::LEGACY_DEFAULT_DEVICE_NAME
            && id.os_version == crate::crypto::header::LEGACY_DEFAULT_OS_VERSION;
        if pristine_legacy_ios {
            id.platform = "android".into();
            id.device_name.clear();
            id.os_version.clear();
            id.pixel.clear();
            id.logic_pixel.clear();
            id.idfa.clear();
        }
        id.platform_migrated = true;
        dirty = true;
    }
    // Android 身份的机型/系统版本必须是机型库里的真实组合（机型与版本成对）：
    // "Android"/"Android 16" 这类占位值会让 header 报 A、UA 报 B，也会被本地自检判非法。
    if id.platform == "android" {
        let (model, os) = id.android_model_os();
        if id.device_name != model {
            id.device_name = model;
            dirty = true;
        }
        if id.os_version != os {
            id.os_version = os;
            dirty = true;
        }
    }
    // 指纹自洽：老配置补齐 androidId / 分辨率（同一 device_id 结果恒定）。
    if id.ensure_fingerprint() {
        dirty = true;
    }
    if dirty {
        let _ = save_identity(&id);
    }
    id
}

#[cfg(any(target_os = "android", test))]
#[derive(Debug, serde::Deserialize)]
pub struct DeviceInfo {
    pub manufacturer: String,
    pub model: String,
    pub os_version: String,
    /// 本机当前窗口分辨率（`widthPixels x heightPixels`）。老版本 Java 不返回时为空。
    #[serde(default)]
    pub pixel: String,
    /// `DisplayMetrics.densityDpi`，仅本地展示与自检用。
    #[serde(default)]
    pub density_dpi: u32,
    /// `Settings.Secure.ANDROID_ID`：16 位小写 hex。老版本 Java 不返回时为空。
    #[serde(default)]
    pub android_id: String,
}

#[cfg(any(target_os = "android", test))]
pub fn identity_with_device_info(
    identity: &HeaderIdentity,
    info: &DeviceInfo,
) -> Result<HeaderIdentity, String> {
    if info.model.trim().is_empty() || info.os_version.trim().is_empty() {
        return Err("未能读取完整的机型和系统版本，请手动填写".into());
    }
    let mut updated = identity.clone();
    updated.platform = "android".into();
    updated.device_name = info.model.trim().into();
    updated.manufacturer = info.manufacturer.trim().into();
    updated.os_version = info.os_version.trim().into();
    // 真机读到的分辨率 / androidId 优先：这两个值**由构造即自洽**，
    // 比任何机型对照表都可靠 —— 官方客户端在同一台手机上上报的就是它们。
    // 只有真机把两个值都给全了才标记来源为「真机」；否则仍按机型库补齐并标记为推算值。
    let from_device = !info.pixel.trim().is_empty() && !info.android_id.trim().is_empty();
    if !info.pixel.trim().is_empty() {
        updated.pixel = info.pixel.trim().into();
    }
    if !info.android_id.trim().is_empty() {
        updated.android_id = info.android_id.trim().into();
    }
    updated.fingerprint_from_device = from_device;
    if !from_device {
        updated.ensure_fingerprint();
    }
    Ok(updated)
}

pub fn save_identity(id: &HeaderIdentity) -> Result<(), String> {
    write_json("identity.json", id)
}

/// AI 项目列表永久缓存：拉取成功即落盘，网络异常时兜底展示。
pub fn load_ai_sports() -> Option<Vec<crate::api::ai::AiSport>> {
    read_json("ai_sports.json")
}

pub fn save_ai_sports(list: &[crate::api::ai::AiSport]) -> Result<(), String> {
    write_json("ai_sports.json", &list.to_vec())
}

/// 敏感字段落盘前的加密改写：`plain` 非空时写成 `sealed` 密文并清空明文。
///
/// 结构体本身不加密（内存里必须是明文才能登录），只有写盘这一层改写 JSON；
/// 这样既不动 `Config`/`Session` 的字段，也不影响任何读代码 —— 迁移面最小。
fn seal_field(v: &mut serde_json::Value, plain: &str, sealed: &str) {
    let Some(value) = v.get(plain).and_then(|x| x.as_str()).map(str::to_string) else {
        return;
    };
    if value.is_empty() {
        return;
    }
    match crate::crypto::local_seal::seal(&value) {
        Some(cipher) => {
            v[sealed] = serde_json::Value::String(cipher);
            v[plain] = serde_json::Value::String(String::new());
        }
        // 加密不可用（salt 写不进去）：保持旧行为，至少功能不中断。
        None => eprintln!("[data] {plain} 未能加密保存（本地加密不可用）"),
    }
}

/// 读取时把密文还原进明文字段；旧版明文文件原样可用（下次保存自动升级）。
fn open_field(v: &mut serde_json::Value, plain: &str, sealed: &str) {
    let existing = v.get(plain).and_then(|x| x.as_str()).unwrap_or("");
    if !existing.is_empty() {
        return;
    }
    let Some(cipher) = v.get(sealed).and_then(|x| x.as_str()).map(str::to_string) else {
        return;
    };
    if cipher.is_empty() {
        return;
    }
    match crate::crypto::local_seal::open(&cipher) {
        Some(text) => {
            v[plain] = serde_json::Value::String(text);
            // 明文不该继续留在内存里的 Value 副本中
            v[sealed] = serde_json::Value::String(String::new());
        }
        None => eprintln!(
            "[data] {plain} 无法解密（换机器/改用户名/删了 local.salt），请重新登录或重新输入"
        ),
    }
}

pub fn load_session() -> Session {
    match read_json::<serde_json::Value>("session.json") {
        Some(mut v) => {
            open_field(&mut v, "token", "token_sealed");
            serde_json::from_value(v).unwrap_or_default()
        }
        None => Session::default(),
    }
}

pub fn save_session(s: &Session) -> Result<(), String> {
    let mut v = serde_json::to_value(s).map_err(|e| format!("序列化 session.json 失败: {e}"))?;
    seal_field(&mut v, "token", "token_sealed");
    write_json("session.json", &v)
}

pub fn clear_session() {
    let _ = std::fs::remove_file(exe_dir().join("session.json"));
}

pub fn load_config() -> Config {
    match read_json::<serde_json::Value>("config.json") {
        Some(mut v) => {
            open_field(&mut v, "password", "password_sealed");
            serde_json::from_value(v).unwrap_or_default()
        }
        None => Config::default(),
    }
}

pub fn save_config(c: &Config) -> Result<(), String> {
    let mut v = serde_json::to_value(c).map_err(|e| format!("序列化 config.json 失败: {e}"))?;
    seal_field(&mut v, "password", "password_sealed");
    write_json("config.json", &v)
}

#[cfg(test)]
mod storage_tests {
    use super::*;

    #[test]
    fn android_identity_import_persists_brand_and_reuses_uuid() {
        let directory =
            std::env::temp_dir().join(format!("neko-identity-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = Some(directory.clone()));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = None);
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(directory);
        let first = load_identity_for_platform("android");
        assert_eq!(first.platform, "android");
        assert_ne!(first.device_name, "iPhone");
        assert!(uuid::Uuid::parse_str(&first.device_id).is_ok());
        let again = load_identity_for_platform("android");
        assert_eq!(
            serde_json::to_value(&first).unwrap(),
            serde_json::to_value(&again).unwrap()
        );

        let mut existing = first.clone();
        existing.platform = "ios".into();
        existing.device_name = "Existing manual name".into();
        existing.idfa = "manual-value".into();
        let mut legacy = serde_json::to_value(&existing).unwrap();
        legacy.as_object_mut().unwrap().remove("manufacturer");
        write_json("identity.json", &legacy).unwrap();
        let loaded = load_identity_for_platform("android");
        assert_eq!(
            serde_json::to_value(&loaded).unwrap(),
            serde_json::to_value(&existing).unwrap(),
            "opening an existing install must not overwrite a manual identity"
        );

        let info: DeviceInfo = serde_json::from_str(
            r#"{"manufacturer":"Example","model":"Phone 16","os_version":"16"}"#,
        )
        .unwrap();
        assert_eq!(info.manufacturer, "Example");
        let imported = identity_with_device_info(&loaded, &info).unwrap();
        assert_eq!(imported.platform, "android");
        assert_eq!(imported.device_name, "Phone 16");
        assert_eq!(imported.os_version, "16");
        let mut expected = serde_json::to_value(&existing).unwrap();
        expected["platform"] = "android".into();
        expected["device_name"] = "Phone 16".into();
        expected["manufacturer"] = "Example".into();
        expected["os_version"] = "16".into();
        // 转到 Android 身份后必须补齐指纹（本次改动新增的语义）
        expected["android_id"] = imported.android_id.clone().into();
        expected["pixel"] = imported.pixel.clone().into();
        // 本用例的 info 不含真机分辨率/androidId → 按机型库补齐，来源标记为 false
        expected["fingerprint_from_device"] = false.into();
        assert_eq!(imported.android_id.len(), 16, "导入后必须补齐 16 位 androidId");
        assert!(!imported.pixel.is_empty(), "导入后必须有分辨率");
        assert_eq!(
            serde_json::to_value(&imported).unwrap(),
            expected,
            "import must preserve UUID, MAC, manual IMEI/IDFA, install time and location"
        );
        assert_eq!(
            load_identity().device_name,
            existing.device_name,
            "preview must not save implicitly"
        );
        save_identity(&imported).unwrap();
        let stored: serde_json::Value = read_json("identity.json").unwrap();
        assert_eq!(
            stored.get("manufacturer").and_then(|value| value.as_str()),
            Some("Example"),
            "the consented brand must be written to identity.json"
        );
        let reloaded = load_identity();
        assert_eq!(
            serde_json::to_value(&reloaded).unwrap(),
            serde_json::to_value(&imported).unwrap(),
            "brand and identity must survive a fresh load from disk"
        );
        assert_eq!(reloaded.device_id, first.device_id);
        let (header, _) =
            crate::crypto::header::build_header_for(&reloaded, 0, "", Some(1_700_000_000_000));
        let header: serde_json::Value = serde_json::from_str(&header).unwrap();
        assert_eq!(header["DeviceId"], first.device_id);
        assert_eq!(
            header["deviceName"], "Phone 16",
            "brand must not be prepended to the protocol model field"
        );
        assert_eq!(header["osVersion"], "16");
        assert!(
            header.get("manufacturer").is_none() && header.get("brand").is_none(),
            "persisting local brand metadata must not invent new protocol fields"
        );
    }

    #[test]
    fn device_read_fingerprint_is_authoritative_and_needs_no_model_table() {
        let identity = HeaderIdentity::default();
        // 一台不在机型库里的真机（分辨率/AndroidID 都由系统读到）
        let info: DeviceInfo = serde_json::from_str(
            r#"{"manufacturer":"Xiaomi","model":"23013RK75C","os_version":"13",
                "pixel":"1080x2400","density_dpi":440,"android_id":"0123456789abcdef"}"#,
        )
        .unwrap();
        let updated = identity_with_device_info(&identity, &info).unwrap();
        assert!(updated.fingerprint_from_device, "真机读数必须标记来源");
        assert_eq!(updated.pixel, "1080x2400");
        assert_eq!(updated.android_id, "0123456789abcdef");
        // 机型不在库内也不该被自检报警（真机读数是权威值）
        let profile = crate::api::fingerprint::DeviceProfile::from_parts(
            &updated.device_name,
            &updated.os_version,
            &updated.pixel,
            &updated.android_id,
            0,
        );
        assert!(
            crate::api::fingerprint::audit_ex(&profile, false).is_empty(),
            "真机读数不应触发机型库告警"
        );
        // 只给机型、不给读数时退回机型库补齐。
        // 注意：真实路径里 device_id 一定由 load_identity_for_platform 补好，
        // 空 device_id 不会派生指纹（MD5("") 是常量，会让所有设备撞同一个 androidId）。
        let with_id = HeaderIdentity {
            device_id: "0E5A5B4C-1111-4A2B-9C3D-5E6F70819234".into(),
            ..identity.clone()
        };
        let partial: DeviceInfo =
            serde_json::from_str(r#"{"manufacturer":"Xiaomi","model":"23127PN0CC","os_version":"14"}"#)
                .unwrap();
        let inferred = identity_with_device_info(&with_id, &partial).unwrap();
        assert!(!inferred.fingerprint_from_device);
        assert!(!inferred.pixel.is_empty() && inferred.android_id.len() == 16);
        // 空 device_id 不派生：宁可缺字段，也不要所有设备共用一个 androidId
        let no_id = identity_with_device_info(&identity, &partial).unwrap();
        assert!(no_id.android_id.is_empty(), "空 device_id 不应派生指纹");
    }

    #[test]
    fn incomplete_native_device_info_cannot_replace_identity() {
        let identity = HeaderIdentity::default();
        for (model, os_version) in [("", "16"), ("Phone", " ")] {
            let info = DeviceInfo {
                manufacturer: String::new(),
                model: model.into(),
                os_version: os_version.into(),
                pixel: String::new(),
                density_dpi: 0,
                android_id: String::new(),
            };
            assert!(identity_with_device_info(&identity, &info).is_err());
        }
    }

    #[test]
    fn session_roundtrip_and_logout_use_private_data_directory() {
        let directory = std::env::temp_dir().join(format!("neko-storage-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = Some(directory.clone()));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = None);
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(directory.clone());
        assert_eq!(
            exe_dir(),
            directory,
            "storage must use the injected app directory"
        );
        let session = Session {
            uid: 123,
            token: "test-only-token".into(),
            ..Default::default()
        };
        save_session(&session).unwrap();
        assert!(directory.join("session.json").is_file());
        assert_eq!(load_session().uid, 123);
        assert_eq!(load_session().token, session.token);
        clear_session();
        assert!(!directory.join("session.json").exists());
        assert_eq!(load_session().uid, 0);
    }

    #[test]
    fn saved_data_is_atomic_private_and_corruption_is_quarantined() {
        let directory = std::env::temp_dir().join(format!("neko-atomic-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = Some(directory.clone()));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = None);
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(directory.clone());

        let config = Config {
            username: "alice".into(),
            password: "s3cret".into(),
            remember: true,
            ..Config::default()
        };
        save_config(&config).unwrap();
        let path = directory.join("config.json");
        assert_eq!(load_config().password, "s3cret");
        // 原子写不得留下临时文件（否则崩溃点上会有一份半截 JSON）
        let leftovers: Vec<String> = std::fs::read_dir(&directory)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "不应残留临时文件: {leftovers:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "凭据文件必须仅当前用户可读写，实际 {mode:o}");
        }

        // 损坏的 JSON：回默认值，但原文件被留档而不是被静默丢弃
        std::fs::write(&path, "{ 这不是 JSON").unwrap();
        let recovered = load_config();
        assert!(recovered.password.is_empty());
        assert!(
            directory.join("config.json.corrupt").is_file(),
            "解析失败的文件必须另存留证"
        );
        assert!(!path.exists(), "坏文件应被移走");
    }
}

/// 点位缓存：{ts_ms, points}，TTL 300s（服务端限流 10603：5 分钟 3 次）。
pub const POINTS_TTL_MS: i64 = 300_000;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PointsCacheScope {
    pub uid: i64,
    pub unid: String,
    pub run_area_id: Option<String>,
    /// 拉点位时用的目标距离（米，取整）。不同距离对应不同点位集合，不能复用。
    #[serde(default)]
    pub distance_m: i64,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct PointsCache {
    ts: i64,
    anchor: crate::location::Coordinate,
    scope: PointsCacheScope,
    points: Vec<serde_json::Value>,
}

impl PointsCache {
    fn matches(
        &self,
        anchor: crate::location::Coordinate,
        scope: &PointsCacheScope,
        now: i64,
    ) -> bool {
        self.scope == *scope
            && self.anchor.is_near(anchor, 0.0001)
            && !self.points.is_empty()
            && now
                .checked_sub(self.ts)
                .is_some_and(|age| (0..POINTS_TTL_MS).contains(&age))
    }
}

/// 旧版未记录账号、学校、区域的缓存自然失效；拒绝过期或来自未来的缓存。
pub fn load_points_cache_for(
    anchor: crate::location::Coordinate,
    scope: &PointsCacheScope,
) -> Option<(i64, Vec<serde_json::Value>)> {
    let v: serde_json::Value = read_json("points_cache.json")?;
    let cache: PointsCache = serde_json::from_value(v).ok()?;
    if !cache.matches(anchor, scope, crate::crypto::envelope::now_ms()) {
        return None;
    }
    let ts = cache.ts;
    let pts = cache.points;
    Some((ts, pts))
}

pub fn save_points_cache(
    anchor: crate::location::Coordinate,
    scope: &PointsCacheScope,
    points: &[serde_json::Value],
) -> Result<(), String> {
    let doc = PointsCache {
        ts: crate::crypto::envelope::now_ms(),
        anchor,
        scope: scope.clone(),
        points: points.to_vec(),
    };
    write_json("points_cache.json", &doc)
}

#[cfg(test)]
mod points_cache_tests {
    use super::*;
    #[test]
    fn cache_is_scoped_to_anchor() {
        let cache = PointsCache {
            ts: 1,
            anchor: crate::location::Coordinate::new(39.9, 116.4, 0.0).unwrap(),
            scope: PointsCacheScope {
                uid: 1,
                unid: "school".into(),
                run_area_id: None,
                distance_m: 2000,
            },
            points: vec![serde_json::json!({"lat":39.9,"lon":116.4})],
        };
        assert!(cache.matches(cache.anchor, &cache.scope, 1));
        assert!(cache.matches(cache.anchor, &cache.scope, POINTS_TTL_MS));
        assert!(!cache.matches(cache.anchor, &cache.scope, POINTS_TTL_MS + 1));
        assert!(!cache.matches(cache.anchor, &cache.scope, 0));
        for scope in [
            PointsCacheScope {
                uid: 2,
                ..cache.scope.clone()
            },
            PointsCacheScope {
                unid: "other".into(),
                ..cache.scope.clone()
            },
            PointsCacheScope {
                run_area_id: Some("42".into()),
                ..cache.scope.clone()
            },
            // 距离不同 → 点位集合不同，不能复用
            PointsCacheScope {
                distance_m: 3000,
                ..cache.scope.clone()
            },
        ] {
            assert!(!cache.matches(cache.anchor, &scope, 1));
        }
        assert!(!cache.matches(
            crate::location::Coordinate::new(38.9, 121.5, 0.0).unwrap(),
            &cache.scope,
            1
        ));
        let mut old = serde_json::to_value(&cache).unwrap();
        old.as_object_mut().unwrap().remove("scope");
        assert!(serde_json::from_value::<PointsCache>(old).is_err());
        let decoded: PointsCache =
            serde_json::from_value(serde_json::to_value(cache).unwrap()).unwrap();
        assert!(decoded.anchor.is_near(
            crate::location::Coordinate::new(39.9, 116.4, 0.0).unwrap(),
            0.0001
        ));
        assert!(!decoded.anchor.is_near(
            crate::location::Coordinate::new(38.9, 121.5, 0.0).unwrap(),
            0.0001
        ));
    }

    /// 老配置里的 Android 占位档（`device_name: "Android"`）必须在加载时纠正成
    /// 机型库里的真机组合，否则 header 的 deviceName/osVersion 与 UA 各说各话。
    #[test]
    fn legacy_android_placeholders_are_normalized_and_persisted() {
        let directory =
            std::env::temp_dir().join(format!("neko-identity-mig-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = Some(directory.clone()));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = None);
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(directory);

        let mut legacy = serde_json::to_value(HeaderIdentity::default()).unwrap();
        legacy["platform"] = "android".into();
        legacy["device_name"] = "Android".into();
        legacy["os_version"] = "16".into();
        legacy["device_id"] = "11111111-1111-4111-8111-111111111111".into();
        write_json("identity.json", &legacy).unwrap();

        let fixed = load_identity_for_platform("android");
        assert_eq!(fixed.platform, "android");
        assert_ne!(
            fixed.device_name, "Android",
            "占位机型必须被纠正成机型库里的真机"
        );
        assert!(
            fixed.os_version.chars().all(|c| c.is_ascii_digit()),
            "系统版本必须是纯整数（iOS 的 26.5.2 不能出现在 Android 身份里）：{}",
            fixed.os_version
        );
        assert_eq!(
            fixed.device_id, "11111111-1111-4111-8111-111111111111",
            "纠正不得改动 device_id"
        );

        // 纠正结果必须落盘，且再次加载稳定 —— 否则每次启动都换一台"手机"。
        let again = load_identity_for_platform("android");
        assert_eq!(
            serde_json::to_value(&fixed).unwrap(),
            serde_json::to_value(&again).unwrap()
        );
    }

    /// 老配置里存的随机 MAC 必须在加载时归一成 Android 占位值。
    ///
    /// 请求从不读 `identity.mac_address`（登录体与 `blMac`/`wifiMac` 都用常量），
    /// 但磁盘上留一个随机 MAC 会在导出/复制 identity.json 时变成假的「设备指纹」，
    /// 也会诱导后续有人把它接进某条链路——那就是凭空造出来的异常值。
    #[test]
    fn legacy_random_mac_is_normalized_to_the_android_placeholder() {
        let directory =
            std::env::temp_dir().join(format!("neko-identity-mac-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = Some(directory.clone()));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = None);
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(directory);

        let mut legacy = serde_json::to_value(HeaderIdentity::default()).unwrap();
        legacy["platform"] = "android".into();
        legacy["device_id"] = "22222222-2222-4222-8222-222222222222".into();
        legacy["mac_address"] = "08:a4:11:22:33:44".into();
        write_json("identity.json", &legacy).unwrap();

        let fixed = load_identity_for_platform("android");
        assert_eq!(
            fixed.mac_address,
            crate::api::fingerprint::ANDROID_PLACEHOLDER_MAC,
            "随机 MAC 必须被归一"
        );
        // 幂等：再加载一次还是同一个值，且不该再被判定为 dirty 反复重写。
        let again = load_identity_for_platform("android");
        assert_eq!(again.mac_address, crate::api::fingerprint::ANDROID_PLACEHOLDER_MAC);
    }

    /// 老版本桌面端写下的「从没动过的 iOS 默认身份」要一次性迁到 Android；
    /// 用户自己改过的 iOS 身份必须原样保留。
    #[test]
    fn pristine_legacy_ios_identity_migrates_once_but_edited_ios_does_not() {
        let directory =
            std::env::temp_dir().join(format!("neko-identity-plat-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = Some(directory.clone()));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = None);
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(directory);

        // ① 从没动过的旧默认：iOS + iPhone + 26.5.2
        let mut pristine = serde_json::to_value(HeaderIdentity::default()).unwrap();
        pristine["platform"] = "ios".into();
        pristine["device_name"] = "iPhone".into();
        pristine["os_version"] = "26.5.2".into();
        pristine["device_id"] = "22222222-2222-4222-8222-222222222222".into();
        write_json("identity.json", &pristine).unwrap();

        let migrated = load_identity_for_platform("android");
        assert_eq!(migrated.platform, "android", "旧默认身份应一次性迁到 Android");
        assert!(migrated.platform_migrated);
        assert_ne!(
            migrated.device_name, "iPhone",
            "迁移后必须是 Android 机型，不能留着 iPhone"
        );
        assert!(migrated.idfa.is_empty(), "iOS 的 idfa 必须清空");
        let reload = load_identity_for_platform("android");
        assert_eq!(
            serde_json::to_value(&migrated).unwrap(),
            serde_json::to_value(&reload).unwrap(),
            "迁移只能发生一次"
        );

        // ② 用户自己改过的 iOS 身份：保持不动
        let mut edited = serde_json::to_value(HeaderIdentity::default()).unwrap();
        edited["platform"] = "ios".into();
        edited["device_name"] = "iPhone".into();
        edited["os_version"] = "17.6".into();
        edited["platform_migrated"] = true.into();
        edited["device_id"] = "33333333-3333-4333-8333-333333333333".into();
        write_json("identity.json", &edited).unwrap();
        let kept = load_identity_for_platform("android");
        assert_eq!(kept.platform, "ios", "用户显式选的 iOS 不该被改写");
        assert_eq!(kept.os_version, "17.6");
    }

    /// 凭据落盘不能是明文，且重新加载后仍能登录。
    #[test]
    fn credentials_are_sealed_on_disk_but_readable_after_reload() {
        let directory =
            std::env::temp_dir().join(format!("neko-seal-cred-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = Some(directory.clone()));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = None);
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(directory.clone());

        let config = Config {
            username: "13800000000".into(),
            password: "s3cret-pass".into(),
            remember: true,
            ..Default::default()
        };
        save_config(&config).unwrap();
        let raw = std::fs::read_to_string(directory.join("config.json")).unwrap();
        assert!(
            !raw.contains("s3cret-pass"),
            "config.json 不能出现明文口令：{raw}"
        );
        assert!(raw.contains("password_sealed"), "{raw}");
        let loaded = load_config();
        assert_eq!(loaded.password, "s3cret-pass", "重新加载必须还原明文供登录用");
        assert_eq!(loaded.username, "13800000000");
        assert!(loaded.remember);

        let session = Session {
            uid: 7,
            token: "tok-abcdef-1234".into(),
            ..Default::default()
        };
        save_session(&session).unwrap();
        let raw = std::fs::read_to_string(directory.join("session.json")).unwrap();
        assert!(
            !raw.contains("tok-abcdef-1234"),
            "session.json 不能出现明文令牌：{raw}"
        );
        assert!(raw.contains("token_sealed"), "{raw}");
        assert_eq!(load_session().token, "tok-abcdef-1234");
        assert_eq!(load_session().uid, 7);
    }

    /// 旧版本留下的明文文件必须还能读（迁移是"下次保存时"发生，不是"读不出来"）。
    #[test]
    fn legacy_plaintext_credentials_still_load() {
        let directory =
            std::env::temp_dir().join(format!("neko-seal-legacy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = Some(directory.clone()));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = None);
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(directory.clone());

        let plain = serde_json::json!({
            "username": "13900000000",
            "password": "legacy-plain",
            "remember": true,
        });
        write_json("config.json", &plain).unwrap();
        let loaded = load_config();
        assert_eq!(loaded.password, "legacy-plain", "旧明文必须照读");
        assert_eq!(loaded.username, "13900000000");

        // 再存一次就升级成密文
        save_config(&loaded).unwrap();
        let raw = std::fs::read_to_string(directory.join("config.json")).unwrap();
        assert!(!raw.contains("legacy-plain"), "{raw}");
        assert_eq!(load_config().password, "legacy-plain");
    }
}
