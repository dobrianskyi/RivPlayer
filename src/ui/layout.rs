use std::time::{Duration, Instant};

use eframe::egui::{self, Pos2, Rect, Vec2, pos2, vec2};
use winit::{
    dpi::PhysicalPosition,
    raw_window_handle::{HasWindowHandle, RawWindowHandle},
    window::Window,
};
use x11rb::{
    connection::Connection,
    protocol::xproto::{AtomEnum, ConnectionExt},
    rust_connection::RustConnection,
};

pub const INITIAL_SIZE: [f32; 2] = [960.0, 600.0];
pub const MIN_VIDEO: [f32; 2] = [250.0, 170.0];
const PANEL_WIDTH: f32 = 306.0;

pub struct Layout {
    envelope: Vec2,
    video: Vec2,
    aspect: Option<f32>,
    panel: bool,
    presented: bool,
    constrained: bool,
    dirty: bool,
    observed: Option<Vec2>,
    requested: Option<(Vec2, Instant)>,
    x11: Option<X11>,
}

impl Layout {
    pub fn new(window: Option<&Window>) -> Self {
        Self {
            envelope: Vec2::from(INITIAL_SIZE),
            video: Vec2::from(INITIAL_SIZE),
            aspect: None,
            panel: false,
            presented: false,
            constrained: false,
            dirty: false,
            observed: None,
            requested: None,
            x11: window.and_then(X11::open),
        }
    }

    pub fn new_file(&mut self) {
        self.aspect = None;
    }

    pub fn regions(rect: Rect, panel: bool) -> (Rect, Option<Rect>) {
        if !panel {
            return (rect, None);
        }
        let edge = rect.right() - sidebar_width(rect.width());
        (
            Rect::from_min_max(rect.min, pos2(edge, rect.bottom())),
            Some(Rect::from_min_max(pos2(edge, rect.top()), rect.max)),
        )
    }

    pub fn pointer(&self, ctx: &egui::Context) -> Option<Pos2> {
        if let Some(x11) = &self.x11 {
            // During an external Xdnd grab Winit gets the drop but no CursorMoved.
            return x11.pointer(ctx.pixels_per_point());
        }
        ctx.input(|input| input.pointer.latest_pos())
    }

    pub fn update(
        &mut self,
        ctx: &egui::Context,
        window: Option<&Window>,
        size: Vec2,
        panel: bool,
        aspect: Option<f32>,
        fullscreen: bool,
    ) -> bool {
        let Some(window) = window else { return panel };
        let constrained = fullscreen
            || ctx.input(|i| {
                i.viewport().fullscreen == Some(true) || i.viewport().maximized == Some(true)
            });
        let changed = self.observed.is_some_and(|old| !near(old, size));
        let pending = self
            .requested
            .is_some_and(|(_, time)| time.elapsed() < Duration::from_millis(250));
        if changed && !constrained && !self.constrained && !pending {
            let (area, _) = Self::regions(Rect::from_min_size(Pos2::ZERO, size), self.panel);
            self.video = area.size();
            self.envelope = self.video;
        }
        if self.requested.is_some_and(|(target, _)| near(target, size)) {
            self.requested = None;
        }
        if !pending {
            self.requested = None;
        }
        self.observed = Some(size);
        self.constrained = constrained;

        if self.panel != panel {
            self.panel = panel;
            self.dirty = true;
        }
        if let Some(aspect) = aspect.filter(|a| a.is_finite() && *a > 0.0)
            && self
                .aspect
                .is_none_or(|previous| (previous - aspect).abs() > 0.001)
        {
            self.aspect = Some(aspect);
            self.video = fit_size(self.envelope, aspect).max(Vec2::from(MIN_VIDEO));
            self.dirty = true;
        }
        if constrained || !std::mem::take(&mut self.dirty) {
            if constrained || self.requested.is_none() {
                self.presented = panel;
            }
            return self.presented;
        }

        let scale = ctx.pixels_per_point();
        let outer = window.outer_size().to_logical::<f32>(scale.into());
        let inner = window.inner_size().to_logical::<f32>(scale.into());
        let decoration = vec2(
            (outer.width - inner.width).max(0.0),
            (outer.height - inner.height).max(0.0),
        );
        let monitor = window
            .current_monitor()
            .or_else(|| window.available_monitors().next());
        let work = monitor.map(|monitor| {
            let origin = monitor.position().to_logical::<f32>(scale.into());
            let size = monitor.size().to_logical::<f32>(scale.into());
            let rect = Rect::from_min_size(pos2(origin.x, origin.y), vec2(size.width, size.height));
            self.x11
                .as_ref()
                .and_then(|x11| x11.work_area(scale))
                .map(|work| work.intersect(rect))
                .filter(|work| work.is_positive())
                .unwrap_or(rect)
        });
        let available = work.map_or(self.video + vec2(PANEL_WIDTH, 0.0), |work| {
            let reserve = if self.x11.is_none() && decoration.y < 2.0 {
                48.0
            } else {
                2.0
            };
            work.size() - decoration - vec2(2.0, reserve)
        });
        let panel_width = if panel {
            sidebar_width(available.x)
        } else {
            0.0
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(
            Vec2::from(MIN_VIDEO) + vec2(panel_width, 0.0),
        ));
        let target = window_size(self.video, available, self.aspect, panel_width);
        if near(size, target) {
            self.presented = panel;
            return panel;
        }
        if let Some(work) = work
            && let Ok(outer) = window.outer_position()
        {
            let old = outer.to_logical::<f32>(scale.into());
            let upper = (work.max - target - decoration).max(work.min);
            let next = pos2(old.x, old.y).clamp(work.min, upper);
            if next.distance(pos2(old.x, old.y)) > 1.0 {
                window.set_outer_position(PhysicalPosition::new(
                    (next.x * scale).round() as i32,
                    (next.y * scale).round() as i32,
                ));
            }
        }
        self.requested = Some((target, Instant::now()));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(target));
        if self.presented != panel {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        self.presented
    }
}

fn window_size(video: Vec2, available: Vec2, aspect: Option<f32>, sidebar: f32) -> Vec2 {
    let min = Vec2::from(MIN_VIDEO);
    let bounds = video.min(available - vec2(sidebar, 0.0)).max(min);
    let fitted = if bounds != video
        && let Some(aspect) = aspect
    {
        fit_size(bounds, aspect).max(min)
    } else {
        bounds
    };
    (fitted + vec2(sidebar, 0.0)).round()
}

pub fn fit_size(bounds: Vec2, aspect: f32) -> Vec2 {
    let width = bounds.x.min(bounds.y * aspect);
    vec2(width, width / aspect)
}

fn sidebar_width(width: f32) -> f32 {
    PANEL_WIDTH.min((width - MIN_VIDEO[0]).max(0.0))
}

fn near(a: Vec2, b: Vec2) -> bool {
    (a - b).abs().max_elem() < 2.0
}

x11rb::atom_manager! {
    Atoms: AtomCookies {
        _NET_CURRENT_DESKTOP,
        _NET_WORKAREA,
    }
}

struct X11 {
    conn: RustConnection,
    root: u32,
    window: u32,
    atoms: Atoms,
}

impl X11 {
    fn open(window: &Window) -> Option<Self> {
        let window = match window.window_handle().ok()?.as_raw() {
            RawWindowHandle::Xlib(handle) => handle.window as u32,
            RawWindowHandle::Xcb(handle) => handle.window.get(),
            _ => return None,
        };
        let (conn, screen) = x11rb::connect(None).ok()?;
        let root = conn.setup().roots.get(screen)?.root;
        let atoms = Atoms::new(&conn).ok()?.reply().ok()?;
        Some(Self {
            conn,
            root,
            window,
            atoms,
        })
    }

    fn pointer(&self, scale: f32) -> Option<Pos2> {
        let reply = self.conn.query_pointer(self.window).ok()?.reply().ok()?;
        reply.same_screen.then(|| {
            pos2(
                f32::from(reply.win_x) / scale,
                f32::from(reply.win_y) / scale,
            )
        })
    }

    fn work_area(&self, scale: f32) -> Option<Rect> {
        let property = |atom, len| {
            self.conn
                .get_property(false, self.root, atom, AtomEnum::CARDINAL, 0, len)
        };
        let active = property(self.atoms._NET_CURRENT_DESKTOP, 1)
            .ok()?
            .reply()
            .ok()?;
        let desktop = active.value32()?.next()? as usize;
        let areas = property(self.atoms._NET_WORKAREA, 256).ok()?.reply().ok()?;
        let mut values = areas.value32()?.skip(desktop.checked_mul(4)?);
        let at = pos2(
            values.next()? as i32 as f32 / scale,
            values.next()? as i32 as f32 / scale,
        );
        let size = vec2(values.next()? as f32 / scale, values.next()? as f32 / scale);
        Some(Rect::from_min_size(at, size))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_adds_width_and_keeps_the_entire_video_region() {
        let video = vec2(960.0, 540.0);
        let monitor = vec2(1364.0, 760.0);
        let enlarged = window_size(video, monitor, Some(16.0 / 9.0), PANEL_WIDTH);
        assert_eq!(enlarged, video + vec2(PANEL_WIDTH, 0.0));
        let (screen, panel) = Layout::regions(Rect::from_min_size(Pos2::ZERO, enlarged), true);
        let panel = panel.unwrap();
        assert_eq!(screen.size(), video);
        assert_eq!(screen.right(), panel.left());
        assert_eq!(window_size(video, monitor, Some(16.0 / 9.0), 0.0), video);
    }

    #[test]
    fn constrained_window_scales_video_uniformly() {
        let video = vec2(960.0, 540.0);
        let monitor = vec2(1024.0, 580.0);
        let window = window_size(video, monitor, Some(16.0 / 9.0), PANEL_WIDTH);
        assert!(window.x <= monitor.x && window.y <= monitor.y);
        let area = window - vec2(PANEL_WIDTH, 0.0);
        assert!((area.x / area.y - 16.0 / 9.0).abs() < 0.004);
        let portrait = fit_size(video, 9.0 / 16.0);
        assert!((portrait.x / portrait.y - 9.0 / 16.0).abs() < 0.001);
        assert_eq!(portrait.y, video.y);
    }
}
