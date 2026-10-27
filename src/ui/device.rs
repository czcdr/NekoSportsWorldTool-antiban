//! 设备信息页：iOS/Android 单选 + 6 字段 + 整套随机 + 持久化。
//! ⚠ 设备 ID 固定复用（防 10121 风控）；随机后必须手动「保存」才生效。

use super::{mobile, theme, App};
use crate::crypto::header::HeaderIdentity;
use eframe::egui;

const IOS_OS_POOL: [&str; 5] = ["17.5.1", "18.0.1", "18.1", "18.2", "26.5.2"];

#[derive(Default)]
pub struct DevicePage {
    saved_flash: f32,
}

impl App {
    #[cfg(target_os = "android")]
    pub(super) fn poll_device_info(&mut self) {
        let Some(reply) = crate::android::take_device_info() else {
            return;
        };
        let info = match reply.result {
            Ok(Some(info)) => info,
            Ok(None) => {
                self.status = "未读取本机信息，可在设备信息页手动填写或再次读取".into();
                return;
            }
            Err(error) => {
                self.status = format!("× {error}");
                return;
            }
        };
        let identity = if reply.initial {
            &self.identity
        } else {
            &self.device_buf
        };
        match crate::api::model::identity_with_device_info(identity, &info) {
            Ok(updated) => {
                self.device_buf = updated;
                if reply.initial {
                    match crate::api::model::save_identity(&self.device_buf) {
                        Ok(()) => {
                            self.identity = self.device_buf.clone();
                            crate::android::complete_device_info();
                            self.status = format!(
                                "已读取 {} {} / Android {} / {}（{}dpi）；UUID 保持不变",
                                info.manufacturer,
                                info.model,
                                info.os_version,
                                info.pixel,
                                info.density_dpi
                            );
                        }
                        Err(error) => {
                            self.tab = 5;
                            self.status = format!("× 保存失败：{error}，请在设备页重新保存");
                        }
                    }
                } else {
                    self.status = format!(
                        "已读取 {} {} / {}；点击保存后生效，UUID 保持不变",
                        info.manufacturer, info.model, info.pixel
                    );
                }
            }
            Err(error) => self.status = format!("× {error}"),
        }
    }

    pub fn draw_device(&mut self, ui: &mut egui::Ui) {
        let mut page = std::mem::take(&mut self.device_page);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| self.draw_device_inner(ui, &mut page));
        self.device_page = page;
    }

    fn draw_device_inner(&mut self, ui: &mut egui::Ui, page: &mut DevicePage) {
        ui.add_space(6.0);

        mobile::row(ui, |ui| {
            ui.radio_value(&mut self.device_buf.platform, "ios".to_string(), "iOS")
                .on_hover_text(
                    "iOS 身份只覆盖登录等链路；官方客户端只有 Android，提交会因平台不一致被拒绝。\
                     要跑记录请选 Android。",
                );
            ui.radio_value(
                &mut self.device_buf.platform,
                "android".to_string(),
                "Android",
            );
            ui.separator();
            let platform = self.device_buf.platform.clone();
            if ui.button("随机生成").clicked() {
                randomize(&mut self.device_buf, &platform);
                self.status = "已生成随机设备（未保存——手动保存后才生效）".into();
            }
        });
        if self.device_buf.platform != "android" {
            // A1：iOS 身份不能提交（提交报文固定 Android）。与其让用户填完表单、
            // 跑完再被拒，不如在选平台这一步就说清。
            ui.colored_label(
                theme::warn(),
                "⚠ 选 iOS 时无法提交跑步记录：官方客户端只有 Android，\
                 提交前会因平台不一致被拒绝。要跑记录请选 Android。",
            );
        }

        #[cfg(target_os = "android")]
        if ui.button("读取本机信息").clicked() {
            crate::android::request_device_info(false);
        }

        ui.add_space(6.0);
        let is_ios = self.device_buf.platform != "android";
        let id_label = if is_ios {
            "DeviceId（UUID 大写）"
        } else {
            "DeviceId（Android）"
        };
        let idfa_label = if is_ios {
            "IDFA（可空）"
        } else {
            "IMEI（可空）"
        };
        let name_label = if is_ios { "设备名" } else { "机型" };
        if mobile::compact_ui(ui) {
            ui.vertical(|ui| {
                ui.label(id_label);
                mobile::text_edit(
                    ui,
                    "device_id",
                    &mut self.device_buf.device_id,
                    crate::platform::InputKind::Text,
                    ui.available_width(),
                );
                ui.label(idfa_label);
                mobile::text_edit(
                    ui,
                    "device_idfa",
                    &mut self.device_buf.idfa,
                    crate::platform::InputKind::Text,
                    ui.available_width(),
                );
                ui.label("系统版本：");
                mobile::text_edit(
                    ui,
                    "device_os",
                    &mut self.device_buf.os_version,
                    crate::platform::InputKind::Text,
                    ui.available_width(),
                );
                ui.label(name_label);
                mobile::text_edit(
                    ui,
                    "device_name",
                    &mut self.device_buf.device_name,
                    crate::platform::InputKind::Text,
                    ui.available_width(),
                );
                if !is_ios {
                    ui.label("品牌（本地展示，可空）：");
                    mobile::text_edit(
                        ui,
                        "device_manufacturer",
                        &mut self.device_buf.manufacturer,
                        crate::platform::InputKind::Text,
                        ui.available_width(),
                    );
                }
                ui.label("城市：");
                mobile::text_edit(
                    ui,
                    "device_city",
                    &mut self.device_buf.city,
                    crate::platform::InputKind::Text,
                    ui.available_width(),
                );
                ui.label("定位锚点纬度：");
                mobile::drag_f64(
                    ui,
                    "device_lat",
                    &mut self.device_buf.anchor_lat,
                    -90.0..=90.0,
                    0.00001,
                    6,
                );
                ui.label("定位锚点经度：");
                mobile::drag_f64(
                    ui,
                    "device_lon",
                    &mut self.device_buf.anchor_lon,
                    -180.0..=180.0,
                    0.00001,
                    6,
                );
            });
        } else {
            egui::Grid::new("device_grid")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    ui.label(id_label);
                    mobile::text_edit(
                        ui,
                        "device_id",
                        &mut self.device_buf.device_id,
                        crate::platform::InputKind::Text,
                        340.0,
                    );
                    ui.end_row();
                    ui.label(idfa_label);
                    mobile::text_edit(
                        ui,
                        "device_idfa",
                        &mut self.device_buf.idfa,
                        crate::platform::InputKind::Text,
                        340.0,
                    );
                    ui.end_row();
                    ui.label("系统版本：");
                    mobile::text_edit(
                        ui,
                        "device_os",
                        &mut self.device_buf.os_version,
                        crate::platform::InputKind::Text,
                        120.0,
                    );
                    ui.end_row();
                    ui.label(name_label);
                    mobile::text_edit(
                        ui,
                        "device_name",
                        &mut self.device_buf.device_name,
                        crate::platform::InputKind::Text,
                        200.0,
                    );
                    ui.end_row();
                    if !is_ios {
                        ui.label("品牌（本地展示，可空）：");
                        mobile::text_edit(
                            ui,
                            "device_manufacturer",
                            &mut self.device_buf.manufacturer,
                            crate::platform::InputKind::Text,
                            200.0,
                        );
                        ui.end_row();
                    }
                    ui.label("城市：");
                    mobile::text_edit(
                        ui,
                        "device_city",
                        &mut self.device_buf.city,
                        crate::platform::InputKind::Text,
                        120.0,
                    );
                    ui.end_row();
                    ui.label("定位锚点纬度：");
                    mobile::drag_f64(
                        ui,
                        "device_lat",
                        &mut self.device_buf.anchor_lat,
                        -90.0..=90.0,
                        0.00001,
                        6,
                    );
                    ui.end_row();
                    ui.label("定位锚点经度：");
                    mobile::drag_f64(
                        ui,
                        "device_lon",
                        &mut self.device_buf.anchor_lon,
                        -180.0..=180.0,
                        0.00001,
                        6,
                    );
                    ui.end_row();
                });
        }

        ui.add_space(4.0);
        ui.colored_label(
            theme::text_dim(),
            "定位锚点填写顺序：先填纬度，再填经度；地图常见的“经度,纬度”格式需要调换后填写。",
        );
        ui.colored_label(
            theme::text_dim(),
            "修改城市或定位锚点后，请点击“保存”；跑步流程只使用已保存的身份信息。",
        );
        if location_fields_changed(&self.identity, &self.device_buf) {
            ui.colored_label(
                theme::warn(),
                "检测到城市或定位锚点有未保存修改，当前跑步仍会使用上次保存的值。",
            );
        }

        ui.add_space(8.0);
        mobile::row(ui, |ui| {
            if ui.add(theme::primary_btn("保存")).clicked() {
                // 机型变了就重置派生指纹：否则会得到"新机型 + 旧分辨率/androidId"
                // 这种自相矛盾的组合，比不做随机更糟。
                // 手改机型后来源不再是真机读取，标记回 false，让自检恢复按机型库核对。
                if self.device_buf.platform != "android" {
                    self.device_buf.android_id.clear();
                    self.device_buf.pixel.clear();
                    self.device_buf.fingerprint_from_device = false;
                } else {
                    if self.device_buf.device_name != self.identity.device_name {
                        self.device_buf.android_id.clear();
                        self.device_buf.pixel.clear();
                        self.device_buf.fingerprint_from_device = false;
                    }
                    // 只补空字段，不覆盖真机读数 / 用户手填值。
                    self.device_buf.ensure_fingerprint();
                }
                match crate::api::model::save_identity(&self.device_buf) {
                    Ok(()) => {
                        self.identity = self.device_buf.clone();
                        self.status = "√ 设备身份已保存并生效".into();
                        self.log.push("√  已更新（新设备 ID 从下个请求开始生效）");
                        page.saved_flash = 2.0;
                    }
                    Err(e) => self.status = format!("× 保存失败：{e}"),
                }
            }
            if ui.button("撤销修改").clicked() {
                self.device_buf = self.identity.clone();
            }
            if page.saved_flash > 0.0 {
                ui.colored_label(theme::ok(), "已保存");
            }
        });

        ui.add_space(12.0);
        ui.separator();
        ui.colored_label(
            theme::warn(),
            "风控提示：设备 ID 参与服务端会话校验，固定复用可显著降低 10121（设备风险）概率；",
        );
        ui.colored_label(
            theme::warn(),
            "不要每次启动都换设备；「随机生成」只改界面缓冲区，手动「保存」后才写入  生效。",
        );
        ui.add_space(4.0);
        ui.label(format!(
            "当前生效身份：{} / {} / {} / 城市 {} / 锚点({:.6},{:.6})",
            if self.identity.platform == "android" {
                "Android"
            } else {
                "iOS"
            },
            self.identity.device_name,
            self.identity.os_version,
            self.identity.city,
            self.identity.anchor_lat,
            self.identity.anchor_lon,
        ));
        if self.identity.has_unconfigured_default_location() {
            ui.colored_label(
                theme::warn(),
                "运行前请填写实际城市和定位锚点；默认大连配置不会用于提交",
            );
        }
        // 指纹自洽自检：机型↔分辨率↔系统版本↔androidId，比轨迹更容易被服务端判定。
        // 读数来自真机时（fingerprint_from_device）不按机型库核对 —— 这台机不在机型表里是正常的。
        if self.identity.platform == "android" {
            let profile = crate::api::fingerprint::DeviceProfile::from_parts(
                &self.identity.device_name,
                &self.identity.os_version,
                &self.identity.pixel,
                &self.identity.android_id,
                0,
            );
            let issues =
                crate::api::fingerprint::audit_ex(&profile, !self.identity.fingerprint_from_device);
            if issues.is_empty() {
                ui.colored_label(
                    theme::ok(),
                    format!(
                        "√ 指纹自洽{}：{} / {} / androidId {}",
                        if self.identity.fingerprint_from_device {
                            "（本机读数）"
                        } else {
                            ""
                        },
                        profile.device_name,
                        profile.pixel,
                        profile.android_id
                    ),
                );
            } else {
                for issue in issues {
                    ui.colored_label(theme::warn(), format!("⚠ {issue}"));
                }
            }
        }
        if page.saved_flash > 0.0 {
            page.saved_flash -= ui.ctx().input(|i| i.stable_dt);
        }
        if self.identity.platform == "android" && !self.identity.manufacturer.is_empty() {
            ui.label(format!("当前保存品牌：{}", self.identity.manufacturer));
        }

        // ── 安全护栏开关 ──────────────────────────────────────────
        ui.add_space(12.0);
        ui.separator();
        ui.label("安全护栏（防封）");
        ui.colored_label(
            theme::text_dim(),
            "时段本身已有设计：随机时刻在 7:00-20:00、补签在 7:00-21:00 内抽样。\n\
             这里额外守住三件原本不管的事：深夜提交、同一自然日多条、一次补太多天。关闭后不再有任何频率保护。",
        );
        let policy = self.config.safety.clone();
        let label = format!(
            "启用（深夜 {}-{} 点、每日跑步 {} 次、AI {} 次、补签 ≤{} 天、同日重复 ≤{} 次）",
            policy.night_from,
            policy.night_to,
            policy.daily_runs,
            policy.daily_ai,
            policy.backfill_days,
            policy.batch_per_day
        );
        let mut enabled = policy.enabled;
        if ui.checkbox(&mut enabled, label).changed() {
            self.config.safety.enabled = enabled;
            match crate::api::model::save_config(&self.config) {
                Ok(()) => {
                    self.status = if enabled {
                        "√ 安全护栏已开启".into()
                    } else {
                        "⚠ 安全护栏已关闭：深夜与频率不再受保护".into()
                    }
                }
                Err(error) => self.status = format!("× 保存失败：{error}"),
            }
        }
        if !self.config.safety.enabled {
            ui.colored_label(theme::warn(), "⚠ 护栏已关闭：提交时段与次数完全由你控制");
        }
    }
}

fn location_fields_changed(saved: &HeaderIdentity, pending: &HeaderIdentity) -> bool {
    saved.city.trim() != pending.city.trim()
        || (saved.anchor_lat - pending.anchor_lat).abs() > 1e-9
        || (saved.anchor_lon - pending.anchor_lon).abs() > 1e-9
}

/// 整套随机：uuid v4 设备 ID（大写）；Android 走真实机型库生成自洽指纹。
fn randomize(buf: &mut HeaderIdentity, platform: &str) {
    buf.manufacturer.clear();
    buf.device_id = uuid::Uuid::new_v4().to_string().to_uppercase();
    buf.app_install_time = HeaderIdentity::fresh_install_time(platform);
    if platform == "android" {
        // 机型 / 分辨率 / 系统版本 / androidId 一次生成，保证内部自洽：
        // 伪机型名（Redmi K60、OnePlus ACE3 之类）不是真实 Build.MODEL，
        // 服务端一旦对照机型库就会露馅。
        let profile = crate::api::fingerprint::DeviceProfile::mint(rand::random::<u64>());
        buf.device_name = profile.device_name;
        buf.manufacturer = profile.brand;
        buf.os_version = profile.os_version;
        buf.pixel = profile.pixel;
        // androidId 独立随机（**不**跟着 device_id 派生）：真机上 ANDROID_ID 是
        // 系统在安装时随机给的，和 App 自定义的 DeviceId 没有派生关系。
        buf.android_id = crate::api::fingerprint::random_android_id();
        buf.idfa = String::new();
        // 随机生成的档案来自机型库，不是本机读数 → 自检按机型库核对。
        buf.fingerprint_from_device = false;
        buf.platform = "android".into();
    } else {
        let k = (rand::random::<f64>() * IOS_OS_POOL.len() as f64) as usize % IOS_OS_POOL.len();
        buf.device_name = "iPhone".into();
        buf.os_version = IOS_OS_POOL[k].into();
        buf.idfa = uuid::Uuid::new_v4().to_string().to_uppercase();
        buf.android_id.clear();
        buf.pixel.clear();
        buf.fingerprint_from_device = false;
        buf.platform = "ios".into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_changes_are_detected_until_saved() {
        let saved = HeaderIdentity::default();
        let mut pending = saved.clone();
        assert!(!location_fields_changed(&saved, &pending));

        pending.city = "北京市".into();
        assert!(location_fields_changed(&saved, &pending));

        pending = saved.clone();
        pending.anchor_lat += 0.000001;
        assert!(location_fields_changed(&saved, &pending));
    }

    /// 随机生成必须换掉旧品牌，并且 Android 新身份必须是**自洽**的一套
    /// （机型↔分辨率↔系统版本↔androidId），否则等于换了个更明显的破绽。
    #[test]
    fn random_device_replaces_brand_and_yields_a_consistent_identity() {
        for platform in ["android", "ios"] {
            let mut identity = HeaderIdentity {
                manufacturer: "Previously imported brand".into(),
                ..Default::default()
            };
            randomize(&mut identity, platform);
            assert_ne!(
                identity.manufacturer, "Previously imported brand",
                "random {platform} identity must not retain a different device's brand"
            );
            if platform == "android" {
                assert!(
                    !identity.manufacturer.is_empty(),
                    "Android 必须带与机型匹配的品牌"
                );
                assert_eq!(identity.android_id.len(), 16, "androidId 必须是 16 位");
                assert!(!identity.pixel.is_empty(), "分辨率必须一并生成");
                let profile = crate::api::fingerprint::DeviceProfile::from_parts(
                    &identity.device_name,
                    &identity.os_version,
                    &identity.pixel,
                    &identity.android_id,
                    0,
                );
                let issues = crate::api::fingerprint::audit(&profile);
                assert!(issues.is_empty(), "随机生成不自洽: {issues:?}");
            } else {
                assert!(identity.manufacturer.is_empty(), "iOS 没有品牌字段");
                assert!(identity.android_id.is_empty(), "iOS 不应携带 androidId");
            }
        }
    }
}
