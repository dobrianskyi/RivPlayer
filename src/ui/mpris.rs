use std::{
    collections::HashMap,
    ffi::OsString,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::Instant,
};

use eframe::egui;
use percent_encoding::percent_decode_str;
use zbus::object_server::SignalEmitter;
use zbus::{ObjectPath, OwnedObjectPath, Variant, connection::Builder, fdo};

use super::file_manager::file_uri;

const BUS_NAME: &str = "org.mpris.MediaPlayer2.rivplayer";
const OBJECT: &str = "/org/mpris/MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
pub const MIN_RATE: f64 = 0.25;
pub const MAX_RATE: f64 = 2.0;

type Properties = HashMap<&'static str, Variant<'static>>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Status {
    Playing,
    Paused,
    #[default]
    Stopped,
}

impl Status {
    fn name(self) -> &'static str {
        match self {
            Self::Playing => "Playing",
            Self::Paused => "Paused",
            Self::Stopped => "Stopped",
        }
    }
}

pub enum Command {
    Raise,
    Quit,
    Next,
    Previous,
    Pause,
    PlayPause,
    Stop,
    Play,
    Seek(f64),
    SetPosition(OwnedObjectPath, f64),
    Open(PathBuf),
    Rate(f64, async_channel::Sender<()>),
    Volume(f64, async_channel::Sender<()>),
}

#[derive(Clone, Copy)]
pub struct Playback {
    pub status: Status,
    pub position: f64,
    pub advancing: bool,
    pub rate: f64,
    pub volume: f64,
    pub next: bool,
    pub previous: bool,
    pub can_seek: bool,
    pub can_play: bool,
}

struct Track {
    generation: u64,
    title: String,
    uri: String,
    duration: Option<i64>,
}

struct State {
    track: Option<Track>,
    playback: Playback,
    sampled_at: Instant,
}

impl State {
    fn metadata(&self) -> Properties {
        let Some(track) = &self.track else {
            return HashMap::new();
        };
        let mut data = HashMap::from([
            ("mpris:trackid", Variant::new(track_id(track.generation))),
            ("xesam:title", Variant::new(track.title.clone())),
            ("xesam:url", Variant::new(track.uri.clone())),
        ]);
        if let Some(length) = track.duration {
            data.insert("mpris:length", Variant::I64(length));
        }
        data
    }

    fn position(&self, now: Instant) -> f64 {
        let mut position = self.playback.position;
        if self.playback.advancing {
            position += now.duration_since(self.sampled_at).as_secs_f64() * self.playback.rate;
        }
        if let Some(end) = self.track.as_ref().and_then(|track| track.duration) {
            position = position.min(seconds(end));
        }
        position.max(0.0)
    }
}

pub struct MediaControls {
    state: Arc<Mutex<State>>,
    changes: async_channel::Sender<(Properties, Option<i64>)>,
    pub commands: crossbeam_channel::Receiver<Command>,
}

impl MediaControls {
    pub fn start(ctx: egui::Context) -> Self {
        let state = Arc::new(Mutex::new(State {
            track: None,
            playback: Playback {
                status: Status::Stopped,
                position: 0.0,
                advancing: false,
                rate: 1.0,
                volume: 1.0,
                next: false,
                previous: false,
                can_seek: false,
                can_play: false,
            },
            sampled_at: Instant::now(),
        }));
        let (changes, updates) = async_channel::unbounded();
        let (send, commands) = crossbeam_channel::bounded(32);
        let root = Root { ctx, send };
        let player = Transport {
            root: root.clone(),
            state: state.clone(),
        };
        let _ = thread::Builder::new().name("mpris".into()).spawn(move || {
            // Playback also works without a session bus.
            let _ = zbus::block_on(serve(root, player, updates));
        });
        Self {
            state,
            changes,
            commands,
        }
    }

    pub fn update(
        &self,
        media: Option<(u64, &Path, &str)>,
        duration: Option<f64>,
        playback: Playback,
        seeked: bool,
    ) {
        if self.changes.is_closed() {
            return;
        }
        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        let metadata = state.track.as_ref().map(|t| t.generation) != media.map(|m| m.0);
        let duration = duration.map(micros);
        let length_changed = state.track.as_ref().is_some_and(|t| t.duration != duration);
        let position_changed = state.track.is_some()
            && !metadata
            && (seeked
                || (state.playback.advancing
                    && playback.advancing
                    && (playback.position - state.position(now)).abs() > 0.15));
        if metadata {
            state.track = media.map(|(generation, path, title)| Track {
                generation,
                title: title.to_owned(),
                uri: file_uri(path),
                duration,
            });
        } else if let Some(track) = &mut state.track {
            track.duration = duration;
        }

        let prev = std::mem::replace(&mut state.playback, playback);
        state.sampled_at = now;
        let mut props = HashMap::new();
        if metadata || length_changed {
            props.insert("Metadata", Variant::new(state.metadata()));
        }
        if prev.status != playback.status {
            props.insert("PlaybackStatus", Variant::new(playback.status.name()));
        }
        if prev.rate != playback.rate {
            props.insert("Rate", Variant::F64(playback.rate));
        }
        if prev.volume != playback.volume {
            props.insert("Volume", Variant::F64(playback.volume));
        }
        if prev.next != playback.next {
            props.insert("CanGoNext", Variant::Bool(playback.next));
        }
        if prev.previous != playback.previous {
            props.insert("CanGoPrevious", Variant::Bool(playback.previous));
        }
        if prev.can_play != playback.can_play {
            props.insert("CanPlay", Variant::Bool(playback.can_play));
            props.insert("CanPause", Variant::Bool(playback.can_play));
        }
        if prev.can_seek != playback.can_seek {
            props.insert("CanSeek", Variant::Bool(playback.can_seek));
        }
        if position_changed || !props.is_empty() {
            let position = position_changed.then(|| micros(state.position(now)));
            let _ = self.changes.try_send((props, position));
        }
    }
}

async fn serve(
    root: Root,
    player: Transport,
    changes: async_channel::Receiver<(Properties, Option<i64>)>,
) -> zbus::Result<()> {
    let conn = Builder::session()
        .serve_at(OBJECT, root)
        .serve_at(OBJECT, player)
        .build()
        .await?;
    match conn.request_name(BUS_NAME).await {
        Ok(()) => {}
        Err(zbus::Error::NameTaken) => {
            conn.request_name(format!("{BUS_NAME}.instance{}", std::process::id()))
                .await?;
        }
        Err(err) => return Err(err),
    }
    let transport = conn
        .object_server()
        .interface::<_, Transport>(OBJECT)
        .await?;
    while let Ok((properties, position)) = changes.recv().await {
        if !properties.is_empty() {
            conn.emit_signal(
                None::<&str>,
                OBJECT,
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                &(PLAYER, properties, Vec::<&str>::new()),
            )
            .await?;
        }
        if let Some(position) = position {
            Transport::seeked(transport.signal_emitter(), position).await?;
        }
    }
    Ok(())
}

pub fn track_id(generation: u64) -> ObjectPath<'static> {
    ObjectPath::try_from(format!("/rivplayer/track/{generation}")).expect("valid track path")
}

fn micros(seconds: f64) -> i64 {
    (seconds.max(0.0) * 1_000_000.0).round() as i64
}

fn seconds(micros: i64) -> f64 {
    micros as f64 / 1_000_000.0
}

fn local_path(uri: &str) -> fdo::Result<PathBuf> {
    let path = uri
        .strip_prefix("file://")
        .ok_or_else(|| fdo::Error::NotSupported("Only local file URLs are supported".into()))?;
    let local = path.strip_prefix("localhost").unwrap_or(path);
    if !local.starts_with('/') || local.contains(['?', '#']) {
        return Err(fdo::Error::InvalidArgs(
            "Expected an absolute local file URL".into(),
        ));
    }
    let path = percent_decode_str(local).collect::<Vec<_>>();
    if path.contains(&0) {
        return Err(fdo::Error::InvalidArgs("NUL in file URL".into()));
    }
    Ok(PathBuf::from(OsString::from_vec(path)))
}

#[derive(Clone)]
struct Root {
    ctx: egui::Context,
    send: crossbeam_channel::Sender<Command>,
}

impl Root {
    fn dispatch(&self, command: Command) -> fdo::Result<()> {
        self.send
            .try_send(command)
            .map_err(|_| fdo::Error::Failed("Player is busy".into()))?;
        self.ctx.request_repaint();
        Ok(())
    }
}

#[zbus::interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    fn raise(&self) -> fdo::Result<()> {
        self.dispatch(Command::Raise)
    }
    fn quit(&self) -> fdo::Result<()> {
        self.dispatch(Command::Quit)
    }
    #[zbus(property)]
    fn can_quit(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_raise(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn identity(&self) -> &str {
        "RivPlayer"
    }
    #[zbus(property)]
    fn desktop_entry(&self) -> &str {
        "rivplayer"
    }
    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<&str> {
        vec!["file"]
    }
    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<&str> {
        vec![
            "video/mp4",
            "video/x-matroska",
            "video/webm",
            "video/quicktime",
            "video/x-msvideo",
            "video/mp2t",
            "video/mpeg",
            "video/ogg",
            "video/x-flv",
            "video/3gpp",
            "audio/flac",
            "audio/mpeg",
            "audio/mp4",
            "audio/ogg",
            "audio/ac3",
            "audio/eac3",
        ]
    }
}

struct Transport {
    root: Root,
    state: Arc<Mutex<State>>,
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl Transport {
    #[zbus(signal)]
    async fn seeked(emitter: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;

    fn next(&self) -> fdo::Result<()> {
        self.root.dispatch(Command::Next)
    }
    fn previous(&self) -> fdo::Result<()> {
        self.root.dispatch(Command::Previous)
    }
    fn pause(&self) -> fdo::Result<()> {
        self.root.dispatch(Command::Pause)
    }
    fn play_pause(&self) -> fdo::Result<()> {
        self.root.dispatch(Command::PlayPause)
    }
    fn stop(&self) -> fdo::Result<()> {
        self.root.dispatch(Command::Stop)
    }
    fn play(&self) -> fdo::Result<()> {
        self.root.dispatch(Command::Play)
    }
    fn seek(&self, offset: i64) -> fdo::Result<()> {
        self.root.dispatch(Command::Seek(seconds(offset)))
    }
    fn set_position(&self, track: OwnedObjectPath, position: i64) -> fdo::Result<()> {
        self.root
            .dispatch(Command::SetPosition(track, seconds(position)))
    }
    fn open_uri(&self, uri: &str) -> fdo::Result<()> {
        self.root.dispatch(Command::Open(local_path(uri)?))
    }
    #[zbus(property)]
    fn playback_status(&self) -> &str {
        self.state.lock().unwrap().playback.status.name()
    }
    #[zbus(property)]
    fn metadata(&self) -> Properties {
        self.state.lock().unwrap().metadata()
    }
    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> i64 {
        micros(self.state.lock().unwrap().position(Instant::now()))
    }
    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        self.state.lock().unwrap().playback.next
    }
    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        self.state.lock().unwrap().playback.previous
    }
    #[zbus(property)]
    fn can_play(&self) -> bool {
        self.state.lock().unwrap().playback.can_play
    }
    #[zbus(property)]
    fn can_pause(&self) -> bool {
        self.state.lock().unwrap().playback.can_play
    }
    #[zbus(property)]
    fn can_seek(&self) -> bool {
        self.state.lock().unwrap().playback.can_seek
    }
    #[zbus(property(emits_changed_signal = "false"))]
    fn can_control(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn loop_status(&self) -> &str {
        "None"
    }
    #[zbus(property)]
    fn set_loop_status(&self, status: &str) -> fdo::Result<()> {
        if status == "None" {
            Ok(())
        } else {
            Err(fdo::Error::NotSupported(
                "Track and playlist repeat are unavailable".into(),
            ))
        }
    }
    #[zbus(property)]
    fn shuffle(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn set_shuffle(&self, enabled: bool) -> fdo::Result<()> {
        if enabled {
            Err(fdo::Error::NotSupported(
                "Playlist shuffle is unavailable".into(),
            ))
        } else {
            Ok(())
        }
    }
    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        MIN_RATE
    }
    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        MAX_RATE
    }
    #[zbus(property)]
    fn rate(&self) -> f64 {
        self.state.lock().unwrap().playback.rate
    }
    #[zbus(property)]
    async fn set_rate(&self, rate: f64) -> fdo::Result<()> {
        if rate == 0.0 {
            return self.root.dispatch(Command::Pause);
        }
        if !(MIN_RATE..=MAX_RATE).contains(&rate) {
            return Err(fdo::Error::InvalidArgs("Unsupported playback rate".into()));
        }
        let (reply, applied) = async_channel::bounded(1);
        self.root.dispatch(Command::Rate(rate, reply))?;
        applied
            .recv()
            .await
            .map_err(|_| fdo::Error::Failed("Player stopped".into()))
    }
    #[zbus(property)]
    fn volume(&self) -> f64 {
        self.state.lock().unwrap().playback.volume
    }
    #[zbus(property)]
    async fn set_volume(&self, volume: f64) -> fdo::Result<()> {
        if !volume.is_finite() {
            return Err(fdo::Error::InvalidArgs("Volume must be finite".into()));
        }
        let (reply, applied) = async_channel::bounded(1);
        self.root.dispatch(Command::Volume(volume, reply))?;
        applied
            .recv()
            .await
            .map_err(|_| fdo::Error::Failed("Player stopped".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_decodes_local_file_urls_and_round_trips_native_paths() {
        let path = PathBuf::from(OsString::from_vec(b"/tmp/p 100% #\xff.mp4".to_vec()));
        assert_eq!(local_path(&file_uri(&path)).unwrap(), path);
        assert_eq!(
            local_path("file://localhost/tmp/movie.mp4").unwrap(),
            PathBuf::from("/tmp/movie.mp4")
        );
        assert!(local_path("file://server/tmp/movie.mp4").is_err());
        assert!(local_path("file:///tmp/%00").is_err());
        assert!(local_path("https://example.com/movie.mp4").is_err());
    }
}
