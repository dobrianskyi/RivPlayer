use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use eframe::egui::{ColorImage, Context, TextureHandle, TextureOptions};
use oxideav_core::{ColorRange, PixelFormat};
use rivplayer::{
    engine::{self, Picture},
    source::LocalFile,
};

const MAX_CACHED: usize = 48;
const SAMPLE_INTERVAL: Duration = Duration::from_millis(48);
const RETARGET_AFTER: Duration = Duration::from_millis(160);

#[derive(Clone, PartialEq, Eq)]
struct Key {
    file: PathBuf,
    track: Option<u32>,
    tenth: u64,
}

impl Key {
    fn same_track(&self, other: &Self) -> bool {
        self.file == other.file && self.track == other.track
    }
}

#[derive(Clone)]
struct Request {
    key: Key,
    cancelled: Arc<AtomicBool>,
    started: Instant,
}

pub struct Preview {
    tx: Sender<Request>,
    pending: Receiver<Request>,
    result: Receiver<(Request, Option<ColorImage>)>,
    cache: VecDeque<(Key, Option<TextureHandle>)>,
    requested: Option<Request>,
    next_sample: Option<Instant>,
}

impl Preview {
    pub fn new(ctx: Context) -> Self {
        let (tx, pending) = bounded::<Request>(1);
        let worker = pending.clone();
        let (ready, result) = unbounded();
        thread::spawn(move || {
            // The decoder can create worker threads. Lower this job's
            // scheduler priority so they leave playback frames time to run.
            // SAFETY: who=0 changes only the calling Linux thread.
            unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 8) };
            while let Ok(request) = worker.recv() {
                let Request { key, cancelled, .. } = &request;
                if cancelled.load(Ordering::Relaxed) {
                    continue;
                }
                let frame = engine::preview(
                    &LocalFile(key.file.clone()),
                    key.tenth as f64 / 10.0,
                    key.track,
                    cancelled,
                )
                .ok()
                .flatten()
                .and_then(|frame| thumbnail(&frame.picture));
                if !cancelled.load(Ordering::Relaxed) {
                    if ready.send((request, frame)).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            }
        });
        Self {
            tx,
            pending,
            result,
            cache: VecDeque::new(),
            requested: None,
            next_sample: None,
        }
    }

    pub fn cancel(&mut self) {
        self.next_sample = None;
        self.cancel_request();
    }

    fn cancel_request(&mut self) {
        if let Some(request) = self.requested.take() {
            request.cancelled.store(true, Ordering::Relaxed);
        }
        let _ = self.pending.try_recv();
    }

    pub fn invalidate(&mut self, path: &Path) {
        self.cancel();
        self.cache.retain(|(key, _)| key.file != path);
        while self.result.try_recv().is_ok() {}
    }

    pub fn get(
        &mut self,
        ctx: &Context,
        file: &Path,
        track: Option<u32>,
        time: f64,
        tolerance: f64,
    ) -> Option<TextureHandle> {
        let key = Key {
            file: file.to_owned(),
            track,
            tenth: (time * 10.0).floor() as u64,
        };
        if self
            .requested
            .as_ref()
            .is_some_and(|request| !request.key.same_track(&key))
        {
            self.cancel();
        }
        while let Ok((finished, frame)) = self.result.try_recv() {
            if self
                .requested
                .as_ref()
                .is_some_and(|request| Arc::ptr_eq(&request.cancelled, &finished.cancelled))
            {
                self.requested = None;
                let texture = frame
                    .map(|image| ctx.load_texture("seek-frame", image, TextureOptions::LINEAR));
                self.cache.push_front((finished.key, texture));
                self.cache.truncate(MAX_CACHED);
            }
        }
        if let Some(position) = self.cache.iter().position(|(cached, _)| *cached == key) {
            let cached = self.cache.remove(position).unwrap();
            let texture = cached.1.clone();
            self.cache.push_front(cached);
            return texture;
        }
        let max_gap = (tolerance * 10.0).ceil() as u64;
        if let Some(request) = &self.requested
            && request.key.tenth.abs_diff(key.tenth) > max_gap
        {
            if let Some(wait) = RETARGET_AFTER.checked_sub(request.started.elapsed()) {
                ctx.request_repaint_after(wait);
            } else {
                self.cancel_request();
            }
        }
        if self.requested.is_none() {
            let now = Instant::now();
            if let Some(wait) = self
                .next_sample
                .and_then(|at| at.checked_duration_since(now))
            {
                ctx.request_repaint_after(wait);
            } else {
                let request = Request {
                    key: key.clone(),
                    cancelled: Arc::new(AtomicBool::new(false)),
                    started: now,
                };
                if self.tx.try_send(request.clone()).is_ok() {
                    self.requested = Some(request);
                    self.next_sample = Some(now + SAMPLE_INTERVAL);
                }
            }
        }
        self.cache
            .iter()
            .filter_map(|(cached, texture)| {
                let gap = cached.tenth.abs_diff(key.tenth);
                if cached.same_track(&key) && gap <= max_gap {
                    texture.as_ref().map(|texture| (gap, texture))
                } else {
                    None
                }
            })
            .min_by_key(|(gap, _)| *gap)
            .map(|(_, texture)| texture.clone())
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub fn thumbnail(picture: &Picture) -> Option<ColorImage> {
    let format = picture.format;
    let (width, height) = (picture.width, picture.height);
    if width == 0 || height == 0 {
        return None;
    }
    let scale = (176.0 / f64::from(width))
        .min(100.0 / f64::from(height))
        .min(1.0);
    let w = (f64::from(width) * scale).round().max(1.0) as usize;
    let h = (f64::from(height) * scale).round().max(1.0) as usize;
    let planes = picture.frame.image_planes();
    let sample = |plane: usize, x: u32, y: u32, channel: usize| -> Option<f32> {
        let data = planes.get(plane)?;
        let (pw, ph) = format.plane_dimensions(plane, width, height)?;
        let cx = (u64::from(x) * u64::from(pw) / u64::from(width)) as usize;
        let cy = (u64::from(y) * u64::from(ph) / u64::from(height)) as usize;
        let packed = matches!(
            format,
            PixelFormat::Rgb24
                | PixelFormat::Rgba
                | PixelFormat::Bgr24
                | PixelFormat::Bgra
                | PixelFormat::Nv12
                | PixelFormat::Nv21
        );
        let bytes = if picture.p010 { 2 } else { 1 };
        let bpp = bytes * format.plane_row_bytes(plane, width)? / pw as usize;
        let at = cy
            .checked_mul(data.stride)?
            .checked_add(cx * bpp + channel * bytes)?;
        if picture.p010 {
            let packed = u16::from_le_bytes([*data.data.get(at)?, *data.data.get(at + 1)?]);
            return Some(f32::from(packed >> 6) * 255.0 / 1023.0);
        }
        if !packed && bpp == 2 {
            let depth = picture
                .frame
                .plane_significant_bits(plane)
                .unwrap_or_else(|| {
                    use PixelFormat::*;
                    match format {
                        Gray10Le | Yuv420P10Le | Yuv422P10Le | Yuv444P10Le | Yuv440P10Le => 10,
                        Gray12Le | Yuv420P12Le | Yuv422P12Le | Yuv444P12Le | Yuv440P12Le => 12,
                        _ => 16,
                    }
                })
                .clamp(8, 16);
            let value = u16::from_le_bytes([*data.data.get(at)?, *data.data.get(at + 1)?]);
            return Some(f32::from(value) * 255.0 / ((1_u32 << depth) - 1) as f32);
        }
        Some(f32::from(*data.data.get(at)?))
    };
    let mut rgb = Vec::with_capacity(w * h * 3);
    let full = picture.color.range == ColorRange::Full
        || format.implied_color_range() == Some(ColorRange::Full);
    let hd = picture.color.matrix.0 == 1 || (picture.color.matrix.0 == 2 && height >= 720);
    for row in 0..h {
        for col in 0..w {
            let x = (((col as f64 + 0.5) * f64::from(width)) / w as f64) as u32;
            let y = (((row as f64 + 0.5) * f64::from(height)) / h as f64) as u32;
            use PixelFormat::*;
            let get = |plane, channel| sample(plane, x, y, channel);
            let color = match format {
                Rgb24 | Rgba => [get(0, 0)?, get(0, 1)?, get(0, 2)?],
                Bgr24 | Bgra => [get(0, 2)?, get(0, 1)?, get(0, 0)?],
                _ => {
                    let luma = get(0, 0)?;
                    let (u, v) = match format {
                        Nv12 => (get(1, 0)?, get(1, 1)?),
                        Nv21 => (get(1, 1)?, get(1, 0)?),
                        _ if planes.len() >= 3 => (get(1, 0)?, get(2, 0)?),
                        _ => (128.0, 128.0),
                    };
                    let (l, u, v) = if full {
                        (luma, u - 128.0, v - 128.0)
                    } else {
                        (
                            (luma - 16.0) * 255.0 / 219.0,
                            (u - 128.0) * 255.0 / 224.0,
                            (v - 128.0) * 255.0 / 224.0,
                        )
                    };
                    if hd {
                        [l + 1.575 * v, l - 0.187 * u - 0.468 * v, l + 1.856 * u]
                    } else {
                        [l + 1.402 * v, l - 0.344 * u - 0.714 * v, l + 1.772 * u]
                    }
                }
            };
            rgb.extend(color.map(|v| v.clamp(0.0, 255.0) as u8));
        }
    }
    Some(ColorImage::from_rgb([w, h], &rgb))
}

pub fn encode(image: &ColorImage) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(2 + image.pixels.len() * 3);
    bytes.extend([image.size[0] as u8, image.size[1] as u8]);
    for pixel in &image.pixels {
        bytes.extend([pixel.r(), pixel.g(), pixel.b()]);
    }
    bytes
}

pub fn decode(data: &[u8]) -> Option<ColorImage> {
    let (&w, data) = data.split_first()?;
    let (&h, data) = data.split_first()?;
    let size = [usize::from(w), usize::from(h)];
    (w > 0 && h > 0 && data.len() == size[0] * size[1] * 3)
        .then(|| ColorImage::from_rgb(size, data))
}

pub fn has_detail(image: &ColorImage) -> bool {
    let Some(reference) = image.pixels.get(image.pixels.len() / 3) else {
        return false;
    };
    image.pixels.iter().any(|pixel| {
        pixel.r().abs_diff(reference.r()) > 12
            || pixel.g().abs_diff(reference.g()) > 12
            || pixel.b().abs_diff(reference.b()) > 12
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview_without_worker() -> (Preview, Sender<(Request, Option<ColorImage>)>) {
        let (tx, pending) = bounded(1);
        let (ready, result) = unbounded();
        (
            Preview {
                tx,
                pending,
                result,
                cache: VecDeque::new(),
                requested: None,
                next_sample: None,
            },
            ready,
        )
    }

    #[test]
    fn scrub_starts_immediately_and_requests_the_latest_position_after_completion() {
        let (mut preview, ready) = preview_without_worker();
        let ctx = Context::default();
        let file = Path::new("sample.mp4");
        assert!(preview.get(&ctx, file, Some(0), 1.0, 0.4).is_none());
        let first = preview
            .pending
            .try_recv()
            .expect("the first request starts immediately");
        assert!(preview.get(&ctx, file, Some(0), 1.1, 0.4).is_none());
        assert!(!first.cancelled.load(Ordering::Relaxed));
        assert!(preview.pending.is_empty());
        ready
            .send((
                first,
                Some(ColorImage::filled([2, 2], eframe::egui::Color32::WHITE)),
            ))
            .unwrap();
        preview.next_sample = None;
        assert!(preview.get(&ctx, file, Some(0), 1.2, 0.4).is_some());
        assert_eq!(preview.pending.try_recv().unwrap().key.tenth, 12);
        assert!(preview.get(&ctx, file, Some(1), 1.2, 0.4).is_none());
    }

    #[test]
    fn cancelled_completion_is_ignored_even_when_requesting_the_same_frame_again() {
        let (mut preview, ready) = preview_without_worker();
        let ctx = Context::default();
        let file = Path::new("sample.mp4");
        preview.get(&ctx, file, Some(0), 1.0, 0.4);
        let cancelled = preview.pending.try_recv().unwrap();
        preview.cancel();
        preview.get(&ctx, file, Some(0), 1.0, 0.4);
        let current = preview.pending.try_recv().unwrap();
        ready.send((cancelled, None)).unwrap();
        preview.get(&ctx, file, Some(0), 1.0, 0.4);
        assert!(preview.cache.is_empty());
        assert!(Arc::ptr_eq(
            &preview.requested.as_ref().unwrap().cancelled,
            &current.cancelled
        ));
    }
}
