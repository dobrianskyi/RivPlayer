use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, bounded, unbounded};

use crate::source::MediaSource;

use super::{
    AudioChunk, BitmapFrame, Cue, DecodeMode, Event, HardwareError, Info, Options, Sidecar,
    TimedPicture,
    captions::{load_sidecar, transcript},
    output::{AudioOutput, Clock, LoopCut, MAX_VOLUME},
    pipeline,
    stretch::Stretch,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Captions {
    Off,
    Sidecar,
    Track(u32),
}

type Transcript = HashMap<u32, Vec<Cue>>;

struct TranscriptLoad {
    cancel: Arc<AtomicBool>,
    receive: Receiver<anyhow::Result<Transcript>>,
}

impl Drop for TranscriptLoad {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Repeat {
    pub start: f64,
    pub end: f64,
}

impl Repeat {
    pub fn new(start: f64, end: f64) -> Option<Self> {
        (start.is_finite() && end.is_finite() && start >= 0.0 && end - start >= 0.08)
            .then_some(Self { start, end })
    }
}

struct RepeatPreroll {
    session: Session,
    armed: bool,
}

struct Session {
    frames: Receiver<TimedPicture>,
    audio: Option<Receiver<AudioChunk>>,
    events: Receiver<Notice>,
    cancel: Arc<AtomicBool>,
}

enum Notice {
    Pipeline(Event),
    Error(String, bool),
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Session {
    fn spawn(
        source: Arc<dyn MediaSource>,
        options: Options,
        rate: f64,
        load_sidecar: bool,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> std::io::Result<Self> {
        let (frame_tx, frames) = bounded(5);
        let (audio_tx, audio) = bounded(24);
        let (tx, events) = unbounded();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        thread::Builder::new()
            .name("media-decoder".into())
            .spawn(move || {
                let wake = || wake();
                let mut stretch: Option<Stretch> = None;
                let mut output_error = None;
                let mut audio_connected = true;
                let mut send_audio = |mut chunk| {
                    while audio_connected && !worker_cancel.load(Ordering::Relaxed) {
                        match audio_tx.send_timeout(chunk, Duration::from_millis(25)) {
                            Ok(()) => break,
                            Err(crossbeam_channel::SendTimeoutError::Timeout(returned)) => {
                                chunk = returned
                            }
                            Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => {
                                audio_connected = false;
                                break;
                            }
                        }
                    }
                };
                let result = pipeline::run_session(
                    source.as_ref(),
                    options,
                    &worker_cancel,
                    load_sidecar,
                    |event| {
                        match event {
                            Event::Video(picture) => {
                                if frame_tx.send(picture).is_err() {
                                    return false;
                                }
                                if frame_tx.len() == 1 {
                                    wake();
                                }
                            }
                            Event::Audio(chunk) if rate == 1.0 => send_audio(chunk),
                            Event::Audio(chunk) => {
                                if stretch.as_ref().is_some_and(|s| !s.contiguous(&chunk)) {
                                    let mut old = stretch.take().unwrap();
                                    old.drain();
                                    while let Some(output) = old.pull() {
                                        send_audio(output);
                                    }
                                }
                                let dsp = match stretch.as_mut() {
                                    Some(dsp) => dsp,
                                    None => match Stretch::new(&chunk, rate) {
                                        Ok(dsp) => stretch.insert(dsp),
                                        Err(err) => {
                                            output_error = Some(err);
                                            return false;
                                        }
                                    },
                                };
                                dsp.push(&chunk);
                                while let Some(output) = dsp.pull() {
                                    send_audio(output);
                                }
                            }
                            event => {
                                if matches!(event, Event::End(_))
                                    && let Some(mut dsp) = stretch.take()
                                {
                                    dsp.drain();
                                    while let Some(output) = dsp.pull() {
                                        send_audio(output);
                                    }
                                }
                                if tx.send(Notice::Pipeline(event)).is_err() {
                                    return false;
                                }
                                wake();
                            }
                        }
                        true
                    },
                );
                if let Some(err) = output_error.or_else(|| result.err())
                    && !worker_cancel.load(Ordering::Relaxed)
                {
                    let hardware = err.is::<HardwareError>();
                    let _ = tx.send(Notice::Error(format!("{err:#}"), hardware));
                    wake();
                }
            })?;
        Ok(Self {
            frames,
            audio: Some(audio),
            events,
            cancel,
        })
    }
}

/// Owns playback state and schedules video against the same monotonic clock
/// used at the audio device's predicted presentation time.
pub struct Player {
    wake: Arc<dyn Fn() + Send + Sync>,
    source: Option<Arc<dyn MediaSource>>,
    session: Option<Session>,
    audio: Option<AudioOutput>,
    retiring_audio: Option<Receiver<()>>,
    clock: Arc<Mutex<Clock>>,
    options: Options,
    user_mode: DecodeMode,
    start_pending: Option<Instant>,
    wants_play: bool,
    preroll: bool,
    decoded_end: Option<f64>,
    reached_end: bool,
    next_frame: Option<TimedPicture>,
    embedded: HashMap<u32, VecDeque<Cue>>,
    sidecar: Option<Sidecar>,
    pending_sidecar: Option<Receiver<anyhow::Result<(PathBuf, Sidecar)>>>,
    sidecar_path: Option<PathBuf>,
    transcript: Option<Transcript>,
    transcript_load: Option<TranscriptLoad>,
    transcript_origin: Option<f64>,
    pub transcript_error: Option<String>,
    repeat: Option<Repeat>,
    repeat_preroll: Option<RepeatPreroll>,
    repeat_epoch: u64,
    external: bool,
    pub captions: Captions,
    pub info: Option<Box<Info>>,
    pub error: Option<String>,
    pub notice: Option<String>,
    pub volume: f32,
    pub muted: bool,
}

impl Player {
    pub fn new(wake: impl Fn() + Send + Sync + 'static, mode: DecodeMode) -> Self {
        Self {
            wake: Arc::new(wake),
            source: None,
            session: None,
            audio: None,
            retiring_audio: None,
            clock: Arc::default(),
            options: Options::default(),
            user_mode: mode,
            start_pending: None,
            wants_play: false,
            preroll: false,
            decoded_end: None,
            reached_end: false,
            next_frame: None,
            embedded: HashMap::new(),
            sidecar: None,
            pending_sidecar: None,
            sidecar_path: None,
            transcript: None,
            transcript_load: None,
            transcript_origin: None,
            transcript_error: None,
            repeat: None,
            repeat_preroll: None,
            repeat_epoch: 0,
            external: false,
            captions: Captions::Off,
            info: None,
            error: None,
            notice: None,
            volume: 1.0,
            muted: false,
        }
    }

    pub fn has_source(&self) -> bool {
        self.source.is_some()
    }
    pub fn position(&self) -> f64 {
        self.clock.lock().unwrap().position_at(Instant::now())
    }
    pub fn duration(&self) -> Option<f64> {
        self.decoded_end
            .or_else(|| self.info.as_ref().and_then(|i| i.duration))
    }
    pub fn is_playing(&self) -> bool {
        self.wants_play
    }
    pub fn is_loading(&self) -> bool {
        self.preroll && self.error.is_none()
    }
    pub fn ended(&self) -> bool {
        self.reached_end
    }
    pub fn rate(&self) -> f64 {
        self.clock.lock().unwrap().rate
    }

    pub fn open(&mut self, source: Arc<dyn MediaSource>, position: f64) {
        self.open_with_tracks(source, position, true, None, None);
    }

    pub fn open_paused(&mut self, source: Arc<dyn MediaSource>, position: f64) {
        self.open_with_tracks(source, position, false, None, None);
    }

    pub fn open_with_tracks(
        &mut self,
        source: Arc<dyn MediaSource>,
        position: f64,
        playing: bool,
        video: Option<u32>,
        audio: Option<u32>,
    ) {
        self.source = Some(source);
        self.info = None;
        self.notice = None;
        self.sidecar = None;
        self.pending_sidecar = None;
        self.sidecar_path = None;
        self.transcript = None;
        self.transcript_load = None;
        self.transcript_origin = None;
        self.transcript_error = None;
        self.set_repeat(None);
        self.external = false;
        self.captions = Captions::Off;
        self.options = Options {
            position,
            video,
            audio,
            mode: self.user_mode,
            track_fallback: true,
        };
        self.wants_play = playing;
        self.restart(position);
    }

    fn restart(&mut self, position: f64) {
        self.cancel_repeat_preroll();
        self.freeze();
        self.session = None;
        self.retire_audio();
        self.options.position = position;
        self.decoded_end = None;
        self.reached_end = false;
        self.next_frame = None;
        self.embedded.clear();
        self.error = None;
        let rate = {
            let mut clock = self.clock.lock().unwrap();
            clock.position = position;
            clock.start = None;
            clock.rate
        };
        if let Some(source) = &self.source {
            let load_sidecar = self.sidecar.is_none() && self.pending_sidecar.is_none();
            match Session::spawn(
                source.clone(),
                self.options,
                rate,
                load_sidecar,
                self.wake.clone(),
            ) {
                Ok(session) => {
                    self.session = Some(session);
                    self.start_pending = Some(Instant::now());
                    self.preroll = true;
                }
                Err(err) => self.fail(format!("Cannot start playback: {err}")),
            }
        }
    }

    pub fn seek(&mut self, position: f64) {
        if self.has_source() {
            let end = self.duration().unwrap_or(f64::MAX);
            self.restart(position.clamp(0.0, end));
        }
    }

    pub fn play_from(&mut self, position: f64) {
        if self.has_source() {
            self.wants_play = true;
            self.seek(position);
        }
    }

    pub fn repeat(&self) -> Option<Repeat> {
        self.repeat
    }

    pub fn set_repeat(&mut self, repeat: Option<Repeat>) {
        if self.repeat != repeat {
            self.cancel_repeat_preroll();
            self.repeat = repeat;
        }
    }

    fn cancel_repeat_preroll(&mut self) {
        if self.repeat_preroll.take().is_some() {
            self.clock.lock().unwrap().cut = None;
            if let Some(audio) = &self.audio {
                audio.clear_staged();
            }
        }
    }

    fn tick_repeat(&mut self) {
        let Some(range) = self.repeat else { return };
        if !self.wants_play || self.preroll || self.error.is_some() {
            return;
        }
        let Some(info) = &self.info else { return };
        let clock = *self.clock.lock().unwrap();
        let Some(anchor) = clock.start else { return };
        let now = Instant::now();
        let position = clock.position_at(now);
        if position < range.end - 2.0 * clock.rate {
            return;
        }
        if self.repeat_preroll.is_none()
            && position < range.end
            && let Some(source) = &self.source
            && let Ok(session) = Session::spawn(
                source.clone(),
                Options {
                    position: range.start,
                    ..self.options
                },
                clock.rate,
                false,
                self.wake.clone(),
            )
        {
            self.repeat_preroll = Some(RepeatPreroll {
                session,
                armed: false,
            });
        }
        if let Some(next) = &mut self.repeat_preroll
            && !next.armed
            && (info.current_video.is_none() || !next.session.frames.is_empty())
            && (self.audio.is_none()
                || next
                    .session
                    .audio
                    .as_ref()
                    .is_some_and(|audio| !audio.is_empty()))
        {
            self.repeat_epoch = self.repeat_epoch.wrapping_add(1);
            if let Some(audio) = &self.audio
                && let Some(queue) = next.session.audio.take()
            {
                audio.stage(self.repeat_epoch, queue);
            }
            self.clock.lock().unwrap().cut = Some(LoopCut {
                start: range.start,
                end: range.end,
                epoch: self.repeat_epoch,
            });
            next.armed = true;
        }
        if position < range.end && !self.decoded_end.is_some_and(|end| position >= end) {
            return;
        }
        let Some(next) = self.repeat_preroll.take().filter(|next| next.armed) else {
            self.restart(range.start);
            return;
        };
        // Keep the audio stream and the original wall-clock boundary.
        let boundary =
            anchor + Duration::from_secs_f64(((range.end - clock.position) / clock.rate).max(0.0));
        {
            let mut clock = self.clock.lock().unwrap();
            clock.cut = None;
            clock.epoch = self.repeat_epoch;
            clock.position = range.start;
            clock.start = Some(boundary.min(now));
        }
        self.session = Some(next.session);
        self.options.position = range.start;
        self.next_frame = None;
        self.decoded_end = None;
        self.embedded.clear();
    }

    pub fn play_pause(&mut self) {
        if !self.has_source() {
            return;
        }
        if self.reached_end || self.error.is_some() {
            self.wants_play = true;
            self.seek(if self.reached_end {
                0.0
            } else {
                self.position()
            });
            return;
        }
        if self.wants_play {
            self.wants_play = false;
            self.cancel_repeat_preroll();
            self.freeze();
            self.retire_audio();
            self.session = None;
            self.next_frame = None;
            self.start_pending = None;
            self.preroll = false;
        } else {
            self.wants_play = true;
            self.restart(self.position());
        }
    }

    fn freeze(&self) {
        let mut clock = self.clock.lock().unwrap();
        clock.position = clock.position_at(Instant::now());
        clock.start = None;
    }

    fn retire_audio(&mut self) {
        if let Some(audio) = self.audio.take() {
            audio.pause();
            let (sent, done) = bounded(1);
            let wake = self.wake.clone();
            thread::spawn(move || {
                drop(audio);
                let _ = sent.send(());
                wake();
            });
            self.retiring_audio = Some(done);
        }
    }

    pub fn set_rate(&mut self, rate: f64) {
        let position = self.position();
        self.clock.lock().unwrap().rate = rate;
        if self.has_source() {
            self.restart(position);
        }
    }

    pub fn select_video(&mut self, index: u32) {
        self.options.video = Some(index);
        self.options.mode = self.user_mode;
        self.restart(self.position());
    }

    pub fn select_audio(&mut self, index: u32) {
        self.options.audio = Some(index);
        self.restart(self.position());
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, MAX_VOLUME);
        self.muted = self.volume == 0.0;
        self.apply_volume();
    }

    pub fn voice_boost(&self) -> bool {
        self.clock.lock().unwrap().voice
    }

    pub fn set_voice_boost(&mut self, enabled: bool) {
        self.clock.lock().unwrap().voice = enabled;
    }

    pub fn restore_volume(&mut self, volume: f32, muted: bool) {
        self.volume = volume.clamp(0.0, MAX_VOLUME);
        self.muted = muted || self.volume == 0.0;
        self.apply_volume();
    }

    pub fn toggle_mute(&mut self) {
        self.muted = !self.muted;
        if !self.muted && self.volume == 0.0 {
            self.volume = 0.5;
        }
        self.apply_volume();
    }

    fn apply_volume(&self) {
        self.clock.lock().unwrap().volume = if self.muted { 0.0 } else { self.volume };
    }

    pub fn load_captions(&mut self, path: &Path) {
        let path = path.to_owned();
        let (send, receive) = bounded(1);
        let wake = self.wake.clone();
        self.pending_sidecar = Some(receive);
        thread::spawn(move || {
            let _ = send.send(load_sidecar(&path).map(|cues| (path, cues)));
            wake();
        });
    }

    pub fn has_sidecar(&self) -> bool {
        self.sidecar.is_some()
    }

    pub fn sidecar_path(&self) -> Option<&Path> {
        self.sidecar_path.as_deref()
    }

    pub fn read_transcript(&mut self) {
        let Some((source, info)) = self.source.as_ref().zip(self.info.as_ref()) else {
            return;
        };
        if info.captions.is_empty() || self.transcript_origin == Some(info.start) {
            return;
        }
        self.transcript_origin = Some(info.start);
        self.transcript = None;
        self.transcript_error = None;
        let origin = info.start;
        let (send, receive) = bounded(1);
        let cancel = Arc::new(AtomicBool::new(false));
        self.transcript_load = Some(TranscriptLoad {
            cancel: cancel.clone(),
            receive,
        });
        let wake = self.wake.clone();
        let source = source.clone();
        thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                transcript(source.as_ref(), origin, &cancel)
            }))
            .unwrap_or_else(|_| Err(anyhow::anyhow!("Cannot index subtitles in this file")));
            let _ = send.send(result);
            wake();
        });
    }

    pub fn transcript_loading(&self) -> bool {
        self.transcript_load.is_some()
    }

    pub fn cues(&self, track: Captions) -> &[Cue] {
        match track {
            Captions::Track(id) => self
                .transcript
                .as_ref()
                .and_then(|tracks| tracks.get(&id))
                .map_or(&[], Vec::as_slice),
            Captions::Sidecar => self.sidecar.as_ref().map_or(&[], Sidecar::cues),
            Captions::Off => &[],
        }
    }

    pub fn toggle_captions(&mut self) {
        self.captions = if self.captions != Captions::Off {
            Captions::Off
        } else if self.has_sidecar() {
            Captions::Sidecar
        } else {
            self.info
                .as_ref()
                .and_then(|i| i.captions.first())
                .map_or(Captions::Off, |t| Captions::Track(t.index))
        };
    }

    pub fn caption_at(&self, time: f64) -> Option<String> {
        let mut active = Vec::new();
        if self.captions == Captions::Sidecar
            && let Some(Sidecar::Text(cues)) = &self.sidecar
        {
            let until = cues.partition_point(|cue| cue.start <= time);
            active.extend(
                cues[..until]
                    .iter()
                    .filter(|c| c.end > time)
                    .map(|cue| cue.text.as_str()),
            );
        } else if let Captions::Track(index) = self.captions {
            if let Some(cues) = self
                .transcript
                .as_ref()
                .and_then(|tracks| tracks.get(&index))
            {
                let until = cues.partition_point(|cue| cue.start <= time);
                active.extend(
                    cues[..until]
                        .iter()
                        .filter(|cue| cue.end > time)
                        .map(|cue| cue.text.as_str()),
                );
            } else if let Some(cues) = self.embedded.get(&index) {
                active.extend(
                    cues.iter()
                        .filter(|cue| cue.start <= time && cue.end > time)
                        .map(|cue| cue.text.as_str()),
                );
            }
        }
        (!active.is_empty()).then(|| active.join("\n"))
    }

    pub fn bitmap_at(&mut self, time: f64) -> Option<Arc<BitmapFrame>> {
        if self.captions == Captions::Sidecar
            && let Some(Sidecar::Bitmap(track)) = &mut self.sidecar
        {
            track.frame_at(time)
        } else {
            None
        }
    }

    fn fail(&mut self, message: String) {
        self.cancel_repeat_preroll();
        self.error = Some(message);
        self.wants_play = false;
        self.start_pending = None;
        self.preroll = false;
        self.freeze();
        self.retire_audio();
        self.session = None;
    }

    pub fn tick(&mut self) -> Option<TimedPicture> {
        if let Some(load) = &self.transcript_load {
            match load.receive.try_recv() {
                Ok(Ok(tracks)) => {
                    self.transcript = Some(tracks);
                    self.transcript_load = None;
                }
                Ok(Err(err)) => {
                    self.transcript_error = Some(format!("{err:#}"));
                    self.transcript_load = None;
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => self.transcript_load = None,
                Err(crossbeam_channel::TryRecvError::Empty) => {}
            }
        }
        if let Some(pending) = &self.pending_sidecar {
            match pending.try_recv() {
                Ok(Ok((path, sidecar))) => {
                    self.sidecar = Some(sidecar);
                    self.sidecar_path = Some(path);
                    self.external = true;
                    self.captions = Captions::Sidecar;
                    self.pending_sidecar = None;
                }
                Ok(Err(err)) => {
                    self.notice = Some(format!("{err:#}"));
                    self.pending_sidecar = None;
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => self.pending_sidecar = None,
                Err(crossbeam_channel::TryRecvError::Empty) => {}
            }
        }
        if self
            .audio
            .as_ref()
            .is_some_and(|audio| audio.failed.load(Ordering::Relaxed))
        {
            self.notice = Some("Audio device stopped".to_owned());
            self.audio = None;
        }
        if let Some(retiring) = &self.retiring_audio {
            if matches!(
                retiring.try_recv(),
                Err(crossbeam_channel::TryRecvError::Empty)
            ) {
                return None;
            }
            self.retiring_audio = None;
        }
        self.tick_repeat();
        while let Some(event) = self.session.as_ref().and_then(|s| s.events.try_recv().ok()) {
            match event {
                Notice::Pipeline(Event::Info(info)) => {
                    self.options.video = info.current_video;
                    self.options.audio = info.current_audio;
                    self.options.track_fallback = false;
                    let rx = self.session.as_mut().and_then(|s| s.audio.take());
                    if self.wants_play
                        && info.current_audio.is_some()
                        && let Some(rx) = rx
                    {
                        match AudioOutput::open(rx, self.clock.clone()) {
                            Ok(audio) => self.audio = Some(audio),
                            Err(error) => self.notice = Some(format!("Audio output: {error:#}")),
                        }
                    }
                    self.info = Some(info);
                }
                Notice::Pipeline(Event::Caption(id, cue)) => {
                    let time = self.position();
                    let cues = self.embedded.entry(id).or_default();
                    cues.retain(|cue| cue.end > time);
                    if cues.len() >= 256 {
                        cues.pop_front();
                    }
                    cues.push_back(cue);
                }
                Notice::Pipeline(Event::Sidecar(sidecar))
                    if !self.external && self.sidecar.is_none() =>
                {
                    self.sidecar = Some(sidecar);
                }
                Notice::Pipeline(Event::Notice(message)) => self.notice = Some(message),
                Notice::Pipeline(Event::End(end)) => self.decoded_end = Some(end),
                Notice::Pipeline(_) => {}
                Notice::Error(message, true) if self.options.mode == DecodeMode::Auto => {
                    let position = self.position();
                    self.notice = Some(format!("{message}. Switching to software decoding."));
                    self.options.mode = DecodeMode::Software;
                    self.restart(position);
                    return None;
                }
                Notice::Error(message, _) => {
                    self.fail(message);
                    return None;
                }
            }
        }
        let session = self.session.as_ref()?;
        let mut time = self.position();
        if self.next_frame.is_none() {
            self.next_frame = session.frames.try_recv().ok();
        }
        let mut frame = None;
        let audio_only = self
            .info
            .as_ref()
            .is_some_and(|i| i.current_video.is_none());
        let audio_queued = self.audio.as_ref().map_or(0, AudioOutput::queued);
        if let Some(started) = self.start_pending
            && (self.next_frame.is_some()
                || self.decoded_end.is_some()
                || (audio_only && audio_queued >= 2)
                || audio_queued >= 24
                || (audio_only
                    && self.audio.is_none()
                    && started.elapsed() >= Duration::from_millis(120)))
        {
            self.start_pending = None;
            self.preroll = false;
            if self.wants_play {
                let mut clock = self.clock.lock().unwrap();
                clock.start = Some(Instant::now() + Duration::from_millis(35));
                time = clock.position;
            } else {
                frame = self.next_frame.take();
            }
        }
        while self
            .next_frame
            .as_ref()
            .is_some_and(|p| self.wants_play && p.time <= time + 0.006)
        {
            frame = self.next_frame.take();
            self.next_frame = session.frames.try_recv().ok();
        }
        if let Some(end) = self.decoded_end
            && time >= end
            && self.next_frame.is_none()
        {
            self.wants_play = false;
            self.reached_end = true;
            self.retire_audio();
            let mut clock = self.clock.lock().unwrap();
            clock.start = None;
            clock.position = end.max(0.0);
        }
        frame
    }

    pub fn repaint_after(&self) -> Option<Duration> {
        if self.preroll {
            return Some(Duration::from_millis(25));
        }
        if !self.wants_play {
            return None;
        }
        let seconds = self
            .next_frame
            .as_ref()
            .map_or(0.04, |f| (f.time - self.position() - 0.012) / self.rate());
        let seconds = self.repeat.map_or(seconds, |range| {
            seconds.min((range.end - self.position()) / self.rate())
        });
        Some(Duration::from_secs_f64(seconds.clamp(0.002, 0.04)))
    }
}
