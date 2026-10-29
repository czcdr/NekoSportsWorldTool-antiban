//! 运动记录页：跑步 / AI 两个分栏，egui_extras 表格。

use super::record_badges::BadgeState;
use super::record_status::RecordStatus;
use super::{mobile, theme, App};
use chrono::TimeZone;
use eframe::egui;
use egui_extras::{Column, TableBuilder};

/// 记录页在标签栏里的位置（`draw_shell` 的 match 与「离开页面即取消回填」共用）。
pub const RECORDS_TAB: usize = 2;

#[derive(Default)]
pub struct RecordsPage {
    pub rows: Vec<crate::api::records::RecordRow>,
    pub ai_groups: Vec<crate::api::ai::AiRecordGroup>,
    pub ai_total: i64,
    pub sub: usize,
    /// 当前展开详情的 rrid
    pub detail_rrid: Option<i64>,
    pub detail_raw: Option<serde_json::Value>,
    pub detail_loading: bool,
    /// 列表徽标：最近 5 条的三态判定（限速 + 缓存 + 可取消，见 `record_badges`）。
    pub badges: BadgeState,
    /// AI 详情页当前记录
    pub ai_detail_id: Option<i64>,
    pub ai_detail_raw: Option<serde_json::Value>,
    pub ai_detail_loading: bool,
}

impl App {
    pub fn draw_records(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        mobile::row(ui, |ui| {
            ui.selectable_value(&mut self.records_page.sub, 0, "跑步");
            ui.selectable_value(&mut self.records_page.sub, 1, "AI 运动");
            ui.add_space(8.0);
            match self.records_page.sub {
                0 => {
                    mobile::action_row(ui, |ui| {
                        if ui
                            .add_enabled(!self.records_busy, theme::primary_btn("刷新"))
                            .clicked()
                        {
                            self.refresh_records();
                        }
                    });
                    if self.records_busy {
                        ui.label("拉取中…");
                    }
                }
                _ => {
                    mobile::action_row(ui, |ui| {
                        if ui
                            .add_enabled(!self.records_busy, theme::primary_btn("刷新全部记录"))
                            .clicked()
                        {
                            self.refresh_ai_records();
                        }
                    });
                    if self.records_busy {
                        ui.label("拉取中…");
                    }
                }
            }
        });
        ui.separator();

        // 徽标回填的弱提示：只在跑的时候出现，不占位、不闪烁。
        // 「正在核对」不等于「列表还在加载」：列表数据始终是已经拿到的那份。
        if self.records_page.sub == 0 {
            if self.records_page.badges.pending() {
                ui.label(
                    egui::RichText::new("正在核对最近 5 条判定（只读，1 条/秒，串行）…")
                        .small()
                        .color(theme::text_dim()),
                );
            }
            if let Some(error) = self.records_page.badges.error() {
                ui.label(
                    egui::RichText::new(format!(
                        "判定没全部取到（只影响徽标，不影响列表）：{error}"
                    ))
                    .small()
                    .color(theme::warn()),
                );
            }
        }

        match self.records_page.sub {
            0 => self.draw_run_view(ui),
            _ => self.draw_ai_view(ui),
        }
    }

    /// AI 视图：列表与详情两个页面互斥。
    fn draw_ai_view(&mut self, ui: &mut egui::Ui) {
        if self.records_page.ai_detail_id.is_some() {
            self.draw_ai_detail_view(ui);
        } else {
            self.draw_ai_table(ui);
        }
    }

    /// AI 详情独立页：返回 + 全量字段。
    fn draw_ai_detail_view(&mut self, ui: &mut egui::Ui) {
        let id = self.records_page.ai_detail_id.unwrap_or(0);
        ui.add_space(4.0);
        mobile::row(ui, |ui| {
            if ui.button("← 返回列表").clicked() {
                self.records_page.ai_detail_id = None;
                self.records_page.ai_detail_raw = None;
                self.records_page.ai_detail_loading = false;
            }
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(format!("AI 运动详情 #{}", id))
                    .strong()
                    .color(theme::accent()),
            );
        });
        ui.separator();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.records_page.ai_detail_loading && self.records_page.ai_detail_raw.is_none()
                {
                    ui.label("加载详情中…");
                } else if let Some(raw) = &self.records_page.ai_detail_raw {
                    if let Some(err) = raw.get("fetchError").and_then(|x| x.as_str()) {
                        ui.colored_label(theme::err(), err);
                    } else {
                        draw_ai_detail_panel(ui, raw);
                    }
                } else {
                    ui.label("（无详情数据）");
                }
            });
    }

    /// 跑步视图：列表与详情两个页面互斥。
    fn draw_run_view(&mut self, ui: &mut egui::Ui) {
        if self.records_page.detail_rrid.is_some() {
            self.draw_detail_view(ui);
        } else {
            self.draw_run_table(ui);
        }
    }

    /// 详情独立页：返回 + 全部字段。
    fn draw_detail_view(&mut self, ui: &mut egui::Ui) {
        let rrid = self.records_page.detail_rrid.unwrap_or(0);
        ui.add_space(4.0);
        mobile::row(ui, |ui| {
            if ui.button("← 返回列表").clicked() {
                self.records_page.detail_rrid = None;
                self.records_page.detail_raw = None;
                self.records_page.detail_loading = false;
            }
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(format!("跑步详情 #{}", rrid))
                    .strong()
                    .color(theme::accent()),
            );
        });
        ui.separator();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.records_page.detail_loading && self.records_page.detail_raw.is_none() {
                    ui.label("加载详情中…");
                } else if let Some(raw) = &self.records_page.detail_raw {
                    draw_detail_panel(ui, raw);
                } else {
                    ui.label("（无详情数据）");
                }
            });
    }

    fn draw_run_table(&mut self, ui: &mut egui::Ui) {
        if self.records_page.rows.is_empty() {
            ui.add_space(20.0);
            ui.centered_and_justified(|ui| ui.label("暂无跑步记录"));
            return;
        }
        // 列表已经在了，先按需补徽标（限速 + 缓存都在 record_badges 里）。
        self.ensure_record_badges();
        let now_ms = crate::crypto::envelope::now_ms();
        let badges = &self.records_page.badges;
        let fmt_time = |ms: i64| -> String {
            chrono::Local
                .timestamp_millis_opt(ms)
                .single()
                .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_default()
        };
        let fmt_dur = |s: i64| -> String {
            if s >= 3600 {
                format!("{}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
            } else {
                format!("{}:{:02}", s / 60, s % 60)
            }
        };
        let pace_of = |dis: f64, t: i64| -> (i64, i64) {
            let p = if dis > 0.0 {
                t as f64 / (dis / 1000.0)
            } else {
                0.0
            };
            ((p / 60.0) as i64, (p as i64) % 60)
        };

        let toggle_target = std::cell::Cell::new(None::<i64>);

        if mobile::narrow_list(ui) {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for r in &self.records_page.rows {
                        let rrid = r.rrid;
                        let (pace_min, pace_sec) = pace_of(r.total_dis, r.total_time);
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            mobile::row(ui, |ui| {
                                ui.strong(fmt_time(r.start_time));
                                draw_run_badge_word(ui, r, badges, now_ms);
                            });
                            // 「通过但异常」必须能自解释：规则都达标了，凭什么标红。
                            if let Some(explain) = badge_explanation(r, badges, now_ms) {
                                ui.label(egui::RichText::new(explain).small().color(theme::err()));
                            }
                            ui.label(format!(
                                "{:.2} km · {} · 配速 {pace_min}:{pace_sec:02}/km",
                                r.total_dis / 1000.0,
                                fmt_dur(r.total_time),
                            ));
                            ui.label(format!("步频 {} spm · rrid {rrid}", r.avg_step_freq));
                            if ui
                                .add_sized([96.0, mobile::TOUCH_HEIGHT], egui::Button::new("详情"))
                                .clicked()
                            {
                                toggle_target.set(Some(rrid));
                            }
                        });
                        ui.add_space(6.0);
                    }
                });
            if let Some(rrid) = toggle_target.get() {
                self.records_page.detail_rrid = Some(rrid);
                self.records_page.detail_raw = None;
                self.records_page.detail_loading = true;
                self.fetch_run_detail(rrid);
            }
            return;
        }

        TableBuilder::new(ui)
            .striped(true)
            .vscroll(true)
            .auto_shrink([false, false])
            .column(Column::auto().at_least(130.0)) // 时间
            .column(Column::auto().at_least(70.0)) // 距离
            .column(Column::auto().at_least(70.0)) // 时长
            .column(Column::auto().at_least(60.0)) // 配速
            .column(Column::auto().at_least(70.0)) // 步频
            .column(Column::auto().at_least(60.0)) // 达标
            .column(Column::auto().at_least(90.0)) // rrid
            .column(Column::remainder()) // 操作
            .header(22.0, |mut h| {
                for t in [
                    "时间", "距离", "时长", "配速", "步频", "达标", "rrid", "操作",
                ] {
                    h.col(|ui| {
                        ui.label(egui::RichText::new(t).strong().color(theme::text()));
                    });
                }
            })
            .body(|mut body| {
                let sel = self.records_page.detail_rrid;
                for r in &self.records_page.rows {
                    let rrid = r.rrid;
                    let is_detail = sel == Some(rrid);
                    body.row(20.0, |mut row| {
                        row.col(|ui| {
                            let color = if is_detail {
                                theme::accent()
                            } else {
                                theme::text()
                            };
                            ui.label(egui::RichText::new(fmt_time(r.start_time)).color(color));
                        });
                        row.col(|ui| {
                            ui.monospace(format!("{:.2} km", r.total_dis / 1000.0));
                        });
                        row.col(|ui| {
                            ui.monospace(fmt_dur(r.total_time));
                        });
                        row.col(|ui| {
                            let (m, s) = pace_of(r.total_dis, r.total_time);
                            ui.monospace(format!("{m}:{s:02}"));
                        });
                        row.col(|ui| {
                            ui.monospace(format!("{} spm", r.avg_step_freq));
                        });
                        row.col(|ui| {
                            draw_run_badge_word(ui, r, badges, now_ms);
                        });
                        row.col(|ui| {
                            ui.monospace(rrid.to_string());
                        });
                        row.col(|ui| {
                            let txt = if is_detail {
                                egui::RichText::new("收起").color(theme::accent())
                            } else {
                                egui::RichText::new("详情").color(theme::accent())
                            };
                            if ui.small_button(txt).clicked() {
                                toggle_target.set(if is_detail { None } else { Some(rrid) });
                            }
                        });
                    });
                }
            });

        if let Some(rrid) = toggle_target.get() {
            self.records_page.detail_rrid = Some(rrid);
            self.records_page.detail_raw = None;
            self.records_page.detail_loading = true;
            self.fetch_run_detail(rrid);
        }
    }

    fn draw_ai_table(&mut self, ui: &mut egui::Ui) {
        if self.records_page.ai_groups.is_empty() {
            ui.add_space(20.0);
            ui.centered_and_justified(|ui| ui.label("暂无 AI 记录，选择项目后「刷新记录」"));
            return;
        }
        ui.label(format!(
            "共 {} 条（{} 天）",
            self.records_page.ai_total,
            self.records_page.ai_groups.len()
        ));
        let num = |s: &str| s.parse::<f64>().unwrap_or(0.0);
        let toggle_target = std::cell::Cell::new(None::<i64>);
        if mobile::narrow_list(ui) {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for g in &self.records_page.ai_groups {
                        let date = chrono::Local
                            .timestamp_millis_opt(g.score_date)
                            .single()
                            .map(|t| t.format("%Y-%m-%d").to_string())
                            .unwrap_or_default();
                        for r in &g.records {
                            egui::Frame::group(ui.style()).show(ui, |ui| {
                                ui.strong(format!("{} · {}", date, r.name));
                                let score = if r.rtype == 2 {
                                    format!("{:.1} 秒", num(&r.score) / 1000.0)
                                } else {
                                    format!("{} 个", r.score)
                                };
                                ui.label(format!(
                                    "成绩 {score} · 视频 {}",
                                    if r.has_video { "有" } else { "-" }
                                ));
                                ui.label(format!(
                                    "提交 {}",
                                    chrono::Local
                                        .timestamp_millis_opt(r.upload_time)
                                        .single()
                                        .map(|t| t.format("%m-%d %H:%M").to_string())
                                        .unwrap_or_default(),
                                ));
                                if ui
                                    .add_sized(
                                        [96.0, mobile::TOUCH_HEIGHT],
                                        egui::Button::new("详情"),
                                    )
                                    .clicked()
                                {
                                    toggle_target.set(Some(r.id));
                                }
                            });
                            ui.add_space(6.0);
                        }
                    }
                });
            if let Some(id) = toggle_target.get() {
                self.records_page.ai_detail_id = Some(id);
                self.records_page.ai_detail_raw = None;
                self.records_page.ai_detail_loading = true;
                self.fetch_ai_detail(id);
            }
            return;
        }
        TableBuilder::new(ui)
            .striped(true)
            .vscroll(true)
            .auto_shrink([false, false])
            .column(Column::auto().at_least(100.0)) // 日期
            .column(Column::auto().at_least(110.0)) // 项目
            .column(Column::auto().at_least(90.0)) // 成绩
            .column(Column::auto().at_least(100.0)) // 完成时间
            .column(Column::auto().at_least(90.0)) // 提交时间
            .column(Column::auto().at_least(60.0)) // 视频
            .column(Column::remainder()) // 操作
            .header(22.0, |mut h| {
                for t in [
                    "日期",
                    "项目",
                    "成绩",
                    "完成时间",
                    "提交时间",
                    "视频",
                    "操作",
                ] {
                    h.col(|ui| {
                        ui.label(egui::RichText::new(t).strong().color(theme::text()));
                    });
                }
            })
            .body(|mut body| {
                for g in &self.records_page.ai_groups {
                    let date = chrono::Local
                        .timestamp_millis_opt(g.score_date)
                        .single()
                        .map(|t| t.format("%Y-%m-%d").to_string())
                        .unwrap_or_default();
                    for r in &g.records {
                        body.row(20.0, |mut row| {
                            row.col(|ui| {
                                ui.monospace(&date);
                            });
                            row.col(|ui| {
                                ui.label(&r.name);
                            });
                            row.col(|ui| {
                                if r.rtype == 2 {
                                    // 计时类：成绩即用时（毫秒）
                                    ui.monospace(format!("{:.1} 秒", num(&r.score) / 1000.0));
                                } else {
                                    ui.monospace(format!("{} 个", r.score));
                                }
                            });
                            row.col(|ui| {
                                let ms = chrono::Local
                                    .timestamp_millis_opt(r.score_date)
                                    .single()
                                    .map(|t| t.format("%H:%M:%S").to_string())
                                    .unwrap_or_default();
                                ui.monospace(ms);
                            });
                            row.col(|ui| {
                                ui.monospace(
                                    chrono::Local
                                        .timestamp_millis_opt(r.upload_time)
                                        .single()
                                        .map(|t| t.format("%m-%d %H:%M").to_string())
                                        .unwrap_or_default(),
                                );
                            });
                            row.col(|ui| {
                                if r.has_video {
                                    ui.colored_label(theme::accent(), "有");
                                } else {
                                    ui.label("-");
                                }
                            });
                            row.col(|ui| {
                                if ui.small_button("详情").clicked() {
                                    toggle_target.set(Some(r.id));
                                }
                            });
                        });
                    }
                }
            });

        if let Some(id) = toggle_target.get() {
            self.records_page.ai_detail_id = Some(id);
            self.records_page.ai_detail_raw = None;
            self.records_page.ai_detail_loading = true;
            self.fetch_ai_detail(id);
        }
    }
}

/// AI 记录详情：record/info 全量字段。
fn draw_ai_detail_panel(ui: &mut egui::Ui, raw: &serde_json::Value) {
    let i = |k: &str| raw.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
    let f = |k: &str| raw.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
    let s = |k: &str| -> String {
        raw.get(k)
            .and_then(|v| v.as_str())
            .filter(|x| !x.is_empty())
            .map(|x| x.to_string())
            .unwrap_or_else(|| "-".into())
    };
    let dt = |k: &str| -> String {
        let ms = i(k);
        if ms <= 0 {
            return "-".into();
        }
        chrono::Local
            .timestamp_millis_opt(ms)
            .single()
            .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "-".into())
    };
    let fmt_ms = |ms: i64| -> String {
        if ms <= 0 {
            return "-".into();
        }
        if ms >= 60_000 {
            format!(
                "{}:{:02}:{:02}",
                ms / 60_000,
                ms % 60_000 / 1000,
                ms % 1000 / 10
            )
        } else {
            format!("{}.{:02} 秒", ms / 1000, ms % 1000 / 10)
        }
    };

    let rtype = i("type");
    // 详情接口的 score 单位与列表不同（计时类为计分制），展示原始值，用时以 timeConsume 为准
    let score_text = if rtype == 2 {
        format!("{}（原始计分）", i("score"))
    } else {
        format!("{} 个", i("score"))
    };

    let mut rows: Vec<(&str, String)> = vec![
        ("项目名称", s("name")),
        (
            "类型",
            if rtype == 2 {
                "计时".into()
            } else {
                "计次".into()
            },
        ),
        ("成绩", score_text),
        ("用时", fmt_ms(i("timeConsume"))),
        ("速度", {
            let sp = f("speed");
            if sp > 0.0 {
                format!("{sp:.0} /分")
            } else {
                "-".into()
            }
        }),
        ("消耗", {
            let c = f("consume");
            if c > 0.0 {
                format!("{c:.1} kcal")
            } else {
                "0 kcal".into()
            }
        }),
        ("完成时间", dt("scoreDate")),
        ("提交时间", dt("uploadTime")),
        ("状态", i("status").to_string()),
        ("记录 ID", i("id").to_string()),
        ("项目 ID", i("sportId").to_string()),
        ("任务 ID", i("taskId").to_string()),
        ("用户 ID", i("uid").to_string()),
        ("记录 UUID", s("uuid")),
        ("视频", s("exerciseMediaUrl")),
    ];
    if let Some(reason) = raw.get("reason").and_then(|v| v.as_str()) {
        if !reason.is_empty() {
            rows.push(("原因", reason.to_string()));
        }
    }
    let draw_value = |ui: &mut egui::Ui, k: &str, v: &String| {
        mobile::row(ui, |ui| {
            ui.label(egui::RichText::new(v).color(theme::text()));
            let copyable = (k == "视频" && v != "-") || k == "记录 UUID";
            if copyable && ui.button("复制").clicked() {
                ui.ctx().copy_text(v.clone());
            }
        });
    };
    if mobile::compact_ui(ui) {
        for (k, v) in &rows {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.label(egui::RichText::new(*k).color(theme::text_dim()));
                draw_value(ui, k, v);
            });
            ui.add_space(4.0);
        }
    } else {
        egui::Grid::new("ai_detail_grid")
            .num_columns(2)
            .spacing([18.0, 4.0])
            .striped(true)
            .show(ui, |ui| {
                for (k, v) in rows {
                    ui.label(egui::RichText::new(k).color(theme::text_dim()));
                    draw_value(ui, k, &v);
                    ui.end_row();
                }
            });
    }
}

/// 详情页内容：**先给结论**（三态 + 异常项置顶），再给全量字段。
///
/// 顺序是有意的：手机上详情页内容比一屏长，字段区永远要先滚。结论与「通过但异常」
/// 那一行必须落在第一屏 —— 它就是这个页面存在的理由（见 `ui::record_status` 模块注释）。
fn draw_detail_panel(ui: &mut egui::Ui, raw: &serde_json::Value) {
    draw_detail_verdict(ui, raw);
    ui.add_space(6.0);
    draw_detail_fields(ui, raw);
}

/// 三态结论 + 判定明细（非规则项置顶标红，规则项随后）。
fn draw_detail_verdict(ui: &mut egui::Ui, raw: &serde_json::Value) {
    use super::record_status::{self, ReasonKind, RecordStatus};

    let reasons = record_status::reasons_of(raw);
    // 没有判定信息（拉详情失败时只有 statusInfo 的空壳）：不猜状态，字段区会显示原始错误。
    let Some(status) = record_status::classify_detail(raw) else {
        return;
    };
    let anomalies = record_status::anomaly_reasons(&reasons);
    let unclassified = record_status::unclassified_count(&reasons);

    // ── 顶部一行结论 ────────────────────────────────────────────
    // 只有「规则全过仍被标记」才标红；「未达标」是规则层结论，用普通字色，不标红。
    let color = if status.is_anomalous() {
        theme::err()
    } else if status == RecordStatus::Passed {
        theme::ok()
    } else {
        theme::text()
    };
    let size = if status.is_anomalous() { 18.0 } else { 15.0 };
    let (note, note_color) = match status {
        RecordStatus::Passed => (
            "规则项全部通过，服务端没有额外标记".to_string(),
            theme::text_dim(),
        ),
        RecordStatus::Failed => {
            if anomalies.is_empty() {
                (
                    "规则没过（下方 × 项）：这是规则层结论，不是风控标记".to_string(),
                    theme::text_dim(),
                )
            } else {
                (
                    format!(
                        "规则没过，但另有 {} 项非规则标记（下方红色条目）：规则层结论不是风控信号，红条目才是",
                        anomalies.len()
                    ),
                    theme::warn(),
                )
            }
        }
        RecordStatus::PassedButAnomalous => (
            format!(
                "规则项全部达标，但服务端另外标记了 {} 项 —— 这是风控信号，不是「未达标」",
                anomalies.len()
            ),
            theme::err(),
        ),
    };
    mobile::row(ui, |ui| {
        ui.colored_label(
            color,
            egui::RichText::new(status.label()).strong().size(size),
        );
        ui.label(egui::RichText::new(note).color(note_color));
    });

    // ── 判定明细 ────────────────────────────────────────────────
    if reasons.is_empty() {
        return;
    }
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new("判定明细")
            .strong()
            .color(theme::text()),
    );
    for item in record_status::sorted_for_display(&reasons) {
        let kind = record_status::item_kind(&item);
        mobile::row(ui, |ui| {
            match kind {
                // 非规则项：规则全过也可能出现的那一行，红色 + 图标 + 「异常」标签。
                ReasonKind::NonRule => {
                    ui.colored_label(theme::err(), if item.complete { "√" } else { "×" });
                    ui.colored_label(theme::err(), egui::RichText::new("异常").small().strong());
                    ui.colored_label(theme::err(), egui::RichText::new(&item.reason).strong());
                }
                ReasonKind::Rule => {
                    if item.complete {
                        ui.colored_label(theme::ok(), "√");
                    } else {
                        ui.colored_label(theme::err(), "×");
                    }
                    ui.label(&item.reason);
                }
                // 认不出的条目不冒充规则项，也不冒充异常：如实标出来。
                ReasonKind::Unclassified => {
                    ui.colored_label(theme::text_dim(), "？");
                    ui.label(egui::RichText::new(&item.reason).color(theme::text_dim()));
                }
            }
        });
    }
    if unclassified > 0 {
        ui.label(
            egui::RichText::new(format!(
                "其中 {unclassified} 项无法归类，按原样列出（不作为异常）"
            ))
            .small()
            .color(theme::text_dim()),
        );
    }
}

/// 详情字段（距离 / 时长 / … 与服务端原始状态文案）。
fn draw_detail_fields(ui: &mut egui::Ui, raw: &serde_json::Value) {
    let g = |k: &str| -> String {
        match raw.get(k) {
            Some(serde_json::Value::String(s)) if !s.is_empty() => s.clone(),
            Some(serde_json::Value::Number(n)) => n.to_string(),
            _ => String::new(),
        }
    };
    let f = |k: &str| -> f64 { raw.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0) };
    let i = |k: &str| -> i64 { raw.get(k).and_then(|v| v.as_i64()).unwrap_or(0) };

    let mut items: Vec<(&str, String)> = vec![
        ("距离", format!("{:.2} km", f("totalDis") / 1000.0)),
        ("有效里程", format!("{:.2} km", f("validDis") / 1000.0)),
        ("时长", {
            let t = i("totalTime");
            if t >= 3600 {
                format!("{}:{:02}:{:02}", t / 3600, t % 3600 / 60, t % 60)
            } else {
                format!("{}:{:02}", t / 60, t % 60)
            }
        }),
        ("卡路里", format!("{} kcal", i("calorie"))),
        ("功率", format!("{} W", i("avgPower"))),
        ("步数", i("totalSteps").to_string()),
        ("步频", format!("{} spm", i("avgStepFreq"))),
        ("爬升", format!("{} m", i("totalAscent"))),
        ("配速区间", {
            let lo = f("speedBottom");
            let hi = f("speedTop");
            if hi > 0.0 {
                format!("{:.2} ~ {:.2} m/s", lo, hi)
            } else {
                "-".into()
            }
        }),
        ("地址", g("address")),
        ("状态", g("statusInfo")),
    ];
    let st_ms = i("startTime");
    if st_ms > 0 {
        let st = chrono::Local
            .timestamp_millis_opt(st_ms)
            .single()
            .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_default();
        if !st.is_empty() {
            items.push(("开始时间", st));
        }
    }
    if mobile::compact_ui(ui) {
        for (k, v) in &items {
            if v.is_empty() || v == "0" || v == "0.0" {
                continue;
            }
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.label(egui::RichText::new(*k).color(theme::text_dim()));
                ui.label(egui::RichText::new(v).color(theme::text()));
            });
            ui.add_space(4.0);
        }
    } else {
        egui::Grid::new("detail_grid")
            .num_columns(2)
            .spacing([18.0, 4.0])
            .striped(true)
            .show(ui, |ui| {
                for (k, v) in &items {
                    if v.is_empty() || v == "0" || v == "0.0" {
                        continue;
                    }
                    ui.label(egui::RichText::new(*k).color(theme::text_dim()));
                    ui.label(egui::RichText::new(v).color(theme::text()));
                    ui.end_row();
                }
            });
    }
}

/// 一行列表的结论：以详情确认过的徽标为准；还没核到时退回列表自带的 `complete`。
///
/// 为什么不只看 `row.complete`：真实那条被判可疑的记录在列表里也是「通过」，
/// 只看这一个字段永远看不见风控标记。
fn badge_status(
    row: &crate::api::records::RecordRow,
    badges: &BadgeState,
    now_ms: i64,
) -> RecordStatus {
    badges.status_of(row.rrid, now_ms).unwrap_or({
        if row.complete {
            RecordStatus::Passed
        } else {
            RecordStatus::Failed
        }
    })
}

/// 徽标短词：只有「通过但异常」标红。
///
/// 「未达标」用中性色 —— 规则没过是用户能看懂、也能改的结论，把红色留给风控信号，
/// 红色才有含义（与详情页同一口径，见 `record_status`）。
fn draw_run_badge_word(
    ui: &mut egui::Ui,
    row: &crate::api::records::RecordRow,
    badges: &BadgeState,
    now_ms: i64,
) {
    match badge_status(row, badges, now_ms) {
        RecordStatus::PassedButAnomalous => {
            ui.colored_label(theme::err(), egui::RichText::new("异常").strong());
        }
        RecordStatus::Passed => {
            ui.colored_label(theme::ok(), "达标");
        }
        RecordStatus::Failed => {
            ui.label(egui::RichText::new("未达标").color(theme::text_dim()));
        }
    }
}

/// 「通过但异常」的自解释文案（其它状态返回 `None`）。
fn badge_explanation(
    row: &crate::api::records::RecordRow,
    badges: &BadgeState,
    now_ms: i64,
) -> Option<String> {
    if badge_status(row, badges, now_ms) != RecordStatus::PassedButAnomalous {
        return None;
    }
    let reason = badges
        .anomaly_of(row.rrid, now_ms)
        .unwrap_or("运动数据被判有问题");
    Some(format!("规则已达标，但{reason}（风控标记，不是未达标）"))
}
