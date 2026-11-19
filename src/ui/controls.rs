use std::time::{Duration, Instant};

use eframe::egui::{
    self, Color32, Mesh, Painter, Pos2, Rect, Response, Role, Sense, Shape, Stroke, Vec2,
    WidgetInfo, pos2, vec2,
};

pub const RED: Color32 = Color32::from_rgb(255, 0, 51);
pub const WHITE: Color32 = Color32::from_gray(245);
pub const PANEL_BACKGROUND: Color32 = Color32::from_gray(32);
pub const PANEL_BORDER: Color32 = Color32::from_gray(63);
pub const ACTION_WIDTH: f32 = 27.0;

#[derive(Clone, Copy)]
pub enum Icon {
    Play,
    Pause,
    Replay,
    Sound,
    Quiet,
    Mute,
    Captions,
    Alarm,
    Settings,
    Fullscreen,
    ExitFullscreen,
    Folder,
    Arrow,
    PanelOpen,
    PanelClose,
    Grip,
    Remove,
}

pub fn button(ui: &egui::Ui, rect: Rect, label: &str, icon: Icon) -> Response {
    named_button(ui, rect, label, label, icon)
}

pub fn named_button(ui: &egui::Ui, rect: Rect, id: &str, label: &str, icon: Icon) -> Response {
    let response = ui.interact(rect, ui.scope_id().with(id), Sense::click());
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, label));
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    let color = if response.is_pointer_button_down_on() {
        Color32::from_gray(190)
    } else {
        WHITE
    };
    draw_scaled(
        ui.painter(),
        rect.center(),
        icon,
        color,
        (rect.height() / 35.0).clamp(1.0, 1.17),
    );
    response.on_hover_text(label)
}

pub fn draw(p: &Painter, center: Pos2, icon: Icon, color: Color32) {
    draw_scaled(p, center, icon, color, 1.0);
}

pub(super) fn draw_scaled(p: &Painter, center: Pos2, icon: Icon, color: Color32, scale: f32) {
    use Icon::*;
    let at = |x, y| center + vec2(x, y) * scale;
    let points = |path: &[(f32, f32)]| path.iter().map(|&(x, y)| at(x, y)).collect::<Vec<_>>();
    let solid = |coords: &[(f32, f32)]| {
        p.add(Shape::convex_polygon(points(coords), color, Stroke::NONE));
    };
    let line = |coords: &[(f32, f32)], width| {
        p.add(Shape::line(
            points(coords),
            Stroke::new(width * scale, color),
        ));
    };
    let rect = |x0, y0, x1, y1| Rect::from_min_max(at(x0, y0), at(x1, y1));

    match icon {
        Play => solid(&[(-6.5, -9.0), (8.5, 0.0), (-6.5, 9.0)]),
        Pause => {
            p.rect_filled(rect(-7.0, -8.5, -2.0, 8.5), 0.5, color);
            p.rect_filled(rect(2.0, -8.5, 7.0, 8.5), 0.5, color);
        }
        Replay => {
            let coords: Vec<_> = (0..=26)
                .map(|i| {
                    let angle = i as f32 * 5.15 / 26.0 - 1.55;
                    (8.1 * angle.cos(), 8.1 * angle.sin())
                })
                .collect();
            line(&coords, 2.0);
            solid(&[(-5.0, -8.3), (1.5, -11.1), (1.5, -5.3)]);
        }
        Sound | Quiet | Mute => {
            solid(&[
                (-10.0, -3.4),
                (-6.2, -3.4),
                (-1.3, -7.5),
                (-1.3, 7.5),
                (-6.2, 3.4),
                (-10.0, 3.4),
            ]);
            if matches!(icon, Mute) {
                line(&[(2.2, -3.5), (8.7, 3.5)], 1.65);
                line(&[(2.2, 3.5), (8.7, -3.5)], 1.65);
            } else {
                for r in [4.5, 8.0]
                    .into_iter()
                    .take(if matches!(icon, Sound) { 2 } else { 1 })
                {
                    let arc: Vec<_> = (0..=12)
                        .map(|i| {
                            let a = -0.86 + i as f32 * 1.72 / 12.0;
                            (-0.8 + r * a.cos(), r * a.sin())
                        })
                        .collect();
                    line(&arc, 1.5);
                }
            }
        }
        Captions => {
            p.rect_filled(rect(-10.5, -7.5, 10.5, 7.5), 2.0, color);
            for x in [-7.0, 0.7] {
                p.add(Shape::line(
                    points(&[
                        (x + 5.3, -2.4),
                        (x + 4.0, -3.4),
                        (x + 1.4, -3.4),
                        (x, -2.2),
                        (x, 2.0),
                        (x + 1.4, 3.2),
                        (x + 4.0, 3.2),
                        (x + 5.3, 2.2),
                    ]),
                    Stroke::new(1.45 * scale, PANEL_BACKGROUND),
                ));
            }
        }
        Alarm => {
            p.circle_stroke(at(0.0, 0.5), 7.6 * scale, Stroke::new(1.7 * scale, color));
            line(&[(0.0, -4.0), (0.0, 0.5), (3.2, 2.5)], 1.6);
            for side in [-1.0, 1.0] {
                line(&[(5.8 * side, -9.3), (9.3 * side, -6.3)], 2.0);
                line(&[(4.8 * side, 6.5), (6.9 * side, 9.5)], 1.7);
            }
        }
        Settings => {
            let radius = |i: usize| if matches!(i % 4, 1 | 2) { 9.6 } else { 7.5 };
            let gear: Vec<_> = (0..=32)
                .map(|i| {
                    let a = i as f32 * std::f32::consts::TAU / 32.0;
                    at(a.cos() * radius(i), a.sin() * radius(i))
                })
                .collect();
            let mut mesh = Mesh::default();
            mesh.colored_vertex(center, color);
            gear.iter().for_each(|&pos| mesh.colored_vertex(pos, color));
            for i in 1..=32 {
                mesh.add_triangle(0, i, i + 1);
            }
            p.add(mesh);
            p.circle_filled(center, 3.1, Color32::from_gray(30));
        }
        Fullscreen | ExitFullscreen => {
            for (x, y) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                let corner = if matches!(icon, Fullscreen) { 8.0 } else { 3.0 };
                let tip = if matches!(icon, Fullscreen) { 3.0 } else { 8.0 };
                line(
                    &[
                        (corner * x, tip * y),
                        (corner * x, corner * y),
                        (tip * x, corner * y),
                    ],
                    1.9,
                );
            }
        }
        Folder => {
            line(
                &[
                    (-10.0, 6.9),
                    (-10.0, -5.8),
                    (-3.8, -5.8),
                    (-1.6, -3.5),
                    (9.3, -3.5),
                    (9.3, 6.9),
                    (-10.0, 6.9),
                ],
                1.65,
            );
            line(&[(-9.7, -0.4), (9.0, -0.4)], 1.25);
        }
        PanelOpen => line(&[(-2.5, -6.0), (3.5, 0.0), (-2.5, 6.0)], 1.8),
        PanelClose => line(&[(2.5, -6.0), (-3.5, 0.0), (2.5, 6.0)], 1.8),
        Arrow => line(&[(-2.3, -4.5), (2.2, 0.0), (-2.3, 4.5)], 1.5),
        Grip => {
            for x in [-2.5, 2.5] {
                for y in [-5.5, 0.0, 5.5] {
                    p.circle_filled(at(x, y), 1.15 * scale, color);
                }
            }
        }
        Remove => {
            line(&[(-3.7, -3.7), (3.7, 3.7)], 1.65);
            line(&[(-3.7, 3.7), (3.7, -3.7)], 1.65);
        }
    }
}

pub fn action_icon(ui: &mut egui::Ui, icon: Icon, label: &str, enabled: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(
        vec2(ACTION_WIDTH, 24.0),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    response.widget_info(|| WidgetInfo::labeled(Role::Button, enabled, label));
    if response.hovered() && enabled {
        ui.painter()
            .rect_filled(rect, 5.0, Color32::from_white_alpha(25));
    }
    let color = Color32::from_white_alpha(if enabled { 214 } else { 72 });
    draw_scaled(ui.painter(), rect.center(), icon, color, 0.88);
    if enabled {
        response
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(label)
    } else {
        response.on_hover_text(label)
    }
}

pub struct PlaybackCue {
    start: Instant,
    icon: Icon,
}

impl PlaybackCue {
    pub fn new(playing: bool) -> Self {
        Self {
            start: Instant::now(),
            icon: if playing { Icon::Play } else { Icon::Pause },
        }
    }

    pub fn paint(&self, ui: &egui::Ui, rect: Rect) -> bool {
        const LENGTH: f32 = 0.39;
        let elapsed = self.start.elapsed().as_secs_f32();
        if elapsed >= LENGTH {
            return false;
        }
        let t = elapsed / LENGTH;
        let ease = |x: f32| x * x * (3.0 - 2.0 * x);
        let alpha = ease((elapsed / 0.068).min(1.0)) * (1.0 - ease(((t - 0.24) / 0.76).max(0.0)));
        let scale = 0.82 + 0.34 * ease(t);
        let painter = ui.painter_at(rect);
        painter.circle_filled(
            rect.center(),
            39.0 * scale,
            Color32::from_black_alpha((170.0 * alpha) as u8),
        );
        draw_scaled(
            &painter,
            rect.center(),
            self.icon,
            Color32::from_white_alpha((248.0 * alpha) as u8),
            1.6 * scale,
        );
        ui.ctx().request_repaint_after(Duration::from_millis(16));
        true
    }
}

pub fn gradient(painter: &Painter, rect: Rect, top: Color32, bottom: Color32) {
    let mut mesh = Mesh::default();
    mesh.colored_vertex(rect.left_top(), top);
    mesh.colored_vertex(rect.right_top(), top);
    mesh.colored_vertex(rect.right_bottom(), bottom);
    mesh.colored_vertex(rect.left_bottom(), bottom);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    painter.add(mesh);
}

pub fn timestamp(seconds: f64) -> String {
    let seconds = seconds.max(0.0) as u64;
    if seconds < 3600 {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    } else {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    }
}

pub fn seekbar(
    ui: &egui::Ui,
    track: Rect,
    position: f64,
    duration: Option<f64>,
    drag: &mut Option<f64>,
) -> (Option<f64>, Option<f64>) {
    let hit = track.expand2(vec2(3.0, 10.0));
    let response = ui.interact(
        hit,
        ui.scope_id().with("seekbar"),
        if duration.is_some() {
            Sense::click_and_drag()
        } else {
            Sense::hover()
        },
    );
    response.widget_info(|| WidgetInfo::labeled(Role::Slider, duration.is_some(), "Seek"));
    let p = ui.painter();
    let interact = duration.is_some() && (response.hovered() || response.dragged());
    let line = Rect::from_center_size(
        track.center(),
        vec2(track.width(), if interact { 7.0 } else { 5.0 }),
    );
    let fraction = |time: f64| {
        duration
            .filter(|d| *d > 0.0)
            .map_or(0.0, |d| (time / d).clamp(0.0, 1.0) as f32)
    };
    let cursor = response
        .interact_pointer_pos()
        .or_else(|| response.hover_pos());
    let target = cursor.and_then(|pos| {
        duration
            .map(|len| f64::from(((pos.x - track.left()) / track.width()).clamp(0.0, 1.0)) * len)
    });
    if response.dragged() && target.is_some() {
        *drag = target;
    }
    let seek = if response.drag_stopped() {
        target.or(drag.take())
    } else if response.clicked() {
        target
    } else {
        None
    };
    let progress = fraction(seek.or(*drag).unwrap_or(position));
    p.rect_filled(line, 1.0, Color32::from_white_alpha(91));
    if interact && let Some(t) = target {
        let x = track.left() + track.width() * fraction(t);
        p.rect_filled(
            Rect::from_min_max(line.min, pos2(x, line.bottom())),
            1.0,
            Color32::from_white_alpha(65),
        );
    }
    let x = track.left() + track.width() * progress;
    if progress > 0.0 {
        p.rect_filled(
            Rect::from_min_max(line.min, pos2(x, line.bottom())),
            1.0,
            RED,
        );
    }
    if interact || drag.is_some() {
        p.circle_filled(pos2(x, track.center().y), 7.0, RED);
    }
    (seek, if interact { target } else { None })
}

pub fn volume_slider(
    ui: &egui::Ui,
    normal: Rect,
    id: egui::Id,
    volume: f32,
    limit: f32,
) -> Option<f32> {
    let left = normal.left_center();
    let unity = normal.right_center();
    let end = left + Vec2::X * (normal.width() * limit);
    let response = ui.interact(
        Rect::from_min_max(normal.min, pos2(end.x, normal.bottom())).expand2(vec2(4.0, 8.0)),
        id,
        Sense::click_and_drag(),
    );
    let value = (response.clicked() || response.dragged() || response.drag_stopped())
        .then(|| response.interact_pointer_pos())
        .flatten()
        .map(|pos| {
            let gain = ((pos.x - left.x) / normal.width()).clamp(0.0, limit);
            (gain * 100.0).round() / 100.0
        });
    let level = value.unwrap_or(volume);
    response.widget_info(|| WidgetInfo::slider(true, f64::from(level * 100.0), "Volume (%)"));
    let p = ui.painter();
    p.text(
        left - vec2(8.0, 0.0),
        egui::Align2::RIGHT_CENTER,
        format!("{:.0}%", level * 100.0),
        egui::FontId::proportional(13.0),
        WHITE,
    );
    p.line_segment(
        [left, unity],
        Stroke::new(3.0, Color32::from_white_alpha(95)),
    );
    let knob = left + Vec2::X * (normal.width() * level);
    p.line_segment(
        [left, pos2(knob.x.min(unity.x), left.y)],
        Stroke::new(3.0, WHITE),
    );
    if limit > 1.0 {
        p.line_segment([unity, end], Stroke::new(3.0, RED.gamma_multiply(0.5)));
        if level > 1.0 {
            p.line_segment([unity, knob], Stroke::new(3.0, RED));
        }
        p.line_segment(
            [unity - vec2(0.0, 6.0), unity + vec2(0.0, 6.0)],
            Stroke::new(1.0, WHITE),
        );
    }
    p.circle_filled(knob, 5.0, if level > 1.0 { RED } else { WHITE });
    value
}
