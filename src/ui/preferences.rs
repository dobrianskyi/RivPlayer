#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClickAction {
    Nothing = 0,
    Playback = 1,
    Fullscreen = 2,
}

impl ClickAction {
    pub const CHOICES: [Self; 3] = [Self::Nothing, Self::Playback, Self::Fullscreen];

    pub fn label(self) -> &'static str {
        match self {
            Self::Nothing => "Do nothing",
            Self::Playback => "Play / Pause",
            Self::Fullscreen => "Toggle fullscreen",
        }
    }
}

pub const WHEEL_SEEK_RANGE: std::ops::RangeInclusive<u8> = 1..=120;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MouseSettings {
    pub single: ClickAction,
    pub double: ClickAction,
    pub wheel_seconds: u8,
}

impl Default for MouseSettings {
    fn default() -> Self {
        Self {
            single: ClickAction::Playback,
            double: ClickAction::Fullscreen,
            wheel_seconds: 5,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    History = 0,
    Playlist = 1,
    Favorites = 2,
    Subtitles = 3,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SidebarView {
    pub open: bool,
    pub tab: Tab,
    pub playlist: i64,
}
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClockSize {
    Small = 0,
    #[default]
    Medium = 1,
    Large = 2,
}

impl ClockSize {
    pub const CHOICES: [Self; 3] = [Self::Small, Self::Medium, Self::Large];

    pub fn label(self) -> &'static str {
        match self {
            Self::Small => "Small",
            Self::Medium => "Medium",
            Self::Large => "Large",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverlayPosition {
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlaybackSettings {
    pub rate: f64,
    pub volume: f32,
    pub muted: bool,
    pub voice: bool,
    pub captions: bool,
    pub follow: bool,
    pub show_ab: bool,
    pub show_clock: bool,
    pub clock_size: ClockSize,
}

impl Default for PlaybackSettings {
    fn default() -> Self {
        Self {
            rate: 1.0,
            volume: 1.0,
            muted: false,
            voice: false,
            captions: false,
            follow: true,
            show_ab: true,
            show_clock: false,
            clock_size: ClockSize::default(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SubtitleChoice {
    #[default]
    Auto,
    Embedded(u32),
    Sidecar(Option<PathBuf>),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MediaSettings {
    pub subtitles: SubtitleChoice,
    pub video: Option<u32>,
    pub audio: Option<u32>,
    pub a: Option<f64>,
    pub b: Option<f64>,
    pub clock_position: Option<OverlayPosition>,
    pub caption_position: Option<OverlayPosition>,
}
