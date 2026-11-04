use super::*;
use crate::local_library::{LibraryState, LibraryTitle, LocalLibrary, default_database_path};
use crate::public_users::{PublicUser, parse_public_users, profile_from_url};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportSummary {
    pub profile_id: String,
    pub source: PublicUser,
    pub found: usize,
    pub added: usize,
}

impl ShindenClientBackend {
    pub fn new() -> Result<Self, String> {
        Self::with_library_path(default_database_path()?)
    }

    pub fn with_library_path(path: impl AsRef<Path>) -> Result<Self, String> {
        let library = LocalLibrary::open(path)?;
        // A fresh application session has no remote login. Start in Guest;
        // saved account profiles remain available through explicit local selection.
        library.select_profile("guest")?;
        let network_permission = Arc::new(AtomicBool::new(library.state()?.network_enabled));
        Ok(Self {
            api: ShindenAPI::anonymous_with_permission(network_permission.clone())
                .map_err(|e| e.to_string())?,
            account_session: Mutex::new(AccountSession {
                api: ShindenAPI::anonymous_with_permission(network_permission.clone())
                    .map_err(|e| e.to_string())?,
                profile: None,
                generation: 0,
            }),
            library,
            network_permission,
            import_generation: AtomicU64::new(0),
            refresh_status: Mutex::new(HashMap::new()),
            user_anime_list_refresh_status: Mutex::new(HashMap::new()),
        })
    }

    fn user_refresh_for(
        &self,
        profile: &str,
    ) -> Result<Arc<Mutex<UserAnimeListRefreshStatus>>, String> {
        let mut statuses = self
            .user_anime_list_refresh_status
            .lock()
            .map_err(|_| "Odświeżanie niedostępne")?;
        Ok(statuses
            .entry(profile.into())
            .or_insert_with(|| Arc::new(Mutex::new(Default::default())))
            .clone())
    }
    fn watching_refresh_for(
        &self,
        profile: &str,
    ) -> Result<Arc<Mutex<WatchingCacheRefreshStatus>>, String> {
        let mut statuses = self
            .refresh_status
            .lock()
            .map_err(|_| "Odświeżanie niedostępne")?;
        Ok(statuses
            .entry(profile.into())
            .or_insert_with(|| Arc::new(Mutex::new(Default::default())))
            .clone())
    }

    pub fn get_library_state(&self) -> Result<LibraryState, String> {
        let mut state = self.library.state()?;
        state.authenticated = state.network_enabled
            && self
                .account_session
                .lock()
                .map_err(|_| "Sesja niedostępna")?
                .profile
                .as_deref()
                == Some(&state.active_profile.id);
        Ok(state)
    }

    pub fn select_library_profile(&self, profile_id: String) -> Result<LibraryState, String> {
        self.library.select_profile(&profile_id)?;
        self.clear_account_session()?;
        self.get_library_state()
    }

    pub fn set_library_preferences(
        &self,
        network_enabled: bool,
        import_enabled: bool,
    ) -> Result<LibraryState, String> {
        if !network_enabled || !import_enabled {
            self.import_generation.fetch_add(1, Ordering::SeqCst);
        }
        if !network_enabled {
            self.network_permission.store(false, Ordering::SeqCst);
        }
        self.library
            .set_preferences(network_enabled, import_enabled)?;
        self.network_permission
            .store(network_enabled, Ordering::SeqCst);
        if !network_enabled {
            self.clear_account_session()?;
        }
        self.get_library_state()
    }

    fn profile(&self) -> Result<String, String> {
        Ok(self.library.state()?.active_profile.id)
    }
    pub fn require_network_access(&self) -> Result<(), String> {
        self.library.require_network()
    }
    fn clear_account_session(&self) -> Result<(), String> {
        let mut session = self
            .account_session
            .lock()
            .map_err(|_| "Sesja niedostępna")?;
        session.api.revoke_requests();
        *session = AccountSession {
            api: ShindenAPI::anonymous_with_permission(self.network_permission.clone())
                .map_err(|e| e.to_string())?,
            profile: None,
            generation: session.generation.wrapping_add(1),
        };
        Ok(())
    }

    fn playback_api(&self, profile: &str) -> Result<ShindenAPI, String> {
        let session = self
            .account_session
            .lock()
            .map_err(|_| "Sesja niedostępna")?;
        Ok(if session.profile.as_deref() == Some(profile) {
            session.api.clone()
        } else {
            self.api.clone()
        })
    }

    fn network_error(&self, error: impl ToString) -> String {
        let message = error.to_string();
        if message.contains("403")
            || message.contains("429")
            || message.contains("SHINDEN_ACCESS_BLOCKED")
        {
            self.network_permission.store(false, Ordering::SeqCst);
            self.import_generation.fetch_add(1, Ordering::SeqCst);
            if let Ok(state) = self.library.state() {
                let _ = self.library.set_preferences(false, state.import_enabled);
            }
            let _ = self.clear_account_session();
        }
        message
    }

    pub fn write_log(&self, level: String, message: String) -> Result<(), String> {
        // Do not persist request URLs, account details or server response bodies.
        let safe = if message.contains("http")
            || message.contains("cookie")
            || message.contains("token")
            || message.contains("JWT")
        {
            "Szczegóły żądania pominięto w lokalnym logu".to_string()
        } else {
            message
        };
        discard_log_path(append_project_log(&level, &safe))
    }

    pub async fn test_connection(&self) -> Result<(), String> {
        // Startup is local; there is no automatic network probe.
        Ok(())
    }
    pub async fn verify_connection(&self) -> Result<(), String> {
        self.library.require_network()?;
        self.api
            .get_html("https://shinden.pl/")
            .await
            .map_err(|e| self.network_error(e))?;
        Ok(())
    }

    pub async fn get_image(&self, url: &str) -> Result<crate::images::ShindenImage, String> {
        let profile = self.profile()?;
        if let (Some(bytes), Some(content_type)) = (
            self.library.bytes(&profile, "image", url)?,
            self.library.get::<String>(&profile, "image-type", url)?,
        ) {
            return Ok(crate::images::ShindenImage {
                bytes,
                content_type,
            });
        }
        self.library.require_network()?;
        let image = self
            .api
            .get_image(url)
            .await
            .map_err(|e| self.network_error(e))?;
        self.library
            .put_bytes(&profile, "image", url, &image.bytes)?;
        self.library
            .put(&profile, "image-type", url, &image.content_type)?;
        Ok(image)
    }

    fn save_anime(
        &self,
        profile: &str,
        anime: &Anime,
        id: u64,
        total: Option<u32>,
    ) -> Result<(), String> {
        let mut title = LibraryTitle::new(id, &anime.name, &anime.url);
        title.item.image_url = anime.image_url.clone();
        title.item.anime_type = anime.anime_type.clone();
        title.item.rating = anime.rating.clone();
        title.item.description = anime.description.clone();
        title.item.total_episodes = total.or_else(|| anime.episodes.parse().ok());
        self.library.save_title(profile, &title)
    }

    fn overlay_search(&self, profile: &str, items: Vec<Anime>) -> Result<Vec<SearchAnime>, String> {
        let mut result = map_search_anime_results(items, vec![]);
        for row in &mut result {
            if let Some(id) = row.title_id {
                self.save_anime(profile, &row.anime, id, row.total_episodes)?;
                if let Some(title) = self.library.title(profile, id)? {
                    row.watch_status = title.item.watch_status;
                    row.is_favourite = title.item.is_favourite;
                    row.total_episodes = title.item.total_episodes;
                }
            }
        }
        Ok(result)
    }

    fn local_search(&self, profile: &str, query: &str) -> Result<Vec<Anime>, String> {
        let query = query.trim().to_lowercase();
        Ok(self
            .library
            .titles(profile, false)?
            .into_iter()
            .filter(|t| t.item.name.to_lowercase().contains(&query))
            .map(|t| item_anime(&t.item))
            .collect())
    }

    pub async fn search(&self, query: String) -> Result<Vec<SearchAnime>, String> {
        let profile = self.profile()?;
        let rows = if self.library.state()?.network_enabled {
            self.api
                .search_anime(&query)
                .await
                .map_err(|e| self.network_error(e))?
        } else {
            self.local_search(&profile, &query)?
        };
        self.overlay_search(&profile, rows)
    }

    pub async fn get_search_filter_catalog(&self) -> Result<SearchFilterCatalog, String> {
        let profile = self.profile()?;
        if let Some(catalog) = self.library.get(&profile, "search", "catalog")? {
            return Ok(catalog);
        }
        if !self.library.state()?.network_enabled {
            return Ok(SearchFilterCatalog::default());
        }
        let catalog = self
            .api
            .get_search_filter_catalog()
            .await
            .map_err(|e| self.network_error(e))?;
        self.library.put(&profile, "search", "catalog", &catalog)?;
        Ok(catalog)
    }

    pub async fn search_with_filters(
        &self,
        request: SearchFilterRequest,
    ) -> Result<SearchAnimePage, String> {
        let profile = self.profile()?;
        if !self.library.state()?.network_enabled {
            let catalog: SearchFilterCatalog = self
                .library
                .get(&profile, "search", "catalog")?
                .unwrap_or_default();
            let tag_labels: HashMap<u64, String> = catalog
                .groups
                .into_iter()
                .flat_map(|g| g.options.into_iter().map(|t| (t.id, t.label)))
                .collect();
            let mut rows = self
                .library
                .titles(&profile, false)?
                .into_iter()
                .filter(|title| {
                    title
                        .item
                        .name
                        .to_lowercase()
                        .contains(&request.query.trim().to_lowercase())
                        && request.letter.as_ref().is_none_or(|letter| {
                            title
                                .item
                                .name
                                .to_lowercase()
                                .starts_with(&letter.to_lowercase())
                        })
                        && request.tags.iter().all(|tag| {
                            let matches = tag_labels.get(&tag.tag_id).is_some_and(|label| {
                                title
                                    .item
                                    .tags
                                    .iter()
                                    .any(|t| t.eq_ignore_ascii_case(label))
                            });
                            if tag.mode == crate::models::SearchTagSelectionMode::Exclude {
                                !matches
                            } else {
                                matches
                            }
                        })
                })
                .map(|t| item_anime(&t.item))
                .collect::<Vec<_>>();
            rows.sort_by(|a, b| a.name.cmp(&b.name));
            let total_pages = rows.len().div_ceil(50).max(1) as u32;
            let current_page = request.page.max(1).min(total_pages);
            let items = rows
                .into_iter()
                .skip(((current_page - 1) * 50) as usize)
                .take(50)
                .collect();
            return Ok(SearchAnimePage {
                items: self.overlay_search(&profile, items)?,
                current_page,
                total_pages,
            });
        }
        let page = self
            .api
            .search_anime_with_filters(&request)
            .await
            .map_err(|e| self.network_error(e))?;
        Ok(SearchAnimePage {
            items: self.overlay_search(&profile, page.items)?,
            current_page: page.current_page,
            total_pages: page.total_pages,
        })
    }

    fn local_discovery(
        &self,
        profile: &str,
        year: Option<u16>,
    ) -> Result<Vec<DiscoveryAnime>, String> {
        Ok(self
            .library
            .titles(profile, false)?
            .into_iter()
            .filter(|t| year.is_none_or(|year| t.item.release_year == Some(year)))
            .map(|t| {
                let i = t.item;
                DiscoveryAnime {
                    name: i.name,
                    url: i.url,
                    image_url: i.image_url,
                    anime_type: i.anime_type,
                    rating: i.rating,
                    episodes: i.episodes,
                    description: i.description,
                    title_id: Some(i.title_id),
                    watch_status: i.watch_status,
                    is_favourite: i.is_favourite,
                    total_episodes: i.total_episodes,
                    source_label: Some("Lokalna biblioteka".into()),
                }
            })
            .collect())
    }

    async fn discovery(
        &self,
        key: &str,
        url: &str,
        season: bool,
        year: Option<u16>,
    ) -> Result<Vec<DiscoveryAnime>, String> {
        let profile = self.profile()?;
        let cached: Option<Vec<DiscoveryAnime>> = self.library.get(&profile, "discovery", key)?;
        let rows = if let Some(rows) = cached {
            rows
        } else if self.library.state()?.network_enabled {
            let html = self
                .api
                .get_html(url)
                .await
                .map_err(|e| self.network_error(e))?;
            let base = if season {
                parse_season_anime_html(&html)
            } else {
                parse_main_premieres_html(&html)
            };
            let rows = map_discovery_anime_results(base, vec![]);
            self.library.put(&profile, "discovery", key, &rows)?;
            rows
        } else {
            return self.local_discovery(&profile, year);
        };
        let mut result = rows;
        for row in &mut result {
            if let Some(id) = row.title_id {
                let anime = Anime {
                    name: row.name.clone(),
                    url: row.url.clone(),
                    image_url: row.image_url.clone(),
                    anime_type: row.anime_type.clone(),
                    rating: row.rating.clone(),
                    episodes: row.episodes.clone(),
                    description: row.description.clone(),
                };
                self.save_anime(&profile, &anime, id, row.total_episodes)?;
                if let Some(t) = self.library.title(&profile, id)? {
                    row.watch_status = t.item.watch_status;
                    row.is_favourite = t.item.is_favourite;
                }
            }
        }
        Ok(result)
    }

    pub async fn get_main_premieres(&self) -> Result<Vec<DiscoveryAnime>, String> {
        self.discovery("main", SHINDEN_MAIN_URL, false, None).await
    }
    pub async fn get_season_anime(
        &self,
        year: Option<u16>,
        season: String,
    ) -> Result<Vec<DiscoveryAnime>, String> {
        let url = season_page_url(year, &season);
        self.discovery(&url, &url, true, year).await
    }

    pub async fn get_anime_details(&self, url: String) -> Result<AnimeDetails, String> {
        let profile = self.profile()?;
        self.details_for(&profile, &url, false).await
    }
    async fn details_for(
        &self,
        profile: &str,
        url: &str,
        force: bool,
    ) -> Result<AnimeDetails, String> {
        let id = trusted_title_id(url)?;
        if !force {
            if let Some(title) = self.library.title(profile, id)? {
                if let Some(details) = title.details {
                    return Ok(details);
                }
                if !self.library.state()?.network_enabled {
                    return Ok(details_from_item(&title.item));
                }
            }
        }
        self.library.require_network()?;
        let details = self
            .api
            .get_anime_details(url)
            .await
            .map_err(|e| self.network_error(e))?;
        if details.name.trim().is_empty() {
            return Err("Shinden nie zwrócił szczegółów anime; zachowano dane lokalne".into());
        }
        let mut title = self
            .library
            .title(profile, id)?
            .unwrap_or_else(|| LibraryTitle::new(id, &details.name, url));
        title.item.name = details.name.clone();
        title.item.url = url.into();
        title.item.image_url = details.image_url.clone();
        title.item.description = details.description.clone();
        apply_user_anime_details_to_item(&mut title.item, &details);
        title.details = Some(details);
        self.library.save_title(profile, &title)?;
        Ok(self.library.title(profile, id)?.unwrap().details.unwrap())
    }

    pub async fn get_user_anime_lists(
        &self,
        _force_refresh: Option<bool>,
    ) -> Result<UserAnimeListsPayload, String> {
        let profile = self.profile()?;
        let items = self
            .library
            .titles(&profile, true)?
            .into_iter()
            .map(|t| t.item)
            .collect();
        Ok(user_anime_lists_payload(items, None, None))
    }

    pub async fn get_watching_anime(
        &self,
        filter: Option<WatchingAnimeFilter>,
    ) -> Result<Vec<WatchingAnime>, String> {
        let profile = self.profile()?;
        let filter = filter.unwrap_or_default();
        let mut result = vec![];
        for title in self.library.titles(&profile, true)? {
            let i = title.item;
            if i.watch_status != "in progress" {
                continue;
            }
            if filter.only_available_unwatched()
                && i.total_episodes
                    .is_some_and(|n| i.watched_episodes_count >= n)
            {
                continue;
            }
            if filter.only_available_unwatched() || filter.check_subtitle_availability_online() {
                let episodes = self
                    .library
                    .episodes(&profile, i.title_id)?
                    .unwrap_or_default();
                let mut found = false;
                for ep in episodes
                    .iter()
                    .filter(|e| !e.watched || !filter.only_available_unwatched())
                {
                    if let Some(players) = self
                        .library
                        .get::<Vec<Player>>(&profile, "players", &ep.link)?
                    {
                        found = players.iter().any(|p| {
                            !filter.check_subtitle_availability_online()
                                || subtitle_language_matches_with_options(
                                    &p.lang_subs,
                                    filter.subtitle_language(),
                                    filter.exclude_ai_subtitles(),
                                )
                        });
                        if found {
                            break;
                        }
                    }
                }
                if !found {
                    continue;
                }
            }
            result.push(WatchingAnime {
                title_id: i.title_id,
                name: i.name,
                url: i.url,
                image_url: i.image_url,
                anime_type: i.anime_type,
                rating: i.rating,
                episodes: i.episodes,
                description: i.description,
                watch_status: i.watch_status,
                is_favourite: i.is_favourite,
                watched_episodes_count: i.watched_episodes_count,
                total_episodes: i.total_episodes,
            });
        }
        Ok(result)
    }

    pub async fn get_episodes_with_progress(
        &self,
        url: String,
        title_id: Option<u64>,
        total_episodes: Option<u32>,
        title_name: Option<String>,
    ) -> Result<Vec<EpisodeProgress>, String> {
        let profile = self.profile()?;
        let id = trusted_title_id(&url)?;
        if title_id.is_some_and(|given| given != id) {
            return Err("ID anime nie odpowiada adresowi".into());
        }
        self.episodes_for(
            &profile,
            &url,
            id,
            total_episodes,
            title_name.as_deref(),
            false,
        )
        .await
    }
    async fn episodes_for(
        &self,
        profile: &str,
        url: &str,
        id: u64,
        total: Option<u32>,
        name: Option<&str>,
        force: bool,
    ) -> Result<Vec<EpisodeProgress>, String> {
        if !force {
            if let Some(episodes) = self.library.episodes(profile, id)? {
                return Ok(episodes);
            }
        }
        self.library.require_network()?;
        if self.library.title(profile, id)?.is_none() {
            self.library.save_title(
                profile,
                &LibraryTitle::new(id, name.unwrap_or("Anime"), url),
            )?;
        }
        let total = total.or(self
            .library
            .title(profile, id)?
            .and_then(|t| t.item.total_episodes));
        let playback = self
            .api
            .get_episodes(url)
            .await
            .map_err(|e| self.network_error(e))?;
        let mut episodes = merge_episode_progress(playback, vec![], total);
        for ep in &mut episodes {
            ep.episode_id = episode_id_from_link(&ep.link);
        }
        self.library.save_episodes(profile, id, &episodes)?;
        Ok(self.library.episodes(profile, id)?.unwrap_or_default())
    }

    pub async fn update_anime_status(
        &self,
        title_id: u64,
        status: Option<String>,
        is_favourite: Option<u8>,
    ) -> Result<(), String> {
        let profile = self.profile()?;
        let status = shinden_watch_status_value(status.as_deref())?.unwrap_or("no");
        self.library
            .set_status(&profile, title_id, status, is_favourite)
    }
    pub async fn update_anime_rating(&self, update: AnimeRatingUpdate) -> Result<(), String> {
        let profile = self.profile()?;
        self.library
            .set_rating(&profile, update.title_id, &update.rating_type, update.value)
    }
    pub async fn mark_episode_watched(
        &self,
        title_id: u64,
        episode_id: u64,
        _created_time: String,
    ) -> Result<(), String> {
        self.library
            .mark_episode(&self.profile()?, title_id, episode_id, true)
    }
    pub async fn mark_episode_unwatched(
        &self,
        title_id: u64,
        episode_id: u64,
        _created_time: String,
    ) -> Result<(), String> {
        self.library
            .mark_episode(&self.profile()?, title_id, episode_id, false)
    }

    pub fn get_user_anime_list_refresh_status(&self) -> Result<UserAnimeListRefreshStatus, String> {
        let status = self.user_refresh_for(&self.profile()?)?;
        user_anime_list_refresh_status_snapshot(&status)
    }
    pub async fn resume_user_anime_list_cache_refresh(
        &self,
    ) -> Result<UserAnimeListRefreshSummary, String> {
        Ok(UserAnimeListRefreshSummary {
            status: self.get_user_anime_list_refresh_status()?,
            already_running: false,
        })
    }
    pub async fn refresh_user_anime_list_cache(
        &self,
    ) -> Result<UserAnimeListRefreshSummary, String> {
        self.library.require_network()?;
        let profile = self.profile()?;
        let refresh_status = self.user_refresh_for(&profile)?;
        let titles = self.library.titles(&profile, true)?;
        {
            let mut status = lock_user_anime_list_refresh_status(&refresh_status)?;
            if status.running {
                return Ok(UserAnimeListRefreshSummary {
                    status: status.clone(),
                    already_running: true,
                });
            }
            *status = UserAnimeListRefreshStatus {
                running: true,
                total: titles.len(),
                remaining: titles.len(),
                ..Default::default()
            };
        }
        let _guard = UserRefreshGuard(&refresh_status);
        for (index, title) in titles.iter().enumerate() {
            if !self.library.state()?.network_enabled {
                break;
            }
            update_user_anime_list_refresh_status(&refresh_status, |s| {
                s.current_title = title.item.name.clone()
            })?;
            wait_before_background_request().await;
            let result = self.details_for(&profile, &title.item.url, true).await;
            let result = match result {
                Ok(_) => self
                    .episodes_for(
                        &profile,
                        &title.item.url,
                        title.item.title_id,
                        title.item.total_episodes,
                        Some(&title.item.name),
                        true,
                    )
                    .await
                    .map(|_| ()),
                Err(e) => Err(e),
            };
            update_user_anime_list_refresh_status(&refresh_status, |s| {
                s.current = index + 1;
                s.remaining = s.total.saturating_sub(s.current);
                if let Err(e) = result {
                    s.failed += 1;
                    s.last_error = Some(e)
                } else {
                    s.refreshed += 1
                }
            })?;
        }
        update_user_anime_list_refresh_status(&refresh_status, |s| {
            s.running = false;
            s.current_title = String::new();
            s.last_finished_at_ms = Some(unix_timestamp_ms_u64());
        })?;
        Ok(UserAnimeListRefreshSummary {
            status: user_anime_list_refresh_status_snapshot(&refresh_status)?,
            already_running: false,
        })
    }
    pub fn get_watching_cache_refresh_status(&self) -> Result<WatchingCacheRefreshStatus, String> {
        let status = self.watching_refresh_for(&self.profile()?)?;
        refresh_status_snapshot(&status)
    }
    pub fn get_watching_episode_availability(
        &self,
        title_id: u64,
    ) -> Result<HashMap<String, WatchingEpisodeAvailability>, String> {
        let profile = self.profile()?;
        let episodes = self
            .library
            .episodes(&profile, title_id)?
            .unwrap_or_default();
        let mut result = HashMap::new();
        for ep in episodes {
            if let Some(players) = self
                .library
                .get::<Vec<Player>>(&profile, "players", &ep.link)?
            {
                result.insert(ep.link, watching_episode_availability(&players));
            }
        }
        Ok(result)
    }
    pub async fn refresh_watching_anime_cache(
        &self,
        filter: Option<WatchingAnimeFilter>,
        force: Option<bool>,
    ) -> Result<WatchingCacheRefreshSummary, String> {
        let profile = self.profile()?;
        let titles = self
            .library
            .titles(&profile, true)?
            .into_iter()
            .filter(|t| t.item.watch_status == "in progress")
            .collect();
        self.refresh_availability(
            &profile,
            titles,
            filter.unwrap_or_default(),
            force.unwrap_or(false),
        )
        .await
    }
    pub async fn refresh_watching_anime_cache_item(
        &self,
        title_id: u64,
        filter: Option<WatchingAnimeFilter>,
        force: Option<bool>,
    ) -> Result<WatchingCacheRefreshSummary, String> {
        let profile = self.profile()?;
        let titles = self
            .library
            .title(&profile, title_id)?
            .into_iter()
            .collect();
        self.refresh_availability(
            &profile,
            titles,
            filter.unwrap_or_default(),
            force.unwrap_or(false),
        )
        .await
    }
    async fn refresh_availability(
        &self,
        profile: &str,
        titles: Vec<LibraryTitle>,
        filter: WatchingAnimeFilter,
        force: bool,
    ) -> Result<WatchingCacheRefreshSummary, String> {
        let refresh_status = self.watching_refresh_for(profile)?;
        self.library.require_network()?;
        {
            let mut status = refresh_status
                .lock()
                .map_err(|_| "Odświeżanie niedostępne")?;
            if status.running {
                return Ok(WatchingCacheRefreshSummary {
                    status: status.clone(),
                    already_running: true,
                });
            }
            *status = WatchingCacheRefreshStatus {
                running: true,
                total: titles.len(),
                ..Default::default()
            };
        }
        let _guard = WatchingRefreshGuard(&refresh_status);
        for title in titles {
            if !self.library.state()?.network_enabled {
                break;
            }
            refresh_status
                .lock()
                .map_err(|_| "Odświeżanie niedostępne")?
                .current_title = title.item.name.clone();
            let result: Result<(), String> = async {
                wait_before_background_request().await;
                let episodes = self
                    .episodes_for(
                        profile,
                        &title.item.url,
                        title.item.title_id,
                        title.item.total_episodes,
                        Some(&title.item.name),
                        force,
                    )
                    .await?;
                for episode in episodes
                    .iter()
                    .filter(|ep| !filter.only_available_unwatched() || !ep.watched)
                {
                    if !force
                        && self
                            .library
                            .get::<Vec<Player>>(profile, "players", &episode.link)?
                            .is_some()
                    {
                        continue;
                    }
                    wait_before_background_request().await;
                    self.library.require_network()?;
                    let api = self.playback_api(profile)?;
                    let players = api
                        .get_players(&episode.link)
                        .await
                        .map_err(|e| self.network_error(e))?;
                    self.library
                        .put(profile, "players", &episode.link, &players)?;
                    self.library.put(
                        profile,
                        "players-time",
                        &episode.link,
                        &unix_timestamp_ms_u64(),
                    )?;
                }
                Ok(())
            }
            .await;
            let mut status = refresh_status
                .lock()
                .map_err(|_| "Odświeżanie niedostępne")?;
            status.current += 1;
            match result {
                Ok(()) => status.refreshed += 1,
                Err(e) => {
                    status.failed += 1;
                    status.last_error = Some(e.clone());
                    status.failures.push(WatchingCacheFailure {
                        title_id: title.item.title_id,
                        title: title.item.name,
                        series_url: title.item.url,
                        reason: e,
                    });
                }
            }
        }
        {
            let mut status = refresh_status
                .lock()
                .map_err(|_| "Odświeżanie niedostępne")?;
            status.running = false;
            status.current_title = String::new();
            status.last_finished_at_ms = Some(unix_timestamp_ms_u64());
        }
        Ok(WatchingCacheRefreshSummary {
            status: refresh_status_snapshot(&refresh_status)?,
            already_running: false,
        })
    }

    pub fn import_browser_clearance(&self, user_agent: &str, cookie: &str) -> Result<(), String> {
        self.library.require_network()?;
        self.api
            .import_browser_clearance(user_agent, cookie)
            .map_err(|e| e.to_string())
    }
    pub fn import_browser_session(
        &self,
        user_agent: &str,
        cookies: &[String],
    ) -> Result<(), String> {
        self.library.require_network()?;
        let api = ShindenAPI::anonymous_with_permission(self.network_permission.clone())
            .map_err(|e| e.to_string())?;
        api.import_browser_session(user_agent, cookies)
            .map_err(|e| e.to_string())?;
        let mut session = self
            .account_session
            .lock()
            .map_err(|_| "Sesja niedostępna")?;
        session.api.revoke_requests();
        *session = AccountSession {
            api,
            profile: None,
            generation: session.generation.wrapping_add(1),
        };
        Ok(())
    }
    pub async fn complete_browser_login(&self) -> Result<(), String> {
        self.library.require_network()?;
        let (api, generation) = {
            let s = self
                .account_session
                .lock()
                .map_err(|_| "Sesja niedostępna")?;
            (s.api.clone(), s.generation)
        };
        let user = api
            .get_current_user()
            .await
            .map_err(|e| self.network_error(e))?
            .ok_or("Dokończ logowanie w oknie Shinden")?;
        let mut session = self
            .account_session
            .lock()
            .map_err(|_| "Sesja niedostępna")?;
        if session.generation != generation {
            return Err("Sesja została zmieniona; rozpocznij logowanie ponownie".into());
        }
        self.library.require_network()?;
        let profile = self.library.activate_account(user.id, &user.name)?;
        session.profile = Some(profile.id);
        Ok(())
    }
    pub async fn login(&self, username: String, password: String) -> Result<(), String> {
        self.library.require_network()?;
        self.clear_account_session()?;
        let api = {
            self.account_session
                .lock()
                .map_err(|_| "Sesja niedostępna")?
                .api
                .clone()
        };
        api.login(&username, &password)
            .await
            .map_err(|e| self.network_error(e))?;
        self.complete_browser_login().await
    }
    pub async fn logout(&self) -> Result<(), String> {
        self.clear_account_session()?;
        self.library.select_profile("guest")
    }
    pub async fn get_user_name(&self) -> Result<Option<String>, String> {
        let state = self.get_library_state()?;
        Ok(state.authenticated.then_some(state.active_profile.name))
    }
    pub async fn get_user_profile_image(&self) -> Result<Option<String>, String> {
        Ok(None)
    }
    pub async fn get_episodes(&self, url: String) -> Result<Vec<Episode>, String> {
        Ok(self
            .get_episodes_with_progress(url, None, None, None)
            .await?
            .into_iter()
            .map(|ep| Episode {
                title: ep.title,
                link: ep.link,
            })
            .collect())
    }
    fn cached_players(
        &self,
        profile: &str,
        url: &str,
        online: bool,
    ) -> Result<Option<Vec<Player>>, String> {
        let Some(players) = self.library.get::<Vec<Player>>(profile, "players", url)? else {
            return Ok(None);
        };
        if players.is_empty() {
            return Ok(None);
        }
        let saved = self.library.get::<u64>(profile, "players-time", url)?;
        if online && saved.is_none_or(|time| unix_timestamp_ms_u64().saturating_sub(time) >= 60_000)
        {
            return Ok(None);
        }
        Ok(Some(players))
    }
    pub async fn get_players(&self, url: String) -> Result<Vec<Player>, String> {
        trusted_shinden_url(&url)?;
        let state = self.get_library_state()?;
        let profile = state.active_profile.id;
        if let Some(players) = self.cached_players(&profile, &url, state.network_enabled)? {
            return Ok(players);
        }
        self.library.require_network()?;
        let api = self.playback_api(&profile)?;
        let players = api
            .get_players(&url)
            .await
            .map_err(|e| self.network_error(e))?;
        if !players.is_empty() {
            self.library.put(&profile, "players", &url, &players)?;
            self.library
                .put(&profile, "players-time", &url, &unix_timestamp_ms_u64())?;
        }
        Ok(players)
    }
    pub async fn get_iframe(&self, id: String) -> Result<String, String> {
        let state = self.get_library_state()?;
        let profile = state.active_profile.id;
        if !state.network_enabled {
            if let Some(iframe) = self.library.get::<String>(&profile, "iframe", &id)? {
                if !iframe.trim().is_empty() {
                    return Ok(iframe);
                }
            }
        }
        self.library.require_network()?;
        let api = self.playback_api(&profile)?;
        let iframe = api
            .get_player_iframe(&id)
            .await
            .map_err(|e| self.network_error(e))?;
        if iframe.trim().is_empty() {
            return Err("Shinden nie zwrócił odtwarzacza. Spróbuj ponownie później.".into());
        }
        self.library.put(&profile, "iframe", &id, &iframe)?;
        Ok(iframe)
    }

    fn require_import(&self) -> Result<(), String> {
        if !self.library.state()?.import_enabled {
            return Err("Włącz import danych użytkownika w Ustawieniach".into());
        }
        self.library.require_network()
    }
    fn require_import_generation(&self, generation: u64) -> Result<(), String> {
        if generation != self.import_generation.load(Ordering::SeqCst) {
            return Err("Import anulowany. Nie zmieniono lokalnych danych.".into());
        }
        self.require_import()
    }
    pub async fn search_public_users(&self, query: String) -> Result<Vec<PublicUser>, String> {
        self.require_import()?;
        let query = query.trim();
        if query.starts_with("https://") {
            return Ok(vec![profile_from_url(query)?]);
        }
        if query.chars().count() < 2 || query.chars().count() > 80 {
            return Err("Wpisz od 2 do 80 znaków nicku".into());
        }
        let mut url = reqwest::Url::parse("https://shinden.pl/users").unwrap();
        url.query_pairs_mut().append_pair("search", query);
        let html = self
            .api
            .get_html(url.as_str())
            .await
            .map_err(|e| self.network_error(e))?;
        Ok(parse_public_users(&html))
    }
    pub async fn import_public_user(&self, profile_url: String) -> Result<ImportSummary, String> {
        self.require_import()?;
        let generation = self.import_generation.load(Ordering::SeqCst);
        let profile = self.profile()?;
        let source = profile_from_url(&profile_url)?;
        let mut items = vec![];
        for status in WATCHING_LIST_STATUSES {
            self.require_import_generation(generation)?;
            let mut offset = 0;
            loop {
                self.require_import_generation(generation)?;
                wait_before_background_request().await;
                self.require_import_generation(generation)?;
                let page = fetch_watching_list_status_page(
                    &self.api,
                    &source.id.to_string(),
                    status,
                    WATCHING_LIST_PAGE_LIMIT,
                    offset,
                )
                .await
                .map_err(|e| self.network_error(e))?;
                if append_import_page(&mut items, page, status, &mut offset)? {
                    break;
                }
                if offset >= 100_000 {
                    return Err("Lista przekracza limit bezpiecznego importu; nie zmieniono lokalnych danych".into());
                }
            }
        }
        self.require_import_generation(generation)?;
        let added = self.library.import_titles(&profile, &items)?;
        Ok(ImportSummary {
            profile_id: profile,
            source,
            found: items.len(),
            added,
        })
    }
}

fn append_import_page(
    items: &mut Vec<LibraryTitle>,
    page: WatchingListApiResult,
    status: &str,
    offset: &mut usize,
) -> Result<bool, String> {
    let loaded = page.items.len();
    if (loaded == 0 && *offset < page.count) || offset.saturating_add(loaded) > page.count {
        return Err(
            "Shinden zwrócił niepełną lub niespójną listę. Nie zmieniono lokalnych danych.".into(),
        );
    }
    let mut seen: std::collections::HashSet<u64> = items.iter().map(|t| t.item.title_id).collect();
    for row in page.items {
        let mut item = map_user_anime_list_item(&row, unix_timestamp_ms_u64()).ok_or(
            "Shinden zwrócił nieprawidłową pozycję listy. Nie zmieniono lokalnych danych.",
        )?;
        if item.title_id == 0
            || !seen.insert(item.title_id)
            || shinden_watch_status_value(Some(&item.watch_status))? != Some(status)
        {
            return Err("Shinden zwrócił niespójne statusy lub powtórzone pozycje. Nie zmieniono lokalnych danych.".into());
        }
        item.watch_status = status.into();
        items.push(LibraryTitle {
            item,
            details: None,
        });
    }
    *offset += loaded;
    Ok(*offset == page.count)
}

struct WatchingRefreshGuard<'a>(&'a Mutex<WatchingCacheRefreshStatus>);
impl Drop for WatchingRefreshGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut status) = self.0.lock() {
            status.running = false;
        }
    }
}
struct UserRefreshGuard<'a>(&'a Mutex<UserAnimeListRefreshStatus>);
impl Drop for UserRefreshGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut status) = self.0.lock() {
            status.running = false;
        }
    }
}

fn item_anime(i: &UserAnimeListItem) -> Anime {
    Anime {
        name: i.name.clone(),
        url: i.url.clone(),
        image_url: i.image_url.clone(),
        anime_type: i.anime_type.clone(),
        rating: i.rating.clone(),
        episodes: i.episodes.clone(),
        description: i.description.clone(),
    }
}
fn details_from_item(i: &UserAnimeListItem) -> AnimeDetails {
    AnimeDetails {
        title_id: Some(i.title_id),
        title_type: "anime".into(),
        name: i.name.clone(),
        image_url: i.image_url.clone(),
        description: i.description.clone(),
        watch_status: i.watch_status.clone(),
        is_favourite: i.is_favourite,
        user_status_loaded: true,
        ..Default::default()
    }
}
fn trusted_shinden_url(source: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(source).map_err(|_| "Nieprawidłowy adres Shinden")?;
    if url.scheme() != "https"
        || url.host_str() != Some("shinden.pl")
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("Nieprawidłowy adres Shinden".into());
    }
    Ok(url)
}
fn trusted_title_id(source: &str) -> Result<u64, String> {
    let url = trusted_shinden_url(source)?;
    title_id_from_series_url(url.as_str())
        .and_then(|id| id.parse().ok())
        .filter(|id| *id > 0)
        .ok_or("Nieprawidłowy adres anime".into())
}
fn episode_id_from_link(source: &str) -> Option<u64> {
    let url = trusted_shinden_url(source).ok()?;
    let segments = url.path_segments()?.collect::<Vec<_>>();
    segments
        .windows(2)
        .find(|pair| pair[0] == "view")
        .and_then(|pair| pair[1].parse().ok())
        .or_else(|| segments.iter().rev().find_map(|part| part.parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn review_available_unwatched_filter_requires_a_playable_unwatched_episode() {
        let backend = ShindenClientBackend::with_library_path(":memory:").unwrap();
        let mut title = LibraryTitle::new(42, "Title", "https://shinden.pl/titles/42-test");
        title.item.total_episodes = Some(2);
        backend.library.save_title("guest", &title).unwrap();
        backend
            .library
            .set_status("guest", 42, "in progress", None)
            .unwrap();
        let filter = || {
            Some(WatchingAnimeFilter {
                only_available_unwatched: Some(true),
                ..Default::default()
            })
        };
        assert!(
            backend
                .get_watching_anime(filter())
                .await
                .unwrap()
                .is_empty()
        );
        let url = "https://shinden.pl/epek/42-test/view/7";
        backend
            .library
            .save_episodes(
                "guest",
                42,
                &[EpisodeProgress {
                    title: "1".into(),
                    link: url.into(),
                    episode_id: Some(7),
                    episode_no: 1,
                    watched: false,
                    view_count: 0,
                    total_episodes: Some(2),
                    is_true_final_episode: false,
                }],
            )
            .unwrap();
        backend
            .library
            .put(
                "guest",
                "players",
                url,
                &vec![Player {
                    player: "CDA".into(),
                    max_res: "1080p".into(),
                    lang_audio: "JP".into(),
                    lang_subs: "PL".into(),
                    online_id: "7".into(),
                }],
            )
            .unwrap();
        assert_eq!(backend.get_watching_anime(filter()).await.unwrap().len(), 1);
        backend.library.mark_episode("guest", 42, 7, true).unwrap();
        assert!(
            backend
                .get_watching_anime(filter())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn review_cancelled_import_cannot_resume_after_reenabling_network() {
        let backend = ShindenClientBackend::with_library_path(":memory:").unwrap();
        backend.set_library_preferences(true, true).unwrap();
        backend.set_library_preferences(false, false).unwrap();
        backend.set_library_preferences(true, true).unwrap();
        assert!(backend.require_import_generation(0).is_err());
    }

    #[test]
    fn review_expired_players_are_refreshed_online_but_remain_available_offline() {
        let backend = ShindenClientBackend::with_library_path(":memory:").unwrap();
        let players = vec![Player {
            player: "CDA".into(),
            max_res: "1080p".into(),
            lang_audio: "JP".into(),
            lang_subs: "PL".into(),
            online_id: "7".into(),
        }];
        backend
            .library
            .put("guest", "players", "episode", &players)
            .unwrap();
        backend
            .library
            .put("guest", "players-time", "episode", &0u64)
            .unwrap();
        assert!(
            backend
                .cached_players("guest", "episode", true)
                .unwrap()
                .is_none()
        );
        assert!(
            backend
                .cached_players("guest", "episode", false)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn review_partial_import_pages_do_not_succeed() {
        let mut items = vec![];
        let mut offset = 100;
        assert!(
            append_import_page(
                &mut items,
                WatchingListApiResult {
                    count: 200,
                    items: vec![]
                },
                "plan",
                &mut offset
            )
            .is_err()
        );
    }
    #[test]
    fn review_unmappable_import_rows_are_rejected() {
        let row: WatchingListApiItem = serde_json::from_value(
            serde_json::json!({"titleId":42,"title":"Invalid","watchStatus":null}),
        )
        .unwrap();
        let mut items = vec![];
        let mut offset = 0;
        assert!(
            append_import_page(
                &mut items,
                WatchingListApiResult {
                    count: 1,
                    items: vec![row]
                },
                "plan",
                &mut offset
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn review_empty_cached_playback_is_retryable() {
        let backend = ShindenClientBackend::with_library_path(":memory:").unwrap();
        backend
            .library
            .put("guest", "iframe", "7", &String::new())
            .unwrap();
        assert!(backend.get_iframe("7".into()).await.is_err());
        let url = "https://shinden.pl/epek/42-test/view/7";
        backend
            .library
            .put("guest", "players", url, &Vec::<Player>::new())
            .unwrap();
        assert!(backend.get_players(url.into()).await.is_err());
    }

    #[tokio::test]
    async fn review_refresh_status_is_scoped_to_the_profile() {
        let backend = ShindenClientBackend::with_library_path(":memory:").unwrap();
        backend.library.set_preferences(true, false).unwrap();
        backend
            .library
            .save_title("guest", &LibraryTitle::new(42, "Private title", "invalid"))
            .unwrap();
        backend
            .library
            .set_status("guest", 42, "in progress", None)
            .unwrap();
        backend.refresh_user_anime_list_cache().await.unwrap();
        backend
            .refresh_watching_anime_cache(None, Some(true))
            .await
            .unwrap();
        assert_eq!(
            backend.get_user_anime_list_refresh_status().unwrap().failed,
            1
        );
        backend.library.activate_account(7, "Other").unwrap();
        assert_eq!(
            backend.get_user_anime_list_refresh_status().unwrap().failed,
            0
        );
        assert_eq!(
            backend.get_watching_cache_refresh_status().unwrap().failed,
            0
        );
    }
    #[test]
    fn availability_is_keyed_by_episode_link_and_isolated_by_profile() {
        let backend = ShindenClientBackend::with_library_path(":memory:").unwrap();
        let url = "https://shinden.pl/epek/42-test/view/7";
        backend
            .library
            .save_episodes(
                "guest",
                42,
                &[EpisodeProgress {
                    title: "1".into(),
                    link: url.into(),
                    episode_id: Some(7),
                    episode_no: 1,
                    watched: false,
                    view_count: 0,
                    total_episodes: None,
                    is_true_final_episode: false,
                }],
            )
            .unwrap();
        backend
            .library
            .put(
                "guest",
                "players",
                url,
                &vec![Player {
                    player: "CDA".into(),
                    max_res: "1080p".into(),
                    lang_audio: "JP".into(),
                    lang_subs: "PL".into(),
                    online_id: "7".into(),
                }],
            )
            .unwrap();
        let availability = backend.get_watching_episode_availability(42).unwrap();
        assert!(availability.get(url).unwrap().has_players);
        backend.library.activate_account(7, "Other").unwrap();
        assert!(
            backend
                .get_watching_episode_availability(42)
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn logging_out_replaces_the_cookie_store_without_mutating_inflight_clients() {
        let backend = ShindenClientBackend::with_library_path(":memory:").unwrap();
        backend.library.set_preferences(true, true).unwrap();
        backend
            .import_browser_session("Browser", &["sessionlogin=secret; Secure; Path=/".into()])
            .unwrap();
        let generation = backend.account_session.lock().unwrap().generation;
        backend.clear_account_session().unwrap();
        assert_ne!(
            backend.account_session.lock().unwrap().generation,
            generation
        );
        assert!(!backend.get_library_state().unwrap().authenticated);
    }
}
