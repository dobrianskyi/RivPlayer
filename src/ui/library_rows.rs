use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, mpsc},
    time::Duration,
};

use eframe::egui::{
    self, Align2, Color32, FontId, Id, LayerId, Order, Painter, PointerButton, Pos2, Rect, Sense,
    Stroke, StrokeKind, TextureHandle, TextureOptions, Vec2, pos2, vec2,
};
use rivplayer::{engine, source::LocalFile};

use super::{
    controls::{Icon, draw, timestamp},
    library::{Entry, Library, List, title},
    overlays::{TimeAction, time_menu},
    preview,
};

const HEIGHT: f32 = 54.0;
const PITCH: f32 = 60.0;
const OUTLINE: Color32 = Color32::from_rgb(141, 173, 238);
const FG: Color32 = Color32::from_rgb(238, 242, 249);
const MUTED: Color32 = Color32::from_rgb(164, 176, 194);

pub enum Edit {
    Open(i64, PathBuf),
    Reveal(PathBuf),
    Remove(i64, PathBuf),
    Clear,
    Favorite(PathBuf, bool),
    Reorder(i64, Option<i64>),
    Time(TimeAction),
}

pub struct MenuState<'a> {
    pub favorites: &'a HashSet<PathBuf>,
    pub show_clock: bool,
}

struct Drag {
    list: List,
    id: i64,
    offset: Vec2,
}

pub struct Rows {
    drag: Option<Drag>,
    requested: HashSet<PathBuf>,
    tasks: mpsc::SyncSender<PathBuf>,
    results: mpsc::Receiver<Artwork>,
    textures: HashMap<PathBuf, TextureHandle>,
    texture_order: VecDeque<PathBuf>,
}

struct Artwork {
    path: PathBuf,
    duration: Option<f64>,
    cover: Option<egui::ColorImage>,
}

impl Rows {
    pub fn new(ctx: egui::Context) -> Self {
        let (tasks, queued) = mpsc::sync_channel::<PathBuf>(6);
        let (ready, results) = mpsc::sync_channel(6);
        std::thread::spawn(move || {
            // SAFETY: on Linux, who=0 sets the priority of only this thread.
            unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 9) };
            let cancel = AtomicBool::new(false);
            while let Ok(path) = queued.recv() {
                let source = LocalFile(path.clone());
                let (duration, frame) =
                    engine::sample_preview(&source, 0.35, None, &cancel).unwrap_or_default();
                let mut cover = frame.and_then(|p| preview::thumbnail(&p.picture));
                if cover
                    .as_ref()
                    .is_some_and(|image| !preview::has_detail(image))
                    && let Some(time) = duration.map(|n| (n * 0.2).min(2.0)).filter(|&n| n > 0.55)
                    && let Ok(Some(frame)) = engine::preview(&source, time, None, &cancel)
                {
                    cover = preview::thumbnail(&frame.picture).or(cover);
                }
                if ready
                    .send(Artwork {
                        path,
                        duration,
                        cover,
                    })
                    .is_err()
                {
                    break;
                }
                ctx.request_repaint();
            }
        });
        Self {
            drag: None,
            requested: HashSet::new(),
            tasks,
            results,
            textures: HashMap::new(),
            texture_order: VecDeque::new(),
        }
    }

    pub fn ingest(&mut self, ctx: &egui::Context, db: &Library) -> anyhow::Result<bool> {
        let mut changed = false;
        while let Ok(result) = self.results.try_recv() {
            if result.cover.is_some() {
                self.requested.remove(&result.path);
            }
            if result.duration.is_some() || result.cover.is_some() {
                let cover = result.cover.as_ref().map(preview::encode);
                db.save_details(&result.path, result.duration, cover.as_deref())?;
                if let Some(cover) = result.cover {
                    self.store_texture(ctx, &result.path, cover);
                }
                changed = true;
            }
        }
        Ok(changed)
    }

    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    pub fn cancel(&mut self) {
        self.drag = None;
    }

    fn store_texture(&mut self, ctx: &egui::Context, path: &Path, image: egui::ColorImage) {
        if self.textures.contains_key(path) {
            return;
        }
        if self.texture_order.len() >= 128
            && let Some(old) = self.texture_order.pop_front()
        {
            self.textures.remove(&old);
        }
        self.texture_order.push_back(path.to_owned());
        self.textures.insert(
            path.to_owned(),
            ctx.load_texture("library-thumbnail", image, TextureOptions::LINEAR),
        );
    }

    fn texture(
        &mut self,
        ctx: &egui::Context,
        db: &Library,
        entry: &Entry,
    ) -> Option<TextureHandle> {
        if entry.has_cover {
            if !self.textures.contains_key(&entry.path)
                && let Ok(Some(bytes)) = db.cover(&entry.path)
                && let Some(image) = preview::decode(&bytes)
            {
                self.store_texture(ctx, &entry.path, image);
            }
        } else if !self.requested.contains(&entry.path)
            && self.tasks.try_send(entry.path.clone()).is_ok()
        {
            self.requested.insert(entry.path.clone());
        }
        self.textures.get(&entry.path).cloned()
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        list: List,
        entries: &[Entry],
        active: Option<i64>,
        menu: MenuState<'_>,
        db: &Library,
    ) -> Option<Edit> {
        if self.drag.as_ref().is_some_and(|drag| {
            drag.list != list || !entries.iter().any(|entry| entry.id == drag.id)
        }) {
            self.cancel();
        }
        let mut edit = None;
        let mut floating = None;
        egui::ScrollArea::vertical()
            .id_salt(("media-entries", list))
            .auto_shrink([false, false])
            .scroll_source(egui::scroll_area::ScrollSource {
                drag: egui::scroll_area::DragScroll::Never,
                ..Default::default()
            })
            .show_viewport(ui, |ui, view| {
                let width = ui.available_width().max(1.0);
                let (_, area) = ui.allocate_space(vec2(width, entries.len() as f32 * PITCH));
                let ctx = ui.ctx().clone();
                let (pointer, released, held, escape) = ui.input(|i| {
                    (
                        i.pointer.latest_pos(),
                        i.pointer.button_released(PointerButton::Primary),
                        i.pointer.button_down(PointerButton::Primary),
                        i.key_pressed(egui::Key::Escape),
                    )
                });
                let clip = ui.clip_rect();
                let drag = self.drag.as_ref().and_then(|drag| {
                    let from = entries.iter().position(|e| e.id == drag.id)?;
                    let pointer = pointer?;
                    let offset = drag.offset;
                    let in_list = clip.contains(pointer);
                    let to = if in_list {
                        insertion_slot(
                            pointer.y - offset.y + HEIGHT * 0.5 - area.top(),
                            entries.len(),
                        )
                    } else {
                        from
                    };
                    Some((from, to, pointer, offset, in_list))
                });
                if escape || (!held && !released) {
                    self.cancel();
                }
                let drag = drag.filter(|_| self.drag.is_some());
                if let Some((from, to, point, offset, in_list)) = drag {
                    let active_drag = &entries[from];
                    if released {
                        self.cancel();
                        if in_list && from != to {
                            edit = Some(Edit::Reorder(
                                active_drag.id,
                                before_entry(entries, from, to),
                            ));
                        }
                    } else {
                        ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
                        if in_list {
                            let gap = row_rect(area, to as f32 * PITCH).shrink2(vec2(0.5, 1.5));
                            ui.painter().rect_filled(
                                gap,
                                7.0,
                                Color32::from_rgba_unmultiplied(93, 122, 182, 27),
                            );
                            ui.painter().rect_stroke(
                                gap,
                                7.0,
                                Stroke::new(1.0, OUTLINE),
                                StrokeKind::Inside,
                            );
                            let margin = (clip.height() * 0.2).clamp(1.0, 32.0);
                            let scroll = (clip.top() + margin - point.y).max(0.0)
                                - (point.y - (clip.bottom() - margin)).max(0.0);
                            if scroll != 0.0 {
                                ui.scroll_with_delta_animation(
                                    vec2(0.0, scroll * 0.32),
                                    egui::style::ScrollAnimation::none(),
                                );
                                ctx.request_repaint_after(Duration::from_millis(16));
                            }
                        }
                        floating = Some((
                            from,
                            Rect::from_min_size(point - offset, vec2(width, HEIGHT)),
                        ));
                    }
                }
                let start = (view.top() / PITCH).floor().max(0.0) as usize;
                let stop = (view.bottom() / PITCH).ceil().max(0.0) as usize;
                let range = start.saturating_sub(2).min(entries.len())
                    ..stop.saturating_add(2).min(entries.len());
                for i in range {
                    if drag.is_some_and(|(from, _, _, _, _)| from == i) {
                        continue;
                    }
                    let entry = &entries[i];
                    let key = ui.scope_id().with((list, entry.id));
                    let slot = drag.map_or(i, |(from, to, _, _, _)| row_slot(i, from, to));
                    let y =
                        ctx.animate_value_with_time(key.with("row-y"), slot as f32 * PITCH, 0.12);
                    let rect = row_rect(area, y);
                    if !ui.is_rect_visible(rect) {
                        continue;
                    }
                    let body = ui.interact(rect, key.with("entry"), Sense::click());
                    let thumb = ui
                        .interact(
                            thumbnail_rect(rect),
                            key.with("thumb"),
                            Sense::click_and_drag(),
                        )
                        .on_hover_cursor(egui::CursorIcon::Grab);
                    let grip = ui
                        .interact(grip_rect(rect), key.with("grip"), Sense::drag())
                        .on_hover_cursor(egui::CursorIcon::Grab)
                        .on_hover_text("Drag to Reorder");
                    let remove = ui
                        .interact(remove_rect(rect), key.with("remove"), Sense::click())
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text(match list {
                            List::History => "Remove from History",
                            List::Playlist(_) => "Remove from Playlist",
                            List::Favorites => "Remove from Favorites",
                        });
                    let cover = self.texture(&ctx, db, entry);
                    let selected = active == Some(entry.id);
                    let whole = body
                        .union(thumb.clone())
                        .union(grip.clone())
                        .union(remove.clone());
                    paint_row(
                        ui.painter(),
                        rect,
                        entry,
                        cover.as_ref(),
                        selected,
                        whole.hovered(),
                        false,
                    );
                    if self.drag.is_none()
                        && (thumb.drag_started_by(PointerButton::Primary)
                            || grip.drag_started_by(PointerButton::Primary))
                        && let Some(point) = pointer
                    {
                        let press = ui.input(|i| i.pointer.press_origin()).unwrap_or(point);
                        self.drag = Some(Drag {
                            list,
                            id: entry.id,
                            offset: press - rect.min,
                        });
                        floating = Some((i, rect.translate(point - press)));
                        ctx.request_repaint();
                    }
                    if self.drag.is_some() || released && drag.is_some() {
                        continue;
                    }
                    if body.clicked() || thumb.clicked() {
                        edit = Some(Edit::Open(entry.id, entry.path.clone()));
                    }
                    if remove.clicked() {
                        edit = Some(Edit::Remove(entry.id, entry.path.clone()));
                    }
                    whole.context_menu(|ui| {
                        ui.set_min_width(174.0);
                        let favorite = menu.favorites.contains(&entry.path);
                        if ui
                            .button(if favorite {
                                "Remove from Favorites"
                            } else {
                                "Add to Favorites"
                            })
                            .clicked()
                        {
                            edit = Some(Edit::Favorite(entry.path.clone(), !favorite));
                            ui.close();
                        }
                        if ui.button("Open in File Manager").clicked() {
                            edit = Some(Edit::Reveal(entry.path.clone()));
                            ui.close();
                        }
                        if matches!(list, List::Playlist(_))
                            && let Some(action) = time_menu(ui, menu.show_clock)
                        {
                            edit = Some(Edit::Time(action));
                        }
                        if i > 0 && ui.button("Move up").clicked() {
                            edit = Some(Edit::Reorder(entry.id, Some(entries[i - 1].id)));
                            ui.close();
                        }
                        if i + 1 < entries.len() && ui.button("Move down").clicked() {
                            edit = Some(Edit::Reorder(
                                entry.id,
                                entries.get(i + 2).map(|next| next.id),
                            ));
                            ui.close();
                        }
                        if let Some(label) = match list {
                            List::History => Some("Clear all history"),
                            List::Playlist(_) => Some("Clear all playlist"),
                            List::Favorites => None,
                        } {
                            ui.separator();
                            if ui.button(label).clicked() {
                                edit = Some(Edit::Clear);
                                ui.close();
                            }
                        }
                    });
                    if !grip.hovered() && !remove.hovered() && !whole.context_menu_opened() {
                        file_tooltip(&whole, &entry.path);
                    }
                }
            });
        if let Some((i, rect)) = floating {
            let entry = &entries[i];
            let cover = self.texture(ui.ctx(), db, entry);
            let painter = ui.ctx().layer_painter(LayerId::new(
                Order::Foreground,
                Id::unique("floating-library-row"),
            ));
            paint_row(
                &painter,
                rect,
                entry,
                cover.as_ref(),
                active == Some(entry.id),
                false,
                true,
            );
        }
        edit
    }
}

fn file_tooltip(response: &egui::Response, path: &Path) {
    let width = (response.ctx.content_rect().width() - 34.0).min(460.0);
    let mut tooltip = egui::Tooltip::for_enabled(response)
        .at_pointer()
        .gap(12.0)
        .width(width);
    tooltip.popup = tooltip.popup.frame(
        egui::Frame::new()
            .fill(Color32::from_rgb(26, 33, 47))
            .stroke(Stroke::new(1.0, Color32::from_rgb(64, 79, 105)))
            .corner_radius(8.0)
            .inner_margin(11.0),
    );
    tooltip.show(|ui| {
        ui.set_max_width(width);
        ui.add(
            egui::Label::new(
                egui::RichText::new(title(path))
                    .strong()
                    .size(14.0)
                    .color(FG),
            )
            .wrap(),
        );
        ui.add_space(3.0);
        ui.add(
            egui::Label::new(
                egui::RichText::new(path.to_string_lossy())
                    .size(12.0)
                    .color(MUTED),
            )
            .wrap(),
        );
    });
}

fn insertion_slot(y: f32, count: usize) -> usize {
    ((y / PITCH).floor().max(0.0) as usize).min(count.saturating_sub(1))
}

fn before_entry(entries: &[Entry], from: usize, to: usize) -> Option<i64> {
    entries
        .get(if to < from { to } else { to + 1 })
        .map(|entry| entry.id)
}

fn row_slot(index: usize, from: usize, to: usize) -> usize {
    if from < index && index <= to {
        index - 1
    } else if to <= index && index < from {
        index + 1
    } else {
        index
    }
}

fn row_rect(area: Rect, y: f32) -> Rect {
    Rect::from_min_size(area.min + vec2(0.0, y), vec2(area.width(), HEIGHT))
}

fn thumbnail_rect(rect: Rect) -> Rect {
    Rect::from_min_size(rect.min + vec2(6.0, 7.0), vec2(62.0, 40.0))
}

fn grip_rect(rect: Rect) -> Rect {
    Rect::from_center_size(
        pos2(rect.right() - 38.0, rect.center().y),
        vec2(23.0, HEIGHT),
    )
}

fn remove_rect(rect: Rect) -> Rect {
    Rect::from_center_size(pos2(rect.right() - 14.0, rect.center().y), vec2(22.0, 26.0))
}

fn paint_row(
    painter: &Painter,
    rect: Rect,
    entry: &Entry,
    cover: Option<&TextureHandle>,
    active: bool,
    hovered: bool,
    floating: bool,
) {
    if floating {
        painter.rect_filled(
            rect.translate(vec2(0.0, 4.0)).expand(3.0),
            8.0,
            Color32::from_black_alpha(100),
        );
    }
    let background = match (floating, active, hovered) {
        (true, _, _) => Color32::from_rgb(44, 56, 79),
        (_, true, _) => Color32::from_rgb(39, 54, 77),
        (_, _, true) => Color32::from_rgb(34, 44, 62),
        _ => Color32::from_rgb(29, 36, 50),
    };
    painter.rect_filled(rect, 7.0, background);
    if floating || active {
        let edge = if floating {
            OUTLINE
        } else {
            Color32::from_rgb(64, 84, 116)
        };
        painter.rect_stroke(rect, 7.0, Stroke::new(1.0, edge), StrokeKind::Inside);
    }
    let thumb = thumbnail_rect(rect);
    painter.rect_filled(thumb, 5.0, Color32::from_rgb(45, 55, 73));
    if let Some(cover) = cover {
        painter.image(
            cover.id(),
            super::fit(thumb, cover.size_vec2()),
            Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    } else {
        draw(painter, thumb.center(), Icon::Play, MUTED);
    }
    let left = thumb.right() + 8.0;
    let right = grip_rect(rect).left() - 4.0;
    if right > left {
        let info = match (entry.position, entry.duration, entry.finished) {
            (_, Some(duration), true) => format!("Watched · {}", timestamp(duration)),
            (_, None, true) => "Watched".to_owned(),
            (Some(at), Some(total), false) => format!("{} / {}", timestamp(at), timestamp(total)),
            (Some(at), None, false) => timestamp(at),
            (None, Some(duration), false) => timestamp(duration),
            _ => "Unknown duration".to_owned(),
        };
        let mut job = egui::text::LayoutJob::simple_singleline(
            title(&entry.path),
            FontId::proportional(13.5),
            FG,
        );
        job.wrap.max_width = right - left;
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        let name = painter.layout_job(job);
        let text = painter.with_clip_rect(Rect::from_min_max(
            pos2(left, rect.top()),
            pos2(right, rect.bottom()),
        ));
        text.galley(pos2(left, rect.top() + 9.0), name, FG);
        text.text(
            pos2(left, rect.top() + 29.0),
            Align2::LEFT_TOP,
            info,
            FontId::proportional(12.0),
            MUTED,
        );
    }
    draw(painter, grip_rect(rect).center(), Icon::Grip, MUTED);
    draw(painter, remove_rect(rect).center(), Icon::Remove, MUTED);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moving_rows_leaves_exactly_one_insertion_slot() {
        let n = 6;
        for from in 0..n {
            for to in 0..n {
                let slots: HashSet<_> = (0..n)
                    .filter(|&i| i != from)
                    .map(|i| row_slot(i, from, to))
                    .collect();
                assert_eq!(slots.len(), n - 1);
                assert!(!slots.contains(&to));
            }
        }
        assert_eq!(insertion_slot(-120.0, n), 0);
        assert_eq!(insertion_slot(PITCH * 4.2, n), 4);
        assert_eq!(insertion_slot(PITCH * 12.0, n), n - 1);
    }
}
