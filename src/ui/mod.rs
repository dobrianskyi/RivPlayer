mod controls;
mod dialogs;
mod file_manager;
mod layout;
mod learning;
mod library;
mod library_rows;
mod overlays;
mod preferences;
mod preview;
mod sidebar;
mod video;

use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Key, Pos2,
    Rect, RichText, Sense, Vec2, pos2, vec2,
};
use rvideoplayer::{
    engine::{
        BitmapFrame, Captions, MAX_VOLUME, Options, Picture, Player, Repeat, Track, active_cue,
    },
    source::{LocalFile, is_caption},
};

use preview::Preview;
use sidebar::{Action, QueueItem, Sidebar};

use controls::{
    Icon, PANEL_BACKGROUND, PANEL_BORDER, PlaybackCue, RED, WHITE, button, draw, gradient,
    named_button, seekbar, timestamp, volume_slider,
};
use layout::Layout;
use learning::{Command as Learn, Learning};
use overlays::{Clock, Drag as OverlayDrag, TimeAction};
use preferences::{ClickAction, MouseSettings, PlaybackSettings, SubtitleChoice};

const SEEK_OFFSET: f32 = 59.0;
const VOLUME_STEP: f32 = 5.0;
const WHEEL_SETTLE: Duration = Duration::from_millis(45);
const WHEEL_MAX_WAIT: Duration = Duration::from_millis(140);

pub fn launch(path: Option<PathBuf>, options: Options, position_explicit: bool) -> Result<()> {
    let icon = Arc::new(
        eframe::icon_data::from_png_bytes(include_bytes!("../../assets/rvideoplayer.png"))
            .context("Loading the RVideoPlayer icon")?,
    );
    let mut native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("RVideoPlayer")
            .with_app_id("rvideoplayer")
            .with_icon(icon.clone())
            .with_inner_size(layout::INITIAL_SIZE)
            .with_min_inner_size(layout::MIN_VIDEO),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    match std::env::var("RVIDEOPLAYER_WAYLAND").as_deref() {
        Ok("0") => {
            use winit::platform::x11::EventLoopBuilderExtX11;
            native.event_loop_builder = Some(Box::new(|builder| {
                builder.with_x11();
            }));
        }
        Ok("1") => {
            use winit::platform::wayland::EventLoopBuilderExtWayland;
            native.event_loop_builder = Some(Box::new(|builder| {
                builder.with_wayland();
            }));
        }
        _ => {}
    }
    eframe::run_native(
        "RVideoPlayer",
        native,
        Box::new(move |cc| {
            Ok(Box::new(App::new(
                cc,
                path,
                options,
                position_explicit,
                &icon,
            )?))
        }),
    )
    .map_err(|err| anyhow!("Opening RVideoPlayer: {err}"))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Menu {
    Settings,
    Speed,
    Video,
    Audio,
    Subtitles,
}

#[derive(Clone, Copy)]
enum RowAction {
    Menu(Menu),
    Settings(dialogs::SettingsTab),
    Speed(f64),
    ToggleAb,
    ToggleClock,
    Alarm,
    Video(u32),
    Audio(u32),
    Captions(Captions),
    OpenCaptions,
    ToggleSubtitlesPanel,
    VoiceBoost,
}

struct Row {
    label: String,
    value: String,
    action: RowAction,
    active: bool,
}

impl Row {
    fn toggle(label: &str, enabled: bool, action: RowAction) -> Self {
        Self {
            label: label.into(),
            value: if enabled { "On" } else { "Off" }.into(),
            action,
            active: false,
        }
    }

    fn page(label: &str, value: String, menu: Menu) -> Self {
        Self {
            label: label.into(),
            value,
            action: RowAction::Menu(menu),
            active: false,
        }
    }

    fn choice(label: String, active: bool, action: RowAction) -> Self {
        Self {
            label,
            value: String::new(),
            action,
            active,
        }
    }
}

#[derive(Clone, Copy)]
enum Picker {
    Media,
    Captions,
    Playlist,
}

struct BitmapOverlay {
    frame: Arc<BitmapFrame>,
    textures: Vec<(Rect, egui::TextureHandle)>,
}

struct WheelSeek {
    target: f64,
    started: Instant,
    due: Instant,
}

struct App {
    player: Player,
    learning: Learning,
    playback: PlaybackSettings,
    saved_playback: PlaybackSettings,
    selected_caption: Captions,
    requested_caption: Option<PathBuf>,
    renderer: Arc<Mutex<video::Renderer>>,
    picture: Option<Arc<Picture>>,
    bitmap_captions: Option<BitmapOverlay>,
    clock: Clock,
    clock_drag: OverlayDrag,
    caption_drag: OverlayDrag,
    serial: u64,
    title: String,
    title_icon: egui::TextureHandle,
    last_input: Instant,
    pointer: Option<Pos2>,
    fullscreen: bool,
    menu: Option<Menu>,
    drag: Option<f64>,
    wheel_seek: Option<WheelSeek>,
    volume_open: bool,
    dialog: Option<(Picker, Receiver<Vec<PathBuf>>)>,
    video_dialog: Option<dialogs::Dialog>,
    mouse: MouseSettings,
    last_video_click: Option<ClickAction>,
    sidebar: Sidebar,
    layout: Layout,
    playback_cue: Option<PlaybackCue>,
    preview: Preview,
    path: Option<PathBuf>,
    queued: Option<QueueItem>,
    saved_at: Instant,
    history_saved: bool,
    history_forgotten: bool,
    history_cover: bool,
    cover_attempt: Instant,
}

impl App {
    fn new(
        cc: &eframe::CreationContext<'_>,
        path: Option<PathBuf>,
        options: Options,
        position_explicit: bool,
        icon: &egui::IconData,
    ) -> Result<Self> {
        let gl = cc
            .gl
            .as_ref()
            .ok_or_else(|| anyhow!("OpenGL renderer unavailable"))?;
        let renderer = Arc::new(Mutex::new(video::Renderer::new(gl)?));
        let mut fonts = FontDefinitions::default();
        fonts.font_data.insert(
            "Roboto".to_owned(),
            Arc::new(FontData::from_static(include_bytes!(
                "../../assets/Roboto.ttf"
            ))),
        );
        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .insert(0, "Roboto".to_owned());
        cc.egui_ctx.set_fonts(fonts);
        let mut visuals = egui::Visuals::dark();
        visuals.window_fill = PANEL_BACKGROUND;
        visuals.window_stroke = egui::Stroke::new(1.0, PANEL_BORDER);
        cc.egui_ctx.set_visuals(visuals);
        let title_icon = cc.egui_ctx.load_texture(
            "rvideoplayer-icon",
            egui::ColorImage::from_rgba_unmultiplied(
                [icon.width as usize, icon.height as usize],
                &icon.rgba,
            ),
            egui::TextureOptions::LINEAR,
        );
        let ctx = cc.egui_ctx.clone();
        let sidebar = Sidebar::new(cc.egui_ctx.clone())?;
        let mouse = sidebar.db.mouse_settings()?;
        let playback = sidebar.db.playback_settings()?;
        let mut player = Player::new(move || ctx.request_repaint(), options.mode);
        player.restore_volume(playback.volume, playback.muted);
        player.set_rate(playback.rate);
        player.set_voice_boost(playback.voice);
        let mut app = Self {
            player,
            learning: Learning::default(),
            playback,
            saved_playback: playback,
            selected_caption: Captions::Off,
            requested_caption: None,
            renderer,
            picture: None,
            bitmap_captions: None,
            clock: Clock::default(),
            clock_drag: OverlayDrag::default(),
            caption_drag: OverlayDrag::default(),
            serial: 0,
            title: "RVideoPlayer".into(),
            title_icon,
            last_input: Instant::now(),
            pointer: None,
            fullscreen: false,
            menu: None,
            drag: None,
            wheel_seek: None,
            volume_open: false,
            dialog: None,
            video_dialog: None,
            mouse,
            last_video_click: None,
            sidebar,
            layout: Layout::new(cc.winit_window().map(Arc::as_ref)),
            playback_cue: None,
            preview: Preview::new(cc.egui_ctx.clone()),
            path: None,
            queued: None,
            saved_at: Instant::now(),
            history_saved: false,
            history_forgotten: false,
            history_cover: false,
            cover_attempt: Instant::now(),
        };
        if let Some(path) = path {
            app.open(
                &cc.egui_ctx,
                path,
                position_explicit.then_some(options.position),
                None,
                false,
            );
        } else {
            match app.sidebar.db.last_video() {
                Ok(Some((path, position))) if readable_file(&path) => {
                    app.open(&cc.egui_ctx, path, Some(position), None, true);
                }
                Ok(_) => {}
                Err(err) => app.sidebar.error = Some(format!("{err:#}")),
            }
        }
        Ok(app)
    }

    fn open(
        &mut self,
        ctx: &egui::Context,
        path: PathBuf,
        position: Option<f64>,
        queue: Option<QueueItem>,
        paused: bool,
    ) {
        if is_caption(&path) && self.player.has_source() {
            let path = path.canonicalize().unwrap_or(path);
            self.requested_caption = Some(path.clone());
            self.player.load_captions(&path);
            return;
        }
        self.finish_overlay_drag();
        self.save_history(false);
        let path = path.canonicalize().unwrap_or(path);
        let position = position.unwrap_or_else(|| {
            self.sidebar.db.resume(&path).unwrap_or_else(|err| {
                self.sidebar.error = Some(format!("{err:#}"));
                0.0
            })
        });
        self.preview.invalidate(&path);
        self.path = Some(path.clone());
        self.learning =
            Learning::new(self.sidebar.db.media_settings(&path).unwrap_or_else(|err| {
                self.sidebar.error = Some(format!("{err:#}"));
                Default::default()
            }));
        self.selected_caption = Captions::Off;
        self.requested_caption = None;
        self.queued = queue;
        self.history_saved = false;
        self.history_forgotten = false;
        self.history_cover = false;
        self.cover_attempt = Instant::now();
        self.title = library::title(&path);
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!(
            "{} — RVideoPlayer",
            self.title
        )));
        self.picture = None;
        self.layout.new_file();
        self.playback_cue = None;
        self.drag = None;
        self.wheel_seek = None;
        self.menu = None;
        self.video_dialog = None;
        self.last_video_click = None;
        self.last_input = Instant::now();
        let source = Arc::new(LocalFile(path));
        self.player.open_with_tracks(
            source,
            position,
            !paused,
            self.learning.media.video,
            self.learning.media.audio,
        );
        if let SubtitleChoice::Sidecar(Some(path)) = &self.learning.media.subtitles
            && readable_file(path)
        {
            self.player.load_captions(path);
        }
    }

    fn save_playback_preferences(&mut self) {
        self.playback.rate = self.player.rate();
        self.playback.volume = self.player.volume;
        self.playback.muted = self.player.muted;
        self.playback.voice = self.player.voice_boost();
        if self.playback != self.saved_playback {
            match self.sidebar.db.save_playback_settings(self.playback) {
                Ok(()) => self.saved_playback = self.playback,
                Err(err) => {
                    self.sidebar.error = Some(format!("Saving playback settings · {err:#}"))
                }
            }
        }
    }

    fn save_media_preferences(&mut self) {
        if let Some(path) = &self.path
            && let Err(err) = self
                .sidebar
                .db
                .save_media_settings(path, &self.learning.media)
        {
            self.sidebar.error = Some(format!("Saving media settings · {err:#}"));
        }
    }

    fn finish_overlay_drag(&mut self) {
        let clock_changed = self.clock_drag.finish();
        let caption_changed = self.caption_drag.finish();
        if clock_changed || caption_changed {
            self.save_media_preferences();
        }
    }

    fn update_captions(&mut self) {
        if let Some(path) = self.requested_caption.as_ref()
            && self.player.sidecar_path() == Some(path.as_path())
        {
            self.learning.media.subtitles = SubtitleChoice::Sidecar(self.requested_caption.take());
            self.playback.captions = true;
            self.learning.cue_loop = None;
            self.sync_repeat();
            self.learning.clear_follow();
            self.save_media_preferences();
        }
        let tracks = self
            .player
            .info
            .as_ref()
            .map_or(&[][..], |i| i.captions.as_slice());
        let selected = match &self.learning.media.subtitles {
            SubtitleChoice::Embedded(id) if tracks.iter().any(|track| track.index == *id) => {
                Captions::Track(*id)
            }
            SubtitleChoice::Sidecar(_) | SubtitleChoice::Auto if self.player.has_sidecar() => {
                Captions::Sidecar
            }
            _ => tracks
                .first()
                .map_or(Captions::Off, |track| Captions::Track(track.index)),
        };
        self.selected_caption = selected;
        self.player.captions = if self.playback.captions {
            selected
        } else {
            Captions::Off
        };
        if (self.playback.captions || self.sidebar.subtitles_open())
            && matches!(selected, Captions::Track(_))
        {
            self.player.read_transcript();
        }
    }

    fn sync_repeat(&mut self) {
        let repeat = self
            .learning
            .cue_loop
            .or_else(|| self.learning.ab.then(|| self.learning.range()).flatten());
        self.player.set_repeat(repeat);
    }

    fn toggle_transcript(&mut self, ctx: &egui::Context) {
        if self.sidebar.subtitles_open() {
            self.sidebar.toggle_open();
        } else {
            if self.fullscreen {
                self.fullscreen(ctx, false);
            }
            self.sidebar.open_subtitles();
            self.update_captions();
        }
        ctx.request_repaint();
    }

    fn seek_to(&mut self, time: f64) {
        self.wheel_seek = None;
        self.preview.cancel();
        if self
            .player
            .repeat()
            .is_some_and(|range| time < range.start || time >= range.end)
        {
            self.learning.ab = false;
            self.learning.cue_loop = None;
            self.sync_repeat();
        }
        self.player.seek(time);
        self.save_history(false);
    }

    fn queue_wheel_seek(&mut self, ctx: &egui::Context, steps: f32) {
        let now = Instant::now();
        let seek = self.wheel_seek.get_or_insert_with(|| WheelSeek {
            target: self.player.position(),
            started: now,
            due: now,
        });
        seek.target = (seek.target + f64::from(steps) * f64::from(self.mouse.wheel_seconds))
            .clamp(0.0, self.player.duration().unwrap_or(f64::MAX));
        seek.due = (now + WHEEL_SETTLE).min(seek.started + WHEEL_MAX_WAIT);
        self.preview.cancel();
        ctx.request_repaint();
    }

    fn learning_action(&mut self, ctx: &egui::Context, command: Learn) {
        if !self.player.has_source() {
            return;
        }
        let position = self.player.position();
        match command {
            Learn::Open => self.picker(ctx, Picker::Captions),
            Learn::Visible => self.playback.captions = !self.playback.captions,
            Learn::Track(track) => {
                self.learning.media.subtitles = match track {
                    Captions::Track(id) => SubtitleChoice::Embedded(id),
                    Captions::Sidecar => {
                        SubtitleChoice::Sidecar(self.player.sidecar_path().map(Path::to_owned))
                    }
                    Captions::Off => SubtitleChoice::Auto,
                };
                self.learning.cue_loop = None;
                self.learning.clear_follow();
                self.sync_repeat();
                self.save_media_preferences();
            }
            Learn::Start => {
                self.learning.media.a = Some(position);
                if self.learning.range().is_none() {
                    self.learning.media.b = None;
                    self.learning.ab = false;
                }
                self.learning.cue_loop = None;
                self.sync_repeat();
                self.save_media_preferences();
            }
            Learn::End => {
                if let Some(range) = self
                    .learning
                    .media
                    .a
                    .and_then(|start| Repeat::new(start, position))
                {
                    self.learning.media.b = Some(position);
                    self.learning.ab = true;
                    self.learning.cue_loop = None;
                    self.sync_repeat();
                    self.seek_to(range.start);
                    self.save_media_preferences();
                }
            }
            Learn::Toggle => {
                if let Some(range) = self.learning.range() {
                    self.learning.ab = !self.learning.ab;
                    self.learning.cue_loop = None;
                    self.sync_repeat();
                    if self.learning.ab && (position < range.start || position >= range.end) {
                        self.seek_to(range.start);
                    }
                }
            }
            Learn::Reset => {
                self.learning.media.a = None;
                self.learning.media.b = None;
                self.learning.ab = false;
                self.learning.cue_loop = None;
                self.sync_repeat();
                self.save_media_preferences();
            }
            Learn::CueLoop if self.learning.cue_loop.is_some() => {
                self.learning.cue_loop = None;
                self.sync_repeat();
            }
            Learn::Go(_)
            | Learn::Previous
            | Learn::Next
            | Learn::Replay
            | Learn::CueLoop
            | Learn::RepeatLine(_) => {
                let cues = self.player.cues(self.selected_caption);
                let before = cues.partition_point(|cue| cue.start <= position + 0.012);
                let current = active_cue(cues, position + 0.012);
                let index = match command {
                    Learn::Go(i) | Learn::RepeatLine(i) => Some(i),
                    Learn::Previous => current.map_or(before.checked_sub(1), |i| i.checked_sub(1)),
                    Learn::Next => Some(before),
                    Learn::Replay => current.or_else(|| before.checked_sub(1)),
                    Learn::CueLoop => current,
                    _ => None,
                };
                let Some((index, cue)) = index.and_then(|i| cues.get(i).map(|cue| (i, cue))) else {
                    return;
                };
                let time = cue.start.max(0.0);
                let end = self
                    .player
                    .duration()
                    .map_or(cue.end, |duration| cue.end.min(duration));
                let range = Repeat::new(time, end);
                let start_loop = matches!(command, Learn::CueLoop | Learn::RepeatLine(_));
                let loop_line = start_loop || self.learning.cue_loop.is_some();
                if loop_line {
                    self.learning.ab = false;
                    self.learning.cue_loop = range;
                    self.sync_repeat();
                }
                self.learning.select_row(index);
                if start_loop || matches!(command, Learn::Replay) {
                    if !loop_line {
                        self.learning.ab = false;
                        self.sync_repeat();
                    }
                    self.player.play_from(time);
                    self.save_history(false);
                } else {
                    self.seek_to(time);
                }
            }
        }
        self.update_captions();
        self.save_playback_preferences();
        ctx.request_repaint();
    }

    fn save_history(&mut self, include_cover: bool) {
        if self.history_forgotten || self.player.error.is_some() || self.player.info.is_none() {
            return;
        }
        let Some(path) = &self.path else { return };
        if !self.history_saved
            && self.picture.is_none()
            && self
                .player
                .info
                .as_ref()
                .is_some_and(|info| info.current_video.is_some())
        {
            return;
        }
        let cover = if include_cover && !self.history_cover {
            self.cover_attempt = Instant::now();
            self.picture
                .as_ref()
                .and_then(|picture| preview::thumbnail(picture))
                .filter(preview::has_detail)
                .map(|image| preview::encode(&image))
        } else {
            None
        };
        self.history_cover |= cover.is_some();
        self.sidebar.save(
            path,
            self.wheel_seek
                .as_ref()
                .map_or_else(|| self.player.position(), |seek| seek.target),
            self.player.duration(),
            self.wheel_seek.is_none() && self.player.ended(),
            cover.as_deref(),
        );
        self.history_saved = true;
        self.saved_at = Instant::now();
    }

    fn load_files(&mut self, ctx: &egui::Context, mut files: Vec<PathBuf>, append_only: bool) {
        files.retain(|path| path.is_file());
        let Some(first) = files.first().cloned() else {
            return;
        };
        if !append_only && files.len() == 1 {
            self.open(ctx, first, None, None, false);
            return;
        }
        files.retain(|path| !is_caption(path));
        if files.is_empty() {
            return;
        }
        for path in &mut files {
            if let Ok(canonical) = path.canonicalize() {
                *path = canonical;
            }
        }
        match self.sidebar.enqueue(&files) {
            Ok(queue) => {
                if !append_only {
                    self.open(ctx, files.remove(0), None, queue, false);
                }
            }
            Err(err) => self.sidebar.error = Some(format!("{err:#}")),
        }
    }

    fn picker(&mut self, ctx: &egui::Context, kind: Picker) {
        if self.dialog.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        self.dialog = Some((kind, rx));
        std::thread::spawn(move || {
            let dialog = rfd::FileDialog::new();
            let files = match kind {
                Picker::Captions => dialog
                    .set_title("Open subtitles")
                    .add_filter("Subtitles", &["srt", "vtt", "ass", "ssa", "sup", "idx"])
                    .pick_file()
                    .into_iter()
                    .collect(),
                Picker::Media | Picker::Playlist => dialog
                    .set_title("Open media")
                    .add_filter(
                        "Video and audio",
                        &[
                            "mp4", "m4v", "m4a", "mkv", "mka", "webm", "mov", "avi", "ts", "m2ts",
                            "mts", "mpg", "mpeg", "vob", "flv", "ogg", "ogv", "opus", "3gp", "mp3",
                            "flac", "ac3", "eac3",
                        ],
                    )
                    .add_filter("All files", &["*"])
                    .pick_files()
                    .unwrap_or_default(),
            };
            let _ = tx.send(files);
            ctx.request_repaint();
        });
    }

    fn fullscreen(&mut self, ctx: &egui::Context, active: bool) {
        self.fullscreen = active;
        self.last_input = Instant::now();
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(active));
    }

    fn toggle_playback(&mut self) {
        self.player.play_pause();
        if self.player.is_playing() {
            self.clock.elapsed = None;
        }
        self.playback_cue = Some(PlaybackCue::new(self.player.is_playing()));
        if !self.player.is_playing() {
            self.save_history(true);
        }
    }

    fn video_click(&mut self, ctx: &egui::Context, action: ClickAction) {
        match action {
            ClickAction::Nothing => {}
            ClickAction::Playback => self.toggle_playback(),
            ClickAction::Fullscreen => self.fullscreen(ctx, !self.fullscreen),
        }
    }

    fn video_context_menu(&mut self, response: &egui::Response) {
        response.context_menu(|ui| {
            ui.set_min_width(172.0);
            let mut open = false;
            ui.menu_button("File", |ui| {
                if ui.button("Open…").clicked() {
                    open = true;
                    ui.close();
                }
            });
            if open {
                self.menu = None;
                self.picker(ui.ctx(), Picker::Media);
                ui.close();
            }
            if ui.button("Settings").clicked() {
                self.menu = None;
                self.video_dialog = Some(dialogs::Dialog::Settings(Default::default()));
                ui.close();
            }
            if self.player.has_source() && ui.button("File Information").clicked() {
                self.menu = None;
                self.video_dialog = Some(dialogs::Dialog::FileInformation);
                ui.close();
            }
            if let Some(action) = overlays::time_menu(ui, self.playback.show_clock) {
                self.time_action(ui.ctx(), action);
            }
        });
    }

    fn time_action(&mut self, ctx: &egui::Context, action: TimeAction) {
        self.menu = None;
        match action {
            TimeAction::Visibility(show) => {
                self.playback.show_clock = show;
                self.save_playback_preferences();
            }
            TimeAction::Alarm => self.video_dialog = Some(dialogs::Dialog::Alarm),
        }
        ctx.request_repaint();
    }

    fn show_video_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.video_dialog else {
            return;
        };
        let prior = self.mouse;
        let size = self.playback.clock_size;
        if dialogs::show(
            dialog,
            ctx,
            self.path.as_deref(),
            &self.player,
            self.picture.as_deref(),
            dialogs::Inputs {
                mouse: &mut self.mouse,
                clock_size: &mut self.playback.clock_size,
                clock: &mut self.clock,
            },
        ) {
            self.video_dialog = None;
        }
        if self.mouse != prior
            && let Err(err) = self.sidebar.db.save_mouse_settings(self.mouse)
        {
            self.sidebar.error = Some(format!("Saving mouse settings · {err:#}"));
        }
        if self.playback.clock_size != size {
            self.save_playback_preferences();
        }
    }

    fn picture_aspect(&self) -> Option<f32> {
        let picture = self.picture.as_ref()?;
        let sample_aspect = self
            .player
            .info
            .as_ref()
            .and_then(|info| {
                info.video
                    .iter()
                    .find(|track| Some(track.index) == info.current_video)
            })
            .map_or(1.0, |track| track.pixel_aspect);
        let aspect = picture.width as f32 * sample_aspect / picture.height as f32;
        (aspect.is_finite() && aspect > 0.0).then_some(aspect)
    }

    fn keyboard(&mut self, ctx: &egui::Context) {
        if self.sidebar.editing() || self.video_dialog.is_some() {
            return;
        }
        let pressed = |key| ctx.input(|i| i.key_pressed(key));
        if ctx.input(|i| i.modifiers.command && i.key_pressed(Key::O)) {
            self.picker(ctx, Picker::Media);
        }
        if pressed(Key::Escape) {
            if self.menu.take().is_none() && self.fullscreen {
                self.fullscreen(ctx, false);
            }
            return;
        }
        if self.menu.is_some() || self.dialog.is_some() || !self.player.has_source() {
            return;
        }
        let shortcut = |key, modifiers| ctx.input_mut(|input| input.consume_key(modifiers, key));
        if shortcut(Key::C, egui::Modifiers::SHIFT) {
            self.toggle_transcript(ctx);
            return;
        }
        for (modifiers, key, action) in [
            (egui::Modifiers::SHIFT, Key::ArrowLeft, Learn::Previous),
            (egui::Modifiers::SHIFT, Key::ArrowRight, Learn::Next),
            (egui::Modifiers::SHIFT, Key::ArrowDown, Learn::Replay),
            (egui::Modifiers::SHIFT, Key::ArrowUp, Learn::CueLoop),
            (egui::Modifiers::SHIFT, Key::R, Learn::Reset),
            (egui::Modifiers::SHIFT, Key::L, Learn::Toggle),
            (egui::Modifiers::NONE, Key::OpenBracket, Learn::Start),
            (egui::Modifiers::NONE, Key::CloseBracket, Learn::End),
        ] {
            if shortcut(key, modifiers) {
                self.learning_action(ctx, action);
                return;
            }
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Space))
            || pressed(Key::K)
        {
            self.toggle_playback();
        }
        if pressed(Key::M) {
            self.player.toggle_mute();
        }
        if pressed(Key::C) {
            self.learning_action(ctx, Learn::Visible);
        }
        if pressed(Key::F) {
            self.fullscreen(ctx, !self.fullscreen);
        }
        let position = self.player.position();
        for (key, delta) in [
            (Key::ArrowLeft, -5.0),
            (Key::ArrowRight, 5.0),
            (Key::J, -10.0),
            (Key::L, 10.0),
        ] {
            if pressed(key) {
                self.seek_to(position + delta);
            }
        }
        for (key, delta) in [(Key::ArrowDown, -VOLUME_STEP), (Key::ArrowUp, VOLUME_STEP)] {
            if pressed(key) {
                self.player
                    .set_volume(((self.player.volume * 100.0).round() + delta) / 100.0);
            }
        }
        if let Some(duration) = self.player.duration() {
            for (i, key) in [
                Key::Num0,
                Key::Num1,
                Key::Num2,
                Key::Num3,
                Key::Num4,
                Key::Num5,
                Key::Num6,
                Key::Num7,
                Key::Num8,
                Key::Num9,
            ]
            .into_iter()
            .enumerate()
            {
                if pressed(key) {
                    self.seek_to(duration * i as f64 / 10.0);
                }
            }
        }
    }

    fn paint_captions(
        &mut self,
        ui: &egui::Ui,
        rect: Rect,
        video_rect: Option<Rect>,
        controls: bool,
        time: f64,
    ) {
        if let Some(frame) = self.player.bitmap_at(time)
            && let Some(video_rect) = video_rect
        {
            if self
                .bitmap_captions
                .as_ref()
                .is_none_or(|cache| !Arc::ptr_eq(&cache.frame, &frame))
            {
                self.bitmap_captions = Some(BitmapOverlay {
                    textures: bitmap_textures(ui, &frame),
                    frame,
                });
            }
            let textures = &self.bitmap_captions.as_ref().unwrap().textures;
            if textures.is_empty() {
                return;
            }
            let bounds = textures
                .iter()
                .fold(Rect::NOTHING, |area, (r, _)| area.union(*r));
            let shift = if controls {
                let bottom = video_rect.top() + bounds.bottom() * video_rect.height();
                (bottom - rect.bottom() + SEEK_OFFSET + 35.0)
                    .clamp(0.0, (bounds.top() * video_rect.height()).max(0.0))
            } else {
                0.0
            };
            let position =
                |p: Pos2| video_rect.min + p.to_vec2() * video_rect.size() - vec2(0.0, shift);
            let default = Rect::from_min_max(position(bounds.min), position(bounds.max));
            let placed = self.caption_drag.place(
                ui,
                "captions-overlay",
                default,
                overlays::drag_area(video_rect),
                &mut self.learning.media.caption_position,
            );
            let offset = placed.min - default.min;
            let painter = ui.painter().with_clip_rect(video_rect);
            for (bounds, texture) in textures {
                painter.image(
                    texture.id(),
                    Rect::from_min_max(position(bounds.min), position(bounds.max))
                        .translate(offset),
                    Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)),
                    WHITE,
                );
            }
            return;
        }
        self.bitmap_captions = None;
        let Some(text) = self.player.caption_at(time) else {
            return;
        };
        let size = (rect.height() * 0.048).clamp(16.0, 40.0);
        let lines: Vec<_> = text
            .lines()
            .map(|line| {
                ui.painter().layout(
                    line.to_owned(),
                    FontId::proportional(size),
                    WHITE,
                    rect.width() * 0.85,
                )
            })
            .collect();
        if lines.is_empty() {
            return;
        }
        let width = lines.iter().map(|line| line.size().x).fold(0.0, f32::max) + 12.0;
        let height = lines.iter().map(|line| line.size().y + 3.0).sum::<f32>() - 1.0;
        let bottom = rect.bottom() - if controls { 92.0 } else { 22.0 };
        let default = Rect::from_min_size(
            pos2(rect.center().x - width * 0.5, bottom - height),
            vec2(width, height),
        );
        let placed = self.caption_drag.place(
            ui,
            "captions-overlay",
            default,
            overlays::drag_area(rect),
            &mut self.learning.media.caption_position,
        );
        let mut y = placed.top() + 1.0;
        let p = ui.painter();
        for layout in lines {
            let place = Rect::from_min_size(
                pos2(placed.center().x - layout.size().x * 0.5, y),
                layout.size(),
            );
            p.rect_filled(
                place.expand2(vec2(6.0, 1.0)),
                2.0,
                Color32::from_black_alpha(205),
            );
            p.galley(place.min, layout, WHITE);
            y = place.bottom() + 3.0;
        }
    }

    fn empty(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let center = rect.center();
        let button_rect = Rect::from_center_size(center - vec2(0.0, 16.0), vec2(64.0, 48.0));
        let response = ui
            .interact(
                button_rect,
                ui.scope_id().with("open-media"),
                Sense::click(),
            )
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        ui.painter().rect_filled(
            button_rect,
            13.0,
            if response.hovered() {
                Color32::from_rgb(227, 0, 43)
            } else {
                RED
            },
        );
        draw(ui.painter(), button_rect.center(), Icon::Play, WHITE);
        let label = Rect::from_center_size(center + vec2(0.0, 37.0), vec2(114.0, 28.0));
        if response.clicked()
            || ui
                .put(
                    label,
                    egui::Button::new(RichText::new("Open video").size(15.0)).frame(false),
                )
                .clicked()
        {
            self.picker(ui.ctx(), Picker::Media);
        }
    }

    fn title_bar(&mut self, ui: &mut egui::Ui, rect: Rect) {
        gradient(
            ui.painter(),
            Rect::from_min_max(rect.min, pos2(rect.right(), rect.top() + 73.0)),
            Color32::from_black_alpha(175),
            Color32::TRANSPARENT,
        );
        let right = rect.right() - 14.0;
        ui.painter().image(
            self.title_icon.id(),
            Rect::from_center_size(
                pos2(rect.left() + 30.0, rect.top() + 30.0),
                vec2(26.0, 26.0),
            ),
            Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)),
            Color32::WHITE,
        );
        let area = Rect::from_min_max(
            pos2(rect.left() + 48.0, rect.top() + 12.0),
            pos2(right - 84.0, rect.top() + 48.0),
        );
        ui.painter_at(area).text(
            area.left_center(),
            Align2::LEFT_CENTER,
            &self.title,
            FontId::proportional(18.0),
            WHITE,
        );
        let open = Rect::from_center_size(pos2(right - 57.0, rect.top() + 27.0), vec2(38.0, 34.0));
        if button(ui, open, "Open file (Ctrl+O)", Icon::Folder).clicked() {
            self.picker(ui.ctx(), Picker::Media);
        }
        let toggle =
            Rect::from_center_size(pos2(right - 18.0, rect.top() + 27.0), vec2(34.0, 34.0));
        let (label, icon) = if self.sidebar.open {
            ("Close library", Icon::PanelClose)
        } else {
            ("Open library", Icon::PanelOpen)
        };
        if named_button(ui, toggle, "library-toggle", label, icon).clicked() {
            self.sidebar.toggle_open();
            self.menu = None;
            ui.ctx().request_repaint();
        }
    }

    fn control_bar(&mut self, ui: &egui::Ui, rect: Rect, position: f64) {
        let left = rect.left() + 12.0;
        let right = rect.right() - 12.0;
        let y = rect.bottom() - 25.0;
        let step = (rect.width() / 7.2).clamp(35.0, 53.0);
        let p = ui.painter();
        gradient(
            p,
            Rect::from_min_max(pos2(rect.left(), rect.bottom() - 100.0), rect.max),
            Color32::TRANSPARENT,
            Color32::from_black_alpha(232),
        );
        let track = Rect::from_center_size(
            pos2(rect.center().x, rect.bottom() - SEEK_OFFSET),
            vec2(right - left, 5.0),
        );
        let duration = self.player.duration();
        let display_position = self
            .wheel_seek
            .as_ref()
            .map_or(position, |seek| seek.target);
        let (seek, seek_hover) = seekbar(ui, track, display_position, duration, &mut self.drag);
        self.learning.markers(ui, track, duration);
        if let Some(seek) = seek {
            self.drag = None;
            self.seek_to(seek);
        } else if self.drag.is_some() {
            self.wheel_seek = None;
        }
        if self.playback.show_ab {
            for (index, (label, hint, selected, enabled, action)) in [
                (
                    "A",
                    "Set A ( [ )",
                    self.learning.media.a.is_some(),
                    true,
                    Learn::Start,
                ),
                (
                    "B",
                    "Set B ( ] )",
                    self.learning.media.b.is_some(),
                    self.learning.media.a.is_some_and(|a| position - a >= 0.08),
                    Learn::End,
                ),
                (
                    "Loop",
                    "Toggle A–B (Shift+L)",
                    self.learning.ab,
                    self.learning.range().is_some(),
                    Learn::Toggle,
                ),
                (
                    "×",
                    "Clear A–B (Shift+R)",
                    false,
                    self.learning.media.a.is_some(),
                    Learn::Reset,
                ),
            ]
            .into_iter()
            .enumerate()
            {
                let center = pos2(right - 14.0 - (3 - index) as f32 * 30.0, track.top() - 16.0);
                let area = Rect::from_center_size(center, vec2(28.0, 22.0));
                if learning::repeat_button(ui, area, label, hint, selected, enabled) {
                    self.learning_action(ui.ctx(), action);
                }
            }
        }

        let btn = |x| Rect::from_center_size(pos2(x, y), vec2(step, 42.0));
        let (icon, label) = if self.player.ended() {
            (Icon::Replay, "Replay")
        } else if self.player.is_playing() {
            (Icon::Pause, "Pause (k)")
        } else {
            (Icon::Play, "Play (k)")
        };
        if named_button(ui, btn(left + step * 0.5), "play-pause", label, icon).clicked() {
            self.toggle_playback();
        }
        let volume = if self.player.muted {
            0.0
        } else {
            self.player.volume
        };
        let audio_icon = if volume == 0.0 {
            Icon::Mute
        } else if volume < 0.5 {
            Icon::Quiet
        } else {
            Icon::Sound
        };
        let label = if self.player.muted {
            "Unmute (m)"
        } else {
            "Mute (m)"
        };
        let sound = btn(left + step * 1.5);
        if button(ui, sound, label, audio_icon).clicked() {
            self.player.toggle_mute();
        }
        let inline_volume = rect.width() >= 535.0;
        let volume_origin = if inline_volume {
            pos2(sound.right() + 5.0, y)
        } else {
            pos2(left + 5.0, rect.bottom() - 100.0)
        };
        let slider = Rect::from_min_size(volume_origin + vec2(42.0, -1.5), vec2(54.0, 3.0));
        let limit = if volume >= 1.0 { MAX_VOLUME } else { 1.0 };
        let volume_rect = Rect::from_min_max(
            volume_origin - vec2(6.0, 14.0),
            slider.left_center() + vec2(slider.width() * limit + 10.0, 14.0),
        );
        let volume_id = ui.scope_id().with("volume");
        self.volume_open = ui.ctx().is_being_dragged(volume_id)
            || ui.input(|i| {
                i.pointer.hover_pos().is_some_and(|pos| {
                    sound.contains(pos)
                        || (self.volume_open && sound.union(volume_rect).contains(pos))
                })
            });
        if self.volume_open {
            if !inline_volume {
                p.rect_filled(volume_rect, 6.0, PANEL_BACKGROUND);
            }
            let wheel = if ui.input(|i| {
                i.pointer
                    .hover_pos()
                    .is_some_and(|pos| sound.contains(pos) || volume_rect.contains(pos))
            }) {
                wheel_step(ui.ctx())
            } else {
                0.0
            };
            let level =
                (((volume * 100.0).round() + VOLUME_STEP * wheel) / 100.0).clamp(0.0, MAX_VOLUME);
            let limit = if level >= 1.0 { MAX_VOLUME } else { limit };
            let adjusted = volume_slider(ui, slider, volume_id, level, limit).unwrap_or(level);
            if adjusted != volume {
                self.player.set_volume(adjusted);
                ui.ctx().request_repaint();
            }
        }

        let time_x = if self.volume_open && inline_volume {
            volume_rect.right() + 5.0
        } else {
            left + step * 2.0 + 4.0
        };
        let alarm = self
            .clock
            .alarm_countdown()
            .map(|remaining| format!("Alarm in {remaining}"))
            .or_else(|| {
                matches!(self.video_dialog, Some(dialogs::Dialog::Alarm)).then(|| "Alarm".into())
            });
        let time_end = right - step * if alarm.is_some() { 3.0 } else { 2.0 } - 3.0;
        if time_x < time_end {
            let current = timestamp(self.drag.unwrap_or(display_position));
            let full = duration.map_or_else(
                || current.clone(),
                |d| format!("{current} / {}", timestamp(d)),
            );
            let font = FontId::proportional(13.0);
            let mut label = p.layout_no_wrap(full, font.clone(), WHITE);
            if label.size().x > time_end - time_x {
                label = p.layout_no_wrap(current, font, WHITE);
            }
            if label.size().x <= time_end - time_x {
                p.galley(pos2(time_x, y - label.size().y * 0.5), label, WHITE);
            }
        }

        if let Some(label) = alarm {
            let alarm = btn(right - step * 2.5);
            if named_button(ui, alarm, "alarm", &label, Icon::Alarm).clicked() {
                self.time_action(ui.ctx(), TimeAction::Alarm);
            }
        }
        let settings = btn(right - step * 1.5);
        if button(ui, settings, "Settings", Icon::Settings).clicked() {
            self.menu = if self.menu.is_some() {
                None
            } else {
                Some(Menu::Settings)
            };
        }
        let full = if self.fullscreen {
            ("Exit full screen (f)", Icon::ExitFullscreen)
        } else {
            ("Full screen (f)", Icon::Fullscreen)
        };
        if button(ui, btn(right - step * 0.5), full.0, full.1).clicked() {
            self.fullscreen(ui.ctx(), !self.fullscreen);
        }
        if let (Some(time), Some(duration)) = (seek_hover, duration) {
            let key = self
                .player
                .info
                .as_ref()
                .and_then(|info| info.current_video);
            let image = self
                .path
                .as_ref()
                .filter(|_| key.is_some() && !self.player.is_loading() && self.wheel_seek.is_none())
                .and_then(|file| self.preview.get(ui.ctx(), file, key, time));
            let height = if image.is_some() { 121.0_f32 } else { 26.0 }
                .min(rect.height() - SEEK_OFFSET - 12.0);
            let width = if image.is_some() { 186.0 } else { 72.0 };
            let x = track.left() + track.width() * (time / duration).clamp(0.0, 1.0) as f32;
            let margin = width * 0.5;
            let center = x.clamp(rect.left() + margin, rect.right() - margin);
            let card = Rect::from_min_size(
                pos2(center - margin, track.top() - height - 8.0),
                vec2(width, height),
            );
            p.rect_filled(card, 6.0, PANEL_BACKGROUND);
            if let Some(image) = image {
                let thumb =
                    Rect::from_min_max(card.min + vec2(4.0, 4.0), card.max - vec2(4.0, 23.0));
                p.image(
                    image.id(),
                    fit(thumb, image.size_vec2()),
                    Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)),
                    WHITE,
                );
            }
            p.text(
                pos2(center, card.bottom() - 11.0),
                Align2::CENTER_CENTER,
                timestamp(time),
                FontId::proportional(13.0),
                WHITE,
            );
        } else {
            self.preview.cancel();
        }
    }

    fn settings(&mut self, ctx: &egui::Context, player_rect: Rect, menu: Menu) {
        let info = self.player.info.as_deref();
        let current_video = info.and_then(|i| i.current_video);
        let current_audio = info.and_then(|i| i.current_audio);
        let video_label = |t: &Track| {
            t.height
                .map_or_else(|| t.codec.to_uppercase(), |h| format!("{h}p"))
        };
        let audio_label = |t: &Track| {
            t.language.as_ref().map_or_else(
                || format!("Track {} · {}", t.index + 1, t.codec.to_uppercase()),
                |lang| format!("{lang} · {}", t.codec.to_uppercase()),
            )
        };
        let caption_label = |t: &Track| {
            t.language
                .clone()
                .unwrap_or_else(|| format!("Track {} · {}", t.index + 1, t.codec.to_uppercase()))
        };
        let video_tracks = info.map_or(&[][..], |i| i.video.as_slice());
        let audio_tracks = info.map_or(&[][..], |i| i.audio.as_slice());
        let text_tracks = info.map_or(&[][..], |i| i.captions.as_slice());
        let normal = |speed: f64| {
            if speed == 1.0 {
                "Normal".to_owned()
            } else {
                format!("{speed}×")
            }
        };
        let caption_value = match self.player.captions {
            Captions::Off => "Off".to_owned(),
            Captions::Sidecar => "Subtitle file".to_owned(),
            Captions::Track(index) => text_tracks
                .iter()
                .find(|t| t.index == index)
                .map_or("On".into(), caption_label),
        };
        let (title, rows) = match menu {
            Menu::Settings => {
                let mut rows = vec![
                    Row::page("Playback speed", normal(self.player.rate()), Menu::Speed),
                    Row::page("Subtitles", caption_value, Menu::Subtitles),
                    Row::toggle("A–B", self.playback.show_ab, RowAction::ToggleAb),
                    Row::toggle("Time", self.playback.show_clock, RowAction::ToggleClock),
                    Row::choice("Alarm".into(), false, RowAction::Alarm),
                ];
                if video_tracks.len() > 1 {
                    let current = video_tracks
                        .iter()
                        .find(|t| Some(t.index) == current_video)
                        .map_or(String::new(), video_label);
                    rows.push(Row::page("Video track", current, Menu::Video));
                }
                if audio_tracks.len() > 1 {
                    let current = audio_tracks
                        .iter()
                        .find(|t| Some(t.index) == current_audio)
                        .map_or(String::new(), audio_label);
                    rows.push(Row::page("Audio track", current, Menu::Audio));
                }
                rows.push(Row::toggle(
                    "Voice Boost",
                    self.player.voice_boost(),
                    RowAction::VoiceBoost,
                ));
                rows.push(Row::choice(
                    "Player settings".into(),
                    false,
                    RowAction::Settings(dialogs::SettingsTab::Controls),
                ));
                rows.push(Row::choice(
                    "About".into(),
                    false,
                    RowAction::Settings(dialogs::SettingsTab::About),
                ));
                ("", rows)
            }
            Menu::Speed => (
                "Playback speed",
                [0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 1.75, 2.0]
                    .into_iter()
                    .map(|s| Row::choice(normal(s), s == self.player.rate(), RowAction::Speed(s)))
                    .collect(),
            ),
            Menu::Video => (
                "Video track",
                video_tracks
                    .iter()
                    .map(|t| {
                        let label = format!(
                            "{} · {} · Track {}",
                            video_label(t),
                            t.codec.to_uppercase(),
                            t.index + 1
                        );
                        Row::choice(
                            label,
                            Some(t.index) == current_video,
                            RowAction::Video(t.index),
                        )
                    })
                    .collect(),
            ),
            Menu::Audio => (
                "Audio track",
                audio_tracks
                    .iter()
                    .map(|t| {
                        Row::choice(
                            audio_label(t),
                            Some(t.index) == current_audio,
                            RowAction::Audio(t.index),
                        )
                    })
                    .collect(),
            ),
            Menu::Subtitles => {
                let panel_label = if self.sidebar.subtitles_open() {
                    "Hide subtitle panel"
                } else {
                    "Show subtitle panel"
                };
                let mut rows = vec![
                    Row::choice(panel_label.into(), false, RowAction::ToggleSubtitlesPanel),
                    Row::choice(
                        "Hide subtitles on video".into(),
                        self.player.captions == Captions::Off,
                        RowAction::Captions(Captions::Off),
                    ),
                ];
                for track in text_tracks {
                    let mode = Captions::Track(track.index);
                    rows.push(Row::choice(
                        caption_label(track),
                        self.player.captions == mode,
                        RowAction::Captions(mode),
                    ));
                }
                if self.player.has_sidecar() {
                    rows.push(Row::choice(
                        "Subtitle file".into(),
                        self.player.captions == Captions::Sidecar,
                        RowAction::Captions(Captions::Sidecar),
                    ));
                }
                rows.push(Row::choice(
                    "Open subtitle file…".into(),
                    false,
                    RowAction::OpenCaptions,
                ));
                ("Subtitles", rows)
            }
        };

        let width = 271.0_f32.min(player_rect.width() - 20.0);
        let row_height = 36.0;
        let top = f32::from(!title.is_empty()) * 40.0;
        let max_height = (player_rect.height() - 82.0).max(64.0);
        let height = (top + row_height * rows.len() as f32 + 12.0).min(max_height);
        let origin = pos2(
            player_rect.right() - width - 15.0,
            player_rect.bottom() - height - 65.0,
        );
        let mut action = None;
        egui::Area::new(egui::Id::unique("player-settings"))
            .fixed_pos(origin)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::click());
                let p = ui.painter();
                p.rect_filled(rect, CornerRadius::same(12), PANEL_BACKGROUND);
                if !title.is_empty() {
                    let label = Rect::from_min_size(rect.min, vec2(width, top));
                    let resp = ui
                        .interact(label, ui.scope_id().with("back"), Sense::click())
                        .on_hover_cursor(egui::CursorIcon::PointingHand);
                    let p = ui.painter();
                    let x = label.left() + 20.0;
                    let y = label.center().y;
                    p.line_segment(
                        [pos2(x + 2.0, y - 5.0), pos2(x - 3.0, y)],
                        egui::Stroke::new(1.5, WHITE),
                    );
                    p.line_segment(
                        [pos2(x - 3.0, y), pos2(x + 2.0, y + 5.0)],
                        egui::Stroke::new(1.5, WHITE),
                    );
                    p.text(
                        pos2(x + 19.0, y),
                        Align2::LEFT_CENTER,
                        title,
                        FontId::proportional(14.5),
                        WHITE,
                    );
                    p.hline(
                        rect.left()..=rect.right(),
                        label.bottom(),
                        egui::Stroke::new(1.0, Color32::from_white_alpha(28)),
                    );
                    if resp.clicked() {
                        action = Some(RowAction::Menu(Menu::Settings));
                    }
                }
                let list_rect =
                    Rect::from_min_max(rect.min + vec2(0.0, top + 6.0), rect.max - vec2(0.0, 6.0));
                ui.scope_builder(egui::UiBuilder::new().max_rect(list_rect), |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    egui::ScrollArea::vertical()
                        .max_height(list_rect.height())
                        .show(ui, |ui| {
                            for row in rows {
                                let (r, resp) =
                                    ui.allocate_exact_size(vec2(width, row_height), Sense::click());
                                let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                                let p = ui.painter();
                                if resp.hovered() {
                                    p.rect_filled(r, 0.0, Color32::from_white_alpha(27));
                                }
                                let y = r.center().y;
                                let submenu = matches!(
                                    row.action,
                                    RowAction::Menu(_) | RowAction::Settings(_) | RowAction::Alarm
                                );
                                let subtitle_panel =
                                    matches!(row.action, RowAction::ToggleSubtitlesPanel);
                                let x = r.left()
                                    + if submenu || subtitle_panel || !row.value.is_empty() {
                                        14.0
                                    } else {
                                        38.0
                                    };
                                if row.active {
                                    let x = r.left() + 18.0;
                                    p.line_segment(
                                        [pos2(x - 4.0, y), pos2(x - 0.5, y + 3.0)],
                                        egui::Stroke::new(1.6, WHITE),
                                    );
                                    p.line_segment(
                                        [pos2(x - 0.5, y + 3.0), pos2(x + 5.0, y - 4.0)],
                                        egui::Stroke::new(1.6, WHITE),
                                    );
                                }
                                p.text(
                                    pos2(x, y),
                                    Align2::LEFT_CENTER,
                                    row.label,
                                    FontId::proportional(14.0),
                                    WHITE,
                                );
                                p.text(
                                    pos2(r.right() - 30.0, y),
                                    Align2::RIGHT_CENTER,
                                    row.value,
                                    FontId::proportional(13.0),
                                    Color32::from_gray(205),
                                );
                                if submenu {
                                    draw(p, pos2(r.right() - 16.0, y), Icon::Arrow, WHITE);
                                } else if subtitle_panel {
                                    let icon = if self.sidebar.subtitles_open() {
                                        Icon::PanelClose
                                    } else {
                                        Icon::PanelOpen
                                    };
                                    draw(p, pos2(r.right() - 16.0, y), icon, WHITE);
                                }
                                if resp.clicked() {
                                    action = Some(row.action);
                                }
                            }
                        });
                });
            });
        if let Some(selected) = action {
            self.menu = None;
            match selected {
                RowAction::Menu(page) => self.menu = Some(page),
                RowAction::Settings(tab) => {
                    self.video_dialog = Some(dialogs::Dialog::Settings(tab))
                }
                RowAction::Speed(rate) if rate != self.player.rate() => self.player.set_rate(rate),
                RowAction::ToggleAb => {
                    self.playback.show_ab = !self.playback.show_ab;
                    self.save_playback_preferences();
                }
                RowAction::ToggleClock => {
                    self.time_action(ctx, TimeAction::Visibility(!self.playback.show_clock))
                }
                RowAction::Alarm => self.time_action(ctx, TimeAction::Alarm),
                RowAction::Video(id) => {
                    self.learning.media.video = Some(id);
                    self.save_media_preferences();
                    if Some(id) != current_video {
                        self.player.select_video(id);
                    }
                }
                RowAction::Audio(id) => {
                    self.learning.media.audio = Some(id);
                    self.save_media_preferences();
                    if Some(id) != current_audio {
                        self.player.select_audio(id);
                    }
                }
                RowAction::Captions(Captions::Off) => {
                    self.playback.captions = false;
                    self.update_captions();
                }
                RowAction::Captions(caption) => {
                    self.playback.captions = true;
                    self.learning_action(ctx, Learn::Track(caption));
                }
                RowAction::OpenCaptions => self.picker(ctx, Picker::Captions),
                RowAction::ToggleSubtitlesPanel => self.toggle_transcript(ctx),
                RowAction::VoiceBoost => self.player.set_voice_boost(!self.player.voice_boost()),
                _ => {}
            }
        }
    }
}

fn bitmap_textures(ui: &egui::Ui, frame: &BitmapFrame) -> Vec<(Rect, egui::TextureHandle)> {
    let canvas = vec2(f32::from(frame.width), f32::from(frame.height));
    if canvas.x == 0.0 || canvas.y == 0.0 {
        return Vec::new();
    }
    frame
        .compositions
        .iter()
        .filter_map(|sprite| {
            let size = [usize::from(sprite.width), usize::from(sprite.height)];
            if size.contains(&0)
                || sprite.rgba.len() != size[0] * size[1] * 4
                || size
                    .iter()
                    .any(|&length| length > ui.input(|i| i.max_texture_side))
            {
                return None;
            }
            let position = vec2(f32::from(sprite.x), f32::from(sprite.y)) / canvas;
            let size_uv = vec2(f32::from(sprite.width), f32::from(sprite.height)) / canvas;
            Some((
                Rect::from_min_size(position.to_pos2(), size_uv),
                ui.ctx().load_texture(
                    "bitmap subtitle",
                    egui::ColorImage::from_rgba_unmultiplied(size, &sprite.rgba),
                    egui::TextureOptions::LINEAR,
                ),
            ))
        })
        .collect()
}

fn wheel_step(ctx: &egui::Context) -> f32 {
    ctx.input_mut(|input| {
        let mut steps = 0.0_f32;
        input.events.retain(|event| {
            if let egui::Event::MouseWheel { delta, .. } = event
                && delta.y != 0.0
            {
                steps += delta.y.signum();
                false
            } else {
                true
            }
        });
        steps.clamp(-1.0, 1.0)
    })
}

fn fit(rect: Rect, size: Vec2) -> Rect {
    Rect::from_center_size(
        rect.center(),
        size * (rect.width() / size.x).min(rect.height() / size.y),
    )
}

fn readable_file(path: &Path) -> bool {
    path.is_file() && File::open(path).is_ok()
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some((kind, dialog)) = &self.dialog {
            match dialog.try_recv() {
                Ok(files) => {
                    let kind = *kind;
                    self.dialog = None;
                    self.load_files(ctx, files, matches!(kind, Picker::Playlist));
                }
                Err(mpsc::TryRecvError::Disconnected) => self.dialog = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        let dropped = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_owned())
                .collect::<Vec<_>>()
        });
        if !dropped.is_empty() {
            let (_, panel) = Layout::regions(ctx.content_rect(), self.sidebar.open);
            let append_only = self.sidebar.accepts_files()
                && panel
                    .zip(self.layout.pointer(ctx))
                    .is_some_and(|(panel, point)| panel.contains(point));
            self.load_files(ctx, dropped, append_only);
        }
        let was_playing = self.player.is_playing();
        self.keyboard(ctx);
        if let Some(seek) = &self.wheel_seek {
            if let Some(wait) = seek.due.checked_duration_since(Instant::now()) {
                ctx.request_repaint_after(wait);
            } else {
                self.seek_to(seek.target);
            }
        }
        if let Some(picture) = self.player.tick() {
            self.picture = Some(Arc::new(picture.picture));
            self.serial = self.serial.wrapping_add(1);
        }
        if self
            .clock
            .tick(ctx, self.playback.show_clock && self.player.has_source())
        {
            if self.player.is_playing() {
                self.toggle_playback();
            }
        } else if self.player.is_playing() {
            self.clock.elapsed = None;
        }
        self.update_captions();
        self.save_playback_preferences();
        if !self.history_saved
            || (self.picture.is_some()
                && !self.history_cover
                && self.cover_attempt.elapsed() >= Duration::from_millis(600))
            || (was_playing && !self.player.is_playing())
            || (self.player.is_playing() && self.saved_at.elapsed() >= Duration::from_secs(5))
        {
            self.save_history(true);
        }
        if self.player.ended()
            && self.wheel_seek.is_none()
            && self.clock.elapsed.is_none()
            && let Some(queue) = self.queued.take()
        {
            match self.sidebar.next(queue) {
                Ok(Some((path, item))) => self.open(ctx, path, None, Some(item), false),
                Ok(None) => {}
                Err(err) => self.sidebar.error = Some(format!("{err:#}")),
            }
        }
        if let Some(delay) = self.player.repaint_after() {
            ctx.request_repaint_after(delay);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let workspace = ui.max_rect();
        let aspect = self.picture_aspect();
        let show_panel = self.layout.update(
            &ctx,
            frame.winit_window().map(Arc::as_ref),
            workspace.size(),
            self.sidebar.open,
            aspect,
            self.fullscreen,
        );
        let (screen, panel) = Layout::regions(workspace, show_panel);
        if let Some(panel) = panel {
            let mut drop_hovered = false;
            if self.sidebar.accepts_files() && ui.input(|input| !input.raw.hovered_files.is_empty())
            {
                drop_hovered = self
                    .layout
                    .pointer(&ctx)
                    .is_some_and(|point| panel.contains(point));
                ctx.request_repaint_after(Duration::from_millis(50));
            }
            if let Some(action) = self.sidebar.show(
                ui,
                panel,
                self.path.as_deref().map(|path| (path, self.queued)),
                self.playback.show_clock,
                drop_hovered,
                |ui| {
                    self.learning.show(
                        ui,
                        &self.player,
                        self.selected_caption,
                        self.playback.captions,
                        &mut self.playback.follow,
                    )
                },
            ) {
                match action {
                    Action::Open(path, queue) => self.open(&ctx, path, None, queue, false),
                    Action::AddFiles => self.picker(&ctx, Picker::Playlist),
                    Action::Forget(path) if self.path.as_ref() == Some(&path) => {
                        self.history_forgotten = true
                    }
                    Action::Forget(_) => {}
                    Action::ForgetAll => self.history_forgotten = true,
                    Action::Subtitle(command) => self.learning_action(&ctx, command),
                    Action::Time(action) => self.time_action(&ctx, action),
                }
            }
        }
        ui.set_clip_rect(screen.intersect(ui.clip_rect()));
        let (pointer, activity) = ui.input(|i| {
            (
                i.pointer.hover_pos(),
                i.pointer.any_pressed()
                    || i.events.iter().any(|event| {
                        matches!(
                            event,
                            egui::Event::Key { pressed: true, .. } | egui::Event::MouseWheel { .. }
                        )
                    }),
            )
        });
        if self.pointer != pointer || activity {
            self.last_input = Instant::now();
        }
        self.pointer = pointer;

        let show_controls = !self.player.is_playing()
            || self.player.is_loading()
            || self.menu.is_some()
            || matches!(self.video_dialog, Some(dialogs::Dialog::Alarm))
            || self.drag.is_some()
            || self.volume_open
            || self.last_input.elapsed() < Duration::from_millis(2700)
            || pointer
                .is_some_and(|p| screen.contains(p) && p.y > screen.bottom() - SEEK_OFFSET - 16.0);
        if !show_controls && self.fullscreen {
            ctx.set_cursor_icon(egui::CursorIcon::None);
        }

        ui.painter().rect_filled(screen, 0.0, Color32::BLACK);
        let video_rect = aspect.map(|aspect| {
            Rect::from_center_size(screen.center(), layout::fit_size(screen.size(), aspect))
        });
        if let Some(picture) = &self.picture
            && let Some(rect) = video_rect
        {
            video::paint(
                ui.painter(),
                rect,
                self.renderer.clone(),
                picture.clone(),
                self.serial,
            );
        }

        let video_hit = Rect::from_min_max(
            screen.min + vec2(0.0, 48.0),
            screen.max - vec2(0.0, SEEK_OFFSET + 13.0),
        );
        let response = ui.interact(
            video_hit,
            ui.scope_id().with("video-surface"),
            Sense::click(),
        );
        self.video_context_menu(&response);
        if !self.player.has_source() {
            self.empty(ui, screen);
            self.title_bar(ui, screen);
            self.show_video_dialog(&ctx);
            return;
        }

        if response.double_clicked() {
            let first = self.last_video_click.take();
            if first != Some(self.mouse.double) {
                // Single clicks act immediately. Reverse their toggle if a second click follows.
                if let Some(first) = first {
                    self.video_click(&ctx, first);
                    if first == ClickAction::Playback {
                        self.playback_cue = None;
                    }
                }
                self.video_click(&ctx, self.mouse.double);
            }
        } else if response.clicked() {
            self.last_video_click = None;
            if self.menu.take().is_none() {
                self.video_click(&ctx, self.mouse.single);
                self.last_video_click = Some(self.mouse.single);
            }
        }

        let position = self.player.position();
        let gpu_error = self.renderer.lock().unwrap().error.clone();
        if let Some(err) = self.player.error.as_ref().or(gpu_error.as_ref()) {
            let galley = ui.painter().layout(
                err.to_owned(),
                FontId::proportional(16.0),
                WHITE,
                screen.width() * 0.7,
            );
            let box_rect = Rect::from_center_size(screen.center(), galley.size());
            ui.painter()
                .rect_filled(box_rect.expand(18.0), 10.0, Color32::from_black_alpha(230));
            ui.painter().galley(box_rect.min, galley, WHITE);
        } else if self.player.is_loading() && self.picture.is_none() {
            ui.put(
                Rect::from_center_size(screen.center(), vec2(40.0, 40.0)),
                egui::Spinner::new().size(36.0),
            );
        }
        if show_controls {
            self.title_bar(ui, screen);
            self.control_bar(ui, screen, position);
            if let Some(notice) = &self.player.notice {
                let at = Rect::from_min_max(
                    pos2(screen.left() + 18.0, screen.bottom() - 94.0),
                    pos2(screen.right() - 20.0, screen.bottom() - 72.0),
                );
                ui.put(
                    at,
                    egui::Label::new(RichText::new(notice).size(12.0)).truncate(),
                );
            }
        }
        self.paint_captions(ui, screen, video_rect, show_controls, position);
        if self.playback.show_clock {
            self.clock.paint(
                ui,
                screen,
                self.playback.clock_size,
                &mut self.clock_drag,
                &mut self.learning.media.clock_position,
            );
        }
        self.clock.paint_alarm(ui, video_rect.unwrap_or(screen));
        if !ui.input(|i| i.pointer.primary_down()) {
            self.finish_overlay_drag();
        }
        // The volume control gets first use of the wheel in its own hover area.
        if response.hovered()
            && self.menu.is_none()
            && self.video_dialog.is_none()
            && self.dialog.is_none()
            && self.drag.is_none()
            && !egui::Popup::is_any_open(&ctx)
        {
            let steps = wheel_step(&ctx);
            if steps != 0.0 {
                self.queue_wheel_seek(&ctx, steps);
            }
        }
        if !show_controls {
            self.preview.cancel();
        }
        if let Some(menu) = self.menu {
            self.settings(&ctx, screen, menu);
        }
        if self
            .playback_cue
            .as_ref()
            .is_some_and(|cue| !cue.paint(ui, screen))
        {
            self.playback_cue = None;
        }
        self.show_video_dialog(&ctx);
    }

    fn on_exit(&mut self, gl: Option<&eframe::glow::Context>) {
        self.finish_overlay_drag();
        self.save_playback_preferences();
        self.save_history(true);
        if let Some(gl) = gl {
            self.renderer.lock().unwrap().destroy(gl);
        }
    }
}
