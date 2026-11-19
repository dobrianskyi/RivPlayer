use std::time::{Duration, SystemTime, UNIX_EPOCH};

use eframe::egui::{
    self, Color32, CursorIcon, FontId, PointerButton, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2,
    pos2, vec2,
};

use super::{
    controls::{Icon, PANEL_BACKGROUND, PANEL_BORDER, WHITE, draw_scaled},
    preferences::{ClockSize, OverlayPosition},
};

#[derive(Clone, Copy)]
pub enum TimeAction {
    Visibility(bool),
    Alarm,
}

pub fn time_menu(ui: &mut egui::Ui, visible: bool) -> Option<TimeAction> {
    let mut choice = None;
    ui.menu_button("Time", |ui| {
        for (show, label) in [(true, "Show"), (false, "Hide")] {
            if ui.selectable_label(visible == show, label).clicked() {
                choice = Some(TimeAction::Visibility(show));
            }
        }
        if ui.button("Alarm…").clicked() {
            choice = Some(TimeAction::Alarm);
        }
    });
    if choice.is_some() {
        ui.close();
    }
    choice
}

#[derive(Default)]
pub struct Drag {
    offset: Option<Vec2>,
    dirty: bool,
}

impl Drag {
    pub fn place(
        &mut self,
        ui: &egui::Ui,
        id: &'static str,
        default: Rect,
        area: Rect,
        custom: &mut Option<OverlayPosition>,
    ) -> Rect {
        let fit = |center: Pos2| {
            let half = (default.size() * 0.5).min(area.size() * 0.5);
            Rect::from_center_size(
                center.clamp(area.min + half, area.max - half),
                default.size(),
            )
        };
        let mut rect = custom.map_or(default, |pos| {
            fit(area.min + area.size() * vec2(pos.x, pos.y))
        });
        let response = ui.interact(rect, ui.scope_id().with(id), Sense::click_and_drag());
        if response.double_clicked_by(PointerButton::Primary) {
            self.dirty |= custom.take().is_some();
            self.offset = None;
            rect = default;
            ui.ctx().request_repaint();
        } else if response.drag_started_by(PointerButton::Primary) {
            self.offset = ui.input(|i| i.pointer.press_origin().map(|p| rect.center() - p));
        }
        if let Some(offset) = self.offset
            && (response.dragged_by(PointerButton::Primary)
                || response.drag_stopped_by(PointerButton::Primary))
            && let Some(pointer) = ui.input(|i| i.pointer.interact_pos())
        {
            let center = pointer + offset;
            if self.dirty || center.distance_sq(rect.center()) > 9.0 {
                rect = fit(center);
                let fraction = (rect.center() - area.min) / area.size();
                *custom = Some(OverlayPosition {
                    x: fraction.x,
                    y: fraction.y,
                });
                self.dirty = true;
            }
        }
        if response.hovered() || response.dragged() {
            let cursor = if self.offset.is_some() {
                CursorIcon::Grabbing
            } else {
                CursorIcon::Grab
            };
            ui.ctx().set_cursor_icon(cursor);
            ui.painter().rect_stroke(
                rect.expand(2.0),
                5.0,
                Stroke::new(1.0, Color32::from_white_alpha(108)),
                StrokeKind::Outside,
            );
        }
        rect
    }

    pub fn finish(&mut self) -> bool {
        self.offset = None;
        std::mem::take(&mut self.dirty)
    }
}

pub fn drag_area(video: Rect) -> Rect {
    let horizontal = (video.width() * 0.1).min(12.0);
    let scale = (video.height() / 180.0).min(1.0);
    Rect::from_min_max(
        video.min + vec2(horizontal, 49.0 * scale),
        video.max - vec2(horizontal, 87.0 * scale),
    )
}

#[derive(Default)]
pub struct Clock {
    minute: Option<u64>,
    text: String,
    alarm: Option<Alarm>,
    pub elapsed: Option<u16>,
    pub hours: u8,
    pub minutes: u8,
}

#[derive(Clone, Copy)]
struct Alarm {
    deadline: SystemTime,
    minutes: u16,
}

impl Clock {
    pub fn alarm_countdown(&self) -> Option<String> {
        let remaining = self
            .alarm?
            .deadline
            .duration_since(SystemTime::now())
            .unwrap_or_default();
        let seconds = remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0);
        Some(format!(
            "{:02}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        ))
    }

    pub fn tick(&mut self, ctx: &egui::Context, visible: bool) -> bool {
        let now = SystemTime::now();
        if visible && let Ok(epoch) = now.duration_since(UNIX_EPOCH) {
            let minute = epoch.as_secs() / 60;
            if self.minute != Some(minute) {
                self.minute = Some(minute);
                self.text = local_time(epoch.as_secs()).unwrap_or_default();
            }
            ctx.request_repaint_after(Duration::from_secs((minute + 1) * 60) - epoch);
        }
        let Some(alarm) = self.alarm else {
            return false;
        };
        if let Ok(remaining) = alarm.deadline.duration_since(now)
            && !remaining.is_zero()
        {
            ctx.request_repaint_after(remaining.min(Duration::from_secs(1)));
            return false;
        }
        self.alarm = None;
        self.elapsed = Some(alarm.minutes);
        true
    }

    pub fn paint(
        &self,
        ui: &egui::Ui,
        video: Rect,
        size: ClockSize,
        drag: &mut Drag,
        position: &mut Option<OverlayPosition>,
    ) {
        let size = match size {
            ClockSize::Small => 15.0,
            ClockSize::Medium => 21.0,
            ClockSize::Large => 30.0,
        };
        let painter = ui.painter();
        let label =
            painter.layout_no_wrap(self.text.clone(), FontId::monospace(size), Color32::WHITE);
        let size = label.size() + vec2(18.0, 8.0);
        let default = Rect::from_min_size(
            pos2(video.right() - size.x - 26.0, video.top() + 53.0),
            size,
        );
        let rect = drag.place(ui, "clock-overlay", default, drag_area(video), position);
        ui.painter()
            .rect_filled(rect, 6.0, Color32::from_black_alpha(191));
        ui.painter()
            .galley(rect.min + vec2(9.0, 4.0), label, Color32::WHITE);
    }

    pub fn settings(&mut self, ui: &mut egui::Ui) -> bool {
        if let Some(countdown) = self.alarm_countdown() {
            ui.horizontal(|ui| {
                ui.label(format!("Alarm in {countdown}"));
                if ui.button("Cancel alarm").clicked() {
                    self.alarm = None;
                }
            });
            ui.separator();
        }
        let mut delay = None;
        ui.label("Pause playback after");
        ui.horizontal_wrapped(|ui| {
            for (label, minutes) in [
                ("30 min", 30),
                ("45 min", 45),
                ("1 hour", 60),
                ("2 hours", 120),
            ] {
                if ui.button(label).clicked() {
                    delay = Some(minutes);
                }
            }
        });
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            ui.label("Custom");
            ui.add(
                egui::DragValue::new(&mut self.hours)
                    .range(0..=99)
                    .suffix(" h"),
            );
            ui.add(
                egui::DragValue::new(&mut self.minutes)
                    .range(0..=59)
                    .suffix(" min"),
            );
            let minutes = u16::from(self.hours) * 60 + u16::from(self.minutes);
            if ui
                .add_enabled(minutes > 0, egui::Button::new("Set alarm"))
                .clicked()
            {
                delay = Some(minutes);
            }
        });
        if let Some(minutes) = delay {
            self.alarm = Some(Alarm {
                deadline: SystemTime::now() + Duration::from_secs(u64::from(minutes) * 60),
                minutes,
            });
            self.elapsed = None;
            ui.ctx().request_repaint();
        }
        delay.is_some()
    }

    pub fn paint_alarm(&self, ui: &egui::Ui, video: Rect) {
        let Some(minutes) = self.elapsed else { return };
        let scale = (video.width() / 300.0)
            .min(video.height() / 220.0)
            .clamp(0.48, 1.0);
        let painter = ui.painter();
        let title =
            painter.layout_no_wrap("Alarm".into(), FontId::proportional(42.0 * scale), WHITE);
        let detail_color = Color32::from_gray(216);
        let detail = painter.layout_no_wrap(
            format!("Playback paused · {minutes} min"),
            FontId::proportional(18.0 * scale),
            detail_color,
        );
        let size = vec2(
            title.size().x.max(detail.size().x) + 56.0 * scale,
            title.size().y + detail.size().y + 116.0 * scale,
        );
        let card = Rect::from_center_size(video.center(), size);
        painter.rect_filled(card, 15.0, PANEL_BACKGROUND);
        painter.rect_stroke(
            card,
            15.0,
            Stroke::new(1.0, PANEL_BORDER),
            StrokeKind::Inside,
        );
        let badge = pos2(card.center().x, card.top() + 48.0 * scale);
        draw_scaled(painter, badge, Icon::Alarm, WHITE, 1.9 * scale);
        let top = badge.y + 39.0 * scale;
        let centered_x = |width| card.center().x - width * 0.5;
        let detail_top = top + title.size().y + 6.0 * scale;
        painter.galley(pos2(centered_x(title.size().x), top), title, WHITE);
        painter.galley(
            pos2(centered_x(detail.size().x), detail_top),
            detail,
            detail_color,
        );
    }
}

fn local_time(epoch: u64) -> Option<String> {
    let epoch = libc::time_t::try_from(epoch).ok()?;
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: localtime_r writes a tm to the supplied output on success.
    let local = unsafe { libc::localtime_r(&epoch, local.as_mut_ptr()).as_ref() }?;
    Some(format!("{:02}:{:02}", local.tm_hour, local.tm_min))
}
