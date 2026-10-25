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
fn write_secret_file(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("neko-data");
    let tmp = path.with_file_name(format!("{file_name}.{}.tmp", std::process::id()));
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
    load_identity_for_platform(if cfg!(target_os = "android") {
        "android"
    } else {
        "ios"
    })
}

fn load_identity_for_platform(platform: &str) -> HeaderIdentity {
    let mut id: HeaderIdentity = read_json("identity.json").unwrap_or_else(|| {
        let mut identity = HeaderIdentity::default();
        if platform == "android" {
            identity.platform = "android".into();
            // Generic fallback until the user opts in or fills the fields manually.
            identity.device_name = "Android".into();
            identity.os_version = "16".into();
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
    if id.mac_address.is_empty() {
        id.mac_address = crate::crypto::header::random_mac();
        dirty = true;
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

pub fn load_session() -> Session {
    read_json("session.json").unwrap_or_default()
}

pub fn save_session(s: &Session) -> Result<(), String> {
    write_json("session.json", s)
}

pub fn clear_session() {
    let _ = std::fs::remove_file(exe_dir().join("session.json"));
}

pub fn load_config() -> Config {
    read_json("config.json").unwrap_or_default()
}

pub fn save_config(c: &Config) -> Result<(), String> {
    write_json("config.json", c)
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
}
