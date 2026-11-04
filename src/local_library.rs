//! The local library is the source of truth. Display names never become paths.
use crate::client_backend::{EpisodeProgress, UserAnimeListItem};
use crate::details::AnimeDetails;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

pub const OFFLINE: &str =
    "Tryb lokalny: włącz połączenia z Shinden w Ustawieniach, aby pobrać brakujące dane.";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryProfile {
    pub id: String,
    pub name: String,
    pub public_profile_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryState {
    pub active_profile: LibraryProfile,
    pub profiles: Vec<LibraryProfile>,
    pub network_enabled: bool,
    pub import_enabled: bool,
    pub authenticated: bool,
    pub database_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryTitle {
    pub item: UserAnimeListItem,
    pub details: Option<AnimeDetails>,
}

impl LibraryTitle {
    pub fn new(id: u64, name: &str, url: &str) -> Self {
        Self {
            item: UserAnimeListItem {
                title_id: id,
                name: name.into(),
                url: url.into(),
                image_url: String::new(),
                anime_type: String::new(),
                rating: String::new(),
                episodes: String::new(),
                description: String::new(),
                watch_status: "no".into(),
                is_favourite: 0,
                watched_episodes_count: 0,
                total_episodes: None,
                release_year: None,
                release_date: None,
                tags: vec![],
                age_rating: None,
                detail_metadata_loaded: false,
                active: false,
                updated_at_ms: now(),
            },
            details: None,
        }
    }
}

pub struct LocalLibrary {
    connection: Mutex<Connection>,
    path: PathBuf,
}

pub fn default_database_path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("SHINDEN_LIBRARY_PATH") {
        return Ok(path.into());
    }
    Ok(dirs::data_local_dir()
        .ok_or("Nie znaleziono lokalnego katalogu danych")?
        .join("shinden-client")
        .join("library.sqlite3"))
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn error(e: impl std::fmt::Display) -> String {
    format!("Błąd lokalnej bazy: {e}")
}

impl LocalLibrary {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(error)?;
        }
        let connection = Connection::open(path).map_err(error)?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(error)?;
        let version: u32 = connection
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(error)?;
        if version > 1 {
            return Err("Baza pochodzi z nowszej wersji aplikacji. Zaktualizuj aplikację.".into());
        }
        connection.execute_batch("PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS profiles(id TEXT PRIMARY KEY, name TEXT NOT NULL, public_id INTEGER);
            INSERT OR IGNORE INTO profiles VALUES('guest','Guest',NULL);
            CREATE TABLE IF NOT EXISTS preferences(key TEXT PRIMARY KEY, value TEXT NOT NULL);
            INSERT OR IGNORE INTO preferences VALUES('active_profile','guest'),('network','0'),('import','0');
            CREATE TABLE IF NOT EXISTS titles(profile TEXT NOT NULL REFERENCES profiles(id), title_id INTEGER NOT NULL, data TEXT NOT NULL, PRIMARY KEY(profile,title_id));
            CREATE TABLE IF NOT EXISTS episodes(profile TEXT NOT NULL REFERENCES profiles(id), title_id INTEGER NOT NULL, episode_no INTEGER NOT NULL, episode_id INTEGER, data TEXT NOT NULL, PRIMARY KEY(profile,title_id,episode_no));
            CREATE INDEX IF NOT EXISTS episode_ids ON episodes(profile,title_id,episode_id);
            CREATE TABLE IF NOT EXISTS resources(profile TEXT NOT NULL REFERENCES profiles(id), kind TEXT NOT NULL, key TEXT NOT NULL, data BLOB NOT NULL, PRIMARY KEY(profile,kind,key));
            PRAGMA user_version=1;").map_err(error)?;
        Ok(Self {
            connection: Mutex::new(connection),
            path: path.to_path_buf(),
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, String> {
        self.connection
            .lock()
            .map_err(|_| "Lokalna baza jest niedostępna".into())
    }

    pub fn state(&self) -> Result<LibraryState, String> {
        let c = self.lock()?;
        let mut stmt = c
            .prepare("SELECT id,name,public_id FROM profiles ORDER BY public_id IS NOT NULL,name")
            .map_err(error)?;
        let profiles = stmt
            .query_map([], |r| {
                Ok(LibraryProfile {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    public_profile_id: r.get(2)?,
                })
            })
            .map_err(error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(error)?;
        let active: String = c
            .query_row(
                "SELECT value FROM preferences WHERE key='active_profile'",
                [],
                |r| r.get(0),
            )
            .map_err(error)?;
        let active_profile = profiles
            .iter()
            .find(|p| p.id == active)
            .cloned()
            .ok_or("Nieprawidłowy profil lokalny")?;
        let flag = |key: &str| {
            c.query_row(
                "SELECT value='1' FROM preferences WHERE key=?",
                [key],
                |r| r.get(0),
            )
            .map_err(error)
        };
        Ok(LibraryState {
            active_profile,
            profiles,
            network_enabled: flag("network")?,
            import_enabled: flag("import")?,
            authenticated: false,
            database_path: self.path.display().to_string(),
        })
    }

    pub fn select_profile(&self, id: &str) -> Result<(), String> {
        let c = self.lock()?;
        let exists: bool = c
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM profiles WHERE id=?)",
                [id],
                |r| r.get(0),
            )
            .map_err(error)?;
        if !exists {
            return Err("Nie znaleziono profilu lokalnego".into());
        }
        c.execute(
            "UPDATE preferences SET value=? WHERE key='active_profile'",
            [id],
        )
        .map_err(error)?;
        Ok(())
    }

    pub fn activate_account(&self, public_id: u64, name: &str) -> Result<LibraryProfile, String> {
        if public_id == 0 || name.trim().is_empty() {
            return Err("Nieprawidłowy profil".into());
        }
        let profile = LibraryProfile {
            id: format!("user:{public_id}"),
            name: name.trim().into(),
            public_profile_id: Some(public_id),
        };
        let mut c = self.lock()?;
        let tx = c.transaction().map_err(error)?;
        tx.execute(
            "INSERT INTO profiles VALUES(?,?,?) ON CONFLICT(id) DO UPDATE SET name=excluded.name",
            params![profile.id, profile.name, public_id],
        )
        .map_err(error)?;
        tx.execute(
            "UPDATE preferences SET value=? WHERE key='active_profile'",
            [&profile.id],
        )
        .map_err(error)?;
        tx.commit().map_err(error)?;
        Ok(profile)
    }

    pub fn set_preferences(&self, network: bool, import: bool) -> Result<(), String> {
        let mut c = self.lock()?;
        let tx = c.transaction().map_err(error)?;
        for (key, enabled) in [("network", network), ("import", import)] {
            tx.execute(
                "UPDATE preferences SET value=? WHERE key=?",
                params![if enabled { "1" } else { "0" }, key],
            )
            .map_err(error)?;
        }
        tx.commit().map_err(error)
    }

    pub fn require_network(&self) -> Result<(), String> {
        if self.state()?.network_enabled {
            Ok(())
        } else {
            Err(OFFLINE.into())
        }
    }

    pub fn title(&self, profile: &str, id: u64) -> Result<Option<LibraryTitle>, String> {
        let c = self.lock()?;
        read_title(&c, profile, id)
    }

    pub fn titles(&self, profile: &str, active_only: bool) -> Result<Vec<LibraryTitle>, String> {
        let c = self.lock()?;
        let mut stmt = c
            .prepare("SELECT data FROM titles WHERE profile=? ORDER BY title_id")
            .map_err(error)?;
        let data = stmt
            .query_map([profile], |r| r.get::<_, String>(0))
            .map_err(error)?;
        let mut result = Vec::new();
        for row in data {
            let title: LibraryTitle = serde_json::from_str(&row.map_err(error)?).map_err(error)?;
            if !active_only || title.item.active {
                result.push(title);
            }
        }
        Ok(result)
    }

    /// Refresh metadata without overwriting local progress, categories or ratings.
    pub fn save_title(&self, profile: &str, title: &LibraryTitle) -> Result<(), String> {
        let c = self.lock()?;
        let mut title = title.clone();
        if let Some(old) = read_title(&c, profile, title.item.title_id)? {
            title.item.watch_status = old.item.watch_status;
            title.item.is_favourite = old.item.is_favourite;
            title.item.watched_episodes_count = old.item.watched_episodes_count;
            title.item.active = old.item.active;
            if title.item.release_year.is_none() {
                title.item.release_year = old.item.release_year;
            }
            if title.item.release_date.is_none() {
                title.item.release_date = old.item.release_date;
            }
            title.item.detail_metadata_loaded |= old.item.detail_metadata_loaded;
            if title.item.anime_type.is_empty() {
                title.item.anime_type = old.item.anime_type;
            }
            if title.item.rating.is_empty() {
                title.item.rating = old.item.rating;
            }
            if title.item.image_url.is_empty() {
                title.item.image_url = old.item.image_url;
            }
            if title.item.description.is_empty() {
                title.item.description = old.item.description;
            }
            if title.item.total_episodes.is_none() {
                title.item.total_episodes = old.item.total_episodes;
            }
            if title.item.tags.is_empty() {
                title.item.tags = old.item.tags;
            }
            if title.item.age_rating.is_none() {
                title.item.age_rating = old.item.age_rating;
            }
            if let Some(details) = title.details.as_mut() {
                if let Some(old_details) = old.details {
                    details.user_ratings = old_details.user_ratings;
                }
            } else {
                title.details = old.details;
            }
        }
        write_title(&c, profile, &mut title)
    }

    /// One transaction: a failed import cannot partially change a library.
    pub fn import_titles(&self, profile: &str, titles: &[LibraryTitle]) -> Result<usize, String> {
        let mut c = self.lock()?;
        let tx = c.transaction().map_err(error)?;
        let mut added = 0;
        for title in titles {
            let mut title = title.clone();
            update_fields(&mut title);
            added += tx
                .execute(
                    "INSERT OR IGNORE INTO titles VALUES(?,?,?)",
                    params![
                        profile,
                        title.item.title_id,
                        serde_json::to_string(&title).map_err(error)?
                    ],
                )
                .map_err(error)?;
        }
        tx.commit().map_err(error)?;
        Ok(added)
    }

    pub fn set_status(
        &self,
        profile: &str,
        id: u64,
        status: &str,
        favourite: Option<u8>,
    ) -> Result<(), String> {
        if ![
            "no",
            "in progress",
            "completed",
            "skip",
            "hold",
            "dropped",
            "plan",
        ]
        .contains(&status)
        {
            return Err("Nieprawidłowy status anime".into());
        }
        let c = self.lock()?;
        let mut title = read_title(&c, profile, id)?
            .ok_or("Najpierw otwórz anime, aby zapisać jego dane lokalnie")?;
        title.item.watch_status = status.into();
        title.item.active = status != "no";
        if let Some(value) = favourite {
            title.item.is_favourite = value.min(1);
        }
        write_title(&c, profile, &mut title)
    }

    pub fn set_rating(&self, profile: &str, id: u64, kind: &str, value: u8) -> Result<(), String> {
        let c = self.lock()?;
        let mut title = read_title(&c, profile, id)?.ok_or("Brak lokalnych danych anime")?;
        let details = title
            .details
            .as_mut()
            .ok_or("Najpierw pobierz szczegóły anime")?;
        let rating = match kind {
            "story" => &mut details.user_ratings.story,
            "graphics" => &mut details.user_ratings.graphics,
            "music" => &mut details.user_ratings.music,
            "characters" => &mut details.user_ratings.characters,
            "overall" | "total" => &mut details.user_ratings.overall,
            _ => return Err("Nieprawidłowy rodzaj oceny".into()),
        };
        *rating = value.min(10);
        write_title(&c, profile, &mut title)
    }

    pub fn save_episodes(
        &self,
        profile: &str,
        title_id: u64,
        episodes: &[EpisodeProgress],
    ) -> Result<(), String> {
        let mut c = self.lock()?;
        let tx = c.transaction().map_err(error)?;
        let title = read_title(&tx, profile, title_id)?;
        for episode in episodes {
            let old: Option<String> = tx
                .query_row(
                    "SELECT data FROM episodes WHERE profile=? AND title_id=? AND episode_no=?",
                    params![profile, title_id, episode.episode_no],
                    |r| r.get(0),
                )
                .optional()
                .map_err(error)?;
            let mut episode = episode.clone();
            if let Some(old) = old {
                let old: EpisodeProgress = serde_json::from_str(&old).map_err(error)?;
                episode.watched = old.watched;
                episode.view_count = old.view_count;
            } else if let Some(t) = &title {
                episode.watched =
                    episode.watched || episode.episode_no <= t.item.watched_episodes_count;
                episode.view_count = u32::from(episode.watched);
            }
            tx.execute("INSERT INTO episodes VALUES(?,?,?,?,?) ON CONFLICT(profile,title_id,episode_no) DO UPDATE SET episode_id=excluded.episode_id,data=excluded.data", params![profile,title_id,episode.episode_no,episode.episode_id,serde_json::to_string(&episode).map_err(error)?]).map_err(error)?;
        }
        tx.commit().map_err(error)
    }

    pub fn episodes(
        &self,
        profile: &str,
        title_id: u64,
    ) -> Result<Option<Vec<EpisodeProgress>>, String> {
        let c = self.lock()?;
        let mut stmt = c
            .prepare("SELECT data FROM episodes WHERE profile=? AND title_id=? ORDER BY episode_no")
            .map_err(error)?;
        let data = stmt
            .query_map(params![profile, title_id], |r| r.get::<_, String>(0))
            .map_err(error)?;
        let mut result = vec![];
        for row in data {
            result.push(serde_json::from_str(&row.map_err(error)?).map_err(error)?);
        }
        Ok(if result.is_empty() {
            None
        } else {
            Some(result)
        })
    }

    pub fn mark_episode(
        &self,
        profile: &str,
        title_id: u64,
        episode_id: u64,
        watched: bool,
    ) -> Result<(), String> {
        let mut c = self.lock()?;
        let tx = c.transaction().map_err(error)?;
        let raw: String = tx
            .query_row(
                "SELECT data FROM episodes WHERE profile=? AND title_id=? AND episode_id=?",
                params![profile, title_id, episode_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(error)?
            .ok_or("Nie znaleziono odcinka w lokalnej bazie")?;
        let mut ep: EpisodeProgress = serde_json::from_str(&raw).map_err(error)?;
        let mut title = read_title(&tx, profile, title_id)?.ok_or("Brak lokalnych danych anime")?;
        if ep.watched != watched {
            title.item.watched_episodes_count = if watched {
                title.item.watched_episodes_count.saturating_add(1)
            } else {
                title.item.watched_episodes_count.saturating_sub(1)
            };
        }
        ep.watched = watched;
        ep.view_count = u32::from(watched);
        tx.execute(
            "UPDATE episodes SET data=? WHERE profile=? AND title_id=? AND episode_no=?",
            params![
                serde_json::to_string(&ep).map_err(error)?,
                profile,
                title_id,
                ep.episode_no
            ],
        )
        .map_err(error)?;
        if watched && ["no", "plan"].contains(&title.item.watch_status.as_str()) {
            title.item.watch_status = "in progress".into();
            title.item.active = true;
        }
        write_title(&tx, profile, &mut title)?;
        tx.commit().map_err(error)
    }

    pub fn put<T: Serialize>(
        &self,
        profile: &str,
        kind: &str,
        key: &str,
        data: &T,
    ) -> Result<(), String> {
        self.put_bytes(
            profile,
            kind,
            key,
            &serde_json::to_vec(data).map_err(error)?,
        )
    }
    pub fn get<T: DeserializeOwned>(
        &self,
        profile: &str,
        kind: &str,
        key: &str,
    ) -> Result<Option<T>, String> {
        self.bytes(profile, kind, key)?
            .map(|data| serde_json::from_slice(&data).map_err(error))
            .transpose()
    }
    pub fn put_bytes(
        &self,
        profile: &str,
        kind: &str,
        key: &str,
        data: &[u8],
    ) -> Result<(), String> {
        self.lock()?.execute("INSERT INTO resources VALUES(?,?,?,?) ON CONFLICT(profile,kind,key) DO UPDATE SET data=excluded.data", params![profile,kind,key,data]).map_err(error)?;
        Ok(())
    }
    pub fn bytes(&self, profile: &str, kind: &str, key: &str) -> Result<Option<Vec<u8>>, String> {
        self.lock()?
            .query_row(
                "SELECT data FROM resources WHERE profile=? AND kind=? AND key=?",
                params![profile, kind, key],
                |r| r.get(0),
            )
            .optional()
            .map_err(error)
    }
}

fn read_title(c: &Connection, profile: &str, id: u64) -> Result<Option<LibraryTitle>, String> {
    let raw: Option<String> = c
        .query_row(
            "SELECT data FROM titles WHERE profile=? AND title_id=?",
            params![profile, id],
            |r| r.get(0),
        )
        .optional()
        .map_err(error)?;
    raw.map(|raw| serde_json::from_str(&raw).map_err(error))
        .transpose()
}
fn write_title(c: &Connection, profile: &str, title: &mut LibraryTitle) -> Result<(), String> {
    update_fields(title);
    c.execute("INSERT INTO titles VALUES(?,?,?) ON CONFLICT(profile,title_id) DO UPDATE SET data=excluded.data", params![profile,title.item.title_id,serde_json::to_string(title).map_err(error)?]).map_err(error)?;
    Ok(())
}
fn update_fields(title: &mut LibraryTitle) {
    title.item.updated_at_ms = now();
    title.item.episodes = format!(
        "{} / {}",
        title.item.watched_episodes_count,
        title
            .item
            .total_episodes
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".into())
    );
    if let Some(details) = title.details.as_mut() {
        details.watch_status = title.item.watch_status.clone();
        details.is_favourite = title.item.is_favourite;
        details.user_status_loaded = true;
    }
}
