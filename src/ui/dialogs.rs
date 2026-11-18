use std::path::Path;

use eframe::egui::{self, Color32, Frame, Id, RichText, Stroke, vec2};
use oxideav_core::{ColorRange, ColorSignal, PixelFormat};
use rvideoplayer::engine::{Picture, Player, Track};

use super::{
    controls::{Icon, PANEL_BACKGROUND, PANEL_BORDER, action_icon, timestamp},
    library::title,
    overlays::Clock,
    preferences::{ClickAction, ClockSize, MouseSettings, WHEEL_SEEK_RANGE},
};

const UNKNOWN: &str = "Unknown";

#[derive(Clone, Copy)]
pub enum Dialog {
    Settings(SettingsTab),
    FileInformation,
    Alarm,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum SettingsTab {
    #[default]
    Controls,
    General,
    Playback,
    Display,
    Audio,
    About,
}

pub struct Inputs<'a> {
    pub mouse: &'a mut MouseSettings,
    pub clock_size: &'a mut ClockSize,
    pub clock: &'a mut Clock,
}

pub fn show(
    dialog: &mut Dialog,
    ctx: &egui::Context,
    path: Option<&Path>,
    player: &Player,
    picture: Option<&Picture>,
    inputs: Inputs<'_>,
) -> bool {
    let bounds = ctx.content_rect().size();
    let width = (bounds.x - 46.0).clamp(166.0, 570.0);
    let (id, default_height) = match dialog {
        Dialog::Settings(_) => (Id::unique("settings-panel"), bounds.y * 0.62),
        Dialog::FileInformation => (Id::unique("file-information-panel"), bounds.y * 0.76),
        Dialog::Alarm => (Id::unique("alarm-panel"), 135.0),
    };
    let response = egui::Modal::new(id)
        .area(egui::Modal::default_area(id).default_size(vec2(width, default_height)))
        .backdrop_color(Color32::from_black_alpha(142))
        .frame(
            Frame::new()
                .fill(PANEL_BACKGROUND)
                .stroke(Stroke::new(1.0, PANEL_BORDER))
                .corner_radius(13.0)
                .inner_margin(16.0),
        )
        .show(ctx, |ui| {
            ui.set_width(width);
            if matches!(dialog, Dialog::Settings(_)) {
                // Let a taller tab grow past the modal's last measured size.
                ui.set_max_height((bounds.y * 0.76).max(137.0));
            }
            let name = match dialog {
                Dialog::Settings(_) => "Settings",
                Dialog::FileInformation => "File Information",
                Dialog::Alarm => "Alarm",
            };
            let close = ui
                .horizontal(|ui| {
                    ui.heading(name);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        action_icon(ui, Icon::Remove, "Close", true).clicked()
                    })
                    .inner
                })
                .inner;
            ui.add_space(8.0);
            match dialog {
                Dialog::Settings(tab) => {
                    ui.horizontal_wrapped(|ui| {
                        ui.selectable_value(tab, SettingsTab::General, "General");
                        ui.selectable_value(tab, SettingsTab::Controls, "Controls");
                        ui.selectable_value(tab, SettingsTab::Playback, "Playback");
                        ui.selectable_value(tab, SettingsTab::Display, "Display");
                        ui.selectable_value(tab, SettingsTab::Audio, "Audio");
                        ui.selectable_value(tab, SettingsTab::About, "About");
                    });
                    ui.separator();
                    ui.add_space(9.0);
                    egui::ScrollArea::vertical()
                        .id_salt("settings-content")
                        .auto_shrink([false, true])
                        .max_height((bounds.y * 0.76 - 65.0).max(72.0))
                        .show(ui, |ui| match tab {
                            SettingsTab::Controls => show_controls(ui, inputs.mouse),
                            SettingsTab::General => show_general(ui, inputs.clock_size),
                            SettingsTab::About => show_about(ui),
                            _ => {
                                ui.weak("Reserved for future settings.");
                                ui.add_space((bounds.y - 145.0).clamp(6.0, 88.0));
                            }
                        });
                }
                Dialog::FileInformation => {
                    egui::ScrollArea::vertical()
                        .id_salt("technical-data")
                        .auto_shrink([false, true])
                        .max_height((bounds.y * 0.72 - 40.0).max(80.0))
                        .show(ui, |ui| show_information(ui, path, player, picture));
                }
                Dialog::Alarm => return inputs.clock.settings(ui) || close,
            }
            close
        });
    response.inner || response.should_close()
}

fn show_general(ui: &mut egui::Ui, clock_size: &mut ClockSize) {
    ui.horizontal_wrapped(|ui| {
        ui.label("Clock size");
        egui::ComboBox::from_id_salt("clock-size")
            .width(120.0)
            .selected_text(clock_size.label())
            .show_ui(ui, |ui| {
                for choice in ClockSize::CHOICES {
                    ui.selectable_value(clock_size, choice, choice.label());
                }
            });
    });
}

fn show_controls(ui: &mut egui::Ui, mouse: &mut MouseSettings) {
    for (label, action) in [
        ("Single left click", &mut mouse.single),
        ("Double left click", &mut mouse.double),
    ] {
        ui.horizontal_wrapped(|ui| {
            ui.label(label);
            egui::ComboBox::from_id_salt(label)
                .width(142.0)
                .selected_text(action.label())
                .show_ui(ui, |ui| {
                    for choice in ClickAction::CHOICES {
                        ui.selectable_value(action, choice, choice.label());
                    }
                });
        });
        ui.add_space(5.0);
    }
    ui.horizontal_wrapped(|ui| {
        ui.label("Mouse wheel seek step");
        ui.add(
            egui::DragValue::new(&mut mouse.wheel_seconds)
                .range(WHEEL_SEEK_RANGE)
                .speed(0.4)
                .suffix(" s")
                .update_while_editing(false),
        );
    });
    ui.weak("Scroll up over the video to seek forward and down to seek back.");
}

fn show_about(ui: &mut egui::Ui) {
    ui.heading("RVideoPlayer");
    ui.weak(concat!(
        "Native Linux media player · ",
        env!("CARGO_PKG_VERSION")
    ));
    ui.add_space(11.0);
    section(
        ui,
        "Playback formats",
        &[
            (
                "Video",
                "H.264 (AVC), H.265 (HEVC), AV1, VP8, VP9, MPEG-1/2 Video, MPEG-4 Part 2, Theora".into(),
            ),
            ("Audio", "AAC-LC, AC-3, E-AC-3, DTS Core (MKV), ALAC, Opus, MP3, MP2, FLAC, Vorbis, PCM".into()),
            (
                "Containers",
                "MP4 (including fragmented), MKV, WebM, MOV, AVI, MPEG-TS / M2TS, MPEG-1 System, MPEG-PS, M4V, FLV, Ogg / OGV, 3GP".into(),
            ),
            ("Audio files", "MP3, FLAC, AC-3, E-AC-3".into()),
            ("Subtitles", "SRT, WebVTT, ASS/SSA (text), PGS (.sup), VobSub (.idx + .sub)".into()),
        ],
    );
    ui.weak("Playback support depends on the combination of container and codecs.");
}

fn show_information(
    ui: &mut egui::Ui,
    path: Option<&Path>,
    player: &Player,
    picture: Option<&Picture>,
) {
    let info = player.info.as_deref();
    let video = info.and_then(|i| i.video.iter().find(|t| Some(t.index) == i.current_video));
    let audio = info.and_then(|i| i.audio.iter().find(|t| Some(t.index) == i.current_audio));
    let video_details = video.and_then(|track| track.video.as_ref());
    let duration = player
        .duration()
        .filter(|time| time.is_finite() && *time > 0.0);
    let file_size = info.and_then(|i| i.file_size);
    let avg_bitrate = file_size
        .zip(duration)
        .map(|(bytes, seconds)| bytes as f64 * 8.0 / seconds);
    let dimensions = picture
        .map(|p| (p.width, p.height))
        .or_else(|| video.and_then(|t| t.width.zip(t.height)))
        .filter(|&(w, h)| w > 0 && h > 0);
    let fallback_color = video_details.map(|v| v.color).unwrap_or_default();
    let color = picture.map_or(fallback_color, |p| p.color.or(fallback_color));
    let depth = video_details
        .and_then(|v| v.bit_depth)
        .or_else(|| {
            picture.and_then(|p| {
                p.frame
                    .plane_significant_bits(0)
                    .or_else(|| pixel_depth(p.format))
            })
        })
        .or_else(|| video_details.and_then(|v| v.format).and_then(pixel_depth));

    section(
        ui,
        "File",
        &[
            ("Name", value(path.map(title))),
            (
                "Full path",
                value(path.map(|p| p.to_string_lossy().into_owned())),
            ),
            (
                "Size",
                value(
                    file_size
                        .map(|size| format!("{:.2} MiB · {size} bytes", size as f64 / 1_048_576.0)),
                ),
            ),
            (
                "Container",
                value(info.map(|i| container_name(&i.container_label))),
            ),
            (
                "Duration",
                value(duration.map(|seconds| format!("{} · {seconds:.3} s", timestamp(seconds)))),
            ),
            ("Avg. bitrate", value(avg_bitrate.map(bitrate))),
        ],
    );
    section(
        ui,
        "Video",
        &[
            ("Codec", value(video.map(|t| codec_name(&t.codec)))),
            (
                "Profile",
                value(video_details.and_then(|v| v.profile.clone())),
            ),
            (
                "Resolution",
                value(dimensions.map(|(w, h)| format!("{w} × {h}"))),
            ),
            (
                "Frame rate",
                value(
                    video_details
                        .and_then(|v| v.frame_rate)
                        .map(|v| format!("{v:.3} fps (nominal)")),
                ),
            ),
            ("Bit depth", value(depth.map(|bits| format!("{bits} bit")))),
            (
                "Video bitrate",
                value(
                    video_details
                        .and_then(|v| v.bit_rate)
                        .map(|n| bitrate(n as f64)),
                ),
            ),
            ("Color space", color_space(color)),
            ("HDR", transfer_mode(color).into()),
            (
                "Decoder",
                value(info.and_then(|i| i.backend).map(|name| {
                    if name == "VA-API" {
                        name.to_owned()
                    } else {
                        format!("Software ({name})")
                    }
                })),
            ),
        ],
    );
    section(
        ui,
        "Audio",
        &[
            ("Codec", value(audio.map(|t| codec_name(&t.codec)))),
            (
                "Sample rate",
                value(audio.and_then(|t| t.sample_rate).map(|n| format!("{n} Hz"))),
            ),
            (
                "Channels",
                value(audio.and_then(|t| t.channels).map(|n| n.to_string())),
            ),
        ],
    );
    if let Some(info) = info {
        track_list(
            ui,
            "Audio tracks",
            &info.available_audio,
            info.current_audio,
            &info.audio,
            false,
        );
        track_list(
            ui,
            "Subtitles",
            &info.available_captions,
            None,
            &info.captions,
            player.has_sidecar(),
        );
    } else {
        section(
            ui,
            "Audio tracks and subtitles",
            &[("Tracks", UNKNOWN.into())],
        );
    }
}

fn section(ui: &mut egui::Ui, name: &str, rows: &[(&str, String)]) {
    ui.label(
        RichText::new(name)
            .strong()
            .color(Color32::from_rgb(170, 192, 236)),
    );
    let width = ui.available_width();
    let label_width = (width * 0.34).min(126.0);
    egui::Grid::new(ui.scope_id().with(name))
        .num_columns(2)
        .spacing(vec2(9.0, 5.0))
        .show(ui, |ui| {
            for (key, value) in rows {
                let columns = [
                    (
                        label_width,
                        RichText::new(*key).color(Color32::from_rgb(163, 175, 195)),
                    ),
                    ((width - label_width - 12.0).max(40.0), RichText::new(value)),
                ];
                for (width, content) in columns {
                    ui.allocate_ui_with_layout(
                        vec2(width, 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.set_width(width);
                            ui.add(egui::Label::new(content).wrap().selectable(true));
                        },
                    );
                }
                ui.end_row();
            }
        });
    ui.add_space(10.0);
}

fn track_list(
    ui: &mut egui::Ui,
    name: &str,
    tracks: &[Track],
    selected: Option<u32>,
    supported: &[Track],
    sidecar: bool,
) {
    let mut rows: Vec<_> = tracks
        .iter()
        .map(|t| {
            let language = t
                .language
                .as_deref()
                .filter(|lang| !lang.is_empty() && *lang != "und")
                .unwrap_or(UNKNOWN);
            let status = if Some(t.index) == selected {
                " · selected"
            } else if supported.iter().any(|known| known.index == t.index) {
                ""
            } else {
                " · unsupported"
            };
            (
                "Track",
                format!(
                    "#{} · {} · {language}{status}",
                    t.index,
                    codec_name(&t.codec)
                ),
            )
        })
        .collect();
    if sidecar {
        rows.push(("External", "Subtitle file".into()));
    }
    if rows.is_empty() {
        rows.push(("Tracks", "None".into()));
    }
    section(ui, name, &rows);
}

fn value(value: Option<String>) -> String {
    value.unwrap_or_else(|| UNKNOWN.into())
}

fn bitrate(rate: f64) -> String {
    format!("{:.1} kb/s", rate / 1_000.0)
}

fn container_name(id: &str) -> String {
    match id {
        "matroska" => "Matroska (MKV)".into(),
        "webm" => "WebM".into(),
        "mov" => "QuickTime (MOV)".into(),
        "mpegts" => "MPEG-TS".into(),
        "mpegps" => "MPEG-PS".into(),
        "mpeg1ps" => "MPEG-1 System".into(),
        "ogg" => "Ogg".into(),
        _ => id.to_ascii_uppercase(),
    }
}

fn codec_name(id: &str) -> String {
    match id {
        "h264" => "H.264 / AVC".into(),
        "h265" | "hevc" => "H.265 / HEVC".into(),
        "mpeg2video" => "MPEG-2 Video".into(),
        "mpeg1video" => "MPEG-1 Video".into(),
        "ac3" => "AC-3".into(),
        "eac3" => "E-AC-3".into(),
        "dts" => "DTS".into(),
        _ => id.to_ascii_uppercase(),
    }
}

fn pixel_depth(format: PixelFormat) -> Option<u8> {
    use PixelFormat::*;
    match format {
        Gray8 | Yuv420P | Yuv422P | Yuv444P | Yuv440P | YuvJ420P | YuvJ422P | YuvJ444P | Rgb24
        | Rgba | Bgr24 | Bgra | Nv12 | Nv21 | Gbrp8 => Some(8),
        Gray10Le | Yuv420P10Le | Yuv422P10Le | Yuv444P10Le | Yuv440P10Le | Gbrp10Le => Some(10),
        Gray12Le | Yuv420P12Le | Yuv422P12Le | Yuv444P12Le | Yuv440P12Le | Gbrp12Le => Some(12),
        Gbrp14Le => Some(14),
        Gray16Le | Yuv420P16Le | Yuv422P16Le | Yuv444P16Le | Gbrp16Le => Some(16),
        _ => None,
    }
}

fn color_space(color: ColorSignal) -> String {
    let entry = |name: Option<&str>| {
        name.filter(|s| *s != "unspecified")
            .unwrap_or(UNKNOWN)
            .to_owned()
    };
    let range = match color.range {
        ColorRange::Full => "Full",
        ColorRange::Limited => "Limited",
        _ => UNKNOWN,
    };
    if color == ColorSignal::unspecified() {
        return UNKNOWN.into();
    }
    format!(
        "Primaries {} · Matrix {} · Transfer {} · {range}",
        entry(color.primaries.name()),
        entry(color.matrix.name()),
        entry(color.transfer.name())
    )
}

fn transfer_mode(color: ColorSignal) -> &'static str {
    match color.transfer.0 {
        16 => "PQ (SMPTE ST 2084)",
        18 => "HLG",
        1 | 4..=7 | 11..=15 => "SDR",
        _ => UNKNOWN,
    }
}
