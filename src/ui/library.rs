use std::{
    ffi::OsString,
    fs,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use super::preferences::{
    ClickAction, ClockSize, MediaSettings, MouseSettings, OverlayPosition, PlaybackSettings,
    SidebarView, SubtitleChoice, Tab, WHEEL_SEEK_RANGE,
};

pub struct Library(Connection);

pub const DEFAULT_PLAYLIST: i64 = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum List {
    History,
    Playlist(i64),
    Favorites,
}

pub struct Entry {
    pub id: i64,
    pub path: PathBuf,
    pub position: Option<f64>,
    pub duration: Option<f64>,
    pub finished: bool,
    pub has_cover: bool,
}

pub struct Playlist {
    pub id: i64,
    pub name: String,
}

impl Library {
    pub fn open() -> Result<Self> {
        let dir = std::env::var_os("XDG_DATA_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
            })
            .context("Local data directory is unavailable")?
            .join("rvideoplayer");
        fs::create_dir_all(&dir)?;
        Self::init(Connection::open(dir.join("library.sqlite3"))?)
    }

    pub fn temporary() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut db: Connection) -> Result<Self> {
        db.busy_timeout(Duration::from_millis(80))?;
        db.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS history (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 path BLOB NOT NULL UNIQUE,
                 position REAL NOT NULL,
                 duration REAL,
                 finished INTEGER NOT NULL,
                 touched INTEGER NOT NULL,
                 cover BLOB
             );
             CREATE TABLE IF NOT EXISTS playlists (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 name TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS items (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 playlist INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
                 ordinal INTEGER NOT NULL,
                 path BLOB NOT NULL
             );
             CREATE INDEX IF NOT EXISTS items_order ON items(playlist, ordinal, id);
             CREATE INDEX IF NOT EXISTS items_path ON items(path);
             CREATE TABLE IF NOT EXISTS playback_session (
                 id INTEGER PRIMARY KEY CHECK(id = 1),
                 history_id INTEGER REFERENCES history(id) ON DELETE SET NULL
             );
             INSERT OR IGNORE INTO playback_session (id, history_id)
                 VALUES (1, (SELECT id FROM history ORDER BY touched DESC, id DESC LIMIT 1));
             CREATE TABLE IF NOT EXISTS history_order (
                 history_id INTEGER PRIMARY KEY REFERENCES history(id) ON DELETE CASCADE,
                 ordinal INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS favorites (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 path BLOB NOT NULL UNIQUE,
                 ordinal INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS media_details (
                 path BLOB PRIMARY KEY,
                 duration REAL,
                 cover BLOB
             );
             CREATE TABLE IF NOT EXISTS ui_preferences (
                 id INTEGER PRIMARY KEY CHECK(id = 1),
                 sidebar_open INTEGER NOT NULL DEFAULT 0,
                 sidebar_tab INTEGER NOT NULL DEFAULT 0,
                 selected_playlist INTEGER NOT NULL DEFAULT 0,
                 single_click INTEGER NOT NULL DEFAULT 1,
                 double_click INTEGER NOT NULL DEFAULT 2,
                 wheel_seconds INTEGER NOT NULL DEFAULT 5
             );
             INSERT OR IGNORE INTO ui_preferences (id) VALUES (1);
             CREATE TABLE IF NOT EXISTS playback_preferences (
                 id INTEGER PRIMARY KEY CHECK(id = 1),
                 rate REAL NOT NULL DEFAULT 1,
                 volume REAL NOT NULL DEFAULT 1,
                 muted INTEGER NOT NULL DEFAULT 0,
                 voice INTEGER NOT NULL DEFAULT 0,
                 captions INTEGER NOT NULL DEFAULT 0,
                 follow INTEGER NOT NULL DEFAULT 1,
                 show_ab INTEGER NOT NULL DEFAULT 1
             );
             INSERT OR IGNORE INTO playback_preferences (id) VALUES (1);
             CREATE TABLE IF NOT EXISTS media_preferences (
                 path BLOB PRIMARY KEY,
                 subtitle_kind INTEGER NOT NULL,
                 subtitle_track INTEGER,
                 subtitle_path BLOB,
                 video_track INTEGER,
                 audio_track INTEGER,
                 loop_a REAL,
                 loop_b REAL
             );",
        )?;
        let schema = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (table, column, definition) in [
            (
                "playback_preferences",
                "show_ab",
                "INTEGER NOT NULL DEFAULT 1",
            ),
            (
                "playback_preferences",
                "show_clock",
                "INTEGER NOT NULL DEFAULT 0",
            ),
            (
                "playback_preferences",
                "clock_size",
                "INTEGER NOT NULL DEFAULT 1",
            ),
            ("media_preferences", "clock_x", "REAL"),
            ("media_preferences", "clock_y", "REAL"),
            ("media_preferences", "caption_x", "REAL"),
            ("media_preferences", "caption_y", "REAL"),
        ] {
            let exists: bool = schema.query_row(
                "SELECT EXISTS (SELECT 1 FROM pragma_table_info(?1) WHERE name=?2)",
                [table, column],
                |row| row.get(0),
            )?;
            if !exists {
                schema.execute(
                    &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
                    [],
                )?;
            }
        }
        schema.commit()?;
        for table in ["history", "items", "favorites"] {
            db.execute_batch(&format!(
                "CREATE TRIGGER IF NOT EXISTS {table}_prune_details AFTER DELETE ON {table}
                 BEGIN
                     DELETE FROM media_details WHERE path = OLD.path
                         AND NOT EXISTS (SELECT 1 FROM history WHERE path = OLD.path)
                         AND NOT EXISTS (SELECT 1 FROM items WHERE path = OLD.path)
                         AND NOT EXISTS (SELECT 1 FROM favorites WHERE path = OLD.path);
                 END;"
            ))?;
        }
        db.execute(
            "INSERT OR IGNORE INTO playlists (id, name) VALUES (?1, 'Default Playlist')",
            [DEFAULT_PLAYLIST],
        )?;
        Ok(Self(db))
    }

    pub fn sidebar_view(&self) -> Result<SidebarView> {
        Ok(self.0.query_row(
            "SELECT sidebar_open, sidebar_tab,
                    COALESCE((SELECT id FROM playlists WHERE id = selected_playlist), 0)
             FROM ui_preferences WHERE id = 1",
            [],
            |row| {
                Ok(SidebarView {
                    open: row.get(0)?,
                    tab: match row.get(1)? {
                        1_i64 => Tab::Playlist,
                        2 => Tab::Favorites,
                        3 => Tab::Subtitles,
                        _ => Tab::History,
                    },
                    playlist: row.get(2)?,
                })
            },
        )?)
    }

    pub fn playback_settings(&self) -> Result<PlaybackSettings> {
        Ok(self.0.query_row(
            "SELECT rate, volume, muted, voice, captions, follow, show_ab, show_clock, clock_size
             FROM playback_preferences WHERE id = 1",
            [],
            |row| {
                let rate: f64 = row.get(0)?;
                let volume: f32 = row.get(1)?;
                Ok(PlaybackSettings {
                    rate: if (0.25..=2.0).contains(&rate) {
                        rate
                    } else {
                        1.0
                    },
                    volume: if (0.0..=2.0).contains(&volume) {
                        volume
                    } else {
                        1.0
                    },
                    muted: row.get(2)?,
                    voice: row.get(3)?,
                    captions: row.get(4)?,
                    follow: row.get(5)?,
                    show_ab: row.get(6)?,
                    show_clock: row.get(7)?,
                    clock_size: match row.get(8)? {
                        0_i64 => ClockSize::Small,
                        2 => ClockSize::Large,
                        _ => ClockSize::Medium,
                    },
                })
            },
        )?)
    }

    pub fn save_playback_settings(&self, settings: PlaybackSettings) -> Result<()> {
        self.0.execute(
            "UPDATE playback_preferences SET rate=?1, volume=?2, muted=?3, voice=?4, captions=?5, follow=?6, show_ab=?7,
                    show_clock=?8, clock_size=?9 WHERE id=1",
            params![settings.rate, settings.volume, settings.muted, settings.voice, settings.captions, settings.follow,
                    settings.show_ab, settings.show_clock, settings.clock_size as u8],
        )?;
        Ok(())
    }

    pub fn media_settings(&self, path: &Path) -> Result<MediaSettings> {
        Ok(self.0.query_row(
            "SELECT subtitle_kind, subtitle_track, subtitle_path, loop_a, loop_b, video_track, audio_track,
                    clock_x, clock_y, caption_x, caption_y
             FROM media_preferences WHERE path=?1",
            [path.as_os_str().as_bytes()],
            |row| {
                let a: Option<f64> = row.get(3)?;
                let b: Option<f64> = row.get(4)?;
                let a = a.filter(|a| a.is_finite() && *a >= 0.0);
                Ok(MediaSettings {
                    subtitles: match row.get(0)? {
                        1_i64 => row.get::<_, Option<u32>>(1)?.map_or(SubtitleChoice::Auto, SubtitleChoice::Embedded),
                        2 => SubtitleChoice::Sidecar(row.get::<_, Option<Vec<u8>>>(2)?.map(load_path)),
                        _ => SubtitleChoice::Auto,
                    },
                    a,
                    b: b.filter(|b| b.is_finite() && a.is_some_and(|a| *b - a >= 0.08)),
                    video: row.get(5)?,
                    audio: row.get(6)?,
                    clock_position: overlay_position(row.get(7)?, row.get(8)?),
                    caption_position: overlay_position(row.get(9)?, row.get(10)?),
                })
            },
        ).optional()?.unwrap_or_default())
    }

    pub fn save_media_settings(&self, path: &Path, settings: &MediaSettings) -> Result<()> {
        let (kind, track, sidecar) = match &settings.subtitles {
            SubtitleChoice::Auto => (0, None, None),
            SubtitleChoice::Embedded(track) => (1, Some(*track), None),
            SubtitleChoice::Sidecar(path) => {
                (2, None, path.as_ref().map(|p| p.as_os_str().as_bytes()))
            }
        };
        self.0.execute(
            "INSERT INTO media_preferences(path, subtitle_kind, subtitle_track, subtitle_path, loop_a, loop_b, video_track, audio_track,
                                           clock_x, clock_y, caption_x, caption_y)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(path) DO UPDATE SET subtitle_kind=excluded.subtitle_kind, subtitle_track=excluded.subtitle_track,
                                             subtitle_path=excluded.subtitle_path, loop_a=excluded.loop_a, loop_b=excluded.loop_b,
                                             video_track=excluded.video_track, audio_track=excluded.audio_track,
                                             clock_x=excluded.clock_x, clock_y=excluded.clock_y,
                                             caption_x=excluded.caption_x, caption_y=excluded.caption_y",
             params![path.as_os_str().as_bytes(), kind, track, sidecar, settings.a, settings.b, settings.video, settings.audio,
                     settings.clock_position.map(|p| p.x), settings.clock_position.map(|p| p.y),
                     settings.caption_position.map(|p| p.x), settings.caption_position.map(|p| p.y)],
        )?;
        Ok(())
    }

    pub fn save_sidebar_view(&self, view: SidebarView) -> Result<()> {
        self.0.execute(
            "UPDATE ui_preferences SET sidebar_open = ?1, sidebar_tab = ?2,
                                      selected_playlist = ?3 WHERE id = 1",
            params![view.open, view.tab as u8, view.playlist],
        )?;
        Ok(())
    }

    pub fn mouse_settings(&self) -> Result<MouseSettings> {
        let defaults = MouseSettings::default();
        let action = |id: i64, default| {
            ClickAction::CHOICES
                .into_iter()
                .find(|a| *a as i64 == id)
                .unwrap_or(default)
        };
        Ok(self.0.query_row(
            "SELECT single_click, double_click, wheel_seconds FROM ui_preferences WHERE id = 1",
            [],
            |row| {
                Ok(MouseSettings {
                    single: action(row.get(0)?, defaults.single),
                    double: action(row.get(1)?, defaults.double),
                    wheel_seconds: u8::try_from(row.get::<_, i64>(2)?)
                        .ok()
                        .filter(|n| WHEEL_SEEK_RANGE.contains(n))
                        .unwrap_or(defaults.wheel_seconds),
                })
            },
        )?)
    }

    pub fn save_mouse_settings(&self, settings: MouseSettings) -> Result<()> {
        self.0.execute(
            "UPDATE ui_preferences SET single_click = ?1, double_click = ?2,
                                      wheel_seconds = ?3 WHERE id = 1",
            params![
                settings.single as u8,
                settings.double as u8,
                settings.wheel_seconds
            ],
        )?;
        Ok(())
    }

    pub fn last_video(&self) -> Result<Option<(PathBuf, f64)>> {
        Ok(self
            .0
            .query_row(
                "SELECT h.path, CASE WHEN h.finished THEN 0.0 ELSE h.position END
                 FROM playback_session s JOIN history h ON h.id = s.history_id WHERE s.id = 1",
                [],
                |row| Ok((load_path(row.get(0)?), row.get(1)?)),
            )
            .optional()?)
    }

    pub fn resume(&self, path: &Path) -> Result<f64> {
        Ok(self
            .0
            .query_row(
                "SELECT position FROM history WHERE path = ?1 AND finished = 0",
                [path.as_os_str().as_bytes()],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0.0))
    }

    pub fn save(
        &self,
        path: &Path,
        position: f64,
        duration: Option<f64>,
        finished: bool,
        cover: Option<&[u8]>,
    ) -> Result<()> {
        let tx = self.0.unchecked_transaction()?;
        let id: i64 = tx.query_row(
            "INSERT INTO history (path, position, duration, finished, touched)
             VALUES (?1, ?2, ?3, ?4, unixepoch('subsec') * 1000)
             ON CONFLICT(path) DO UPDATE SET position = excluded.position,
                 duration = COALESCE(excluded.duration, history.duration),
                 finished = excluded.finished, touched = excluded.touched
             RETURNING id",
            params![path.as_os_str().as_bytes(), position, duration, finished],
            |row| row.get(0),
        )?;
        tx.execute(
            "UPDATE playback_session SET history_id = ?1 WHERE id = 1",
            [id],
        )?;
        if cover.is_some() {
            save_details(&tx, path, duration, cover)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn entries(&self, list: List) -> Result<Vec<Entry>> {
        entries(&self.0, list)
    }

    pub fn favorite_paths(&self) -> Result<std::collections::HashSet<PathBuf>> {
        let mut query = self.0.prepare("SELECT path FROM favorites")?;
        Ok(query
            .query_map([], |r| Ok(load_path(r.get(0)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_favorite(&mut self, path: &Path, favorite: bool) -> Result<()> {
        let tx = self
            .0
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let path = path.as_os_str().as_bytes();
        if favorite {
            tx.execute(
                "INSERT OR IGNORE INTO favorites (path, ordinal)
                 SELECT ?1, COALESCE(MAX(ordinal) + 1, 0) FROM favorites",
                [path],
            )?;
            tx.execute(
                "INSERT INTO media_details (path, duration, cover)
                 SELECT path, duration, cover FROM history WHERE path = ?1
                 ON CONFLICT(path) DO UPDATE SET
                     duration = COALESCE(media_details.duration, excluded.duration),
                     cover = COALESCE(media_details.cover, excluded.cover)",
                [path],
            )?;
        } else {
            tx.execute("DELETE FROM favorites WHERE path = ?1", [path])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn save_details(
        &self,
        path: &Path,
        duration: Option<f64>,
        cover: Option<&[u8]>,
    ) -> Result<()> {
        save_details(&self.0, path, duration, cover)
    }

    pub fn cover(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        Ok(self.0.query_row(
            "SELECT COALESCE((SELECT cover FROM media_details WHERE path = ?1),
                                        (SELECT cover FROM history WHERE path = ?1))",
            [path.as_os_str().as_bytes()],
            |r| r.get(0),
        )?)
    }

    pub fn remove_history(&self, id: i64) -> Result<()> {
        self.0.execute("DELETE FROM history WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn clear_history(&self) -> Result<()> {
        self.0.execute("DELETE FROM history", [])?;
        Ok(())
    }

    pub fn playlists(&self) -> Result<Vec<Playlist>> {
        let mut query = self
            .0
            .prepare("SELECT id, name FROM playlists ORDER BY id")?;
        Ok(query
            .query_map([], |r| {
                Ok(Playlist {
                    id: r.get(0)?,
                    name: r.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn create(&self, name: &str) -> Result<i64> {
        self.0
            .execute("INSERT INTO playlists (name) VALUES (?1)", [name])?;
        Ok(self.0.last_insert_rowid())
    }

    pub fn rename(&self, id: i64, name: &str) -> Result<()> {
        self.0.execute(
            "UPDATE playlists SET name = ?2 WHERE id = ?1 AND id <> ?3",
            params![id, name, DEFAULT_PLAYLIST],
        )?;
        Ok(())
    }

    pub fn remove_playlist(&self, id: i64) -> Result<()> {
        self.0.execute(
            "DELETE FROM playlists WHERE id = ?1 AND id <> ?2",
            params![id, DEFAULT_PLAYLIST],
        )?;
        Ok(())
    }

    pub fn clear_playlist(&self, id: i64) -> Result<()> {
        self.0
            .execute("DELETE FROM items WHERE playlist = ?1", [id])?;
        Ok(())
    }

    pub fn append(&mut self, playlist: i64, paths: &[PathBuf]) -> Result<Option<i64>> {
        let tx = self
            .0
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut first = None;
        for path in paths {
            tx.execute(
                "INSERT INTO items (playlist, ordinal, path)
                 SELECT ?1, COALESCE(MAX(ordinal) + 1, 0), ?2 FROM items WHERE playlist = ?1",
                params![playlist, path.as_os_str().as_bytes()],
            )?;
            first.get_or_insert(tx.last_insert_rowid());
        }
        tx.commit()?;
        Ok(first)
    }

    pub fn remove_item(&self, id: i64) -> Result<()> {
        self.0.execute("DELETE FROM items WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn reorder(&mut self, list: List, id: i64, before: Option<i64>) -> Result<()> {
        let tx = self
            .0
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut order: Vec<_> = entries(&tx, list)?
            .into_iter()
            .map(|entry| entry.id)
            .collect();
        let Some(index) = order.iter().position(|&entry| entry == id) else {
            return Ok(());
        };
        order.remove(index);
        let other = match before {
            None => order.len(),
            Some(before) => {
                let Some(index) = order.iter().position(|&entry| entry == before) else {
                    return Ok(());
                };
                index
            }
        };
        if index == other {
            return Ok(());
        }
        order.insert(other, id);
        let sql = match list {
            List::History => {
                "INSERT INTO history_order (history_id, ordinal) VALUES (?1, ?2)
                              ON CONFLICT(history_id) DO UPDATE SET ordinal = excluded.ordinal"
            }
            List::Playlist(_) => "UPDATE items SET ordinal = ?2 WHERE id = ?1",
            List::Favorites => "UPDATE favorites SET ordinal = ?2 WHERE id = ?1",
        };
        let mut update = tx.prepare(sql)?;
        for (ordinal, id) in order.into_iter().enumerate() {
            update.execute(params![id, ordinal as i64])?;
        }
        drop(update);
        tx.commit()?;
        Ok(())
    }
}

fn entries(db: &Connection, list: List) -> Result<Vec<Entry>> {
    let (sql, playlist) = match list {
        List::History => (
            "SELECT l.id, l.path, l.position, COALESCE(l.duration, m.duration),
                                  l.finished, COALESCE(m.cover, l.cover) IS NOT NULL
                           FROM history l LEFT JOIN media_details m ON m.path = l.path
                           LEFT JOIN history_order o ON o.history_id = l.id
                           ORDER BY o.ordinal IS NOT NULL, o.ordinal, l.touched DESC, l.id DESC",
            None,
        ),
        List::Playlist(id) => (
            "SELECT l.id, l.path, NULL, COALESCE(h.duration, m.duration),
                                       0, COALESCE(m.cover, h.cover) IS NOT NULL
                               FROM items l LEFT JOIN media_details m ON m.path = l.path
                               LEFT JOIN history h ON h.path = l.path
                               WHERE l.playlist = ?1 ORDER BY l.ordinal, l.id",
            Some(id),
        ),
        List::Favorites => (
            "SELECT l.id, l.path, NULL, COALESCE(h.duration, m.duration),
                                    0, COALESCE(m.cover, h.cover) IS NOT NULL
                            FROM favorites l LEFT JOIN media_details m ON m.path = l.path
                            LEFT JOIN history h ON h.path = l.path ORDER BY l.ordinal, l.id",
            None,
        ),
    };
    let mut query = db.prepare(sql)?;
    Ok(query
        .query_map(rusqlite::params_from_iter(playlist), |r| {
            Ok(Entry {
                id: r.get(0)?,
                path: load_path(r.get(1)?),
                position: r.get(2)?,
                duration: r.get(3)?,
                finished: r.get(4)?,
                has_cover: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

fn save_details(
    db: &Connection,
    path: &Path,
    duration: Option<f64>,
    cover: Option<&[u8]>,
) -> Result<()> {
    db.execute(
        "INSERT INTO media_details (path, duration, cover) SELECT ?1, ?2, ?3
         WHERE EXISTS (SELECT 1 FROM history WHERE path = ?1)
            OR EXISTS (SELECT 1 FROM items WHERE path = ?1)
            OR EXISTS (SELECT 1 FROM favorites WHERE path = ?1)
                ON CONFLICT(path) DO UPDATE SET
                    duration = COALESCE(excluded.duration, media_details.duration),
                    cover = COALESCE(media_details.cover, excluded.cover)",
        params![path.as_os_str().as_bytes(), duration, cover],
    )?;
    Ok(())
}

pub fn title(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

fn load_path(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes))
}

fn overlay_position(x: Option<f32>, y: Option<f32>) -> Option<OverlayPosition> {
    let (x, y) = (x?, y?);
    ((0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y)).then_some(OverlayPosition { x, y })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_and_mouse_preferences_survive_reopening() -> Result<()> {
        let db = Library::temporary()?;
        assert_eq!(db.sidebar_view()?, SidebarView::default());
        assert_eq!(db.mouse_settings()?, MouseSettings::default());

        let playlist = db.create("Weekend")?;
        let view = SidebarView {
            open: true,
            tab: Tab::Playlist,
            playlist,
        };
        let mouse = MouseSettings {
            single: ClickAction::Nothing,
            double: ClickAction::Playback,
            wheel_seconds: 13,
        };
        db.save_sidebar_view(view)?;
        db.save_mouse_settings(mouse)?;
        let db = Library::init(db.0)?;
        assert_eq!(db.sidebar_view()?, view);
        assert_eq!(db.mouse_settings()?, mouse);

        db.save_sidebar_view(SidebarView {
            open: false,
            ..view
        })?;
        db.remove_playlist(playlist)?;
        assert_eq!(
            db.sidebar_view()?,
            SidebarView {
                open: false,
                playlist: DEFAULT_PLAYLIST,
                ..view
            }
        );
        assert_eq!(db.mouse_settings()?, mouse);
        Ok(())
    }

    #[test]
    fn learning_settings_survive_reopening_and_are_scoped_to_the_file() -> Result<()> {
        let db = Library::temporary()?;
        assert_eq!(db.playback_settings()?, PlaybackSettings::default());
        let playback = PlaybackSettings {
            rate: 1.25,
            volume: 1.8,
            muted: true,
            voice: true,
            captions: true,
            follow: false,
            show_ab: false,
            show_clock: true,
            clock_size: ClockSize::Large,
        };
        let view = SidebarView {
            open: true,
            tab: Tab::Subtitles,
            playlist: DEFAULT_PLAYLIST,
        };
        let first = Path::new("first.mp4");
        let second = Path::new("second.mkv");
        let first_media = MediaSettings {
            subtitles: SubtitleChoice::Sidecar(Some(load_path(b"/tmp/dialog-\xFF.ass".to_vec()))),
            video: Some(1),
            audio: Some(2),
            a: Some(6.12),
            b: Some(9.84),
            clock_position: Some(OverlayPosition { x: 0.30, y: 0.72 }),
            caption_position: Some(OverlayPosition { x: 0.62, y: 0.22 }),
        };
        let second_media = MediaSettings {
            subtitles: SubtitleChoice::Embedded(4),
            ..Default::default()
        };
        db.save_sidebar_view(view)?;
        db.save_playback_settings(playback)?;
        db.save_media_settings(first, &first_media)?;
        db.save_media_settings(second, &second_media)?;
        db.save(first, 8.12, Some(24.0), false, None)?;
        let db = Library::init(db.0)?;
        assert_eq!(db.last_video()?, Some((first.to_owned(), 8.12)));
        assert_eq!(db.sidebar_view()?, view);
        assert_eq!(db.playback_settings()?, playback);
        assert_eq!(db.media_settings(first)?, first_media);
        assert_eq!(db.media_settings(second)?, second_media);
        assert_eq!(
            db.media_settings(Path::new("new.mp4"))?,
            MediaSettings::default()
        );
        Ok(())
    }

    #[test]
    fn existing_playback_settings_gain_visible_ab_controls() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.execute_batch(
            "CREATE TABLE playback_preferences (
                 id INTEGER PRIMARY KEY,
                 rate REAL, volume REAL, muted INTEGER, voice INTEGER, captions INTEGER, follow INTEGER
             );
             INSERT INTO playback_preferences VALUES (1, 1.25, 1.5, 0, 1, 0, 0);",
        )?;
        let db = Library::init(db)?;
        assert_eq!(
            db.playback_settings()?,
            PlaybackSettings {
                rate: 1.25,
                volume: 1.5,
                voice: true,
                follow: false,
                ..Default::default()
            }
        );
        Ok(())
    }

    #[test]
    fn positions_completion_and_native_paths() -> Result<()> {
        let db = Library::temporary()?;
        let path = load_path(b"/tmp/video-\xFF.mp4".to_vec());
        db.save(&path, 12.75, Some(100.0), false, Some(b"cover"))?;
        assert_eq!(db.resume(&path)?, 12.75);
        db.save(&path, 100.0, None, true, None)?;
        assert_eq!(db.resume(&path)?, 0.0);
        let entry = db.entries(List::History)?.pop().unwrap();
        assert_eq!(entry.path, path);
        assert_eq!(entry.duration, Some(100.0));
        assert!(entry.has_cover);
        assert_eq!(db.cover(&entry.path)?.as_deref(), Some(&b"cover"[..]));
        db.remove_history(entry.id)?;
        assert!(db.entries(List::History)?.is_empty());
        Ok(())
    }

    #[test]
    fn playlists_keep_order_and_duplicate_paths() -> Result<()> {
        let mut db = Library::temporary()?;
        let playlist = db.create("Test")?;
        let paths = ["video-a", "video-b", "video-a"].map(PathBuf::from);
        let first = db.append(playlist, &paths)?.unwrap();
        let before = db.entries(List::Playlist(playlist))?[2].id;
        db.reorder(List::Playlist(playlist), first, Some(before))?;
        let items = db.entries(List::Playlist(playlist))?;
        assert_eq!(items[0].path, paths[1]);
        assert_eq!(items[1].id, first);
        assert_ne!(items[1].id, items[2].id);
        db.remove_playlist(playlist)?;
        assert!(db.entries(List::Playlist(playlist))?.is_empty());
        Ok(())
    }

    #[test]
    fn session_tracks_latest_successful_save_and_clears_with_history() -> Result<()> {
        let db = Library::temporary()?;
        let first = PathBuf::from("first.mp4");
        let second = PathBuf::from("second.mp4");
        db.save(&first, 1.0, None, false, None)?;
        db.save(&second, 2.0, None, false, None)?;
        db.save(&first, 23.45678, None, false, None)?;
        let db = Library::init(db.0)?;
        assert_eq!(db.last_video()?, Some((first.clone(), 23.45678)));
        db.save(&first, 96.0, None, true, None)?;
        assert_eq!(db.last_video()?, Some((first, 0.0)));
        db.clear_history()?;
        assert!(Library::init(db.0)?.last_video()?.is_none());
        Ok(())
    }

    #[test]
    fn default_playlist_survives_clear_and_reopening() -> Result<()> {
        let mut db = Library::temporary()?;
        let other = db.create("Weekend")?;
        let paths = ["a.mp4", "b.mp4", "c.mp4", "d.mp4"].map(PathBuf::from);
        let first = db.append(DEFAULT_PLAYLIST, &paths)?.unwrap();
        db.append(other, &paths[..1])?;
        db.reorder(List::Playlist(DEFAULT_PLAYLIST), first, None)?;
        db.remove_playlist(DEFAULT_PLAYLIST)?;
        db.rename(DEFAULT_PLAYLIST, "changed")?;
        let db = Library::init(db.0)?;
        let items = db.entries(List::Playlist(DEFAULT_PLAYLIST))?;
        let order: Vec<_> = items.iter().map(|item| &item.path).collect();
        assert_eq!(order, [&paths[1], &paths[2], &paths[3], &paths[0]]);
        db.clear_playlist(DEFAULT_PLAYLIST)?;
        assert!(db.entries(List::Playlist(DEFAULT_PLAYLIST))?.is_empty());
        assert_eq!(db.entries(List::Playlist(other))?.len(), 1);
        let lists = db.playlists()?;
        assert_eq!(lists[0].id, DEFAULT_PLAYLIST);
        assert_eq!(lists[0].name, "Default Playlist");
        Ok(())
    }

    #[test]
    fn favorites_keep_order_and_artwork_when_history_is_cleared() -> Result<()> {
        let mut db = Library::temporary()?;
        let a = Path::new("a.mp4");
        let b = Path::new("b.mp4");
        db.save(a, 1.0, Some(12.0), false, Some(b"cover"))?;
        db.set_favorite(a, true)?;
        db.set_favorite(b, true)?;
        db.set_favorite(a, true)?;
        let favorites = db.entries(List::Favorites)?;
        db.reorder(List::Favorites, favorites[1].id, Some(favorites[0].id))?;
        db.clear_history()?;
        let mut db = Library::init(db.0)?;
        let favorites = db.entries(List::Favorites)?;
        assert_eq!(favorites[0].path, b);
        assert_eq!(favorites[1].path, a);
        assert_eq!(favorites[1].duration, Some(12.0));
        assert_eq!(db.cover(a)?.as_deref(), Some(&b"cover"[..]));
        db.set_favorite(a, false)?;
        assert!(db.cover(a)?.is_none());
        Ok(())
    }

    #[test]
    fn manual_history_order_survives_playback_updates_and_reopening() -> Result<()> {
        let mut db = Library::temporary()?;
        for path in ["first", "second", "third"] {
            db.save(Path::new(path), 0.0, None, false, None)?;
        }
        let recent = db.entries(List::History)?;
        let last = recent[2].path.clone();
        db.reorder(List::History, recent[0].id, None)?;
        db.save(&last, 2.5, None, false, None)?;
        let db = Library::init(db.0)?;
        let history = db.entries(List::History)?;
        assert_eq!(
            [history[0].id, history[1].id, history[2].id],
            [recent[1].id, recent[2].id, recent[0].id]
        );
        assert_eq!(db.last_video()?, Some((last, 2.5)));
        Ok(())
    }
}
