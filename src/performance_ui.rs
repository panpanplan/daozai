//! 「电脑状态」页面与收起态的 CPU 徽标。
//! 所有数值都来自 `performance::Snapshot`，界面不做系统调用。

use crate::app::{App, Env, PERF_PAGE};
use crate::bar_layout::Layout;
use crate::paint::{Card, H};
use crate::theme::{self, W};
use eframe::egui::Color32;

fn cpu_color(cpu: f32) -> Color32 {
    if cpu >= 85.0 { theme::RED_TOP } else if cpu >= 60.0 { theme::YELLOW } else { theme::BLUE }
}

fn card_frame(c: &Card, x: f32, y: f32, w: f32, h: f32) {
    c.fill(x, y, w, h, 10.0, Color32::from_white_alpha(12));
    c.stroke(x, y, w, h, 10.0, 1.0, Color32::from_white_alpha(16));
}

fn gauge(c: &Card, x: f32, y: f32, w: f32, h: f32, ratio: f32, color: Color32) {
    let ratio = ratio.clamp(0.0, 1.0);
    c.fill(x, y, w, h, h / 2.0, theme::TRACK);
    if ratio > 0.0 { c.fill(x, y, w * ratio, h, h / 2.0, color); }
}

/// 迷你趋势线；`max` 是纵轴上限，数据会裁剪到 0..1。
fn spark(c: &Card, x: f32, y: f32, w: f32, h: f32, data: &[f32], max: f32, color: Color32) {
    if data.len() < 2 { return; }
    let max = max.max(1e-6);
    let n = data.len();
    let points: Vec<(f32, f32)> = data
        .iter()
        .enumerate()
        .map(|(i, value)| {
            (x + w * i as f32 / (n - 1) as f32, y + h - (value / max).clamp(0.0, 1.0) * h)
        })
        .collect();
    c.path(&points, 1.6, color);
}

impl App {
    pub(crate) fn page_performance(&mut self, c: &Card, _env: &Env) {
        let Some(snapshot) = self.performance.clone().filter(|s| s.ready()) else {
            c.text(16.0, 62.0, "正在读取电脑状态…", 13.0, W::Semibold, theme::WHITE, H::Left);
            c.text(16.0, 84.0, "首次采样约需 1 秒；关闭页面与常驻徽标后采集会停止。", 11.0, W::Regular, theme::GRAY, H::Left);
            return;
        };

        // ---- CPU ----
        const LEFT: f32 = 16.0;
        const RIGHT: f32 = 215.0;
        const TOP: f32 = 38.0;
        const BOTTOM: f32 = 108.0;
        const CARD_W: f32 = 188.0;
        const CARD_H: f32 = 62.0;
        let cpu_col = cpu_color(snapshot.cpu);

        card_frame(c, LEFT, TOP, CARD_W, CARD_H);
        c.text(LEFT + 10.0, TOP + 16.0, "CPU", 10.0, W::Semibold, theme::GRAY, H::Left);
        c.text(LEFT + 10.0, TOP + 45.0, &format!("{:.0}%", snapshot.cpu), 24.0, W::Bold, cpu_col, H::Left);
        c.text(
            LEFT + 10.0,
            TOP + 57.0,
            &format!("{} 逻辑处理器", snapshot.cores),
            10.0,
            W::Regular,
            theme::DIM,
            H::Left,
        );
        spark(c, LEFT + 96.0, TOP + 14.0, 82.0, 34.0, &snapshot.cpu_history, 100.0, cpu_col);

        // ---- 内存 ----
        card_frame(c, RIGHT, TOP, CARD_W, CARD_H);
        c.text(RIGHT + 10.0, TOP + 16.0, "内存", 10.0, W::Semibold, theme::GRAY, H::Left);
        c.text(
            RIGHT + CARD_W - 10.0,
            TOP + 16.0,
            &format!("{:.0}%", snapshot.memory_ratio * 100.0),
            10.0,
            W::Semibold,
            theme::SOFT_GREEN,
            H::Right,
        );
        c.text(
            RIGHT + 10.0,
            TOP + 42.0,
            &format!(
                "{} / {}",
                crate::performance::format_size(snapshot.memory_used),
                crate::performance::format_size(snapshot.memory_total)
            ),
            15.0,
            W::Bold,
            theme::WHITE,
            H::Left,
        );
        gauge(c, RIGHT + 10.0, TOP + 49.0, CARD_W - 20.0, 5.0, snapshot.memory_ratio, theme::SOFT_GREEN);
        spark(
            c,
            RIGHT + 10.0,
            TOP + 20.0,
            CARD_W - 20.0,
            16.0,
            &snapshot.memory_history,
            1.0,
            Color32::from_rgba_unmultiplied(theme::SOFT_GREEN.r(), theme::SOFT_GREEN.g(), theme::SOFT_GREEN.b(), 110),
        );

        // ---- 网络 ----
        card_frame(c, LEFT, BOTTOM, CARD_W, CARD_H);
        c.text(LEFT + 10.0, BOTTOM + 16.0, "网络", 10.0, W::Semibold, theme::GRAY, H::Left);
        c.text(
            LEFT + 10.0,
            BOTTOM + 38.0,
            &format!("↓ {}", crate::performance::format_rate(snapshot.net_down)),
            13.0,
            W::Bold,
            theme::CYAN,
            H::Left,
        );
        c.text(
            LEFT + 10.0,
            BOTTOM + 55.0,
            &format!("↑ {}", crate::performance::format_rate(snapshot.net_up)),
            12.0,
            W::Semibold,
            theme::SOFT_GREEN,
            H::Left,
        );
        let peak = snapshot.net_down_history.iter().copied().fold(0.0f64, f64::max).max(64.0 * 1024.0);
        let curve: Vec<f32> = snapshot.net_down_history.iter().map(|value| (*value / peak) as f32).collect();
        spark(c, LEFT + 100.0, BOTTOM + 12.0, 78.0, 40.0, &curve, 1.0, theme::CYAN);

        // ---- 硬盘 ----
        card_frame(c, RIGHT, BOTTOM, CARD_W, CARD_H);
        c.text(RIGHT + 10.0, BOTTOM + 16.0, "硬盘", 10.0, W::Semibold, theme::GRAY, H::Left);
        if snapshot.disks.len() > 1 {
            c.text(
                RIGHT + CARD_W - 10.0,
                BOTTOM + 16.0,
                &format!("{} 个分区", snapshot.disks.len()),
                10.0,
                W::Regular,
                theme::DIM,
                H::Right,
            );
        }
        match snapshot.disks.first() {
            Some(disk) => {
                c.text(
                    RIGHT + 10.0,
                    BOTTOM + 37.0,
                    &format!("{}  已用 {:.0}%", disk.name, disk.used_ratio() * 100.0),
                    13.0,
                    W::Bold,
                    theme::WHITE,
                    H::Left,
                );
                gauge(c, RIGHT + 10.0, BOTTOM + 43.0, CARD_W - 20.0, 5.0, disk.used_ratio(), theme::YELLOW);
            }
            None => {
                c.text(RIGHT + 10.0, BOTTOM + 37.0, "未检测到固定硬盘", 12.0, W::Regular, theme::GRAY, H::Left);
            }
        }
        let disk_line = match (snapshot.disk_read, snapshot.disk_write) {
            (Some(read), Some(write)) => format!(
                "读 {} · 写 {}",
                crate::performance::format_rate(read),
                crate::performance::format_rate(write)
            ),
            _ => "读 — · 写 —".to_string(),
        };
        c.text(RIGHT + 10.0, BOTTOM + 57.0, &disk_line, 10.5, W::Semibold, theme::GRAY, H::Left);

        c.text(
            LEFT,
            186.0,
            "每秒采样 · CPU 与内存为全机占用 · 网速合计所有已连接网卡",
            10.0,
            W::Regular,
            theme::DIM,
            H::Left,
        );
    }

    /// 顶部停靠：收起药丸里的 CPU 徽标，点击直达性能页。
    pub(crate) fn draw_pill_performance(&mut self, c: &Card, x: f32, layout: &Layout) {
        let Some(snapshot) = self.performance.clone() else { return };
        let width = layout.widths[crate::bar_layout::PERF];
        let faded = c.faded(layout.pet_clearance(crate::bar_layout::PERF));
        let color = cpu_color(snapshot.cpu);
        faded.text(x, 20.0, &format!("CPU {:.0}%", snapshot.cpu), 11.0, W::Bold, color, H::Center);
        let bar_w = (width - 10.0).max(4.0);
        faded.fill(x - bar_w / 2.0, 24.0, bar_w, 2.0, 1.0, theme::TRACK);
        faded.fill(x - bar_w / 2.0, 24.0, bar_w * (snapshot.cpu / 100.0).clamp(0.0, 1.0), 2.0, 1.0, color);
        let tip = format!(
            "CPU {:.0}% · 内存 {:.0}%\n↓ {} · ↑ {}\n点击打开电脑状态页",
            snapshot.cpu,
            snapshot.memory_ratio * 100.0,
            crate::performance::format_rate(snapshot.net_down),
            crate::performance::format_rate(snapshot.net_up)
        );
        let hit = faded.hit(x - width / 2.0, 1.0, width, 28.0, "perf-pill");
        if crate::quota_ui::bar_tooltip(hit, tip).clicked() { self.show_page(PERF_PAGE); }
    }

    /// 左右停靠：竖向胶囊里的 CPU 徽标。
    pub(crate) fn draw_side_performance(&mut self, c: &Card, cx: f32, cy: f32) {
        let Some(snapshot) = self.performance.clone() else { return };
        let color = cpu_color(snapshot.cpu);
        c.text(cx, cy - 3.0, "CPU", 9.0, W::Regular, theme::GRAY, H::Center);
        c.text(cx, cy + 13.0, &format!("{:.0}%", snapshot.cpu), 11.0, W::Bold, color, H::Center);
        let tip = format!(
            "CPU {:.0}% · 内存 {:.0}%\n↓ {} · ↑ {}\n点击打开电脑状态页",
            snapshot.cpu,
            snapshot.memory_ratio * 100.0,
            crate::performance::format_rate(snapshot.net_down),
            crate::performance::format_rate(snapshot.net_up)
        );
        let hit = c.hit(cx - 17.0, cy - 16.0, 34.0, 32.0, "perf-side");
        if crate::quota_ui::bar_tooltip(hit, tip).clicked() { self.show_page(PERF_PAGE); }
    }
}
