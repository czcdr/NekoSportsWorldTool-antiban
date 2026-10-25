//! 设备指纹：机型 ↔ 分辨率 ↔ 系统版本 ↔ androidId / MAC 自洽生成。
//!
//! 服务端能看到两类"不像官方客户端"的信号，判定成本远低于轨迹分析：
//! 1. 机型与分辨率不匹配（例如 `22081212C` 配 1080x2400，真机是 1220x2712）；
//! 2. 安卓端一定能取到的字段为空（`androidId`），或取了不可能的值（随机 MAC）。
//!
//! Android 6.0 起 `WifiInfo.getMacAddress()` 对普通应用固定返回 `02:00:00:00:00:00`，
//! Android 10 起 `TelephonyManager.getImei()` 需要特权权限同样取不到。因此官方客户端
//! 上报的就是空 IMEI + 全零 MAC —— 伪造随机 MAC 反而是异常。
//!
//! 本模块只负责"生成一套内部自洽的档案"，持久化与请求头拼装仍在 `crypto::header`。

/// 一个真实在售的 Android 机型档案。
#[derive(Debug, Clone, Copy)]
pub struct ModelProfile {
    /// 官方 `Build.MODEL`，服务端 `deviceName` 字段。
    pub device_name: &'static str,
    /// 逻辑分辨率（宽, 高），即 `logicPixel` / `physicPixel` 上报值。
    pub logical: (u32, u32),
    /// 屏幕密度 dpi。
    pub dpi: u32,
    /// 该机型出厂可能搭载的 Android 主版本。
    pub os_versions: &'static [&'static str],
    /// 厂商，仅本地展示用。
    pub brand: &'static str,
}

/// Android 6.0+ 普通应用读到的 MAC 固定值。
pub const ANDROID_PLACEHOLDER_MAC: &str = "02:00:00:00:00:00";

/// 机型库。只收录**机型代号与屏幕参数都由机型命名规则/公开规格双向核对过**的机型：
/// - 代号前 4 位是发布年月（`2208` = 2022-08），可用来交叉确认是哪台机；
/// - 参数只填「竖屏宽 x 高」，与上报的 `logicPixel` 一致。
///
/// 口径：**宁可少收录，也不要收录参数没把握的机型** —— 一条错误的
/// 「机型 ↔ 分辨率」对照会让本地自检把错数据当成正确答案，比不检查更糟。
/// 无法核实的机型请自己填写（`audit` 只给提示，不会强行改写）。
pub const MODELS: &[ModelProfile] = &[
    // Redmi K50 至尊版（2208 = 2022-08）：2712x1220 1.5K
    ModelProfile { device_name: "22081212C", logical: (1220, 2712), dpi: 440, os_versions: &["13", "14"], brand: "Xiaomi" },
    // Xiaomi 13（2211 = 2022-11）：2400x1080
    ModelProfile { device_name: "2211133C", logical: (1080, 2400), dpi: 440, os_versions: &["13", "14"], brand: "Xiaomi" },
    // Xiaomi 14（2312 = 2023-12）：2670x1200
    ModelProfile { device_name: "23127PN0CC", logical: (1200, 2670), dpi: 460, os_versions: &["14"], brand: "Xiaomi" },
    // Xiaomi 11 Ultra（M2102 = 2021-02）：3200x1440
    ModelProfile { device_name: "M2102K1AC", logical: (1440, 3200), dpi: 515, os_versions: &["12", "13"], brand: "Xiaomi" },
    // OnePlus 12（PJD110）：3168x1440
    ModelProfile { device_name: "PJD110", logical: (1440, 3168), dpi: 510, os_versions: &["14"], brand: "OnePlus" },
    // OnePlus Ace 2 Pro（PHK110）：2772x1240 1.5K
    ModelProfile { device_name: "PHK110", logical: (1240, 2772), dpi: 450, os_versions: &["13", "14"], brand: "OnePlus" },
    // vivo X90 Pro（V2227A）：2800x1260
    ModelProfile { device_name: "V2227A", logical: (1260, 2800), dpi: 453, os_versions: &["13", "14"], brand: "vivo" },
    // vivo X100（V2309A）：2800x1260
    ModelProfile { device_name: "V2309A", logical: (1260, 2800), dpi: 453, os_versions: &["14"], brand: "vivo" },
    // Huawei Mate 60 Pro（ALN-AL80）：2720x1260
    ModelProfile { device_name: "ALN-AL80", logical: (1260, 2720), dpi: 460, os_versions: &["12", "13"], brand: "HUAWEI" },
    // realme GT5（RMX3820）：2772x1240 1.5K
    ModelProfile { device_name: "RMX3820", logical: (1240, 2772), dpi: 450, os_versions: &["13", "14"], brand: "realme" },
    // OPPO Find X6（PGFM10）：2772x1240 1.5K（竖屏 1240x2772）
    ModelProfile { device_name: "PGFM10", logical: (1240, 2772), dpi: 450, os_versions: &["13", "14"], brand: "OPPO" },
    // OPPO Find X2 Pro（PDEM10）：3168x1440
    ModelProfile { device_name: "PDEM10", logical: (1440, 3168), dpi: 513, os_versions: &["11", "12"], brand: "OPPO" },
    // Samsung Galaxy S23 Ultra（SM-S9180）：3088x1440
    ModelProfile { device_name: "SM-S9180", logical: (1440, 3088), dpi: 500, os_versions: &["13", "14"], brand: "samsung" },
];

/// 一台真实 iPhone 的屏幕与系统组合。
///
/// `deviceName` 在 iOS 上就是 `UIDevice.current.model`，真机固定返回 `"iPhone"`，
/// 所以自洽要落在**逻辑分辨率 × 缩放倍数 = 物理分辨率**上，而不是机型名。
#[derive(Debug, Clone, Copy)]
pub struct IosModelProfile {
    /// 逻辑分辨率（宽, 高），`UIScreen.bounds`。
    pub logical: (u32, u32),
    /// 物理分辨率（宽, 高），`nativeBounds`。
    pub physical: (u32, u32),
    /// 该机型可能搭载的 iOS 主版本。
    pub os_versions: &'static [&'static str],
}

/// iOS 机型库：都是公开规格里能双向核对的组合（逻辑×2 或 ×3 = 物理）。
pub const IOS_MODELS: &[IosModelProfile] = &[
    // iPhone 16 Pro：402x874 @3x
    IosModelProfile { logical: (402, 874), physical: (1206, 2622), os_versions: &["18.1", "18.2"] },
    // iPhone 15 Pro / 14 Pro：393x852 @3x
    IosModelProfile { logical: (393, 852), physical: (1179, 2556), os_versions: &["17.6", "18.1"] },
    // iPhone 14 / 13 / 12：390x844 @3x
    IosModelProfile { logical: (390, 844), physical: (1170, 2532), os_versions: &["17.6", "18.1"] },
    // iPhone 11 / XR：414x896 @2x
    IosModelProfile { logical: (414, 896), physical: (828, 1792), os_versions: &["16.7", "17.4"] },
    // iPhone SE 3：375x667 @2x
    IosModelProfile { logical: (375, 667), physical: (750, 1334), os_versions: &["16.7", "17.5"] },
];

/// 一套自洽的 iOS 屏幕档案（`deviceName` 恒为 `iPhone`，见上）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IosProfile {
    pub device_name: String,
    pub os_version: String,
    pub logic: String,
    pub physic: String,
}

impl IosProfile {
    /// 由 seed 确定性选一台机型 + 一个该系统版本。
    pub fn mint(seed: u64) -> Self {
        let mut state = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut next = || {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        let model = IOS_MODELS[(next() % IOS_MODELS.len() as u64) as usize];
        let os_version =
            model.os_versions[(next() % model.os_versions.len() as u64) as usize];
        Self {
            device_name: "iPhone".into(),
            os_version: os_version.to_string(),
            logic: format!("{}x{}", model.logical.0, model.logical.1),
            physic: format!("{}x{}", model.physical.0, model.physical.1),
        }
    }

    /// 自检：物理分辨率必须是逻辑分辨率的整数倍（2x / 3x）。
    pub fn audit(logic: &str, physic: &str) -> Vec<String> {
        let parse = |s: &str| -> Option<(u32, u32)> {
            let (w, h) = s.trim().split_once(['x', 'X', '*'])?;
            Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
        };
        let mut issues = Vec::new();
        match (parse(logic), parse(physic)) {
            (Some((lw, lh)), Some((pw, ph))) => {
                if lw == 0 || lh == 0 || pw % lw != 0 || ph % lh != 0 || pw / lw != ph / lh {
                    issues.push(format!(
                        "iOS 分辨率不自洽：physicPixel {physic} 不是 logicPixel {logic} 的整数倍"
                    ));
                }
            }
            _ => issues.push(format!("iOS 分辨率格式无效：{logic} / {physic}")),
        }
        issues
    }
}

/// 一套完整、内部自洽的设备档案。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceProfile {
    pub device_name: String,
    pub brand: String,
    pub os_version: String,
    /// `logicPixel` 与 `physicPixel` 的上报值。
    pub pixel: String,
    pub dpi: u32,
    /// Android `Settings.Secure.ANDROID_ID`：16 位小写十六进制。
    pub android_id: String,
    pub bl_mac: String,
    pub wifi_mac: String,
}

impl DeviceProfile {
    /// 由 seed 确定性地生成一套自洽档案（同 seed 恒等，便于持久化与复现）。
    pub fn mint(seed: u64) -> Self {
        // splitmix64：把连续 seed 打散，避免相邻 seed 选到同一机型。
        let mut state = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut next = || {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };

        let model = MODELS[(next() % MODELS.len() as u64) as usize];
        let os_version = model.os_versions[(next() % model.os_versions.len() as u64) as usize];

        // androidId：16 位小写 hex。官方值是随机的 64-bit，这里保持同构。
        let android_id = format!("{:016x}", next());

        Self {
            device_name: model.device_name.to_string(),
            brand: model.brand.to_string(),
            os_version: os_version.to_string(),
            pixel: format!("{}x{}", model.logical.0, model.logical.1),
            dpi: model.dpi,
            android_id,
            bl_mac: ANDROID_PLACEHOLDER_MAC.to_string(),
            wifi_mac: ANDROID_PLACEHOLDER_MAC.to_string(),
        }
    }

    /// 从已有身份字段还原档案；缺失字段回退到 seed 生成值，保证老配置也能补齐。
    pub fn from_parts(
        device_name: &str,
        os_version: &str,
        pixel: &str,
        android_id: &str,
        seed: u64,
    ) -> Self {
        let base = Self::mint(seed);
        if device_name.trim().is_empty() {
            // 空机型：用 seed 抽到的机型补齐，但**保留调用方已经给的值**
            // （早退会连同 os_version / pixel / android_id 一起丢掉）。
            return Self {
                device_name: base.device_name,
                brand: base.brand,
                os_version: if os_version.trim().is_empty() {
                    base.os_version
                } else {
                    os_version.to_string()
                },
                pixel: if pixel.trim().is_empty() {
                    base.pixel
                } else {
                    pixel.to_string()
                },
                dpi: base.dpi,
                android_id: if android_id.trim().is_empty() {
                    base.android_id
                } else {
                    android_id.to_string()
                },
                bl_mac: ANDROID_PLACEHOLDER_MAC.to_string(),
                wifi_mac: ANDROID_PLACEHOLDER_MAC.to_string(),
            };
        }
        // 已有机型：只在缺失时补，不覆盖用户手填值。
        let model = MODELS.iter().find(|m| m.device_name == device_name);
        let pixel = if pixel.trim().is_empty() {
            model
                .map(|m| format!("{}x{}", m.logical.0, m.logical.1))
                .unwrap_or(base.pixel)
        } else {
            pixel.to_string()
        };
        let os_version = if os_version.trim().is_empty() {
            model
                .and_then(|m| m.os_versions.last())
                .map(|v| v.to_string())
                .unwrap_or(base.os_version)
        } else {
            os_version.to_string()
        };
        Self {
            device_name: device_name.to_string(),
            brand: model.map(|m| m.brand.to_string()).unwrap_or_default(),
            os_version,
            pixel,
            dpi: model.map(|m| m.dpi).unwrap_or(base.dpi),
            android_id: if android_id.trim().is_empty() {
                base.android_id
            } else {
                android_id.to_string()
            },
            bl_mac: ANDROID_PLACEHOLDER_MAC.to_string(),
            wifi_mac: ANDROID_PLACEHOLDER_MAC.to_string(),
        }
    }

    /// 机型是否在库内（用于判定"分辨率是否可信"）。
    pub fn is_known_model(&self) -> bool {
        MODELS.iter().any(|m| m.device_name == self.device_name)
    }
}

/// 严格版：只按机型库核对（设备页在"非真机读数"时用它）。
#[cfg_attr(not(test), allow(dead_code))]
pub fn audit(profile: &DeviceProfile) -> Vec<String> {
    audit_ex(profile, true)
}

/// 校验一套指纹内部是否自洽，返回问题列表（空 = 自洽）。
///
/// 这是"防封自检"的硬规则：只要有一条不满足，服务端就有理由把请求
/// 和"官方客户端"区分开。
///
/// `check_model=false` 用于**读数来自本机真机**的身份：那台机不在机型表里
/// 是正常的（表只收录了十几款），此时不该告警，只校验 androidId 与 MAC。
pub fn audit_ex(profile: &DeviceProfile, check_model: bool) -> Vec<String> {
    let mut issues = Vec::new();
    if check_model {
        if !profile.is_known_model() {
            issues.push(format!("机型 {} 不在已知库内，分辨率无法核对", profile.device_name));
        } else if let Some(model) = MODELS.iter().find(|m| m.device_name == profile.device_name) {
            let expected = format!("{}x{}", model.logical.0, model.logical.1);
            if profile.pixel != expected {
                issues.push(format!(
                    "机型 {} 的分辨率应为 {expected}，当前 {}",
                    profile.device_name, profile.pixel
                ));
            }
            if !model.os_versions.iter().any(|v| *v == profile.os_version) {
                issues.push(format!(
                    "机型 {} 不支持 Android {}（支持 {:?}）",
                    profile.device_name, profile.os_version, model.os_versions
                ));
            }
        }
    }

    if profile.android_id.len() != 16 || !profile.android_id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()) {
        issues.push(format!(
            "androidId 必须是 16 位小写十六进制，当前 {:?}",
            profile.android_id
        ));
    }
    if profile.wifi_mac != ANDROID_PLACEHOLDER_MAC || profile.bl_mac != ANDROID_PLACEHOLDER_MAC {
        issues.push(format!(
            "Android 6+ 普通应用只能读到 {ANDROID_PLACEHOLDER_MAC}，随机 MAC 属于异常指纹"
        ));
    }
    issues
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 机型表的硬约束。`logical.0 <= logical.1` 这一条专门防
    /// 「宽高写反」这类错误（PGFM10 曾经就是 2772x1240）。
    #[test]
    fn model_table_is_well_formed() {
        assert!(!MODELS.is_empty(), "机型库不能为空（取模会除零）");
        for m in MODELS {
            assert!(!m.os_versions.is_empty(), "{} 缺少系统版本", m.device_name);
            assert!(
                m.logical.0 > 0 && m.logical.1 > 0,
                "{} 分辨率非法",
                m.device_name
            );
            assert!(
                m.logical.0 <= m.logical.1,
                "{} 分辨率应为竖屏口径（宽<=高），当前 {}x{}",
                m.device_name,
                m.logical.0,
                m.logical.1
            );
            assert!(m.dpi >= 200 && m.dpi <= 800, "{} dpi 离谱", m.device_name);
        }
    }

    #[test]
    fn mint_is_deterministic_and_self_consistent() {
        for seed in 0..200u64 {
            let a = DeviceProfile::mint(seed);
            let b = DeviceProfile::mint(seed);
            assert_eq!(a, b, "同 seed 必须恒等");
            assert!(audit(&a).is_empty(), "seed {seed} 生成不自洽: {:?}", audit(&a));
        }
    }

    #[test]
    fn mint_spreads_across_models() {
        let models: std::collections::HashSet<String> =
            (0..200u64).map(|s| DeviceProfile::mint(s).device_name).collect();
        assert!(models.len() >= 10, "机型分布过窄: {}", models.len());
    }

    #[test]
    fn audit_catches_the_real_anomalies() {
        // 上游默认：22081212C + 1080x2400（真机 1220x2712）
        let bad = DeviceProfile {
            device_name: "22081212C".into(),
            brand: "Xiaomi".into(),
            os_version: "14".into(),
            pixel: "1080x2400".into(),
            dpi: 440,
            android_id: "0123456789abcdef".into(),
            bl_mac: ANDROID_PLACEHOLDER_MAC.into(),
            wifi_mac: ANDROID_PLACEHOLDER_MAC.into(),
        };
        let issues = audit(&bad);
        assert!(issues.iter().any(|i| i.contains("1220x2712")), "{issues:?}");

        // androidId 为空 = 官方客户端不可能出现
        let empty_id = DeviceProfile { android_id: String::new(), ..bad.clone() };
        assert!(audit(&empty_id).iter().any(|i| i.contains("androidId")));

        // 随机 MAC = 异常
        let random_mac = DeviceProfile { wifi_mac: "a4:50:46:11:22:33".into(), ..bad.clone() };
        assert!(audit(&random_mac).iter().any(|i| i.contains("MAC")));

        // 系统版本超出机型范围
        let wrong_os = DeviceProfile { os_version: "16".into(), ..bad };
        assert!(audit(&wrong_os).iter().any(|i| i.contains("不支持")));
    }

    #[test]
    fn from_parts_never_overwrites_user_values() {
        let p = DeviceProfile::from_parts("22081212C", "13", "1220x2712", "deadbeefdeadbeef", 7);
        assert_eq!(p.device_name, "22081212C");
        assert_eq!(p.os_version, "13");
        assert_eq!(p.pixel, "1220x2712");
        assert_eq!(p.android_id, "deadbeefdeadbeef");
        assert!(audit(&p).is_empty());

        // 空字段用 seed 补齐，仍然自洽
        let filled = DeviceProfile::from_parts("2211133C", "", "", "", 9);
        assert_eq!(filled.pixel, "1080x2400");
        assert_eq!(filled.android_id.len(), 16);
        assert!(audit(&filled).is_empty());
    }
}
