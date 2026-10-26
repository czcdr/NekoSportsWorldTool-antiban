//! 请求头明文构造。
//! 固定键序紧凑 JSON；appInstallTime/appUpdateTime 从设备身份读取，
//! 同一 DeviceId 全生命周期稳定（逐请求漂移会影响设备一致性）。

use super::envelope::{md5_hex, now_ms};

/// tokenSign 固定后缀（与 Android 签名盐同源）。
pub const TOKEN_SIGN_SUFFIX: &str = "2slhe02lsfiwowlcixisla_sls-_slaor";

pub const IOS_APP_VERSION: &str = "7.3.40";
pub const ANDROID_APP_VERSION: &str = "7.4.0";

pub const UA_IOS: &str = "SWCampus/7.3.40 (iPhone; iOS 18.1; Scale/3.00)";

/// Android 侧实际发出的 User-Agent。
///
/// 证据来自官方 **Android** 包（7.4.0）：`classes.dex` 68 MB、54 个 arm64 原生库、
/// Dart AOT 全量扫描三处，`SWCampus` 命中共 **0** 次；dex 里能找到的 UA 只有 WebView
/// 模板和 `okhttp/3.14.9`、`okhttp/4.12.0` 这两条 OkHttp 自身的默认值。
///
/// 结论的边界要说清：这只能证明**官方 Android 客户端没有设置自定义 UA**，
/// 发出去的就是 OkHttp 默认值。它**不能**证明 iOS 客户端不发 `SWCampus` ——
/// 我们手上没有 iOS 包，Android 包里本来就不该出现 iOS 的 UA 字符串（`Scale/`、
/// `CFNetwork` 为 0 是这个原因，不是证据）。所以 [`UA_IOS`] 保持原样、仍待验证。
///
/// Android 侧自己拼一个「看起来像官方」的串，反而是把一个「用 OkHttp 的 App」
/// 变成一个「谁都没见过的客户端」。这里直接用 OkHttp 的默认串。
///
/// 若日后抓包证明服务端确实要 `SWCampus/...`，改这一个常量即可 ——
/// 所有链路（登录/策略/围栏/点位/提交/OBS/详情）都从 `ua_for` 取值。
pub const UA_ANDROID: &str = "okhttp/4.12.0";

/// 设备身份。app_install_time <= 0 表示尚未生成。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HeaderIdentity {
    #[serde(default = "default_platform")]
    pub platform: String, // "ios" | "android"
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub idfa: String,
    #[serde(default = "default_os_version")]
    pub os_version: String,
    #[serde(default = "default_device_name")]
    pub device_name: String,
    /// 本地展示的制造商品牌；不改变服务端 deviceName（机型）字段。
    #[serde(default)]
    pub manufacturer: String,
    #[serde(default = "default_anchor_lat")]
    pub anchor_lat: f64,
    #[serde(default = "default_anchor_lon")]
    pub anchor_lon: f64,
    #[serde(default)]
    pub app_install_time: i64,
    /// 设备 MAC。真机上 Android 6+ 的普通应用只能读到占位值
    /// `02:00:00:00:00:00`，所以这里**恒为该占位值**，不生成随机 MAC：
    /// 随机值既不符合官方取数行为，也会成为磁盘上一个假指纹。
    #[serde(default)]
    pub mac_address: String,
    /// Android `Settings.Secure.ANDROID_ID`：16 位小写 hex，首次生成后固定。
    /// 官方客户端一定能取到该值，留空是"非官方请求"的强特征。
    #[serde(default)]
    pub android_id: String,
    /// 机型对应的物理分辨率（如 `"1220x2712"`）；空则按机型库推断。
    /// iOS 侧这是 `physicPixel`，Android 侧同时用作 `logicPixel`（历史口径）。
    #[serde(default)]
    pub pixel: String,
    /// iOS 的逻辑分辨率（如 `"393x852"`）。Android 侧不使用。
    /// 官方 iOS 客户端上报 logicPixel×scale = physicPixel，两者必须整除自洽。
    #[serde(default)]
    pub logic_pixel: String,
    /// `pixel` / `android_id` 是否**读自本机真机**（而不是按机型库推算）。
    /// 真机读数是权威值：机型表里没有他这台机是正常的，本地自检不该因此告警。
    #[serde(default)]
    pub fingerprint_from_device: bool,
    /// 提交 body 的城市名
    #[serde(default = "default_city")]
    pub city: String,
    /// 「默认平台改为 Android」的一次性迁移标记（见 `model::load_identity_for_platform`）。
    /// 老版本桌面端写下的身份是 iOS + 旧默认机型，与"用户主动选了 iOS"在磁盘上无法区分；
    /// 迁移过一次就置位，避免反复改写用户的选择。
    #[serde(default)]
    pub platform_migrated: bool,
}

fn default_platform() -> String { "ios".into() }
fn default_os_version() -> String { LEGACY_DEFAULT_OS_VERSION.into() }
fn default_device_name() -> String { LEGACY_DEFAULT_DEVICE_NAME.into() }
/// 与 [`default_device_name`] / [`default_os_version`] 组成「从没被动过的旧默认身份」，
/// 供一次性平台迁移识别；用户手动改过任一项就不再算旧默认。
pub(crate) const LEGACY_DEFAULT_DEVICE_NAME: &str = "iPhone";
pub(crate) const LEGACY_DEFAULT_OS_VERSION: &str = "26.5.2";
fn default_anchor_lat() -> f64 { 38.901678 }
fn default_anchor_lon() -> f64 { 121.540241 }
fn default_city() -> String { "大连市".into() }

impl HeaderIdentity {
    /// 返回设备页配置的锚点，并执行统一边界校验。
    pub fn anchor_coordinate(&self) -> Result<crate::location::Coordinate, String> {
        crate::location::Coordinate::new(self.anchor_lat, self.anchor_lon, 0.0)
    }

    /// 旧版本首次启动会写入大连默认值。未明确配置前禁止将它作为异地跑步位置使用。
    pub fn has_unconfigured_default_location(&self) -> bool {
        self.city.trim().is_empty()
            || self.city.trim() == crate::location::default_city()
            || self.anchor_coordinate().map(|c| c.is_default_dalian()).unwrap_or(true)
    }

    /// 安装时间：持久值优先；缺失时按平台惯例回退（iOS 3 天 / Android 90 天前）。
    pub fn install_time(&self, ts: i64) -> i64 {
        if self.app_install_time > 0 {
            self.app_install_time
        } else if self.platform == "android" {
            ts - 90 * 86_400_000
        } else {
            ts - 3 * 86_400_000
        }
    }

    /// 按平台生成一份新的安装时间（设备随机化时调用）。
    pub fn fresh_install_time(platform: &str) -> i64 {
        let days = if platform == "android" { 90 } else { 3 };
        now_ms() - days * 86_400_000
    }
}

impl Default for HeaderIdentity {
    fn default() -> Self {
        Self {
            platform: default_platform(),
            device_id: String::new(),
            idfa: String::new(),
            os_version: default_os_version(),
            device_name: default_device_name(),
            manufacturer: String::new(),
            anchor_lat: default_anchor_lat(),
            anchor_lon: default_anchor_lon(),
            app_install_time: 0,
            mac_address: crate::api::fingerprint::ANDROID_PLACEHOLDER_MAC.into(),
            android_id: String::new(),
            pixel: String::new(),
            logic_pixel: String::new(),
            fingerprint_from_device: false,
            city: default_city(),
            // `false` = 「磁盘上还没有这个标记」，与老版本写下的 identity.json 一致；
            // 加载时由 `model::load_identity_for_platform` 判断是否需要迁移并置位。
            platform_migrated: false,
        }
    }
}

impl HeaderIdentity {
    /// 上报用的 Android 机型与系统版本。
    ///
    /// 提交接口与非提交接口（策略/围栏/点位/OBS/详情）必须拿到同一对值。
    /// 以前提交按 `device_id` 派生真机档案、其余链路走写死兜底，于是同一个账号
    /// 在一次运行里报出两种机型（`ALN-AL80/12` 与 `22081212C/14`），
    /// 这是比"机型不像真机"更容易被关联的矛盾。
    pub fn android_model_os(&self) -> (String, String) {
        let is_placeholder_model = |m: &str| matches!(m.trim(), "" | "iPhone" | "iPad" | "Android");
        // Android 版本号是纯整数（"13"/"14"）；带点的都是 iOS 版本号（"26.5.2"）。
        let os_text = self.os_version.trim();
        let os_is_android = !os_text.is_empty()
            && os_text != "0"
            && os_text.chars().all(|c| c.is_ascii_digit());
        // 两边都是用户自己填的真值：原样保留，不拿机型库去覆盖手填数据。
        if !is_placeholder_model(&self.device_name) && os_is_android {
            return (self.device_name.trim().to_string(), os_text.to_string());
        }
        let device_id = if self.device_id.is_empty() {
            stable_fallback_device_id()
        } else {
            self.device_id.clone()
        };
        // 机型和系统版本必须**成对**从同一台设备取：占位机型 + 保留一个数字版本
        // （老默认的 "16"，或用户从 iOS 切过来留下的 "18.1"）会拼出机型库里不存在
        // 的组合，本地自检和机型库对照都会判它非法。
        let keep_model = if is_placeholder_model(&self.device_name) {
            ""
        } else {
            self.device_name.trim()
        };
        let profile = crate::api::fingerprint::DeviceProfile::from_parts(
            keep_model,
            "",
            "",
            "",
            fingerprint_seed(&device_id),
        );
        (profile.device_name, profile.os_version)
    }

    /// 补齐指纹字段：androidId / 分辨率缺失时按机型库推导，保证一套身份内部自洽。
    /// 返回 true 表示有字段被补上（调用方负责落盘）。
    ///
    /// iOS 与 Android 都要补：官方 iOS 客户端上报的 `logicPixel`×`scale` 必须等于
    /// `physicPixel`，而原来这两个值是写死的 `360x640` / `1080x1920`（配 `cpuModel:
    /// x86_64`），那是模拟器签名，不是任何一台真机的组合。
    pub fn ensure_fingerprint(&mut self) -> bool {
        // device_id 为空时**不派生**指纹：MD5("") 是常量，会让所有空 ID 的设备
        // 拿到同一个机型 + 同一个 androidId（ANDROID_ID 跨设备重复在真机不可能）。
        // 调用方（load_identity_for_platform）会先把 device_id 补好再进来。
        if self.device_id.is_empty() {
            return false;
        }
        let seed = fingerprint_seed(&self.device_id);
        if self.platform == "android" {
            if !self.android_id.is_empty() && !self.pixel.is_empty() {
                return false;
            }
            let profile = crate::api::fingerprint::DeviceProfile::from_parts(
                &self.device_name,
                &self.os_version,
                &self.pixel,
                &self.android_id,
                seed,
            );
            let mut changed = false;
            if self.android_id.is_empty() {
                // **不**从 device_id 派生：真机上 ANDROID_ID 与任何 App 的 DeviceId
                // 都不同源。派生出来的值一旦被服务端按 deviceId 建画像，就会把
                // 「同一台机器换过的身份」串成一条线。生成后立即落盘，终身稳定。
                self.android_id = crate::api::fingerprint::random_android_id();
                changed = true;
            }
            if self.pixel.is_empty() {
                self.pixel = profile.pixel;
                changed = true;
            }
            return changed;
        }
        // iOS：只在两个分辨率**都为空**时补一套自洽档案（新身份）。
        // 已有手填值一律不动 —— 打开旧配置不该改写用户手动设过的字段，
        // 缺的那一半在请求头里按 @3x 反推补上（见 `derive_ios_logic`）。
        if !self.logic_pixel.is_empty() || !self.pixel.is_empty() {
            return false;
        }
        let profile = crate::api::fingerprint::IosProfile::mint(seed);
        let mut changed = false;
        if self.logic_pixel.is_empty() {
            self.logic_pixel = profile.logic;
            changed = true;
        }
        if self.pixel.is_empty() {
            self.pixel = profile.physic;
            changed = true;
        }
        changed
    }
}

/// 只给了物理分辨率时，按 iPhone 的主流 @3x 反推逻辑分辨率。
///
/// 官方客户端上报的 `logicPixel × scale = physicPixel`，所以这两个值不能各写各的：
/// 逻辑值缺失时用常量兜底、物理值却来自用户，就会出现「不是整数倍」的矛盾组合。
/// 除不尽（说明填的不是 iPhone 参数）时退回常见真机值，由自检去提示。
fn derive_ios_logic(physic: &str) -> String {
    let parse = |s: &str| -> Option<(u32, u32)> {
        let (w, h) = s.trim().split_once(['x', 'X', '*'])?;
        Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
    };
    match parse(physic) {
        Some((w, h)) if w % 3 == 0 && h % 3 == 0 => format!("{}x{}", w / 3, h / 3),
        _ => "393x852".to_string(),
    }
}

/// 按身份平台给出该请求应使用的 User-Agent。
///
/// **全链路唯一来源**：登录、策略、围栏、点位、提交、OBS 直传、详情都从这里取。
/// 曾经登录/点位写死 iOS UA、OBS 直传漏设（发出 `ureq/2.12.1`）、提交另拼一套，
/// 同一次运行四条不同 UA —— 那才是最廉价可关联的特征。
pub fn ua_for(identity: &HeaderIdentity) -> String {
    if identity.platform == "android" {
        UA_ANDROID.to_string()
    } else {
        UA_IOS.to_string()
    }
}

/// 由稳定 device_id 派生指纹 seed：同一设备反复启动得到同一套补齐值。
pub(crate) fn fingerprint_seed(device_id: &str) -> u64 {
    use md5::{Digest, Md5};
    let digest = Md5::digest(device_id.as_bytes());
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&digest[..8]);
    u64::from_le_bytes(buf)
}

/// 空 device_id 的兜底值：进程内固定。提交与其余链路共用，避免派生两种指纹。
pub(crate) fn stable_fallback_device_id() -> String {
    use std::sync::OnceLock;
    static FALLBACK: OnceLock<String> = OnceLock::new();
    FALLBACK
        .get_or_init(|| uuid::Uuid::new_v4().to_string().to_uppercase())
        .clone()
}

/// SWEncryption.signObject:suffix: 的 tokenSign。
/// SWJSON.sortedQueryString 按 key 排序：`timeStamp=<ts>&token=<token>&uid=<uid>`。
pub fn native_token_sign(uid: i64, token: &str, timestamp_ms: i64) -> String {
    let query = format!("timeStamp={}&token={}&uid={}", timestamp_ms, token, uid);
    md5_hex(format!("{}{}", query, TOKEN_SIGN_SUFFIX).as_bytes())
}

fn insert_uuid(map: &mut serde_json::Map<String, serde_json::Value>) -> String {
    let u = uuid::Uuid::new_v4().to_string().to_uppercase();
    map.insert("nonce".into(), serde_json::Value::String(u.clone()));
    u
}

/// iOS 请求头明文（SWRequestHeaderProvider._fillFullHeaders 字段序）。
/// 返回 (紧凑 JSON, HTTP 附加头字段: nonce/tokenSign/timeStamp)。
pub fn build_native_ios_header(
    identity: &HeaderIdentity,
    uid: i64,
    token: &str,
    timestamp_ms: Option<i64>,
) -> (String, Vec<(String, String)>) {
    let mut m = serde_json::Map::new();
    let device_id = if identity.device_id.is_empty() {
        // 与 Android 侧一致：进程内固定，避免「DeviceId 逐请求漂移」。
        stable_fallback_device_id()
    } else {
        identity.device_id.clone()
    };
    let ts = timestamp_ms.unwrap_or_else(now_ms);
    let install = identity.install_time(ts);

    m.insert("osType".into(), serde_json::Value::String("1".into()));
    m.insert("DeviceId".into(), serde_json::Value::String(device_id.clone()));
    m.insert("deviceName".into(), serde_json::Value::String(identity.device_name.clone()));
    m.insert(
        "CustomDeviceId".into(),
        serde_json::Value::String(format!("{device_id}_iOS_sportsWorld_campus")),
    );
    m.insert("osVersion".into(), serde_json::Value::String(identity.os_version.clone()));
    if !identity.idfa.is_empty() {
        m.insert("IDFA".into(), serde_json::Value::String(identity.idfa.clone()));
    }
    // 逻辑/物理分辨率必须整数倍自洽，且与 cpuModel 一起构成"真机"特征。
    // 写死 360x640 / 1080x1920 再配 x86_64，等于把自己标成 iOS 模拟器。
    let (logic_pixel, physic_pixel) = match (identity.logic_pixel.trim(), identity.pixel.trim()) {
        ("", "") => ("393x852".to_string(), "1179x2556".to_string()),
        ("", physic) => (derive_ios_logic(physic), physic.to_string()),
        (logic, "") => (logic.to_string(), "1179x2556".to_string()),
        (logic, physic) => (logic.to_string(), physic.to_string()),
    };
    m.insert("logicPixel".into(), serde_json::Value::String(logic_pixel));
    m.insert("physicPixel".into(), serde_json::Value::String(physic_pixel));
    m.insert("cpuModel".into(), serde_json::Value::String("arm64".into()));
    m.insert("appVersion".into(), serde_json::Value::String(IOS_APP_VERSION.into()));
    m.insert("isRoot".into(), serde_json::Value::Bool(false));
    m.insert("appInstallTime".into(), serde_json::Value::from(install));
    let nonce = insert_uuid(&mut m);
    if uid >= 1 {
        m.insert("uid".into(), serde_json::Value::from(uid));
    }
    if !token.is_empty() {
        m.insert("token".into(), serde_json::Value::String(token.to_string()));
    }
    m.insert("timeStamp".into(), serde_json::Value::from(ts));
    m.insert("studentId".into(), serde_json::Value::from(if uid >= 1 { uid } else { 0 }));
    if uid >= 1 && !token.is_empty() {
        m.insert(
            "tokenSign".into(),
            serde_json::Value::String(native_token_sign(uid, token, ts)),
        );
    }
    let extra = vec![
        ("nonce".to_string(), nonce),
        ("timeStamp".to_string(), ts.to_string()),
        (
            "tokenSign".to_string(),
            native_token_sign(uid, token, ts),
        ),
    ];
    (serde_json::to_string(&m).unwrap_or_default(), extra)
}

/// Android 399 请求头明文（NativeNetworkCommonHeaders.build 键集，osType="0"）。
pub fn build_android_header(
    identity: &HeaderIdentity,
    uid: i64,
    token: &str,
    timestamp_ms: Option<i64>,
) -> (String, Vec<(String, String)>) {
    let mut m = serde_json::Map::new();
    let device_id = if identity.device_id.is_empty() {
        // 兜底：同一进程内固定，绝不"每请求新生成 UUID"。
        // 否则会形成「DeviceId 逐请求漂移、androidId 恒定」这种真机不可能的组合。
        // 正常路径下 identity.device_id 由 load_identity_for_platform 保证非空。
        stable_fallback_device_id()
    } else {
        identity.device_id.clone()
    };
    let ts = timestamp_ms.unwrap_or_else(now_ms);
    let install = identity.install_time(ts);

    m.insert("Accept".into(), serde_json::Value::String("application/json".into()));
    m.insert("Content-Type".into(), serde_json::Value::String("application/json".into()));
    m.insert("appVersion".into(), serde_json::Value::String(ANDROID_APP_VERSION.into()));
    m.insert("osType".into(), serde_json::Value::String("0".into()));
    m.insert("DeviceId".into(), serde_json::Value::String(device_id.clone()));
    m.insert("osVersion".into(), serde_json::Value::String(identity.os_version.clone()));
    m.insert("deviceName".into(), serde_json::Value::String(identity.device_name.clone()));
    m.insert("IMEI".into(), serde_json::Value::String(String::new()));
    // 分辨率必须与 deviceName 自洽：22081212C 的真机是 1220x2712，
    // 统一上报 1080x2400 会形成"机型-屏幕"矛盾指纹。
    let pixel = if identity.pixel.trim().is_empty() {
        "1080x2400".to_string()
    } else {
        identity.pixel.clone()
    };
    m.insert("logicPixel".into(), serde_json::Value::String(pixel.clone()));
    m.insert("physicPixel".into(), serde_json::Value::String(pixel));
    // Android 10+ 普通应用取不到 IMEI（留空即官方行为）；androidId 一定能取到。
    m.insert(
        "androidId".into(),
        serde_json::Value::String(identity.android_id.clone()),
    );
    // Android 6+ 起 getMacAddress() 对普通应用固定返回 02:00:00:00:00:00，
    // 伪造随机 MAC 反而是异常。
    m.insert(
        "blMac".into(),
        serde_json::Value::String(crate::api::fingerprint::ANDROID_PLACEHOLDER_MAC.into()),
    );
    m.insert(
        "wifiMac".into(),
        serde_json::Value::String(crate::api::fingerprint::ANDROID_PLACEHOLDER_MAC.into()),
    );
    m.insert("cpuModel".into(), serde_json::Value::String("arm64-v8a".into()));
    m.insert("isRoot".into(), serde_json::Value::Bool(false));
    m.insert("appUpdateTime".into(), serde_json::Value::from(install));
    m.insert("appInstallTime".into(), serde_json::Value::from(install));
    let nonce = insert_uuid(&mut m);
    m.insert("timeStamp".into(), serde_json::Value::from(ts));
    m.insert(
        "CustomDeviceId".into(),
        serde_json::Value::String(format!("{device_id}_android_sportsWorld_campus")),
    );
    m.insert("uid".into(), serde_json::Value::from(uid));
    m.insert("token".into(), serde_json::Value::String(token.to_string()));
    m.insert("studentId".into(), serde_json::Value::from(uid));
    let sign = native_token_sign(uid, token, ts);
    m.insert("tokenSign".into(), serde_json::Value::String(sign.clone()));

    let extra = vec![
        ("nonce".to_string(), nonce),
        ("timeStamp".to_string(), ts.to_string()),
        ("tokenSign".to_string(), sign),
    ];
    (serde_json::to_string(&m).unwrap_or_default(), extra)
}

/// 按 identity 平台选择 header 构造器。
pub fn build_header_for(
    identity: &HeaderIdentity,
    uid: i64,
    token: &str,
    timestamp_ms: Option<i64>,
) -> (String, Vec<(String, String)>) {
    if identity.platform == "android" {
        build_android_header(identity, uid, token, timestamp_ms)
    } else {
        build_native_ios_header(identity, uid, token, timestamp_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 实测向量。
    #[test]
    fn test_token_sign_vector() {
        assert_eq!(
            native_token_sign(13056447, "TOKENABC", 1788958186123),
            "4a8d163186d91ac7b539a7bb07d457ba"
        );
    }

    /// iOS 头键序与条件字段（缺省字段跳过）。
    #[test]
    fn test_ios_header_shape() {
        let identity = HeaderIdentity {
            device_id: "0FA3EA16-46F0-4E44-9A97-1B63F0D9B11A".into(),
            idfa: String::new(),
            ..Default::default()
        };
        let (json, _) = build_native_ios_header(&identity, -1, "", Some(1788958186123));
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "osType", "DeviceId", "deviceName", "CustomDeviceId", "osVersion",
                "logicPixel", "physicPixel", "cpuModel", "appVersion", "isRoot",
                "appInstallTime", "nonce", "timeStamp", "studentId"
            ]
        );
        assert_eq!(v["osType"], "1");
        assert_eq!(v["studentId"], 0);
        // 带 uid/token 时出现 tokenSign
        let (json2, _) = build_native_ios_header(&identity, 13056447, "TOKEN", Some(1788958186123));
        let v2: serde_json::Value = serde_json::from_str(&json2).unwrap();
        assert!(v2.get("tokenSign").is_some());
        assert_eq!(v2["uid"], 13056447);
        assert_eq!(v2["tokenSign"], native_token_sign(13056447, "TOKEN", 1788958186123));
    }

    /// Android 头键集 + osType="0"。
    #[test]
    fn test_android_header_shape() {
        let identity = HeaderIdentity {
            platform: "android".into(),
            device_id: "D0FA3EA16-46F0-4E44-9A97-1B63F0D9B11A".into(),
            os_version: "14".into(),
            device_name: "22081212C".into(),
            ..Default::default()
        };
        let (json, _) = build_android_header(&identity, 42, "TK", Some(1788958186123));
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["osType"], "0");
        assert_eq!(v["appVersion"], "7.4.0");
        assert_eq!(v["studentId"], 42);
        assert!(v.get("tokenSign").is_some());
        assert!(json.starts_with("{\"Accept\":\"application/json\""));
    }

    /// 同一身份的 appInstallTime 跨请求零漂移（设备一致性防护）。
    #[test]
    fn test_install_time_stable_across_requests() {
        let identity = HeaderIdentity { app_install_time: 1_700_000_000_000_i64, ..Default::default() };
        let (j1, _) = build_native_ios_header(&identity, 1, "T", Some(1788958186123));
        let (j2, _) = build_native_ios_header(&identity, 1, "T", Some(1788958187000));
        let v1: serde_json::Value = serde_json::from_str(&j1).unwrap();
        let v2: serde_json::Value = serde_json::from_str(&j2).unwrap();
        assert_eq!(v1["appInstallTime"], v2["appInstallTime"]);
        assert_eq!(v1["appInstallTime"], 1_700_000_000_000_i64);
        // Android appUpdateTime == appInstallTime，同样稳定
        let (a1, _) = build_android_header(&identity, 1, "T", Some(1788958186123));
        let (a2, _) = build_android_header(&identity, 1, "T", Some(1788958190000));
        let a1: serde_json::Value = serde_json::from_str(&a1).unwrap();
        let a2: serde_json::Value = serde_json::from_str(&a2).unwrap();
        assert_eq!(a1["appInstallTime"], a2["appInstallTime"]);
        assert_eq!(a1["appUpdateTime"], a1["appInstallTime"]);
        // 身份未持久化安装时间时，回退值在同一请求内至少自洽
        let fallback = HeaderIdentity::default();
        assert_eq!(fallback.install_time(1000), 1000 - 3 * 86_400_000);
    }

    #[test]
    fn legacy_dalian_defaults_are_not_accepted_for_a_run() {
        assert!(HeaderIdentity::default().has_unconfigured_default_location());
        let configured = HeaderIdentity {
            city: "石家庄市".into(),
            anchor_lat: 38.0428,
            anchor_lon: 114.5149,
            ..Default::default()
        };
        assert!(!configured.has_unconfigured_default_location());
    }
}
