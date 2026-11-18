use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::mpsc,
};

use anyhow::Result;
use eframe::egui::{self, Color32, Stroke, StrokeKind};

use super::{
    controls::{Icon, action_icon},
    file_manager, learning,
    library::{DEFAULT_PLAYLIST, Entry, Library, List, Playlist},
    library_rows::{Edit, MenuState, Rows},
    overlays::TimeAction,
    preferences::{SidebarView, Tab},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueItem {
    pub playlist: i64,
    pub item: i64,
}

pub enum Action {
    Open(PathBuf, Option<QueueItem>),
    AddFiles,
    Forget(PathBuf),
    ForgetAll,
    Subtitle(learning::Command),
    Time(TimeAction),
}

pub struct Sidebar {
    pub open: bool,
    pub db: Library,
    tab: Tab,
    playlists: Vec<Playlist>,
    selected: i64,
    entries: Vec<Entry>,
    favorites: HashSet<PathBuf>,
    rows: Rows,
    dirty: bool,
    playlist_name: String,
    renaming: bool,
    edit_focused: bool,
    reveal_error: (mpsc::Sender<String>, mpsc::Receiver<String>),
    pub error: Option<String>,
}

impl Sidebar {
    pub fn new(ctx: egui::Context) -> Result<Self> {
        let (db, error) = match Library::open() {
            Ok(db) => (db, None),
            Err(err) => (
                Library::temporary()?,
                Some(format!("Temporary library · {err:#}")),
            ),
        };
        let view = db.sidebar_view()?;
        Ok(Self {
            db,
            error,
            open: view.open,
            tab: view.tab,
            playlists: Vec::new(),
            selected: view.playlist,
            entries: Vec::new(),
            favorites: HashSet::new(),
            rows: Rows::new(ctx),
            dirty: true,
            playlist_name: String::new(),
            renaming: false,
            edit_focused: false,
            reveal_error: mpsc::channel(),
        })
    }

    pub fn editing(&self) -> bool {
        self.open && (self.edit_focused || self.rows.dragging())
    }

    pub fn toggle_open(&mut self) {
        self.open = !self.open;
        self.rows.cancel();
        self.save_view();
    }

    pub fn subtitles_open(&self) -> bool {
        self.open && self.tab == Tab::Subtitles
    }

    pub fn open_subtitles(&mut self) {
        self.open = true;
        self.tab = Tab::Subtitles;
        self.renaming = false;
        self.rows.cancel();
        self.dirty = true;
        self.save_view();
    }

    pub fn accepts_files(&self) -> bool {
        self.open && self.tab == Tab::Playlist
    }

    pub fn save(
        &mut self,
        path: &Path,
        time: f64,
        duration: Option<f64>,
        done: bool,
        cover: Option<&[u8]>,
    ) {
        let result = self.db.save(path, time, duration, done, cover);
        self.record(result);
    }

    pub fn enqueue(&mut self, paths: &[PathBuf]) -> Result<Option<QueueItem>> {
        let playlist = self.selected;
        let first = self.db.append(playlist, paths)?;
        let prior = self.view();
        self.open = true;
        self.tab = Tab::Playlist;
        self.dirty = true;
        if self.view() != prior {
            self.save_view();
        }
        Ok(first.map(|item| QueueItem { playlist, item }))
    }

    pub fn next(&self, current: QueueItem) -> Result<Option<(PathBuf, QueueItem)>> {
        let items = self.db.entries(List::Playlist(current.playlist))?;
        Ok(items
            .iter()
            .position(|i| i.id == current.item)
            .and_then(|index| items.get(index + 1))
            .map(|item| {
                (
                    item.path.clone(),
                    QueueItem {
                        playlist: current.playlist,
                        item: item.id,
                    },
                )
            }))
    }

    fn record(&mut self, result: Result<()>) {
        match result {
            Ok(()) => self.dirty = true,
            Err(err) => self.error = Some(format!("{err:#}")),
        }
    }

    fn view(&self) -> SidebarView {
        SidebarView {
            open: self.open,
            tab: self.tab,
            playlist: self.selected,
        }
    }

    fn save_view(&mut self) {
        if let Err(err) = self.db.save_sidebar_view(self.view()) {
            self.error = Some(format!("Saving library view · {err:#}"));
        }
    }

    fn list(&self) -> List {
        match self.tab {
            Tab::History | Tab::Subtitles => List::History,
            Tab::Playlist => List::Playlist(self.selected),
            Tab::Favorites => List::Favorites,
        }
    }

    fn refresh(&mut self) -> Result<()> {
        self.playlists = self.db.playlists()?;
        if self
            .playlists
            .iter()
            .all(|playlist| playlist.id != self.selected)
        {
            self.selected = DEFAULT_PLAYLIST;
        }
        self.entries = self.db.entries(self.list())?;
        self.favorites = self.db.favorite_paths()?;
        self.dirty = false;
        Ok(())
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        playing: Option<(&Path, Option<QueueItem>)>,
        show_clock: bool,
        drop_hovered: bool,
        subtitles: impl FnOnce(&mut egui::Ui) -> Option<learning::Command>,
    ) -> Option<Action> {
        let previous_view = self.view();
        self.edit_focused = false;
        if let Some(error) = self.reveal_error.1.try_iter().last() {
            self.error = Some(error);
        }
        match self.rows.ingest(ui.ctx(), &self.db) {
            Ok(changed) => self.dirty |= changed,
            Err(err) => self.error = Some(format!("{err:#}")),
        }
        if self.dirty
            && let Err(err) = self.refresh()
        {
            self.error = Some(format!("{err:#}"));
        }
        let background = if drop_hovered {
            Color32::from_rgb(32, 42, 61)
        } else {
            Color32::from_rgb(24, 27, 37)
        };
        ui.painter().rect_filled(rect, 0.0, background);
        ui.painter().vline(
            rect.left(),
            rect.y_range(),
            Stroke::new(1.0, Color32::from_rgb(56, 63, 80)),
        );
        let action = ui
            .scope_builder(egui::UiBuilder::new().max_rect(rect.shrink(12.0)), |ui| {
                ui.set_clip_rect(rect);
                let mut action = None;
                let prior = self.tab;
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.tab, Tab::History, "History");
                    ui.selectable_value(&mut self.tab, Tab::Playlist, "Playlist");
                    let star = action_icon(ui, Icon::Star, "Favorites", true);
                    if star.clicked() {
                        self.tab = Tab::Favorites;
                    }
                    if self.tab == Tab::Favorites {
                        ui.painter().rect_stroke(
                            star.rect,
                            5.0,
                            Stroke::new(1.0, Color32::from_rgb(129, 159, 218)),
                            StrokeKind::Inside,
                        );
                    }
                    ui.selectable_value(&mut self.tab, Tab::Subtitles, "Subtitles");
                    if matches!(self.tab, Tab::Favorites | Tab::Subtitles) {
                        return;
                    }
                    ui.separator();
                    let label = if self.tab == Tab::History {
                        "Clear history"
                    } else {
                        "Clear playlist"
                    };
                    if action_icon(
                        ui,
                        Icon::Trash,
                        label,
                        self.tab == prior && !self.entries.is_empty(),
                    )
                    .clicked()
                    {
                        let result = if self.tab == Tab::History {
                            self.db.clear_history()
                        } else {
                            self.db.clear_playlist(self.selected)
                        };
                        if result.is_ok() {
                            self.entries.clear();
                            self.rows.cancel();
                            if self.tab == Tab::History {
                                action = Some(Action::ForgetAll);
                            }
                        }
                        self.record(result);
                    }
                });
                if self.tab != prior {
                    self.renaming = false;
                    self.dirty = true;
                }
                ui.add_space(8.0);
                if self.tab == Tab::Subtitles {
                    self.rows.cancel();
                    return subtitles(ui).map(Action::Subtitle);
                }
                if self.tab == Tab::Playlist {
                    action = action.or(self.playlist_controls(ui));
                    ui.add_space(8.0);
                }
                if self.dirty
                    && let Err(err) = self.refresh()
                {
                    self.error = Some(format!("{err:#}"));
                }
                if let Some(error) = &self.error {
                    ui.colored_label(Color32::LIGHT_RED, error);
                }
                if self.entries.is_empty() {
                    self.rows.cancel();
                    ui.weak(match self.tab {
                        Tab::History => "Recently played files appear here.",
                        Tab::Playlist => "Drop files here or choose Add Files.",
                        Tab::Favorites => "Add favorites from a video's context menu.",
                        Tab::Subtitles => unreachable!(),
                    });
                    return action;
                }
                let list = self.list();
                let selected = playing.and_then(|(current, queue)| {
                    queue
                        .filter(|entry| list == List::Playlist(entry.playlist))
                        .map(|entry| entry.item)
                        .or_else(|| {
                            self.entries
                                .iter()
                                .find(|entry| entry.path == current)
                                .map(|entry| entry.id)
                        })
                });
                if let Some(edit) = self.rows.show(
                    ui,
                    list,
                    &self.entries,
                    selected,
                    MenuState {
                        favorites: &self.favorites,
                        show_clock,
                    },
                    &self.db,
                ) {
                    action = action.or(self.apply(edit, list, ui.ctx()));
                    ui.ctx().request_repaint();
                }
                action
            })
            .inner;
        if self.view() != previous_view {
            self.save_view();
        }
        action
    }

    fn apply(&mut self, edit: Edit, list: List, ctx: &egui::Context) -> Option<Action> {
        let result = match edit {
            Edit::Time(action) => return Some(Action::Time(action)),
            Edit::Open(id, path) => {
                let queue = match list {
                    List::Playlist(playlist) => Some(QueueItem { playlist, item: id }),
                    _ => None,
                };
                return Some(Action::Open(path, queue));
            }
            Edit::Reveal(path) => {
                let errors = self.reveal_error.0.clone();
                let ctx = ctx.clone();
                std::thread::spawn(move || {
                    if let Err(error) = file_manager::reveal(&path) {
                        let _ = errors.send(format!("{error:#}"));
                        ctx.request_repaint();
                    }
                });
                return None;
            }
            Edit::Remove(id, path) => match list {
                List::History => {
                    let result = self.db.remove_history(id);
                    let action = result.is_ok().then_some(Action::Forget(path));
                    self.record(result);
                    return action;
                }
                List::Playlist(_) => self.db.remove_item(id),
                List::Favorites => self.db.set_favorite(&path, false),
            },
            Edit::Favorite(path, enabled) => self.db.set_favorite(&path, enabled),
            Edit::Reorder(id, before) => self.db.reorder(list, id, before),
        };
        self.record(result);
        None
    }

    fn playlist_controls(&mut self, ui: &mut egui::Ui) -> Option<Action> {
        let prior = self.selected;
        let name = self
            .playlists
            .iter()
            .find(|p| p.id == prior)
            .map_or("Default Playlist", |p| p.name.as_str());
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("open-playlist")
                .selected_text("Open Playlist")
                .width(107.0)
                .show_ui(ui, |ui| {
                    for playlist in &self.playlists {
                        ui.selectable_value(&mut self.selected, playlist.id, &playlist.name);
                    }
                });
            ui.add(egui::Label::new(name).truncate());
        });
        if self.selected != prior {
            self.renaming = false;
            self.dirty = true;
        }
        ui.horizontal(|ui| {
            let input = ui.add(
                egui::TextEdit::singleline(&mut self.playlist_name)
                    .hint_text("Playlist name")
                    .desired_width(126.0),
            );
            self.edit_focused = input.has_focus();
            let confirmed = ui
                .button(if self.renaming { "Save" } else { "Create" })
                .clicked()
                || (input.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
            let name = self.playlist_name.trim();
            if confirmed && !name.is_empty() {
                let result = if self.renaming {
                    self.db.rename(self.selected, name)
                } else {
                    self.db.create(name).map(|id| self.selected = id)
                };
                if result.is_ok() {
                    self.playlist_name.clear();
                    self.renaming = false;
                }
                self.record(result);
            }
        });
        let mut action = None;
        ui.horizontal_wrapped(|ui| {
            if ui.button("Add Files").clicked() {
                action = Some(Action::AddFiles);
            }
            if self.selected != DEFAULT_PLAYLIST {
                if ui.button("Rename").clicked() {
                    self.playlist_name = self
                        .playlists
                        .iter()
                        .find(|p| p.id == self.selected)
                        .map_or_else(String::new, |p| p.name.clone());
                    self.renaming = true;
                }
                if ui.button("Delete playlist").clicked() {
                    let result = self.db.remove_playlist(self.selected);
                    if result.is_ok() {
                        self.selected = DEFAULT_PLAYLIST;
                        self.renaming = false;
                    }
                    self.record(result);
                }
            }
        });
        action
    }
}
