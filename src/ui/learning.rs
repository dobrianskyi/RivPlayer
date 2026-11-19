use std::ops::Range;

use eframe::egui::{self, Color32, FontId, Rect, Sense, Stroke, pos2, vec2};
use rivplayer::engine::{Captions, Cue, Player, Repeat, active_cue};

use super::preferences::MediaSettings;

#[derive(Clone, Copy)]
pub enum Command {
    Track(Captions),
    Open,
    Visible,
    Go(usize),
    Previous,
    Next,
    Replay,
    CueLoop,
    RepeatLine(usize),
    Start,
    End,
    Toggle,
    Reset,
}

#[derive(Default)]
pub struct Learning {
    pub media: MediaSettings,
    pub ab: bool,
    pub cue_loop: Option<Repeat>,
    last_follow: Option<(Captions, usize)>,
    revealed: Option<usize>,
}

impl Learning {
    pub fn new(media: MediaSettings) -> Self {
        Self {
            media,
            ..Self::default()
        }
    }

    pub fn range(&self) -> Option<Repeat> {
        Repeat::new(self.media.a?, self.media.b?)
    }

    pub fn select_row(&mut self, index: usize) {
        self.revealed = Some(index);
    }

    pub fn clear_follow(&mut self) {
        self.last_follow = None;
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        player: &Player,
        selected: Captions,
        visible: bool,
        follow: &mut bool,
    ) -> Option<Command> {
        let mut command = None;
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("transcript-track")
                .width(162.0)
                .selected_text(track_name(player, selected))
                .show_ui(ui, |ui| {
                    if let Some(info) = &player.info {
                        for track in &info.captions {
                            let mode = Captions::Track(track.index);
                            if ui
                                .selectable_label(selected == mode, track_name(player, mode))
                                .clicked()
                            {
                                command = Some(Command::Track(mode));
                            }
                        }
                    }
                    if player.has_sidecar()
                        && ui
                            .selectable_label(
                                selected == Captions::Sidecar,
                                track_name(player, Captions::Sidecar),
                            )
                            .clicked()
                    {
                        command = Some(Command::Track(Captions::Sidecar));
                    }
                });
            if ui
                .add_enabled(player.has_source(), egui::Button::new("Open…"))
                .on_hover_text("Open a subtitle file")
                .clicked()
            {
                command = Some(Command::Open);
            }
        });
        ui.horizontal(|ui| {
            if ui.selectable_label(visible, "Show on video").clicked() {
                command = Some(Command::Visible);
            }
            if ui.checkbox(follow, "Auto-scroll").changed() && *follow {
                self.clear_follow();
            }
        });
        let cues = player.cues(selected);
        let current = active_cue(cues, player.position());
        ui.horizontal(|ui| {
            for (label, hint, action) in [
                ("‹", "Previous line (Shift+←)", Command::Previous),
                ("Replay", "Replay current line (Shift+↓)", Command::Replay),
                ("›", "Next line (Shift+→)", Command::Next),
            ] {
                if ui
                    .add_enabled(
                        !cues.is_empty(),
                        egui::Button::new(label).min_size(vec2(29.0, 23.0)),
                    )
                    .on_hover_text(hint)
                    .clicked()
                {
                    command = Some(action);
                }
            }
            let loop_enabled = current.is_some() || self.cue_loop.is_some();
            if ui
                .add_enabled(
                    loop_enabled,
                    egui::Button::selectable(self.cue_loop.is_some(), "Repeat line"),
                )
                .on_hover_text("Loop current line (Shift+↑)")
                .clicked()
            {
                command = Some(Command::CueLoop);
            }
        });
        ui.separator();
        if cues.is_empty() {
            if player.transcript_loading() && matches!(selected, Captions::Track(_)) {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.weak("Reading subtitle track…");
                });
            } else if let Some(error) = &player.transcript_error
                && matches!(selected, Captions::Track(_))
            {
                ui.colored_label(Color32::LIGHT_RED, error);
            } else {
                ui.weak(if player.has_source() {
                    "Choose a subtitle track or open a subtitle file."
                } else {
                    "Open a video to view its subtitles."
                });
            }
            return command;
        }
        const HEIGHT: f32 = 44.0;
        let target = self.revealed.take().or_else(|| {
            let index = current.filter(|_| *follow)?;
            let next = (selected, index);
            (self.last_follow != Some(next)).then_some(index)
        });
        if *follow {
            self.last_follow = current.map(|i| (selected, i));
        }
        let mut scroll = egui::ScrollArea::vertical()
            .id_salt("subtitle-rows")
            .max_height(ui.available_height())
            .auto_shrink([false, false]);
        if let Some(index) = target {
            let row = HEIGHT + ui.spacing().item_spacing.y;
            scroll = scroll.vertical_scroll_offset(
                (index as f32 * row - ui.available_height() * 0.38).max(0.0),
            );
        }
        scroll.show_rows(ui, HEIGHT, cues.len(), |ui, rows: Range<usize>| {
            for index in rows {
                let response = row(ui, &cues[index], HEIGHT, current == Some(index));
                if response.clicked() {
                    command = Some(Command::Go(index));
                }
                response.context_menu(|ui| {
                    if ui.button("Repeat this line").clicked() {
                        command = Some(Command::RepeatLine(index));
                        ui.close();
                    }
                });
            }
        });
        command
    }

    pub fn markers(&self, ui: &egui::Ui, track: Rect, duration: Option<f64>) {
        let Some(duration) = duration.filter(|d| *d > 0.0) else {
            return;
        };
        let x = |time: f64| track.left() + (time / duration).clamp(0.0, 1.0) as f32 * track.width();
        let color = if self.ab {
            Color32::from_rgb(132, 192, 255)
        } else {
            Color32::from_rgb(171, 181, 202)
        };
        let painter = ui.painter();
        if let Some(range) = self.range() {
            painter.rect_filled(
                Rect::from_min_max(
                    pos2(x(range.start), track.top()),
                    pos2(x(range.end), track.bottom()),
                ),
                2.0,
                color.gamma_multiply(0.50),
            );
        }
        for (mark, label) in [(self.media.a, "A"), (self.media.b, "B")] {
            if let Some(time) = mark {
                let x = x(time);
                painter.line_segment(
                    [pos2(x, track.top() - 3.0), pos2(x, track.bottom() + 3.0)],
                    Stroke::new(2.0, color),
                );
                painter.text(
                    pos2(x, track.bottom() + 5.0),
                    egui::Align2::CENTER_TOP,
                    label,
                    FontId::proportional(11.0),
                    color,
                );
            }
        }
    }
}

fn row(ui: &mut egui::Ui, cue: &Cue, height: f32, active: bool) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click());
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    if response.hovered() || active {
        let color = if active {
            Color32::from_rgb(49, 65, 93)
        } else {
            Color32::from_rgb(40, 46, 62)
        };
        ui.painter().rect_filled(rect, 6.0, color);
    }
    let seconds = cue.start.max(0.0);
    let time = format!(
        "{}:{:02}.{:01}",
        seconds as u64 / 60,
        seconds as u64 % 60,
        (seconds * 10.0) as u64 % 10
    );
    ui.painter().text(
        rect.min + vec2(7.0, 8.0),
        egui::Align2::LEFT_TOP,
        time,
        FontId::monospace(11.0),
        Color32::from_rgb(167, 189, 225),
    );
    let bounds = Rect::from_min_max(rect.min + vec2(59.0, 5.0), rect.max - vec2(6.0, 5.0));
    let mut job = egui::text::LayoutJob::simple(
        cue.text.clone(),
        FontId::proportional(13.0),
        Color32::WHITE,
        bounds.width(),
    );
    job.wrap.max_rows = 2;
    let painter = ui.painter().with_clip_rect(bounds);
    painter.galley(bounds.min, painter.layout_job(job), Color32::WHITE);
    response.on_hover_text(&cue.text)
}

pub fn repeat_button(
    ui: &egui::Ui,
    area: Rect,
    label: &str,
    hint: &str,
    selected: bool,
    enabled: bool,
) -> bool {
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let response = ui
        .interact(area, ui.scope_id().with(("repeat", label)), sense)
        .on_hover_text(hint);
    let fill = if selected {
        Color32::from_rgba_unmultiplied(57, 102, 150, 220)
    } else if enabled && response.hovered() {
        Color32::from_black_alpha(210)
    } else {
        Color32::from_black_alpha(130)
    };
    let colour = if enabled {
        Color32::WHITE
    } else {
        Color32::from_gray(156)
    };
    ui.painter().rect_filled(area, 5.0, fill);
    ui.painter().text(
        area.center(),
        egui::Align2::CENTER_CENTER,
        label,
        FontId::proportional(13.0),
        colour,
    );
    response.clicked()
}

pub fn track_name(player: &Player, track: Captions) -> String {
    match track {
        Captions::Off => "No subtitle track".into(),
        Captions::Sidecar => player
            .sidecar_path()
            .map_or_else(|| "Subtitle file".into(), super::library::title),
        Captions::Track(id) => player
            .info
            .as_ref()
            .and_then(|info| info.captions.iter().find(|track| track.index == id))
            .map_or_else(
                || "Subtitle track".into(),
                |track| {
                    track.language.as_ref().map_or_else(
                        || format!("Track {} · {}", id + 1, track.codec.to_uppercase()),
                        |language| format!("{language} · {}", track.codec.to_uppercase()),
                    )
                },
            ),
    }
}
