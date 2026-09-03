use crate::client::ShindenAPI;
use crate::details::{
    AnimeDetails, AnimeRatingUpdate, anime_rating_url, basic_auth_token, normalize_rating_type,
    rating_update_form,
};
use crate::models::{Anime, Episode, Player, SearchFilterCatalog, SearchFilterRequest};
use futures_util::stream::{self, StreamExt};
use reqwest::header::{ACCEPT, CONTENT_TYPE, ORIGIN, REFERER};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::sleep;

const WATCHING_LIST_PAGE_LIMIT: usize = 100;
const WATCHING_LIST_STATUSES: [&str; 6] = [
    "in progress",
    "completed",
    "skip",
    "hold",
    "dropped",
    "plan",
];
const WATCHING_CACHE_TTL_MS: u64 = 2 * 60 * 60 * 1000;
const WATCHING_CACHE_REFRESH_CONCURRENCY: usize = 1;
const WATCHING_CACHE_REQUEST_RETRIES: usize = 2;
const WATCHING_CACHE_RETRY_DELAY_MS: u64 = 750;
const BACKGROUND_REQUEST_SPACING_MS: u64 = 900;
const USER_ANIME_LIST_DETAIL_REFRESH_CONCURRENCY: usize = 1;
const USER_ID_CACHE_TTL_MS: u64 = 60 * 60 * 1000;
const SHINDEN_TITLE_PLACEHOLDER: &str =
    "https://shinden.pl/res/other/placeholders/title/100x100.jpg";
const SHINDEN_MAIN_URL: &str = "https://shinden.pl/main";
const SHINDEN_SEASON_CURRENT_URL: &str = "https://shinden.pl/series/season/current";

#[derive(Debug, Deserialize)]
struct WatchingListApiResponse {
    success: bool,
    result: WatchingListApiResult,
}

#[derive(Debug, Deserialize)]
struct WatchingListApiResult {
    count: usize,
    items: Vec<WatchingListApiItem>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TitleEpisodesApiResponse {
    success: bool,
    message: Option<String>,
    result: TitleEpisodesApiResult,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)] // Fields retained to deserialize the complete API response.
struct TitleEpisodesApiResult {
    count: u32,
    items: Vec<TitleEpisodeApiItem>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)] // Fields retained to deserialize the complete API response.
struct TitleEpisodeApiItem {
    episode_id: u64,
    episode_no: u32,
    is_filer: Option<u8>,
    watched: Option<TitleEpisodeWatchedApiItem>,
    #[serde(rename = "titlePL")]
    title_pl: Option<TitleEpisodeTitleApiItem>,
    #[serde(rename = "titleEN")]
    title_en: Option<TitleEpisodeTitleApiItem>,
    #[serde(rename = "titleOfficial")]
    title_official: Option<TitleEpisodeTitleApiItem>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)] // Fields retained to deserialize the complete API response.
struct TitleEpisodeWatchedApiItem {
    episode_id: u64,
    view_cnt: u32,
    created_time: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)] // Fields retained to deserialize the complete API response.
struct TitleEpisodeTitleApiItem {
    lang: String,
    episode_id: u64,
    title: String,
    title_type: String,
}

#[derive(Debug, Deserialize)]
struct ShindenWriteResponse {
    success: bool,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TitleStatusApiResponse {
    success: bool,
    message: Option<String>,
    result: TitleStatusApiResult,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct TitleStatusApiResult {
    title: Option<TitleStatusApiTitle>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct TitleStatusApiTitle {
    watch_status: Option<String>,
    is_favourite: Option<u8>,
    priority: Option<i32>,
    recommend: Option<i32>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct WatchingListApiItem {
    title_id: u64,
    watch_status: Option<String>,
    is_favourite: Option<u8>,
    title: String,
    cover_id: Option<u64>,
    anime_type: Option<String>,
    summary_rating_total: Option<String>,
    episodes: Option<u32>,
    watched_episodes_cnt: Option<String>,
    description_pl: Option<String>,
    description_en: Option<String>,
    #[serde(default)]
    release_date: Option<String>,
    #[serde(default)]
    year: Option<u16>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WatchingAnimeFilter {
    pub only_available_unwatched: Option<bool>,
    pub subtitle_language: Option<String>,
    pub check_subtitle_availability_online: Option<bool>,
    pub exclude_ai_subtitles: Option<bool>,
}

impl WatchingAnimeFilter {
    fn only_available_unwatched(&self) -> bool {
        self.only_available_unwatched.unwrap_or(false)
    }

    fn subtitle_language(&self) -> &str {
        self.subtitle_language.as_deref().unwrap_or_default()
    }

    fn check_subtitle_availability_online(&self) -> bool {
        self.check_subtitle_availability_online.unwrap_or(false)
    }

    fn exclude_ai_subtitles(&self) -> bool {
        self.exclude_ai_subtitles.unwrap_or(false)
    }
}

#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct WatchingAvailabilityCache {
    entries: HashMap<String, WatchingAvailabilityCacheEntry>,
    #[serde(default)]
    canonical_title_urls: HashMap<u64, String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
struct WatchingAvailabilityCacheEntry {
    title_id: u64,
    watched_episodes_cnt: u32,
    total_episodes: Option<u32>,
    has_available_unwatched_episode: bool,
    subtitle_availability: HashMap<String, bool>,
    #[serde(default)]
    episode_availability: HashMap<String, WatchingEpisodeAvailability>,
    checked_at_ms: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct WatchingEpisodeAvailability {
    pub has_players: bool,
    pub subtitle_availability: HashMap<String, bool>,
}

#[derive(Debug, Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct WatchingCacheFailure {
    pub title_id: u64,
    pub title: String,
    pub series_url: String,
    pub reason: String,
}

#[derive(Debug, Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct WatchingCacheRefreshStatus {
    pub running: bool,
    pub current: usize,
    pub total: usize,
    pub refreshed: usize,
    pub skipped: usize,
    pub failed: usize,
    pub failures: Vec<WatchingCacheFailure>,
    pub current_title: String,
    pub last_finished_at_ms: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WatchingCacheRefreshSummary {
    pub status: WatchingCacheRefreshStatus,
    pub already_running: bool,
}

#[derive(Debug, Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct UserAnimeListRefreshStatus {
    pub running: bool,
    pub current: usize,
    pub total: usize,
    pub remaining: usize,
    pub refreshed: usize,
    pub failed: usize,
    pub current_title: String,
    pub last_finished_at_ms: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct UserAnimeListRefreshSummary {
    pub status: UserAnimeListRefreshStatus,
    pub already_running: bool,
}

struct WatchingCacheRefreshPlan {
    items_to_scan: Vec<WatchingListApiItem>,
    skipped: usize,
    processed: usize,
}

#[derive(Debug, Clone, Default)]
struct CachedUserId {
    user_id: Option<String>,
    checked_at_ms: u64,
}

#[derive(Debug, Serialize, Clone)]
pub struct WatchingAnime {
    #[serde(rename = "titleId")]
    pub title_id: u64,
    pub name: String,
    pub url: String,
    pub image_url: String,
    pub anime_type: String,
    pub rating: String,
    pub episodes: String,
    pub description: String,
    #[serde(rename = "watchStatus")]
    pub watch_status: String,
    #[serde(rename = "isFavourite")]
    pub is_favourite: u8,
    #[serde(rename = "watchedEpisodesCount")]
    pub watched_episodes_count: u32,
    #[serde(rename = "totalEpisodes")]
    pub total_episodes: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UserAnimeListItem {
    #[serde(rename = "titleId")]
    pub title_id: u64,
    pub name: String,
    pub url: String,
    pub image_url: String,
    pub anime_type: String,
    pub rating: String,
    pub episodes: String,
    pub description: String,
    #[serde(rename = "watchStatus")]
    pub watch_status: String,
    #[serde(rename = "isFavourite")]
    pub is_favourite: u8,
    #[serde(rename = "watchedEpisodesCount")]
    pub watched_episodes_count: u32,
    #[serde(rename = "totalEpisodes")]
    pub total_episodes: Option<u32>,
    #[serde(rename = "releaseYear")]
    pub release_year: Option<u16>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, rename = "ageRating")]
    pub age_rating: Option<String>,
    #[serde(default, rename = "detailMetadataLoaded")]
    pub detail_metadata_loaded: bool,
    pub active: bool,
    #[serde(rename = "updatedAtMs")]
    pub updated_at_ms: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct UserAnimeListCounts {
    pub in_progress: usize,
    pub completed: usize,
    pub skip: usize,
    pub hold: usize,
    pub dropped: usize,
    pub plan: usize,
    pub all: usize,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct UserAnimeListsPayload {
    pub items: Vec<UserAnimeListItem>,
    pub counts: UserAnimeListCounts,
    pub refreshed_at_ms: Option<u64>,
    pub sync_error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
struct UserAnimeListCache {
    items: HashMap<String, UserAnimeListItem>,
    refreshed_at_ms: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
struct UserAnimeListRefreshState {
    queue: Vec<UserAnimeListRefreshQueueItem>,
    started_at_ms: Option<u64>,
    last_finished_at_ms: Option<u64>,
    last_error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct UserAnimeListRefreshQueueItem {
    key: String,
    title_id: u64,
    title: String,
    url: String,
    done: bool,
    failed: bool,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SearchAnime {
    #[serde(flatten)]
    pub anime: Anime,
    pub title_id: Option<u64>,
    pub watch_status: String,
    pub is_favourite: u8,
    pub total_episodes: Option<u32>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SearchAnimePage {
    pub items: Vec<SearchAnime>,
    pub current_page: u32,
    pub total_pages: u32,
}

#[derive(Debug, Serialize, Clone)]
pub struct DiscoveryAnime {
    pub name: String,
    pub url: String,
    pub image_url: String,
    pub anime_type: String,
    pub rating: String,
    pub episodes: String,
    pub description: String,
    #[serde(rename = "titleId")]
    pub title_id: Option<u64>,
    #[serde(rename = "watchStatus")]
    pub watch_status: String,
    #[serde(rename = "isFavourite")]
    pub is_favourite: u8,
    #[serde(rename = "totalEpisodes")]
    pub total_episodes: Option<u32>,
    #[serde(rename = "sourceLabel")]
    pub source_label: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DiscoveryAnimeBase {
    name: String,
    url: String,
    image_url: String,
    anime_type: String,
    rating: String,
    episodes: String,
    description: String,
    title_id: Option<u64>,
    total_episodes: Option<u32>,
    source_label: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EpisodeProgress {
    pub title: String,
    pub link: String,
    pub episode_id: Option<u64>,
    pub episode_no: u32,
    pub watched: bool,
    pub view_count: u32,
    pub total_episodes: Option<u32>,
    pub is_true_final_episode: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TitleStatusChangePayload {
    input: Vec<TitleStatusChangeInput>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TitleStatusChangeInput {
    title_id: u64,
    watch_status: Option<&'static str>,
    is_favourite: u8,
    priority: i32,
    recommend: i32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WatchedEpisodesChangePayload {
    title_id: u64,
    episodes: Vec<WatchedEpisodeChangeInput>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WatchedEpisodeChangeInput {
    episode_id: u64,
    view_cnt: u32,
    created_time: String,
}

pub struct ShindenClientBackend {
    api: ShindenAPI,
    refresh_status: Mutex<WatchingCacheRefreshStatus>,
    user_anime_list_refresh_status: Mutex<UserAnimeListRefreshStatus>,
    user_id_cache: Mutex<CachedUserId>,
}

impl ShindenClientBackend {
    pub fn new() -> Result<Self, String> {
        let api = ShindenAPI::new().map_err(|error| {
            let _ = append_project_log("FATAL", &format!("Failed to create ShindenAPI: {error}"));
            format!("Failed to create ShindenAPI: {error}")
        })?;

        Ok(Self {
            api,
            refresh_status: Mutex::new(WatchingCacheRefreshStatus::default()),
            user_anime_list_refresh_status: Mutex::new(UserAnimeListRefreshStatus::default()),
            user_id_cache: Mutex::new(CachedUserId::default()),
        })
    }

    pub fn write_log(&self, level: String, message: String) -> Result<(), String> {
        discard_log_path(append_project_log(&level, &message))
    }

    pub async fn test_connection(&self) -> Result<(), String> {
        match self.api.get_html("http://shinden.pl").await {
            Ok(_) => Ok(()),
            Err(e) => Err(command_error(
                "test_connection",
                format!("Connection failed: {}", e),
            )),
        }
    }

    pub async fn search(&self, query: String) -> Result<Vec<SearchAnime>, String> {
        let results = self
            .api
            .search_anime(&query)
            .await
            .map_err(|e| command_error("search", e))?;

        let watching_items = fetch_all_userlist_items(&self.api, &self.user_id_cache)
            .await
            .unwrap_or_default();

        Ok(map_search_anime_results(results, watching_items))
    }

    pub async fn get_search_filter_catalog(&self) -> Result<SearchFilterCatalog, String> {
        self.api
            .get_search_filter_catalog()
            .await
            .map_err(|error| command_error("get_search_filter_catalog", error))
    }

    pub async fn search_with_filters(
        &self,
        request: SearchFilterRequest,
    ) -> Result<SearchAnimePage, String> {
        let page = self
            .api
            .search_anime_with_filters(&request)
            .await
            .map_err(|error| command_error("search_with_filters", error))?;

        let watching_items = fetch_all_userlist_items(&self.api, &self.user_id_cache)
            .await
            .unwrap_or_default();

        Ok(SearchAnimePage {
            items: map_search_anime_results(page.items, watching_items),
            current_page: page.current_page,
            total_pages: page.total_pages,
        })
    }

    pub async fn get_main_premieres(&self) -> Result<Vec<DiscoveryAnime>, String> {
        let html = self
            .api
            .get_html(SHINDEN_MAIN_URL)
            .await
            .map_err(|e| command_error("get_main_premieres", e))?;

        let watching_items = fetch_all_userlist_items(&self.api, &self.user_id_cache)
            .await
            .unwrap_or_default();

        Ok(map_discovery_anime_results(
            parse_main_premieres_html(&html),
            watching_items,
        ))
    }

    pub async fn get_season_anime(
        &self,
        year: Option<u16>,
        season: String,
    ) -> Result<Vec<DiscoveryAnime>, String> {
        let url = season_page_url(year, &season);
        let html = self
            .api
            .get_html(&url)
            .await
            .map_err(|e| command_error("get_season_anime", e))?;

        let watching_items = fetch_all_userlist_items(&self.api, &self.user_id_cache)
            .await
            .unwrap_or_default();

        Ok(map_discovery_anime_results(
            parse_season_anime_html(&html),
            watching_items,
        ))
    }

    pub async fn get_anime_details(&self, url: String) -> Result<AnimeDetails, String> {
        let details = self
            .api
            .get_anime_details(&url)
            .await
            .map_err(|e| command_error("get_anime_details", e))?;

        let (title_status, user_status_loaded) = match details.title_id {
            Some(title_id) => match fetch_current_user_id_cached(
                &self.api,
                &self.user_id_cache,
                "get_anime_details",
            )
            .await
            {
                Ok(user_id) => match fetch_title_status(&self.api, title_id, &user_id).await {
                    Ok(title_status) => (title_status, true),
                    Err(error) => {
                        let _ = command_error("get_anime_details status fallback", error);
                        (None, false)
                    }
                },
                Err(error) => {
                    let _ = command_error("get_anime_details user fallback", error);
                    (None, false)
                }
            },
            None => (None, false),
        };

        Ok(anime_details_with_title_status(
            details,
            title_status,
            user_status_loaded,
        ))
    }

    pub async fn get_watching_anime(
        &self,
        filter: Option<WatchingAnimeFilter>,
    ) -> Result<Vec<WatchingAnime>, String> {
        let filter = filter.unwrap_or_default();
        let cache = load_watching_availability_cache();
        if !watching_filter_requires_availability_cache(&filter) {
            let cached_anime = cached_watching_anime(&load_user_anime_list_cache());
            if !cached_anime.is_empty() {
                return Ok(cached_anime);
            }
        }

        let items = fetch_all_watching_items(&self.api, &self.user_id_cache).await?;
        let mut canonical_urls = cached_canonical_title_urls(&load_user_anime_list_cache());
        canonical_urls.extend(
            cache.canonical_title_urls.iter()
                .filter(|(title_id, url)| is_canonical_title_url(url, **title_id))
                .map(|(title_id, url)| (*title_id, url.clone())),
        );
        let mut anime = Vec::new();

        for item in items
            .into_iter()
            .filter(|item| watching_cache_filter_matches(item, &filter, &cache))
        {
            let url = canonical_url_from_cache_or_fallback(item.title_id, &canonical_urls);

            if let Some(item) = map_watching_list_item_details_with_url(item, url) {
                anime.push(item);
            }
        }

        Ok(anime)
    }

    pub async fn get_user_anime_lists(
        &self,
        force_refresh: Option<bool>,
    ) -> Result<UserAnimeListsPayload, String> {
        let force_refresh = force_refresh.unwrap_or(false);
        let mut cache = load_user_anime_list_cache();
        let now_ms = unix_timestamp_ms_u64();

        if should_return_cached_user_anime_lists(&cache, force_refresh) {
            let active_items = active_user_anime_list_items(&cache);
            return Ok(user_anime_lists_payload(
                active_items,
                cache.refreshed_at_ms,
                None,
            ));
        }

        match fetch_all_userlist_items(&self.api, &self.user_id_cache).await {
            Ok(items) => {
                let existing_keys = user_anime_list_cache_keys(&cache);
                merge_user_anime_list_cache(&mut cache, items, force_refresh, now_ms);
                let new_keys = new_active_user_anime_list_cache_keys(&cache, &existing_keys);
                let sync_error = refresh_new_user_anime_detail_metadata(
                    &self.api,
                    &mut cache,
                    new_keys,
                    now_ms,
                )
                .await;
                let active_items = active_user_anime_list_items(&cache);
                cache.refreshed_at_ms = Some(now_ms);
                save_user_anime_list_cache(&cache)?;

                Ok(user_anime_lists_payload(
                    active_items,
                    cache.refreshed_at_ms,
                    sync_error,
                ))
            }
            Err(error) => {
                let active_items = active_user_anime_list_items(&cache);
                if active_items.is_empty() {
                    Err(error)
                } else {
                    Ok(user_anime_lists_payload(
                        active_items,
                        cache.refreshed_at_ms,
                        Some(error),
                    ))
                }
            }
        }
    }

    pub fn get_user_anime_list_refresh_status(&self) -> Result<UserAnimeListRefreshStatus, String> {
        user_anime_list_refresh_status_snapshot(&self.user_anime_list_refresh_status)
    }

    pub async fn refresh_user_anime_list_cache(
        &self,
    ) -> Result<UserAnimeListRefreshSummary, String> {
        if let Some(summary) =
            begin_user_anime_list_refresh(&self.user_anime_list_refresh_status, None)?
        {
            return Ok(summary);
        }

        let refresh_result = refresh_user_anime_list_cache_inner(
            &self.api,
            &self.user_id_cache,
            &self.user_anime_list_refresh_status,
        )
        .await;

        match refresh_result {
            Ok(status) => Ok(UserAnimeListRefreshSummary {
                status,
                already_running: false,
            }),
            Err(error) => {
                fail_user_anime_list_refresh(&self.user_anime_list_refresh_status, &error)?;
                Err(error)
            }
        }
    }

    pub async fn resume_user_anime_list_cache_refresh(
        &self,
    ) -> Result<UserAnimeListRefreshSummary, String> {
        let mut state = load_user_anime_list_refresh_state();
        if !user_anime_list_refresh_state_has_pending(&state) {
            let status = user_anime_list_refresh_status_from_state(&state, false);
            replace_user_anime_list_refresh_status(
                &self.user_anime_list_refresh_status,
                status.clone(),
            )?;
            return Ok(UserAnimeListRefreshSummary {
                status,
                already_running: false,
            });
        }

        if let Some(summary) =
            begin_user_anime_list_refresh(&self.user_anime_list_refresh_status, Some(&state))?
        {
            return Ok(summary);
        }

        let mut cache = load_user_anime_list_cache();
        let refresh_result = process_user_anime_list_refresh_queue(
            &self.api,
            &self.user_anime_list_refresh_status,
            &mut cache,
            &mut state,
        )
        .await;

        match refresh_result {
            Ok(status) => Ok(UserAnimeListRefreshSummary {
                status,
                already_running: false,
            }),
            Err(error) => {
                fail_user_anime_list_refresh(&self.user_anime_list_refresh_status, &error)?;
                Err(error)
            }
        }
    }

    pub async fn get_episodes_with_progress(
        &self,
        url: String,
        title_id: Option<u64>,
        total_episodes: Option<u32>,
        title_name: Option<String>,
    ) -> Result<Vec<EpisodeProgress>, String> {
        let playback_url = match (
            title_id.or_else(|| title_id_from_series_url(&url).and_then(|value| value.parse::<u64>().ok())),
            title_name.as_deref(),
        ) {
            (Some(resolved_title_id), Some(title_name)) => {
                resolve_playback_title_url(&self.api, resolved_title_id, title_name, &url).await?
            }
            _ => url.clone(),
        };

        let playback_episodes = self
            .api
            .get_episodes(&playback_url)
            .await
            .map_err(|e| command_error("get_episodes_with_progress playback", e))?;

        let Some(title_id) = title_id.or_else(|| {
            title_id_from_series_url(&url).and_then(|title_id| title_id.parse::<u64>().ok())
        }) else {
            return Ok(merge_episode_progress(
                playback_episodes,
                Vec::new(),
                total_episodes,
            ));
        };

        let progress_episodes = match fetch_current_user_id_cached(
            &self.api,
            &self.user_id_cache,
            "get_episodes_with_progress",
        )
        .await
        {
            Ok(user_id) => fetch_title_episode_progress(&self.api, title_id, &user_id)
                .await
                .unwrap_or_else(|error| {
                    let _ = command_error("get_episodes_with_progress progress fallback", error);
                    Vec::new()
                }),
            Err(error) => {
                let _ = command_error("get_episodes_with_progress user fallback", error);
                Vec::new()
            }
        };

        Ok(merge_episode_progress(
            playback_episodes,
            progress_episodes,
            total_episodes,
        ))
    }

    pub async fn update_anime_status(
        &self,
        title_id: u64,
        status: Option<String>,
        is_favourite: Option<u8>,
    ) -> Result<(), String> {
        let user_id =
            fetch_current_user_id_cached(&self.api, &self.user_id_cache, "update_anime_status")
                .await?;
        let current_status = fetch_title_status(&self.api, title_id, &user_id)
            .await
            .unwrap_or_default();
        let payload = build_title_status_payload_with_details(
            title_id,
            status.as_deref(),
            is_favourite.or_else(|| {
                current_status
                    .as_ref()
                    .and_then(|status| status.is_favourite)
            }),
            current_status
                .as_ref()
                .and_then(|status| status.priority)
                .unwrap_or_default(),
            current_status
                .as_ref()
                .and_then(|status| status.recommend)
                .unwrap_or_default(),
        )?;

        post_shinden_json(
            &self.api,
            "https://lista.shinden.pl/api/title-status-change",
            &payload,
            "update_anime_status",
        )
        .await?;

        match verify_title_status_change_with_user(
            &self.api,
            title_id,
            &user_id,
            status.as_deref(),
            "update_anime_status",
        )
        .await
        {
            Ok(()) => Ok(()),
            Err(verify_error) => {
                let _ = append_project_log(
                    "WARNING",
                    &format!(
                        "update_anime_status fallback after failed list verification: {verify_error}"
                    ),
                );
                post_legacy_anime_status(
                    &self.api,
                    title_id,
                    &user_id,
                    status.as_deref(),
                    &payload.input[0],
                )
                .await?;
                verify_title_status_change_with_user(
                    &self.api,
                    title_id,
                    &user_id,
                    status.as_deref(),
                    "update_anime_status legacy verify",
                )
                .await
            }
        }
    }

    pub async fn update_anime_rating(&self, update: AnimeRatingUpdate) -> Result<(), String> {
        let rating_type = normalize_rating_type(&update.rating_type).ok_or_else(|| {
            command_error(
                "update_anime_rating",
                format!("Unsupported anime rating type: {}", update.rating_type),
            )
        })?;
        let title_type = if update.title_type.trim().is_empty() {
            "anime".to_string()
        } else {
            update.title_type.trim().to_ascii_lowercase()
        };
        let update = AnimeRatingUpdate {
            title_id: update.title_id,
            title_type,
            rating_type,
            value: update.value.min(10),
        };
        let page_html = self
            .api
            .get_html(&series_url(update.title_id))
            .await
            .map_err(|e| command_error("update_anime_rating auth", e))?;
        let auth = basic_auth_token(&page_html).ok_or_else(|| {
            command_error("update_anime_rating auth", "Shinden auth token missing")
        })?;
        let response = self
            .api
            .post_form(
                &anime_rating_url(&update.title_type, update.title_id),
                &rating_update_form(&update, &auth),
                None,
            )
            .await
            .map_err(|e| command_error("update_anime_rating", e))?;

        if rating_response_is_success(&response) {
            Ok(())
        } else {
            Err(command_error(
                "update_anime_rating",
                format!("Shinden rejected rating update: {response}"),
            ))
        }
    }

    pub async fn mark_episode_watched(
        &self,
        title_id: u64,
        episode_id: u64,
        created_time: String,
    ) -> Result<(), String> {
        let payload = build_watched_episode_payload(title_id, episode_id, created_time, 1);
        post_shinden_json(
            &self.api,
            "https://lista.shinden.pl/api/title-watched-episodes-change",
            &payload,
            "mark_episode_watched",
        )
        .await
    }

    pub async fn mark_episode_unwatched(
        &self,
        title_id: u64,
        episode_id: u64,
        created_time: String,
    ) -> Result<(), String> {
        let payload = build_watched_episode_payload(title_id, episode_id, created_time, 0);
        post_shinden_json(
            &self.api,
            "https://lista.shinden.pl/api/title-watched-episodes-change",
            &payload,
            "mark_episode_unwatched",
        )
        .await
    }

    pub fn get_watching_cache_refresh_status(&self) -> Result<WatchingCacheRefreshStatus, String> {
        refresh_status_snapshot(&self.refresh_status)
    }

    pub fn get_watching_episode_availability(
        &self,
        title_id: u64,
    ) -> Option<HashMap<String, WatchingEpisodeAvailability>> {
        load_watching_availability_cache()
            .entries
            .get(&watching_cache_key(title_id))
            .filter(|entry| entry.title_id == title_id)
            .map(|entry| entry.episode_availability.clone())
    }

    pub async fn refresh_watching_anime_cache(
        &self,
        filter: Option<WatchingAnimeFilter>,
        force: Option<bool>,
    ) -> Result<WatchingCacheRefreshSummary, String> {
        let filter = filter.unwrap_or_default();
        let force = force.unwrap_or(false);

        if let Some(summary) = begin_watching_cache_refresh(&self.refresh_status)? {
            return Ok(summary);
        }

        let refresh_result = refresh_watching_cache_inner(
            &self.api,
            &self.refresh_status,
            &self.user_id_cache,
            &filter,
            force,
        )
        .await;

        match refresh_result {
            Ok(status) => Ok(WatchingCacheRefreshSummary {
                status,
                already_running: false,
            }),
            Err(error) => {
                update_refresh_status(&self.refresh_status, |status| {
                    status.running = false;
                    status.current_title.clear();
                    status.last_finished_at_ms = Some(unix_timestamp_ms_u64());
                    status.last_error = Some(error.clone());
                })?;
                Err(error)
            }
        }
    }

    pub async fn refresh_watching_anime_cache_item(
        &self,
        title_id: u64,
        filter: Option<WatchingAnimeFilter>,
        force: Option<bool>,
    ) -> Result<WatchingCacheRefreshSummary, String> {
        let filter = filter.unwrap_or_default();
        let force = force.unwrap_or(false);

        if let Some(summary) = begin_watching_cache_refresh(&self.refresh_status)? {
            return Ok(summary);
        }

        let refresh_result = refresh_watching_cache_item_inner(
            &self.api,
            &self.refresh_status,
            &self.user_id_cache,
            title_id,
            &filter,
            force,
        )
        .await;

        match refresh_result {
            Ok(status) => Ok(WatchingCacheRefreshSummary {
                status,
                already_running: false,
            }),
            Err(error) => {
                update_refresh_status(&self.refresh_status, |status| {
                    status.running = false;
                    status.current_title.clear();
                    status.last_finished_at_ms = Some(unix_timestamp_ms_u64());
                    status.last_error = Some(error.clone());
                })?;
                Err(error)
            }
        }
    }

    pub async fn login(&self, username: String, password: String) -> Result<(), String> {
        let result = self
            .api
            .login(&username, &password)
            .await
            .map_err(|e| command_error("login", e));

        if result.is_ok() {
            clear_cached_user_id(&self.user_id_cache)?;
        }

        result
    }

    pub async fn logout(&self) -> Result<(), String> {
        self.api
            .logout()
            .await
            .map_err(|e| command_error("logout", e))?;
        clear_cached_user_id(&self.user_id_cache)
    }

    pub async fn get_user_name(&self) -> Result<Option<String>, String> {
        self.api
            .get_user_name()
            .await
            .map_err(|e| command_error("get_user_name", e))
    }

    pub async fn get_user_profile_image(&self) -> Result<Option<String>, String> {
        self.api
            .get_user_profile_image()
            .await
            .map_err(|e| command_error("get_user_profile_image", e))
    }

    pub async fn get_episodes(&self, url: String) -> Result<Vec<Episode>, String> {
        self.api
            .get_episodes(&url)
            .await
            .map_err(|e| command_error("get_episodes", e))
    }

    pub async fn get_players(&self, url: String) -> Result<Vec<Player>, String> {
        self.api
            .get_players(&url)
            .await
            .map_err(|e| command_error("get_players", e))
    }

    pub async fn get_iframe(&self, id: String) -> Result<String, String> {
        self.api
            .get_player_iframe(&id)
            .await
            .map_err(|e| command_error("get_iframe", e))
    }
}

async fn fetch_all_watching_items(
    api: &ShindenAPI,
    user_id_cache: &Mutex<CachedUserId>,
) -> Result<Vec<WatchingListApiItem>, String> {
    let user_id = fetch_current_user_id_cached(api, user_id_cache, "get_watching_anime").await?;
    fetch_all_watching_items_for_status(api, &user_id, "in progress").await
}

async fn fetch_all_userlist_items(
    api: &ShindenAPI,
    user_id_cache: &Mutex<CachedUserId>,
) -> Result<Vec<WatchingListApiItem>, String> {
    let user_id = fetch_current_user_id_cached(api, user_id_cache, "search").await?;
    let mut items = Vec::new();

    for status in WATCHING_LIST_STATUSES {
        items.extend(fetch_all_watching_items_for_status(api, &user_id, status).await?);
    }

    Ok(items)
}

async fn fetch_all_watching_items_for_status(
    api: &ShindenAPI,
    user_id: &str,
    status: &str,
) -> Result<Vec<WatchingListApiItem>, String> {
    let mut offset = 0;
    let mut items = Vec::new();

    loop {
        let page =
            fetch_watching_list_status_page(api, user_id, status, WATCHING_LIST_PAGE_LIMIT, offset)
                .await?;
        let loaded = page.items.len();
        let total = page.count;

        items.extend(page.items);

        offset += loaded;
        if loaded == 0 || offset >= total {
            break;
        }
    }

    Ok(items)
}

async fn fetch_current_user_id(api: &ShindenAPI, context: &str) -> Result<String, String> {
    let profile_context = format!("{context} profile");
    let profile_html = api
        .get_html("https://shinden.pl/user")
        .await
        .map_err(|e| command_error(&profile_context, e))?;

    extract_user_id_from_profile_html(&profile_html)
        .ok_or_else(|| command_error(&profile_context, "User is not logged in"))
}

async fn fetch_current_user_id_cached(
    api: &ShindenAPI,
    user_id_cache: &Mutex<CachedUserId>,
    context: &str,
) -> Result<String, String> {
    let now_ms = unix_timestamp_ms_u64();
    {
        let cache = lock_user_id_cache(user_id_cache)?;
        if let Some(user_id) = cached_user_id_if_fresh(&*cache, now_ms) {
            return Ok(user_id);
        }
    }

    match fetch_current_user_id(api, context).await {
        Ok(user_id) => {
            store_cached_user_id(user_id_cache, &user_id, now_ms)?;
            Ok(user_id)
        }
        Err(error) => {
            if is_transient_user_profile_error(&error) {
                let cached_user_id = {
                    let cache = lock_user_id_cache(user_id_cache)?;
                    cache.user_id.clone()
                };
                if let Some(user_id) = cached_user_id {
                    let _ = append_project_log(
                        "WARNING",
                        &format!(
                            "Using cached Shinden user id after {context} profile error: {error}"
                        ),
                    );
                    return Ok(user_id);
                }
            }

            Err(error)
        }
    }
}

fn is_transient_user_profile_error(error: &str) -> bool {
    error.contains("429 Too Many Requests") || error.contains("error sending request")
}

fn cached_user_id_if_fresh(cache: &CachedUserId, now_ms: u64) -> Option<String> {
    let user_id = cache.user_id.as_ref()?;
    if now_ms.saturating_sub(cache.checked_at_ms) <= USER_ID_CACHE_TTL_MS {
        Some(user_id.clone())
    } else {
        None
    }
}

fn store_cached_user_id(
    user_id_cache: &Mutex<CachedUserId>,
    user_id: &str,
    checked_at_ms: u64,
) -> Result<(), String> {
    let mut cache = lock_user_id_cache(user_id_cache)?;
    store_cached_user_id_value(&mut cache, user_id, checked_at_ms);
    Ok(())
}

fn store_cached_user_id_value(cache: &mut CachedUserId, user_id: &str, checked_at_ms: u64) {
    cache.user_id = Some(user_id.to_string());
    cache.checked_at_ms = checked_at_ms;
}

fn clear_cached_user_id(user_id_cache: &Mutex<CachedUserId>) -> Result<(), String> {
    let mut cache = lock_user_id_cache(user_id_cache)?;
    *cache = CachedUserId::default();
    Ok(())
}

fn lock_user_id_cache(
    user_id_cache: &Mutex<CachedUserId>,
) -> Result<std::sync::MutexGuard<'_, CachedUserId>, String> {
    user_id_cache
        .lock()
        .map_err(|_| command_error("user_id_cache", "User id cache lock poisoned"))
}

async fn fetch_shinden_basic_auth(api: &ShindenAPI) -> Result<String, String> {
    let profile_html = api
        .get_html("https://shinden.pl/user")
        .await
        .map_err(|e| command_error("legacy auth profile", e))?;

    extract_shinden_basic_auth(&profile_html)
        .ok_or_else(|| command_error("legacy auth profile", "Could not find Shinden auth token"))
}

async fn fetch_watching_list_status_page(
    api: &ShindenAPI,
    user_id: &str,
    status: &str,
    limit: usize,
    offset: usize,
) -> Result<WatchingListApiResult, String> {
    let url = watching_list_status_url(user_id, status, limit, offset);
    let response = api
        .client
        .get(&url)
        .header(ACCEPT, "application/json")
        .send()
        .await
        .map_err(|e| command_error("get_watching_anime request", e))?
        .error_for_status()
        .map_err(|e| command_error("get_watching_anime response", e))?;
    let payload = response
        .json::<WatchingListApiResponse>()
        .await
        .map_err(|e| command_error("get_watching_anime json", e))?;

    if !payload.success {
        return Err(command_error(
            "get_watching_anime json",
            "List API returned success=false",
        ));
    }

    Ok(payload.result)
}

async fn fetch_title_status(
    api: &ShindenAPI,
    title_id: u64,
    user_id: &str,
) -> Result<Option<TitleStatusApiTitle>, String> {
    let response = api
        .client
        .get(title_status_url(title_id, user_id))
        .header(ACCEPT, "application/json")
        .send()
        .await
        .map_err(|e| command_error("title_status request", e))?
        .error_for_status()
        .map_err(|e| command_error("title_status response", e))?;

    let payload = response
        .json::<TitleStatusApiResponse>()
        .await
        .map_err(|e| command_error("title_status json", e))?;

    if !payload.success {
        return Err(command_error(
            "title_status json",
            payload
                .message
                .unwrap_or_else(|| "List API returned success=false".to_string()),
        ));
    }

    Ok(payload.result.title)
}

fn anime_details_with_title_status(
    mut details: AnimeDetails,
    title_status: Option<TitleStatusApiTitle>,
    user_status_loaded: bool,
) -> AnimeDetails {
    if user_status_loaded {
        details.user_status_loaded = true;
    }

    if let Some(title_status) = title_status {
        details.watch_status = title_status
            .watch_status
            .unwrap_or_else(|| "no".to_string());
        details.is_favourite = title_status.is_favourite.unwrap_or_default();
    } else if user_status_loaded {
        details.watch_status = "no".to_string();
        details.is_favourite = 0;
    }

    details
}

async fn fetch_title_episode_progress(
    api: &ShindenAPI,
    title_id: u64,
    user_id: &str,
) -> Result<Vec<TitleEpisodeApiItem>, String> {
    let response = api
        .client
        .get(title_episodes_url(title_id, user_id))
        .header(ACCEPT, "application/json")
        .send()
        .await
        .map_err(|e| command_error("title_episodes request", e))?
        .error_for_status()
        .map_err(|e| command_error("title_episodes response", e))?;

    let payload = response
        .json::<TitleEpisodesApiResponse>()
        .await
        .map_err(|e| command_error("title_episodes json", e))?;

    if !payload.success {
        return Err(command_error(
            "title_episodes json",
            payload
                .message
                .unwrap_or_else(|| "List API returned success=false".to_string()),
        ));
    }

    Ok(payload.result.items)
}

async fn post_shinden_json<T: Serialize>(
    api: &ShindenAPI,
    url: &str,
    payload: &T,
    context: &str,
) -> Result<(), String> {
    let request_context = format!("{context} request");
    let response_context = format!("{context} response");
    let json_context = format!("{context} json");
    let response = api
        .client
        .post(url)
        .header(ACCEPT, "application/json")
        .header(CONTENT_TYPE, "application/json")
        .header(ORIGIN, "https://lista.shinden.pl")
        .header(REFERER, "https://lista.shinden.pl/")
        .json(payload)
        .send()
        .await
        .map_err(|e| command_error(&request_context, e))?
        .error_for_status()
        .map_err(|e| command_error(&response_context, e))?;

    let payload = response
        .json::<ShindenWriteResponse>()
        .await
        .map_err(|e| command_error(&json_context, e))?;

    if payload.success {
        Ok(())
    } else {
        Err(command_error(
            &json_context,
            payload
                .message
                .unwrap_or_else(|| "Shinden returned success=false".to_string()),
        ))
    }
}

async fn post_legacy_anime_status(
    api: &ShindenAPI,
    title_id: u64,
    user_id: &str,
    status: Option<&str>,
    input: &TitleStatusChangeInput,
) -> Result<(), String> {
    let legacy_statuses = shinden_legacy_watch_status_values(status)?;
    if legacy_statuses.is_empty() {
        return post_legacy_anime_status_delete(api, title_id, user_id).await;
    }

    let basic_auth = fetch_shinden_basic_auth(api).await?;
    let priority = input.priority.to_string();
    let recommend = input.recommend.to_string();
    let url = legacy_userlist_series_url(user_id, title_id);

    let mut last_error = None;

    for legacy_status in legacy_statuses {
        let response = api
            .client
            .post(&url)
            .header(ACCEPT, "application/json")
            .header("X-Requested-With", "XMLHttpRequest")
            .header(ORIGIN, "https://shinden.pl")
            .header(REFERER, series_url(title_id))
            .form(&[
                ("status", legacy_status),
                ("priority", priority.as_str()),
                ("recommend", recommend.as_str()),
                ("auth", basic_auth.as_str()),
            ])
            .send()
            .await
            .map_err(|e| command_error("legacy_update_anime_status request", e))?
            .error_for_status()
            .map_err(|e| command_error("legacy_update_anime_status response", e))?;

        if let Err(error) = validate_legacy_write_response(
            response
                .text()
                .await
                .map_err(|e| command_error("legacy_update_anime_status text", e))?,
        ) {
            last_error = Some(error);
            continue;
        }

        match verify_title_status_change_with_user(
            api,
            title_id,
            user_id,
            status,
            "legacy_update_anime_status",
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
    }

    Err(last_error.unwrap_or_else(|| {
        command_error(
            "legacy_update_anime_status",
            "Shinden did not confirm legacy status change",
        )
    }))
}

async fn post_legacy_anime_status_delete(
    api: &ShindenAPI,
    title_id: u64,
    user_id: &str,
) -> Result<(), String> {
    let basic_auth = fetch_shinden_basic_auth(api).await?;
    let url = legacy_userlist_series_url(user_id, title_id);
    let response = api
        .client
        .post(&url)
        .header(ACCEPT, "application/json")
        .header(CONTENT_TYPE, "application/json")
        .header("X-HTTP-Method-Override", "DELETE")
        .header("X-Requested-With", "XMLHttpRequest")
        .header(ORIGIN, "https://shinden.pl")
        .header(REFERER, series_url(title_id))
        .json(&serde_json::json!({ "auth": basic_auth }))
        .send()
        .await
        .map_err(|e| command_error("legacy_delete_anime_status request", e))?
        .error_for_status()
        .map_err(|e| command_error("legacy_delete_anime_status response", e))?;

    validate_legacy_write_response(
        response
            .text()
            .await
            .map_err(|e| command_error("legacy_delete_anime_status text", e))?,
    )
}

fn validate_legacy_write_response(response_text: String) -> Result<(), String> {
    let trimmed = response_text.trim();
    if trimmed.is_empty() {
        return Ok(());
    }

    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return Ok(());
    };

    if value
        .get("success")
        .and_then(|success| success.as_bool())
        .is_some_and(|success| !success)
    {
        return Err(command_error(
            "legacy_update_anime_status json",
            value
                .get("message")
                .and_then(|message| message.as_str())
                .unwrap_or("Shinden returned success=false"),
        ));
    }

    if let Some(error) = value
        .get("error")
        .or_else(|| value.get("err"))
        .and_then(|error| error.as_str())
    {
        if !error.trim().is_empty() {
            return Err(command_error("legacy_update_anime_status json", error));
        }
    }

    Ok(())
}

async fn verify_title_status_change_with_user(
    api: &ShindenAPI,
    title_id: u64,
    user_id: &str,
    status: Option<&str>,
    context: &str,
) -> Result<(), String> {
    let expected_status = shinden_watch_status_value(status)?;
    let statuses = match expected_status {
        Some(status) => vec![status],
        None => WATCHING_LIST_STATUSES.to_vec(),
    };

    let mut found_status = None;

    for status in statuses {
        let items = fetch_all_watching_items_for_status(api, &user_id, status).await?;

        if let Some(item) = items.into_iter().find(|item| item.title_id == title_id) {
            found_status = Some(item.watch_status.unwrap_or_else(|| status.to_string()));
            break;
        }
    }

    match expected_status {
        Some(expected_status) => {
            let Some(found_status) = found_status else {
                return Err(command_error(
                    context,
                    format!("Shinden did not confirm status change for title {title_id}"),
                ));
            };

            if title_status_matches(Some(&found_status), Some(expected_status))? {
                Ok(())
            } else {
                Err(command_error(
                    context,
                    format!(
                        "Shinden saved status {found_status}, expected {expected_status} for title {title_id}"
                    ),
                ))
            }
        }
        None => {
            if found_status.is_none() {
                Ok(())
            } else {
                Err(command_error(
                    context,
                    format!("Shinden did not remove title {title_id} from the user list"),
                ))
            }
        }
    }
}

async fn refresh_watching_cache_inner(
    api: &ShindenAPI,
    status: &Mutex<WatchingCacheRefreshStatus>,
    user_id_cache: &Mutex<CachedUserId>,
    filter: &WatchingAnimeFilter,
    force: bool,
) -> Result<WatchingCacheRefreshStatus, String> {
    let items = fetch_all_watching_items(api, user_id_cache).await?;
    let mut cache = load_watching_availability_cache();
    let subtitle_key = selected_subtitle_language_key(filter);
    let subtitle_cache_key = selected_subtitle_cache_key(filter);
    let now_ms = unix_timestamp_ms_u64();

    update_refresh_status(status, |status| {
        status.total = items.len();
    })?;

    let plan = collect_watching_cache_refresh_plan(
        &items,
        &cache,
        subtitle_cache_key.as_deref(),
        now_ms,
        force,
    );

    update_refresh_status(status, |status| {
        status.current = plan.processed;
        status.skipped = plan.skipped;
    })?;

    let subtitle_key = subtitle_key.as_deref();
    let subtitle_cache_key = subtitle_cache_key.as_deref();
    let exclude_ai_subtitles = filter.exclude_ai_subtitles();
    let mut scan_results = stream::iter(plan.items_to_scan.into_iter().map(|item| async move {
        let cache_key = watching_cache_key(item.title_id);
        let item_title = item.title.clone();
        let title_id = item.title_id;
        let result = scan_watching_item_availability(
            api,
            &item,
            subtitle_key,
            subtitle_cache_key,
            exclude_ai_subtitles,
        )
        .await;

        (cache_key, item_title, title_id, result)
    }))
    .buffer_unordered(WATCHING_CACHE_REFRESH_CONCURRENCY);

    while let Some((cache_key, item_title, title_id, scan_result)) = scan_results.next().await {
        update_refresh_status(status, |status| {
            status.current += 1;
            status.current_title = item_title.clone();
        })?;

        match scan_result {
            Ok(entry) => {
                cache.entries.insert(cache_key, entry);
                save_watching_availability_cache(&cache)?;
                update_refresh_status(status, |status| {
                    status.refreshed += 1;
                })?;
            }
            Err(error) => {
                let visible_error = watching_cache_item_error_message(&item_title);
                let _ = command_error("watching_cache item", format!("{visible_error}: {error}"));
                let failure = WatchingCacheFailure {
                    title_id,
                    title: item_title.clone(),
                    series_url: series_url(title_id),
                    reason: error.to_string(),
                };
                update_refresh_status(status, |status| {
                    status.failed += 1;
                    status.failures.push(failure);
                    status.last_error = Some(visible_error);
                })?;
            }
        }
    }

    update_refresh_status(status, |status| {
        status.running = false;
        status.current_title.clear();
        status.last_finished_at_ms = Some(unix_timestamp_ms_u64());
    })?;

    refresh_status_snapshot(status)
}

async fn refresh_watching_cache_item_inner(
    api: &ShindenAPI,
    status: &Mutex<WatchingCacheRefreshStatus>,
    user_id_cache: &Mutex<CachedUserId>,
    title_id: u64,
    filter: &WatchingAnimeFilter,
    force: bool,
) -> Result<WatchingCacheRefreshStatus, String> {
    let items = fetch_all_watching_items(api, user_id_cache).await?;
    let item = items.into_iter().find(|item| item.title_id == title_id);
    let cache_key = watching_cache_key(title_id);
    let mut cache = load_watching_availability_cache();

    update_refresh_status(status, |status| {
        status.total = 1;
        status.current_title = item
            .as_ref()
            .map(|item| item.title.clone())
            .unwrap_or_else(|| format!("Anime {title_id}"));
    })?;

    let Some(item) = item else {
        let removed = cache.entries.remove(&cache_key).is_some();
        if removed {
            save_watching_availability_cache(&cache)?;
        }
        return finish_single_watching_cache_refresh(status, removed);
    };

    if !has_unwatched_episodes(&item) {
        let removed = cache.entries.remove(&cache_key).is_some();
        if removed {
            save_watching_availability_cache(&cache)?;
        }
        return finish_single_watching_cache_refresh(status, removed);
    }

    let subtitle_key = selected_subtitle_language_key(filter);
    let subtitle_cache_key = selected_subtitle_cache_key(filter);

    if !force
        && cache.entries.get(&cache_key).is_some_and(|entry| {
            cache_entry_satisfies_refresh(
                entry,
                &item,
                subtitle_cache_key.as_deref(),
                unix_timestamp_ms_u64(),
                false,
            )
        })
    {
        return finish_single_watching_cache_refresh(status, false);
    }

    match scan_watching_item_availability(
        api,
        &item,
        subtitle_key.as_deref(),
        subtitle_cache_key.as_deref(),
        filter.exclude_ai_subtitles(),
    )
    .await
    {
        Ok(entry) => {
            cache.entries.insert(cache_key, entry);
            save_watching_availability_cache(&cache)?;
            finish_single_watching_cache_refresh(status, true)
        }
        Err(error) => {
            let visible_error = watching_cache_item_error_message(&item.title);
            let _ = command_error("watching_cache item", format!("{visible_error}: {error}"));
            update_refresh_status(status, |status| {
                status.current = 1;
                status.failed = 1;
                status.last_error = Some(visible_error.clone());
            })?;
            Err(visible_error)
        }
    }
}

fn finish_single_watching_cache_refresh(
    status: &Mutex<WatchingCacheRefreshStatus>,
    refreshed: bool,
) -> Result<WatchingCacheRefreshStatus, String> {
    update_refresh_status(status, |status| {
        status.current = 1;
        if refreshed {
            status.refreshed = 1;
        } else {
            status.skipped = 1;
        }
        status.running = false;
        status.current_title.clear();
        status.last_finished_at_ms = Some(unix_timestamp_ms_u64());
    })?;

    refresh_status_snapshot(status)
}

fn collect_watching_cache_refresh_plan(
    items: &[WatchingListApiItem],
    cache: &WatchingAvailabilityCache,
    subtitle_cache_key: Option<&str>,
    now_ms: u64,
    force: bool,
) -> WatchingCacheRefreshPlan {
    let mut items_to_scan = Vec::new();
    let mut skipped = 0;

    for item in items {
        if !has_unwatched_episodes(item) {
            skipped += 1;
            continue;
        }

        let cache_key = watching_cache_key(item.title_id);
        if cache.entries.get(&cache_key).is_some_and(|entry| {
            cache_entry_satisfies_refresh(entry, item, subtitle_cache_key, now_ms, force)
        }) {
            skipped += 1;
            continue;
        }

        items_to_scan.push(item.clone());
    }

    WatchingCacheRefreshPlan {
        items_to_scan,
        skipped,
        processed: skipped,
    }
}

async fn scan_watching_item_availability(
    api: &ShindenAPI,
    item: &WatchingListApiItem,
    _subtitle_key: Option<&str>,
    subtitle_cache_key: Option<&str>,
    _exclude_ai_subtitles: bool,
) -> Result<WatchingAvailabilityCacheEntry, String> {
    let series_url = resolve_canonical_title_url(api, item).await?;
    let episodes = get_watching_cache_episodes(api, &series_url).await?;
    let watched_count = watched_episode_count(item) as usize;
    let mut has_available_unwatched_episode = false;
    let mut subtitle_availability = HashMap::new();
    let mut episode_availability = HashMap::new();

    for (episode_index, episode) in episodes.into_iter().enumerate() {
        let players = get_watching_cache_players(api, &episode.link).await?;
        let availability = watching_episode_availability(&players);

        if episode_index >= watched_count {
            has_available_unwatched_episode |= availability.has_players;

            for (cache_key, is_available) in &availability.subtitle_availability {
                if *is_available {
                    subtitle_availability.insert(cache_key.clone(), true);
                }
            }
        }

        episode_availability.insert(episode.link, availability);
    }

    if let Some(cache_key) = subtitle_cache_key {
        subtitle_availability
            .entry(cache_key.to_string())
            .or_insert(false);
    }

    Ok(WatchingAvailabilityCacheEntry {
        title_id: item.title_id,
        watched_episodes_cnt: watched_episode_count(item),
        total_episodes: item.episodes,
        has_available_unwatched_episode,
        subtitle_availability,
        episode_availability,
        checked_at_ms: unix_timestamp_ms_u64(),
    })
}

fn watching_episode_availability(players: &[Player]) -> WatchingEpisodeAvailability {
    let mut subtitle_availability = HashMap::new();

    for player in players {
        record_subtitle_language_availability(&mut subtitle_availability, &player.lang_subs);
    }

    WatchingEpisodeAvailability {
        has_players: !players.is_empty(),
        subtitle_availability,
    }
}

#[cfg(test)]
fn record_watching_cache_episode_subtitle_availability<'a, I>(
    player_subtitles: I,
    subtitle_key: Option<&str>,
    subtitle_availability: &mut HashMap<String, bool>,
) -> bool
where
    I: IntoIterator<Item = &'a str>,
{
    let mut has_players = false;

    for language in player_subtitles {
        has_players = true;
        if subtitle_key.is_some() {
            record_subtitle_language_availability(subtitle_availability, language);
        }
    }

    has_players
}

async fn get_watching_cache_episodes(
    api: &ShindenAPI,
    series_url: &str,
) -> Result<Vec<Episode>, String> {
    let mut last_error = String::new();

    for attempt in 0..=WATCHING_CACHE_REQUEST_RETRIES {
        wait_before_background_request().await;
        match api.get_episodes(series_url).await {
            Ok(episodes) => return Ok(episodes),
            Err(error) => {
                last_error = error.to_string();
                log_watching_cache_retry("episodes", series_url, attempt, &last_error);
                wait_before_watching_cache_retry(attempt);
            }
        }
    }

    Err(format!("Nie udalo sie pobrac listy odcinkow: {last_error}"))
}

async fn get_watching_cache_players(
    api: &ShindenAPI,
    episode_url: &str,
) -> Result<Vec<Player>, String> {
    let mut last_error = String::new();

    for attempt in 0..=WATCHING_CACHE_REQUEST_RETRIES {
        wait_before_background_request().await;
        match api.get_players(episode_url).await {
            Ok(players) => return Ok(players),
            Err(error) => {
                last_error = error.to_string();
                log_watching_cache_retry("players", episode_url, attempt, &last_error);
                wait_before_watching_cache_retry(attempt);
            }
        }
    }

    Err(format!("Nie udalo sie sprawdzic odcinka: {last_error}"))
}

fn wait_before_watching_cache_retry(attempt: usize) {
    if attempt < WATCHING_CACHE_REQUEST_RETRIES {
        std::thread::sleep(Duration::from_millis(WATCHING_CACHE_RETRY_DELAY_MS));
    }
}

async fn wait_before_background_request() {
    sleep(Duration::from_millis(BACKGROUND_REQUEST_SPACING_MS)).await;
}

fn log_watching_cache_retry(context: &str, url: &str, attempt: usize, error: &str) {
    let _ = append_project_log(
        "WARNING",
        &format!(
            "watching_cache {context} attempt {}/{} failed for {url}: {error}",
            attempt + 1,
            WATCHING_CACHE_REQUEST_RETRIES + 1
        ),
    );
}

fn watching_cache_item_error_message(title: &str) -> String {
    format!("Nie udalo sie sprawdzic: {title}")
}

fn watching_list_status_url(user_id: &str, status: &str, limit: usize, offset: usize) -> String {
    let status = watch_status_list_slug(status);

    format!(
        "https://lista.shinden.pl/api/userlist/{user_id}/anime/{status}?limit={limit}&offset={offset}"
    )
}

fn title_status_url(title_id: u64, user_id: &str) -> String {
    format!("https://lista.shinden.pl/api/title-status/{title_id}/{user_id}")
}

fn season_page_url(year: Option<u16>, season: &str) -> String {
    let normalized = normalize_season_slug(season).unwrap_or_else(|| "current".to_string());
    if normalized == "current" {
        SHINDEN_SEASON_CURRENT_URL.to_string()
    } else {
        let year = year.unwrap_or(2026);
        format!("https://shinden.pl/series/season/{year}/{normalized}")
    }
}

fn normalize_season_slug(season: &str) -> Option<String> {
    let normalized = normalize_polish_ascii(season.trim());

    match normalized.as_str() {
        "current" | "obecny" | "aktualny" => Some("current".to_string()),
        "winter" | "zima" => Some("winter".to_string()),
        "spring" | "wiosna" => Some("spring".to_string()),
        "summer" | "lato" => Some("summer".to_string()),
        "fall" | "autumn" | "jesien" => Some("fall".to_string()),
        _ => None,
    }
}

fn normalize_polish_ascii(value: &str) -> String {
    let mut normalized = String::new();
    for character in value.to_lowercase().chars() {
        match character {
            '\u{0105}' => normalized.push('a'),
            '\u{0107}' => normalized.push('c'),
            '\u{0119}' => normalized.push('e'),
            '\u{0142}' => normalized.push('l'),
            '\u{0144}' => normalized.push('n'),
            '\u{00f3}' => normalized.push('o'),
            '\u{015b}' => normalized.push('s'),
            '\u{017a}' | '\u{017c}' => normalized.push('z'),
            _ => normalized.push(character),
        }
    }

    normalized
}

fn legacy_userlist_series_url(user_id: &str, title_id: u64) -> String {
    format!("https://shinden.pl/api/userlist/{user_id}/series/{title_id}")
}

fn series_url(title_id: u64) -> String {
    format!("https://shinden.pl/series/{title_id}")
}

fn rating_response_is_success(response: &str) -> bool {
    let normalized = response.trim().to_ascii_lowercase();
    normalized == "ok" || normalized.contains("\"success\":true")
}

fn canonical_title_url_from_search_results(title_id: u64, results: &[Anime]) -> Option<String> {
    results
        .iter()
        .find(|anime| {
            title_id_from_series_url(&anime.url)
                .and_then(|value| value.parse::<u64>().ok())
                == Some(title_id)
        })
        .map(|anime| anime.url.clone())
}

async fn resolve_canonical_title_url(
    api: &ShindenAPI,
    item: &WatchingListApiItem,
) -> Result<String, String> {
    wait_before_background_request().await;
    let results = api
        .search_anime(&item.title)
        .await
        .map_err(|error| command_error("resolve_canonical_title_url search", error))?;

    canonical_title_url_from_search_results(item.title_id, &results).ok_or_else(|| {
        command_error(
            "resolve_canonical_title_url result",
            format!("No search result matched title ID {}", item.title_id),
        )
    })
}

async fn resolve_playback_title_url(
    api: &ShindenAPI,
    title_id: u64,
    title_name: &str,
    fallback_url: &str,
) -> Result<String, String> {
    if is_canonical_title_url(fallback_url, title_id) {
        return Ok(fallback_url.to_string());
    }

    let mut cache = load_watching_availability_cache();
    if let Some(url) = cache.canonical_title_urls.get(&title_id) {
        if is_canonical_title_url(url, title_id) {
            return Ok(url.clone());
        }
    }

    if let Some(url) = cached_canonical_title_urls(&load_user_anime_list_cache()).get(&title_id) {
        return Ok(url.clone());
    }

    wait_before_background_request().await;
    let results = api
        .search_anime(title_name)
        .await
        .map_err(|error| command_error("resolve_playback_title_url search", error))?;
    let url = canonical_title_url_from_search_results(title_id, &results).ok_or_else(|| {
        command_error(
            "resolve_playback_title_url result",
            format!("No search result matched title ID {title_id}"),
        )
    })?;

    cache.canonical_title_urls.insert(title_id, url.clone());
    save_watching_availability_cache(&cache)?;
    Ok(url)
}

fn is_canonical_title_url(url: &str, title_id: u64) -> bool {
    ["/series/", "/titles/"]
        .iter()
        .filter_map(|marker| url.split_once(marker).map(|(_, path)| path))
        .any(|path| {
            let segment = path.split('/').next().unwrap_or_default();
            segment
                .strip_prefix(&title_id.to_string())
                .is_some_and(|suffix| suffix.starts_with('-'))
        })
}

fn cached_canonical_title_urls(cache: &UserAnimeListCache) -> HashMap<u64, String> {
    cache
        .items
        .values()
        .filter(|item| item.active && is_canonical_title_url(&item.url, item.title_id))
        .map(|item| (item.title_id, item.url.clone()))
        .collect()
}

fn canonical_url_from_cache_or_fallback(title_id: u64, urls: &HashMap<u64, String>) -> String {
    urls
        .get(&title_id)
        .cloned()
        .unwrap_or_else(|| series_url(title_id))
}

fn title_id_from_series_url(url: &str) -> Option<String> {
    ["/series/", "/titles/"]
        .iter()
        .find_map(|marker| extract_ascii_digits_after(url, marker))
}

fn title_episodes_url(title_id: u64, user_id: &str) -> String {
    format!("https://lista.shinden.pl/api/title-episodes/{title_id}/{user_id}")
}

fn is_true_final_episode(episode_no: u32, total_episodes: Option<u32>) -> bool {
    total_episodes
        .map(|total| total > 0 && episode_no == total)
        .unwrap_or(false)
}

fn merge_episode_progress(
    playback_episodes: Vec<Episode>,
    progress_episodes: Vec<TitleEpisodeApiItem>,
    total_episodes: Option<u32>,
) -> Vec<EpisodeProgress> {
    let progress_by_number: HashMap<u32, TitleEpisodeApiItem> = progress_episodes
        .into_iter()
        .map(|episode| (episode.episode_no, episode))
        .collect();

    playback_episodes
        .into_iter()
        .enumerate()
        .map(|(index, episode)| {
            let fallback_episode_no = (index + 1).min(u32::MAX as usize) as u32;
            let playback_episode_no =
                episode_number_from_playback_title(&episode.title).unwrap_or(fallback_episode_no);
            let progress = progress_by_number.get(&playback_episode_no);
            let episode_no = progress
                .map(|progress| progress.episode_no)
                .unwrap_or(playback_episode_no);
            let watched = progress.and_then(|progress| progress.watched.as_ref());

            EpisodeProgress {
                title: episode.title,
                link: episode.link,
                episode_id: progress.map(|progress| progress.episode_id),
                episode_no,
                watched: watched.is_some(),
                view_count: watched.map(|watched| watched.view_cnt).unwrap_or_default(),
                total_episodes,
                is_true_final_episode: is_true_final_episode(episode_no, total_episodes),
            }
        })
        .collect()
}

fn episode_number_from_playback_title(title: &str) -> Option<u32> {
    let normalized = title.trim().to_ascii_lowercase();
    if !(normalized.starts_with("episode ") || normalized.starts_with("odcinek ")) {
        return None;
    }

    last_ascii_number(title)
}

fn last_ascii_number(value: &str) -> Option<u32> {
    let mut digits_reversed = String::new();
    let mut found_digit = false;

    for character in value.chars().rev() {
        if character.is_ascii_digit() {
            found_digit = true;
            digits_reversed.push(character);
            continue;
        }

        if found_digit {
            break;
        }
    }

    if digits_reversed.is_empty() {
        return None;
    }

    let digits: String = digits_reversed.chars().rev().collect();
    digits.parse::<u32>().ok()
}

fn extract_user_id_from_profile_html(html: &str) -> Option<String> {
    ["https://lista.shinden.pl/animelist/", "/animelist/"]
        .iter()
        .find_map(|marker| extract_ascii_digits_after(html, marker))
}

fn extract_shinden_basic_auth(html: &str) -> Option<String> {
    [
        "_Storage.basic = \"",
        "_Storage.basic=\"",
        "_Storage.basic = '",
        "_Storage.basic='",
        "\"basic\":\"",
        "'basic':'",
        "basic: \"",
        "basic:\"",
        "basic: '",
        "basic:'",
    ]
    .iter()
    .find_map(|marker| extract_until_quote_after(html, marker))
    .filter(|token| !token.trim().is_empty())
}

fn extract_ascii_digits_after(source: &str, marker: &str) -> Option<String> {
    let start = source.find(marker)? + marker.len();
    let digits: String = source[start..]
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect();

    if digits.is_empty() {
        None
    } else {
        Some(digits)
    }
}

fn extract_until_quote_after(source: &str, marker: &str) -> Option<String> {
    let start = source.find(marker)? + marker.len();
    let quote = marker.chars().last()?;
    let value = source[start..].split(quote).next()?.trim();

    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn parse_main_premieres_html(html: &str) -> Vec<DiscoveryAnimeBase> {
    parse_discovery_links(html, true)
}

fn parse_season_anime_html(html: &str) -> Vec<DiscoveryAnimeBase> {
    parse_discovery_links(html, false)
}

fn parse_discovery_links(html: &str, include_source_label: bool) -> Vec<DiscoveryAnimeBase> {
    let mut rows: Vec<DiscoveryAnimeBase> = Vec::new();
    let mut seen = HashMap::<u64, usize>::new();
    let mut offset = 0;

    while let Some(anchor_start_relative) = html[offset..].find("<a") {
        let anchor_start = offset + anchor_start_relative;
        let Some(open_end_relative) = html[anchor_start..].find('>') else {
            break;
        };
        let open_end = anchor_start + open_end_relative;
        let open_tag = &html[anchor_start..=open_end];
        let Some(href) = extract_attr(open_tag, "href") else {
            offset = open_end + 1;
            continue;
        };
        let title_id = title_id_from_series_url(&href).and_then(|value| value.parse::<u64>().ok());
        let Some(title_id) = title_id else {
            offset = open_end + 1;
            continue;
        };

        let Some(close_relative) = html[open_end + 1..].find("</a>") else {
            break;
        };
        let close = open_end + 1 + close_relative;
        let anchor_body = &html[open_end + 1..close];
        let mut name = compact_text(anchor_body);
        let mut image_url = extract_first_image_url(anchor_body)
            .or_else(|| extract_background_image_url(open_tag))
            .unwrap_or_default();

        if name.is_empty() {
            name = extract_first_image_alt(anchor_body).unwrap_or_default();
        }
        if name.is_empty() {
            offset = close + 4;
            continue;
        }

        if image_url.is_empty() {
            image_url = extract_nearby_image_url(html, anchor_start, close)
                .unwrap_or_else(|| SHINDEN_TITLE_PLACEHOLDER.to_string());
        }

        let context = nearby_context(html, anchor_start, close);
        let rating_context = enclosing_discovery_card(html, anchor_start, close)
            .unwrap_or(anchor_body);
        let (anime_type, episodes) = extract_type_and_episodes(&context);
        let source_label = include_source_label
            .then(|| extract_episode_label(&context))
            .flatten();
        let row = DiscoveryAnimeBase {
            name,
            url: absolute_shinden_url(&href),
            image_url: if image_url.is_empty() {
                SHINDEN_TITLE_PLACEHOLDER.to_string()
            } else {
                absolute_shinden_url(&image_url)
            },
            anime_type,
            rating: extract_rating(rating_context),
            episodes,
            description: String::new(),
            title_id: Some(title_id),
            total_episodes: extract_total_episodes(&context),
            source_label,
        };

        if let Some(existing_index) = seen.get(&title_id).copied() {
            if rows[existing_index].source_label.is_none() && row.source_label.is_some() {
                rows[existing_index].source_label = row.source_label;
            }
            if rows[existing_index].image_url == SHINDEN_TITLE_PLACEHOLDER
                && row.image_url != SHINDEN_TITLE_PLACEHOLDER
            {
                rows[existing_index].image_url = row.image_url;
            }
        } else {
            seen.insert(title_id, rows.len());
            rows.push(row);
        }

        offset = close + 4;
    }

    rows
}

fn absolute_shinden_url(url: &str) -> String {
    let trimmed = html_unescape(url.trim());
    if trimmed.starts_with("https://") || trimmed.starts_with("http://") {
        trimmed
    } else if trimmed.starts_with("//") {
        format!("https:{trimmed}")
    } else if trimmed.starts_with('/') {
        format!("https://shinden.pl{trimmed}")
    } else {
        format!("https://shinden.pl/{trimmed}")
    }
}

fn html_unescape(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#039;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .trim()
        .to_string()
}

fn strip_html_tags(value: &str) -> String {
    let mut output = String::new();
    let mut inside_tag = false;
    for character in value.chars() {
        match character {
            '<' => inside_tag = true,
            '>' => inside_tag = false,
            _ if !inside_tag => output.push(character),
            _ => {}
        }
    }

    html_unescape(&output.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn extract_attr(tag: &str, attr: &str) -> Option<String> {
    let marker = format!("{attr}=\"");
    if let Some(start) = tag.find(&marker) {
        let value_start = start + marker.len();
        return tag[value_start..]
            .split('"')
            .next()
            .map(html_unescape)
            .filter(|value| !value.is_empty());
    }

    let marker = format!("{attr}='");
    let start = tag.find(&marker)?;
    let value_start = start + marker.len();
    tag[value_start..]
        .split('\'')
        .next()
        .map(html_unescape)
        .filter(|value| !value.is_empty())
}

fn compact_text(value: &str) -> String {
    strip_html_tags(value)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn extract_first_image_src(html: &str) -> Option<String> {
    let img_start = html.find("<img")?;
    let img_end = img_start + html[img_start..].find('>')?;
    extract_attr(&html[img_start..=img_end], "src")
}

fn extract_first_image_url(html: &str) -> Option<String> {
    extract_first_image_src(html).or_else(|| extract_background_image_url(html))
}

fn extract_first_image_alt(html: &str) -> Option<String> {
    let img_start = html.find("<img")?;
    let img_end = img_start + html[img_start..].find('>')?;
    extract_attr(&html[img_start..=img_end], "alt")
}

fn extract_nearby_image_url(html: &str, start: usize, end: usize) -> Option<String> {
    let context = nearby_context(html, start, end);
    extract_first_image_url(&context)
}

fn extract_background_image_url(html: &str) -> Option<String> {
    [
        "background-image:url(",
        "background-image: url(",
        "background:url(",
        "background: url(",
    ]
    .iter()
    .find_map(|marker| {
        let start = html.find(marker)? + marker.len();
        let value = html[start..].split(')').next()?.trim();
        let value = value.trim_matches('"').trim_matches('\'');
        if value.is_empty() {
            None
        } else {
            Some(html_unescape(value))
        }
    })
}

fn nearby_context(html: &str, start: usize, end: usize) -> String {
    let mut context_start = start.saturating_sub(900);
    while context_start > 0 && !html.is_char_boundary(context_start) {
        context_start -= 1;
    }

    let mut context_end = (end + 900).min(html.len());
    while context_end < html.len() && !html.is_char_boundary(context_end) {
        context_end += 1;
    }

    html[context_start..context_end].to_string()
}

fn enclosing_discovery_card(html: &str, start: usize, end: usize) -> Option<&str> {
    ["article", "li"].iter().find_map(|tag| {
        let opening_start = html[..start].rfind(&format!("<{tag}"))?;
        let opening_end = opening_start + html[opening_start..].find('>')?;
        let closing_marker = format!("</{tag}>");
        let closing_start = opening_end + html[opening_end..].find(&closing_marker)?;
        let closing_end = closing_start + closing_marker.len();

        (closing_end >= end).then_some(&html[opening_start..closing_end])
    })
}

fn extract_type_and_episodes(context: &str) -> (String, String) {
    let compact = compact_text(context);
    let tokens: Vec<&str> = compact.split_whitespace().collect();

    for (index, token) in tokens.iter().enumerate() {
        if !is_anime_type_token(token) {
            continue;
        }

        let episodes = tokens[index + 1..]
            .iter()
            .take(4)
            .find_map(|candidate| episode_token(candidate));
        return (token.to_string(), episodes.unwrap_or_default());
    }

    (String::new(), String::new())
}

fn is_anime_type_token(token: &str) -> bool {
    matches!(token, "TV" | "ONA" | "OVA" | "Movie" | "Special" | "Music")
}

fn episode_token(token: &str) -> Option<String> {
    let trimmed = token
        .trim_matches(|character: char| matches!(character, ',' | '.' | ';' | ':' | ')' | '('));
    let lower = trimmed.to_ascii_lowercase();
    if lower.ends_with("ep") || lower.ends_with("odc") {
        Some(trimmed.to_string())
    } else {
        None
    }
}

fn extract_total_episodes(context: &str) -> Option<u32> {
    let (_, episodes) = extract_type_and_episodes(context);
    let digits: String = episodes
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect();
    digits.parse::<u32>().ok()
}

fn extract_episode_label(context: &str) -> Option<String> {
    let compact = compact_text(context);
    let lower = compact.to_ascii_lowercase();
    let marker = "odcinek ";
    let start = lower.find(marker)? + marker.len();
    let number: String = lower[start..]
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect();

    if number.is_empty() {
        None
    } else {
        Some(format!("Odcinek {number}"))
    }
}

fn extract_rating(context: &str) -> String {
    ["data-rate", "data-rating", "data-score"]
        .iter()
        .find_map(|attribute| extract_attr(context, attribute))
        .and_then(|value| normalize_rating(&value))
        .or_else(|| compact_text(context).split_whitespace().find_map(normalize_rating))
        .unwrap_or_default()
}

fn normalize_rating(value: &str) -> Option<String> {
    let value = value
        .trim_matches(|character: char| !character.is_ascii_digit() && character != ',' && character != '.')
        .replace('.', ",");
    let (whole, fraction) = value.split_once(',')?;

    (whole.parse::<u8>().ok()? <= 10
        && !fraction.is_empty()
        && fraction.len() <= 2
        && fraction.chars().all(|character| character.is_ascii_digit()))
        .then_some(value)
}

fn shinden_watch_status_value(status: Option<&str>) -> Result<Option<&'static str>, String> {
    let Some(status) = status else {
        return Ok(None);
    };

    let normalized = status.trim().to_ascii_lowercase().replace('_', " ");
    match normalized.as_str() {
        "" | "no" | "none" | "null" => Ok(None),
        "in progress" | "in-progress" | "inprogress" | "watching" | "ogladam" => {
            Ok(Some("in progress"))
        }
        "completed" | "obejrzane" => Ok(Some("completed")),
        "skip" | "pomijam" => Ok(Some("skip")),
        "hold" | "wstrzymane" => Ok(Some("hold")),
        "dropped" | "porzucone" => Ok(Some("dropped")),
        "plan" | "planuje" => Ok(Some("plan")),
        _ => Err(format!("Unsupported anime status: {status}")),
    }
}

fn shinden_legacy_watch_status_values(status: Option<&str>) -> Result<Vec<&'static str>, String> {
    Ok(match shinden_watch_status_value(status)? {
        Some("in progress") => vec!["in progress", "in-progress", "watching"],
        Some("completed") => vec!["completed", "watched"],
        Some("plan") => vec!["plan", "planned", "to-watch"],
        Some("dropped") => vec!["dropped"],
        Some("hold") => vec!["hold", "on-hold"],
        Some("skip") => vec!["skip", "skipped"],
        Some(status) => vec![status],
        None => Vec::new(),
    })
}

fn title_status_matches(found: Option<&str>, expected: Option<&str>) -> Result<bool, String> {
    Ok(shinden_watch_status_value(found)? == shinden_watch_status_value(expected)?)
}

fn watch_status_list_slug(status: &str) -> &'static str {
    match status.trim().to_ascii_lowercase().as_str() {
        "in progress" | "in-progress" | "inprogress" => "in-progress",
        "completed" => "completed",
        "skip" => "skip",
        "hold" => "hold",
        "dropped" => "dropped",
        "plan" => "plan",
        _ => "in-progress",
    }
}

#[cfg(test)]
fn build_title_status_payload(
    title_id: u64,
    status: Option<&str>,
    is_favourite: Option<u8>,
) -> Result<TitleStatusChangePayload, String> {
    build_title_status_payload_with_details(title_id, status, is_favourite, 0, 0)
}

fn build_title_status_payload_with_details(
    title_id: u64,
    status: Option<&str>,
    is_favourite: Option<u8>,
    priority: i32,
    recommend: i32,
) -> Result<TitleStatusChangePayload, String> {
    Ok(TitleStatusChangePayload {
        input: vec![TitleStatusChangeInput {
            title_id,
            watch_status: shinden_watch_status_value(status)?,
            is_favourite: is_favourite.unwrap_or_default(),
            priority,
            recommend,
        }],
    })
}

fn build_watched_episode_payload(
    title_id: u64,
    episode_id: u64,
    created_time: String,
    view_count: u32,
) -> WatchedEpisodesChangePayload {
    WatchedEpisodesChangePayload {
        title_id,
        episodes: vec![WatchedEpisodeChangeInput {
            episode_id,
            view_cnt: view_count,
            created_time,
        }],
    }
}

fn map_watching_list_item_details(item: WatchingListApiItem) -> Option<WatchingAnime> {
    let url = series_url(item.title_id);
    map_watching_list_item_details_with_url(item, url)
}

fn cached_watching_anime(cache: &UserAnimeListCache) -> Vec<WatchingAnime> {
    active_user_anime_list_items(cache)
        .into_iter()
        .filter(|item| item.watch_status == "in progress")
        .map(|item| WatchingAnime {
            title_id: item.title_id,
            name: item.name,
            url: item.url,
            image_url: item.image_url,
            anime_type: item.anime_type,
            rating: item.rating,
            episodes: item.episodes,
            description: item.description,
            watch_status: item.watch_status,
            is_favourite: item.is_favourite,
            watched_episodes_count: item.watched_episodes_count,
            total_episodes: item.total_episodes,
        })
        .collect()
}

fn map_watching_list_item_details_with_url(
    item: WatchingListApiItem,
    url: String,
) -> Option<WatchingAnime> {
    let name = item.title.trim().to_string();
    if name.is_empty() {
        return None;
    }

    let watched_episodes_count = watched_episode_count(&item);
    let watch_status = item
        .watch_status
        .as_deref()
        .unwrap_or("in progress")
        .to_string();

    Some(WatchingAnime {
        title_id: item.title_id,
        name,
        url,
        image_url: item
            .cover_id
            .map(|cover_id| format!("https://cdn.shinden.eu/cdn1/images/genuine/{cover_id}.jpg"))
            .unwrap_or_else(|| SHINDEN_TITLE_PLACEHOLDER.to_string()),
        anime_type: item.anime_type.unwrap_or_default(),
        rating: format_rating(item.summary_rating_total.as_deref()),
        episodes: format_episode_progress(item.watched_episodes_cnt.as_deref(), item.episodes),
        description: item
            .description_pl
            .or(item.description_en)
            .unwrap_or_default(),
        watch_status,
        is_favourite: item.is_favourite.unwrap_or_default(),
        watched_episodes_count,
        total_episodes: item.episodes,
    })
}

fn map_user_anime_list_item(
    item: &WatchingListApiItem,
    updated_at_ms: u64,
) -> Option<UserAnimeListItem> {
    let details = map_watching_list_item_details(item.clone())?;

    Some(UserAnimeListItem {
        title_id: details.title_id,
        name: details.name,
        url: details.url,
        image_url: details.image_url,
        anime_type: details.anime_type,
        rating: details.rating,
        episodes: details.episodes,
        description: details.description,
        watch_status: details.watch_status,
        is_favourite: details.is_favourite,
        watched_episodes_count: details.watched_episodes_count,
        total_episodes: details.total_episodes,
        release_year: item
            .year
            .or_else(|| release_year_from_date(item.release_date.as_deref())),
        tags: Vec::new(),
        age_rating: None,
        detail_metadata_loaded: false,
        active: true,
        updated_at_ms,
    })
}

async fn refresh_user_anime_list_cache_inner(
    api: &ShindenAPI,
    user_id_cache: &Mutex<CachedUserId>,
    status: &Mutex<UserAnimeListRefreshStatus>,
) -> Result<UserAnimeListRefreshStatus, String> {
    let mut cache = load_user_anime_list_cache();
    let now_ms = unix_timestamp_ms_u64();
    let items = fetch_all_userlist_items(api, user_id_cache).await?;

    merge_user_anime_list_cache(&mut cache, items, false, now_ms);
    cache.refreshed_at_ms = Some(now_ms);
    save_user_anime_list_cache(&cache)?;

    let mut state = build_user_anime_list_refresh_state(&cache, now_ms);
    save_user_anime_list_refresh_state(&state)?;
    replace_user_anime_list_refresh_status(
        status,
        user_anime_list_refresh_status_from_state(&state, true),
    )?;

    process_user_anime_list_refresh_queue(api, status, &mut cache, &mut state).await
}

async fn process_user_anime_list_refresh_queue(
    api: &ShindenAPI,
    status: &Mutex<UserAnimeListRefreshStatus>,
    cache: &mut UserAnimeListCache,
    state: &mut UserAnimeListRefreshState,
) -> Result<UserAnimeListRefreshStatus, String> {
    for index in 0..state.queue.len() {
        if state.queue[index].done || state.queue[index].failed {
            continue;
        }

        update_user_anime_list_refresh_status(status, |status| {
            status.current_title = state.queue[index].title.clone();
        })?;

        let key = state.queue[index].key.clone();
        let title = state.queue[index].title.clone();
        let url = state.queue[index].url.clone();

        wait_before_background_request().await;
        match api.get_anime_details(&url).await {
            Ok(details) => {
                if let Some(item) = cache.items.get_mut(&key) {
                    apply_user_anime_details_to_item(item, &details);
                    item.updated_at_ms = unix_timestamp_ms_u64();
                    state.queue[index].done = true;
                    cache.refreshed_at_ms = Some(item.updated_at_ms);
                    save_user_anime_list_cache(cache)?;
                } else {
                    state.queue[index].failed = true;
                    state.last_error =
                        Some(format!("Nie znaleziono anime w lokalnym cache: {title}"));
                }
            }
            Err(error) => {
                state.queue[index].failed = true;
                state.last_error = Some(format!(
                    "Nie udalo sie odswiezyc szczegolow anime {title}: {error}"
                ));
                let _ = command_error("user_anime_list_details refresh", error);
            }
        }

        save_user_anime_list_refresh_state(state)?;
        replace_user_anime_list_refresh_status(
            status,
            user_anime_list_refresh_status_from_state(state, true),
        )?;
    }

    let failed = state.queue.iter().filter(|item| item.failed).count();
    state.last_finished_at_ms = Some(unix_timestamp_ms_u64());
    state.last_error = if failed > 0 {
        Some(format!(
            "Nie udalo sie odswiezyc szczegolow czesci anime: {failed}"
        ))
    } else {
        None
    };
    save_user_anime_list_refresh_state(state)?;

    let final_status = user_anime_list_refresh_status_from_state(state, false);
    replace_user_anime_list_refresh_status(status, final_status.clone())?;
    Ok(final_status)
}

fn build_user_anime_list_refresh_state(
    cache: &UserAnimeListCache,
    started_at_ms: u64,
) -> UserAnimeListRefreshState {
    let mut queue: Vec<UserAnimeListRefreshQueueItem> = cache
        .items
        .iter()
        .filter(|(_, item)| user_anime_list_item_needs_detail_metadata(item))
        .map(|(key, item)| UserAnimeListRefreshQueueItem {
            key: key.clone(),
            title_id: item.title_id,
            title: item.name.clone(),
            url: item.url.clone(),
            done: false,
            failed: false,
        })
        .collect();

    queue.sort_by(|a, b| {
        a.title
            .to_ascii_lowercase()
            .cmp(&b.title.to_ascii_lowercase())
            .then_with(|| a.title_id.cmp(&b.title_id))
    });

    UserAnimeListRefreshState {
        queue,
        started_at_ms: Some(started_at_ms),
        last_finished_at_ms: None,
        last_error: None,
    }
}

fn user_anime_list_item_needs_detail_metadata(item: &UserAnimeListItem) -> bool {
    item.active && !item.detail_metadata_loaded && item.tags.is_empty() && item.age_rating.is_none()
}

fn user_anime_list_refresh_state_has_pending(state: &UserAnimeListRefreshState) -> bool {
    state.queue.iter().any(|item| !item.done && !item.failed)
}

fn user_anime_list_refresh_status_from_state(
    state: &UserAnimeListRefreshState,
    running: bool,
) -> UserAnimeListRefreshStatus {
    let total = state.queue.len();
    let refreshed = state.queue.iter().filter(|item| item.done).count();
    let failed = state.queue.iter().filter(|item| item.failed).count();
    let current = refreshed + failed;
    let remaining = total.saturating_sub(current);
    let current_title = if running {
        state
            .queue
            .iter()
            .find(|item| !item.done && !item.failed)
            .map(|item| item.title.clone())
            .unwrap_or_default()
    } else {
        String::new()
    };

    UserAnimeListRefreshStatus {
        running,
        current,
        total,
        remaining,
        refreshed,
        failed,
        current_title,
        last_finished_at_ms: state.last_finished_at_ms,
        last_error: state.last_error.clone(),
    }
}

async fn refresh_new_user_anime_detail_metadata(
    api: &ShindenAPI,
    cache: &mut UserAnimeListCache,
    keys: Vec<String>,
    updated_at_ms: u64,
) -> Option<String> {
    if keys.is_empty() {
        return None;
    }

    let targets: Vec<(String, String)> = keys
        .into_iter()
        .filter_map(|key| {
            cache
                .items
                .get(&key)
                .filter(|item| item.active)
                .map(|item| (key, item.url.clone()))
        })
        .collect();

    let mut errors = 0usize;
    let mut detail_results = stream::iter(targets)
        .map(|(key, url)| async move {
            wait_before_background_request().await;
            (key, api.get_anime_details(&url).await)
        })
        .buffer_unordered(USER_ANIME_LIST_DETAIL_REFRESH_CONCURRENCY);

    while let Some((key, result)) = detail_results.next().await {
        match result {
            Ok(details) => {
                if let Some(item) = cache.items.get_mut(&key) {
                    apply_user_anime_details_to_item(item, &details);
                    item.updated_at_ms = updated_at_ms;
                }
            }
            Err(error) => {
                errors += 1;
                let _ = command_error("user_anime_list_new_details refresh", error);
            }
        }
    }

    if errors > 0 {
        Some(format!(
            "Nie udalo sie pobrac szczegolow nowych anime: {errors}"
        ))
    } else {
        None
    }
}

fn apply_user_anime_details_to_item(item: &mut UserAnimeListItem, details: &AnimeDetails) {
    if !details.name.trim().is_empty() {
        item.name = details.name.clone();
    }
    if !details.image_url.trim().is_empty() {
        item.image_url = details.image_url.clone();
    }
    if !details.description.trim().is_empty() {
        item.description = details.description.clone();
    }

    item.tags = anime_detail_tags(details);
    item.age_rating = anime_detail_age_rating(details);
    item.detail_metadata_loaded = true;
}

fn anime_detail_tags(details: &AnimeDetails) -> Vec<String> {
    let mut tags = Vec::new();

    for group in &details.categories {
        for tag in &group.items {
            let tag = tag.trim();
            if tag.is_empty() || tags.iter().any(|existing| existing == tag) {
                continue;
            }

            tags.push(tag.to_string());
        }
    }

    tags
}

fn anime_detail_age_rating(details: &AnimeDetails) -> Option<String> {
    details
        .information
        .iter()
        .find(|row| {
            let label = row.label.trim_end_matches(':').trim().to_ascii_lowercase();
            label.contains("wiek") || label == "mpaa"
        })
        .map(|row| row.value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn merge_user_anime_list_cache(
    cache: &mut UserAnimeListCache,
    incoming_items: Vec<WatchingListApiItem>,
    force_refresh: bool,
    updated_at_ms: u64,
) -> Vec<UserAnimeListItem> {
    for item in cache.items.values_mut() {
        item.active = false;
    }

    for incoming in incoming_items {
        let cache_key = user_anime_list_cache_key(incoming.title_id);
        let Some(mapped) = map_user_anime_list_item(&incoming, updated_at_ms) else {
            continue;
        };

        if force_refresh || !cache.items.contains_key(&cache_key) {
            let mut mapped = mapped;
            if force_refresh {
                if let Some(existing) = cache.items.get(&cache_key) {
                    mapped.tags = existing.tags.clone();
                    mapped.age_rating = existing.age_rating.clone();
                    mapped.detail_metadata_loaded = existing.detail_metadata_loaded;
                }
            }
            cache.items.insert(cache_key, mapped);
            continue;
        }

        if let Some(cached) = cache.items.get_mut(&cache_key) {
            cached.watch_status = mapped.watch_status;
            cached.is_favourite = mapped.is_favourite;
            cached.episodes = mapped.episodes;
            cached.watched_episodes_count = mapped.watched_episodes_count;
            cached.total_episodes = mapped.total_episodes;
            cached.release_year = cached.release_year.or(mapped.release_year);
            cached.active = true;
            cached.updated_at_ms = updated_at_ms;
        }
    }
    active_user_anime_list_items(cache)
}

fn active_user_anime_list_items(cache: &UserAnimeListCache) -> Vec<UserAnimeListItem> {
    let mut items: Vec<UserAnimeListItem> = cache
        .items
        .values()
        .filter(|item| item.active)
        .cloned()
        .collect();
    items.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    items
}

fn should_return_cached_user_anime_lists(cache: &UserAnimeListCache, force_refresh: bool) -> bool {
    !force_refresh && cache.items.values().any(|item| item.active)
}
fn user_anime_list_cache_keys(cache: &UserAnimeListCache) -> HashSet<String> {
    cache.items.keys().cloned().collect()
}

fn new_active_user_anime_list_cache_keys(
    cache: &UserAnimeListCache,
    existing_keys: &HashSet<String>,
) -> Vec<String> {
    let mut keys: Vec<String> = cache
        .items
        .iter()
        .filter(|(key, item)| item.active && !existing_keys.contains(*key))
        .map(|(key, _)| key.clone())
        .collect();

    keys.sort();
    keys
}

fn user_anime_lists_payload(
    items: Vec<UserAnimeListItem>,
    refreshed_at_ms: Option<u64>,
    sync_error: Option<String>,
) -> UserAnimeListsPayload {
    UserAnimeListsPayload {
        counts: user_anime_list_counts(&items),
        items,
        refreshed_at_ms,
        sync_error,
    }
}

fn user_anime_list_counts(items: &[UserAnimeListItem]) -> UserAnimeListCounts {
    let mut counts = UserAnimeListCounts::default();

    for item in items {
        match item.watch_status.trim().to_ascii_lowercase().as_str() {
            "in progress" | "in-progress" | "inprogress" => counts.in_progress += 1,
            "completed" => counts.completed += 1,
            "skip" => counts.skip += 1,
            "hold" => counts.hold += 1,
            "dropped" => counts.dropped += 1,
            "plan" => counts.plan += 1,
            _ => {}
        }
    }

    counts.all = items.len();
    counts
}

fn release_year_from_date(value: Option<&str>) -> Option<u16> {
    let value = value?.trim();
    if value.len() < 4 {
        return None;
    }

    value.get(0..4)?.parse::<u16>().ok()
}

fn user_anime_list_cache_key(title_id: u64) -> String {
    title_id.to_string()
}

#[cfg(test)]
fn map_watching_list_item(item: WatchingListApiItem) -> Option<Anime> {
    map_watching_list_item_details(item).map(|item| Anime {
        name: item.name,
        url: item.url,
        image_url: item.image_url,
        anime_type: item.anime_type,
        rating: item.rating,
        episodes: item.episodes,
        description: item.description,
    })
}

fn map_search_anime_results(
    results: Vec<Anime>,
    watching_items: Vec<WatchingListApiItem>,
) -> Vec<SearchAnime> {
    let watching_by_title_id: HashMap<u64, WatchingListApiItem> = watching_items
        .into_iter()
        .map(|item| (item.title_id, item))
        .collect();

    results
        .into_iter()
        .map(|anime| map_search_anime_details(anime, &watching_by_title_id))
        .collect()
}

fn map_search_anime_details(
    anime: Anime,
    watching_by_title_id: &HashMap<u64, WatchingListApiItem>,
) -> SearchAnime {
    let title_id = title_id_from_series_url(&anime.url).and_then(|value| value.parse::<u64>().ok());
    let watching_item = title_id.and_then(|title_id| watching_by_title_id.get(&title_id));

    SearchAnime {
        anime,
        title_id,
        watch_status: watching_item
            .and_then(|item| item.watch_status.clone())
            .unwrap_or_else(|| "no".to_string()),
        is_favourite: watching_item
            .and_then(|item| item.is_favourite)
            .unwrap_or_default(),
        total_episodes: watching_item.and_then(|item| item.episodes),
    }
}

fn map_discovery_anime_results(
    rows: Vec<DiscoveryAnimeBase>,
    watching_items: Vec<WatchingListApiItem>,
) -> Vec<DiscoveryAnime> {
    let watching_by_title_id: HashMap<u64, WatchingListApiItem> = watching_items
        .into_iter()
        .map(|item| (item.title_id, item))
        .collect();

    rows.into_iter()
        .map(|row| map_discovery_anime_details(row, &watching_by_title_id))
        .collect()
}

fn map_discovery_anime_details(
    row: DiscoveryAnimeBase,
    watching_by_title_id: &HashMap<u64, WatchingListApiItem>,
) -> DiscoveryAnime {
    let watching_item = row
        .title_id
        .and_then(|title_id| watching_by_title_id.get(&title_id));

    DiscoveryAnime {
        name: row.name,
        url: row.url,
        image_url: row.image_url,
        anime_type: row.anime_type,
        rating: row.rating,
        episodes: row.episodes,
        description: row.description,
        title_id: row.title_id,
        watch_status: watching_item
            .and_then(|item| item.watch_status.clone())
            .unwrap_or_else(|| "no".to_string()),
        is_favourite: watching_item
            .and_then(|item| item.is_favourite)
            .unwrap_or_default(),
        total_episodes: row
            .total_episodes
            .or_else(|| watching_item.and_then(|item| item.episodes)),
        source_label: row.source_label,
    }
}

fn has_unwatched_episodes(item: &WatchingListApiItem) -> bool {
    match item.episodes {
        Some(total) => watched_episode_count(item) < total,
        None => true,
    }
}

fn watching_progress_filter_matches(
    item: &WatchingListApiItem,
    filter: &WatchingAnimeFilter,
) -> bool {
    !filter.only_available_unwatched() || has_unwatched_episodes(item)
}
fn watching_filter_requires_availability_cache(filter: &WatchingAnimeFilter) -> bool {
    filter.only_available_unwatched() || filter.check_subtitle_availability_online()
}


fn watching_cache_filter_matches(
    item: &WatchingListApiItem,
    filter: &WatchingAnimeFilter,
    cache: &WatchingAvailabilityCache,
) -> bool {
    if !watching_progress_filter_matches(item, filter) {
        return false;
    }

    if !watching_filter_requires_availability_cache(filter) {
        return true;
    }

    let Some(entry) = cache.entries.get(&watching_cache_key(item.title_id)) else {
        return false;
    };

    let has_available_unwatched_episode = entry.has_available_unwatched_episode
        || entry
            .subtitle_availability
            .values()
            .any(|available| *available);
    if !cache_entry_matches_item(entry, item) || !has_available_unwatched_episode {
        return false;
    }

    selected_subtitle_cache_key(filter)
        .map(|key| {
            entry
                .subtitle_availability
                .get(&key)
                .copied()
                .unwrap_or(false)
        })
        .unwrap_or(true)
}

fn cache_entry_matches_item(
    entry: &WatchingAvailabilityCacheEntry,
    item: &WatchingListApiItem,
) -> bool {
    entry.title_id == item.title_id
        && entry.watched_episodes_cnt == watched_episode_count(item)
        && entry.total_episodes == item.episodes
}

fn cache_entry_satisfies_refresh(
    entry: &WatchingAvailabilityCacheEntry,
    item: &WatchingListApiItem,
    subtitle_key: Option<&str>,
    now_ms: u64,
    force: bool,
) -> bool {
    if force || !cache_entry_matches_item(entry, item) {
        return false;
    }

    if entry.episode_availability.is_empty() {
        return false;
    }

    if entry.checked_at_ms == 0
        || now_ms.saturating_sub(entry.checked_at_ms) > WATCHING_CACHE_TTL_MS
    {
        return false;
    }

    subtitle_key
        .map(|key| entry.subtitle_availability.contains_key(key))
        .unwrap_or(true)
}

fn selected_subtitle_language_key(filter: &WatchingAnimeFilter) -> Option<String> {
    if !filter.check_subtitle_availability_online() {
        return None;
    }

    let key = subtitle_language_key(filter.subtitle_language());
    if key.is_empty() || key == "any" {
        None
    } else {
        Some(key)
    }
}

fn selected_subtitle_cache_key(filter: &WatchingAnimeFilter) -> Option<String> {
    selected_subtitle_language_key(filter).map(|key| {
        if filter.exclude_ai_subtitles() {
            format!("{key}:human")
        } else {
            key
        }
    })
}

fn record_subtitle_language_availability(
    subtitle_availability: &mut HashMap<String, bool>,
    language: &str,
) {
    let key = subtitle_language_key(language);
    if key.is_empty() || key == "any" {
        return;
    }

    subtitle_availability.insert(key.clone(), true);
    if !is_ai_subtitle_language(language, &key) {
        subtitle_availability.insert(format!("{key}:human"), true);
    }
}

fn watching_cache_key(title_id: u64) -> String {
    title_id.to_string()
}

fn watched_episode_count(item: &WatchingListApiItem) -> u32 {
    item.watched_episodes_cnt
        .as_deref()
        .and_then(|watched| watched.trim().parse::<u32>().ok())
        .unwrap_or_default()
}

#[cfg(test)]
fn subtitle_language_matches(player_lang_subs: &str, selected_language: &str) -> bool {
    subtitle_language_matches_with_options(player_lang_subs, selected_language, false)
}

#[cfg(test)]
fn subtitle_language_matches_with_options(
    player_lang_subs: &str,
    selected_language: &str,
    exclude_ai_subtitles: bool,
) -> bool {
    let selected_language = selected_language.trim();
    if selected_language.is_empty() {
        return true;
    }

    let selected_key = subtitle_language_key(selected_language);
    if selected_key == "any" {
        return true;
    }

    if exclude_ai_subtitles && is_ai_subtitle_language(player_lang_subs, &selected_key) {
        return false;
    }

    let player_key = subtitle_language_key(player_lang_subs);
    player_key == selected_key
}

fn subtitle_language_key(language: &str) -> String {
    let language = language.trim().to_ascii_lowercase();

    let direct_key = subtitle_language_key_without_ai(&language);
    if matches!(direct_key.as_str(), "pl" | "en" | "jp" | "any") {
        return direct_key;
    }

    if let Some(base_language) = language.strip_prefix('i') {
        let base_key = subtitle_language_key_without_ai(base_language);
        if matches!(base_key.as_str(), "pl" | "en" | "jp") {
            return base_key;
        }
    }

    direct_key
}

fn subtitle_language_key_without_ai(language: &str) -> String {
    let language = language.trim().to_ascii_lowercase();

    if language == "any"
        || language == "dowolny"
        || language == "dowolne"
        || language == "wszystkie"
    {
        return "any".to_string();
    }

    if language == "pl"
        || language.contains("pol")
        || language
            .split(|character: char| !character.is_ascii_alphanumeric())
            .any(|token| token == "pl")
    {
        return "pl".to_string();
    }

    if language == "en"
        || language == "eng"
        || language.contains("ang")
        || language.contains("english")
    {
        return "en".to_string();
    }

    if language == "jp" || language == "ja" || language.contains("jap") || language.contains("japo")
    {
        return "jp".to_string();
    }

    language
}

fn is_ai_subtitle_language(language: &str, selected_key: &str) -> bool {
    ai_subtitle_base_key(language)
        .as_deref()
        .is_some_and(|base_key| base_key == selected_key)
}

fn ai_subtitle_base_key(language: &str) -> Option<String> {
    let normalized: String = language
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect();
    let base = normalized.strip_prefix('i')?;
    let base_key = subtitle_language_key(base);

    if matches!(base_key.as_str(), "pl" | "en" | "jp") {
        Some(base_key)
    } else {
        None
    }
}

fn format_rating(raw_rating: Option<&str>) -> String {
    raw_rating
        .and_then(|rating| rating.parse::<f64>().ok())
        .map(|rating| format!("{rating:.2}").replace('.', ","))
        .unwrap_or_default()
}

fn format_episode_progress(watched: Option<&str>, total: Option<u32>) -> String {
    match (watched, total) {
        (Some(watched), Some(total)) => format!("{watched}/{total}"),
        (None, Some(total)) => format!("0/{total}"),
        (Some(watched), None) => watched.to_string(),
        (None, None) => String::new(),
    }
}

fn load_watching_availability_cache() -> WatchingAvailabilityCache {
    load_watching_availability_cache_from(&watching_availability_cache_path())
}

fn load_watching_availability_cache_from(path: &Path) -> WatchingAvailabilityCache {
    fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str::<WatchingAvailabilityCache>(&contents).ok())
        .unwrap_or_default()
}

fn save_watching_availability_cache(cache: &WatchingAvailabilityCache) -> Result<(), String> {
    save_watching_availability_cache_to(&watching_availability_cache_path(), cache)
        .map_err(|e| command_error("watching_cache save", e))
}

fn load_user_anime_list_cache() -> UserAnimeListCache {
    load_user_anime_list_cache_from(&user_anime_list_cache_path())
}

fn load_user_anime_list_cache_from(path: &Path) -> UserAnimeListCache {
    fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str::<UserAnimeListCache>(&contents).ok())
        .unwrap_or_default()
}

fn save_user_anime_list_cache(cache: &UserAnimeListCache) -> Result<(), String> {
    save_user_anime_list_cache_to(&user_anime_list_cache_path(), cache)
        .map_err(|e| command_error("user_anime_list_cache save", e))
}

fn save_user_anime_list_cache_to(path: &Path, cache: &UserAnimeListCache) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let contents = serde_json::to_string_pretty(cache)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    fs::write(path, contents)
}

fn user_anime_list_cache_path() -> PathBuf {
    resolve_project_cache_dir().join("user-anime-lists-cache.json")
}

fn load_user_anime_list_refresh_state() -> UserAnimeListRefreshState {
    load_user_anime_list_refresh_state_from(&user_anime_list_refresh_state_path())
}

fn load_user_anime_list_refresh_state_from(path: &Path) -> UserAnimeListRefreshState {
    fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str::<UserAnimeListRefreshState>(&contents).ok())
        .unwrap_or_default()
}

fn save_user_anime_list_refresh_state(state: &UserAnimeListRefreshState) -> Result<(), String> {
    save_user_anime_list_refresh_state_to(&user_anime_list_refresh_state_path(), state)
        .map_err(|e| command_error("user_anime_list_refresh save", e))
}

fn save_user_anime_list_refresh_state_to(
    path: &Path,
    state: &UserAnimeListRefreshState,
) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let contents = serde_json::to_string_pretty(state)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    fs::write(path, contents)
}

fn user_anime_list_refresh_state_path() -> PathBuf {
    resolve_project_cache_dir().join("user-anime-lists-refresh.json")
}

fn save_watching_availability_cache_to(
    path: &Path,
    cache: &WatchingAvailabilityCache,
) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let contents = serde_json::to_string_pretty(cache)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    fs::write(path, contents)
}

fn watching_availability_cache_path() -> PathBuf {
    resolve_project_cache_dir().join("watching-anime-cache.json")
}

fn begin_watching_cache_refresh(
    status: &Mutex<WatchingCacheRefreshStatus>,
) -> Result<Option<WatchingCacheRefreshSummary>, String> {
    let mut status = lock_refresh_status(status)?;
    if status.running {
        return Ok(Some(WatchingCacheRefreshSummary {
            status: status.clone(),
            already_running: true,
        }));
    }

    let last_finished_at_ms = status.last_finished_at_ms;
    *status = WatchingCacheRefreshStatus {
        running: true,
        current: 0,
        total: 0,
        refreshed: 0,
        skipped: 0,
        failed: 0,
        failures: Vec::new(),
        current_title: String::new(),
        last_finished_at_ms,
        last_error: None,
    };

    Ok(None)
}

fn lock_refresh_status(
    status: &Mutex<WatchingCacheRefreshStatus>,
) -> Result<std::sync::MutexGuard<'_, WatchingCacheRefreshStatus>, String> {
    status
        .lock()
        .map_err(|_| command_error("watching_cache status", "Status lock poisoned"))
}

fn refresh_status_snapshot(
    status: &Mutex<WatchingCacheRefreshStatus>,
) -> Result<WatchingCacheRefreshStatus, String> {
    Ok(lock_refresh_status(status)?.clone())
}

fn update_refresh_status<F>(
    status: &Mutex<WatchingCacheRefreshStatus>,
    update: F,
) -> Result<(), String>
where
    F: FnOnce(&mut WatchingCacheRefreshStatus),
{
    let mut status = lock_refresh_status(status)?;
    update(&mut status);
    Ok(())
}

fn begin_user_anime_list_refresh(
    status: &Mutex<UserAnimeListRefreshStatus>,
    state: Option<&UserAnimeListRefreshState>,
) -> Result<Option<UserAnimeListRefreshSummary>, String> {
    let mut status = lock_user_anime_list_refresh_status(status)?;
    if status.running {
        return Ok(Some(UserAnimeListRefreshSummary {
            status: status.clone(),
            already_running: true,
        }));
    }

    let last_finished_at_ms = status.last_finished_at_ms;
    *status = state
        .map(|state| user_anime_list_refresh_status_from_state(state, true))
        .unwrap_or_else(|| UserAnimeListRefreshStatus {
            running: true,
            current: 0,
            total: 0,
            remaining: 0,
            refreshed: 0,
            failed: 0,
            current_title: String::new(),
            last_finished_at_ms,
            last_error: None,
        });

    Ok(None)
}

fn lock_user_anime_list_refresh_status(
    status: &Mutex<UserAnimeListRefreshStatus>,
) -> Result<std::sync::MutexGuard<'_, UserAnimeListRefreshStatus>, String> {
    status
        .lock()
        .map_err(|_| command_error("user_anime_list_refresh status", "Status lock poisoned"))
}

fn user_anime_list_refresh_status_snapshot(
    status: &Mutex<UserAnimeListRefreshStatus>,
) -> Result<UserAnimeListRefreshStatus, String> {
    Ok(lock_user_anime_list_refresh_status(status)?.clone())
}

fn replace_user_anime_list_refresh_status(
    status: &Mutex<UserAnimeListRefreshStatus>,
    new_status: UserAnimeListRefreshStatus,
) -> Result<(), String> {
    let mut status = lock_user_anime_list_refresh_status(status)?;
    *status = new_status;
    Ok(())
}

fn update_user_anime_list_refresh_status<F>(
    status: &Mutex<UserAnimeListRefreshStatus>,
    update: F,
) -> Result<(), String>
where
    F: FnOnce(&mut UserAnimeListRefreshStatus),
{
    let mut status = lock_user_anime_list_refresh_status(status)?;
    update(&mut status);
    Ok(())
}

fn fail_user_anime_list_refresh(
    status: &Mutex<UserAnimeListRefreshStatus>,
    error: &str,
) -> Result<(), String> {
    update_user_anime_list_refresh_status(status, |status| {
        status.running = false;
        status.current_title.clear();
        status.last_finished_at_ms = Some(unix_timestamp_ms_u64());
        status.last_error = Some(error.to_string());
    })
}

pub fn command_error<E: ToString>(context: &str, error: E) -> String {
    let message = error.to_string();
    let _ = append_project_log("ERROR", &format!("{context}: {message}"));
    message
}

pub fn append_project_log(level: &str, message: &str) -> io::Result<PathBuf> {
    append_log_line(&resolve_project_log_dir(), level, message)
}

fn discard_log_path(result: io::Result<PathBuf>) -> Result<(), String> {
    result.map(|_| ()).map_err(|e| e.to_string())
}

fn append_log_line(log_dir: &Path, level: &str, message: &str) -> io::Result<PathBuf> {
    fs::create_dir_all(log_dir)?;
    let log_file = log_dir.join("shinden-client.log");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file)?;
    writeln!(file, "{} [{level}] {message}", unix_timestamp_ms())?;
    Ok(log_file)
}

fn unix_timestamp_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn unix_timestamp_ms_u64() -> u64 {
    unix_timestamp_ms().min(u64::MAX as u128) as u64
}

fn resolve_project_log_dir() -> PathBuf {
    if let Ok(path) = std::env::var("SHINDEN_CLIENT_LOG_DIR") {
        if !path.trim().is_empty() {
            return PathBuf::from(path);
        }
    }

    if let Some(root) = option_env!("SHINDEN_BUILD_PROJECT_ROOT") {
        let path = PathBuf::from(root);
        if is_project_root(&path) {
            return path.join("logs");
        }
    }

    let mut starts = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            starts.push(parent.to_path_buf());
        }
    }
    if let Ok(current_dir) = std::env::current_dir() {
        starts.push(current_dir);
    }

    for start in starts {
        if let Some(root) = find_project_root_from(&start) {
            return root.join("logs");
        }
    }

    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("logs")
}

fn resolve_project_cache_dir() -> PathBuf {
    if let Ok(path) = std::env::var("SHINDEN_CLIENT_CACHE_DIR") {
        if !path.trim().is_empty() {
            return PathBuf::from(path);
        }
    }

    if let Some(root) = option_env!("SHINDEN_BUILD_PROJECT_ROOT") {
        let path = PathBuf::from(root);
        if is_project_root(&path) {
            return path.join("cache");
        }
    }

    let mut starts = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            starts.push(parent.to_path_buf());
        }
    }
    if let Ok(current_dir) = std::env::current_dir() {
        starts.push(current_dir);
    }

    for start in starts {
        if let Some(root) = find_project_root_from(&start) {
            return root.join("cache");
        }
    }

    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("cache")
}

fn find_project_root_from(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|path| is_project_root(path))
        .map(PathBuf::from)
}

fn is_project_root(path: &Path) -> bool {
    path.join("package.json").is_file() && path.join("src-tauri").join("tauri.conf.json").is_file()
}

#[cfg(test)]
mod tests {
    use crate::details::{AnimeCategoryGroup, AnimeInfoRow};

    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_dir(name: &str) -> std::path::PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after UNIX_EPOCH")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "shinden_client_{}_{}_{}",
            name,
            std::process::id(),
            stamp
        ))
    }

    fn anime_fixture(url: &str) -> Anime {
        Anime {
            name: "Enen no Shouboutai: San no Shou".to_string(),
            url: url.to_string(),
            image_url: "https://shinden.pl/res/cover.jpg".to_string(),
            anime_type: "TV".to_string(),
            rating: "7,90".to_string(),
            episodes: "12".to_string(),
            description: String::new(),
        }
    }

    fn watching_item_fixture(
        title_id: u64,
        watch_status: Option<&str>,
        is_favourite: Option<u8>,
        episodes: Option<u32>,
    ) -> WatchingListApiItem {
        WatchingListApiItem {
            title_id,
            watch_status: watch_status.map(str::to_string),
            is_favourite,
            title: "Enen no Shouboutai: San no Shou".to_string(),
            cover_id: Some(123456),
            anime_type: Some("TV".to_string()),
            summary_rating_total: Some("7.9000".to_string()),
            episodes,
            watched_episodes_cnt: Some("3".to_string()),
            description_pl: Some("Opis".to_string()),
            description_en: None,
            release_date: None,
            year: None,
        }
    }

    fn cached_user_anime_fixture(
        title_id: u64,
        name: &str,
        watch_status: &str,
        active: bool,
    ) -> UserAnimeListItem {
        UserAnimeListItem {
            title_id,
            name: name.to_string(),
            url: series_url(title_id),
            image_url: SHINDEN_TITLE_PLACEHOLDER.to_string(),
            anime_type: "TV".to_string(),
            rating: "7,00".to_string(),
            episodes: "1/12".to_string(),
            description: "Cached description".to_string(),
            watch_status: watch_status.to_string(),
            is_favourite: 0,
            watched_episodes_count: 1,
            total_episodes: Some(12),
            release_year: Some(2024),
            tags: Vec::new(),
            age_rating: None,
            detail_metadata_loaded: false,
            active,
            updated_at_ms: 5_000,
        }
    }

    #[test]
    fn season_page_url_uses_explicit_year_and_slug() {
        assert_eq!(
            season_page_url(Some(2026), "winter"),
            "https://shinden.pl/series/season/2026/winter"
        );
    }

    #[test]
    fn season_page_url_can_use_current_shortcut() {
        assert_eq!(
            season_page_url(None, "current"),
            "https://shinden.pl/series/season/current"
        );
    }

    #[test]
    fn normalize_season_slug_accepts_polish_aliases() {
        assert_eq!(normalize_season_slug("zima").as_deref(), Some("winter"));
        assert_eq!(normalize_season_slug("wiosna").as_deref(), Some("spring"));
        assert_eq!(normalize_season_slug("lato").as_deref(), Some("summer"));
        assert_eq!(normalize_season_slug("jesien").as_deref(), Some("fall"));
        assert_eq!(
            normalize_season_slug("jesie\u{0144}").as_deref(),
            Some("fall")
        );
    }

    #[test]
    fn parse_main_premieres_extracts_series_links() {
        let html = r#"
            <section id="premieres">
                <a class="cover" href="/series/59922-enen-no-shouboutai-san-no-shou-part-2">
                    <img src="https://cdn.shinden.eu/cdn1/images/genuine/59922.jpg" alt="Enen no Shouboutai: San no Shou Part 2">
                </a>
                <a href="/series/59922-enen-no-shouboutai-san-no-shou-part-2">Enen no Shouboutai: San no Shou Part 2</a>
                <span>Odcinek 1</span>
            </section>
        "#;

        let rows = parse_main_premieres_html(html);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title_id, Some(59922));
        assert_eq!(rows[0].name, "Enen no Shouboutai: San no Shou Part 2");
        assert_eq!(
            rows[0].url,
            "https://shinden.pl/series/59922-enen-no-shouboutai-san-no-shou-part-2"
        );
        assert_eq!(rows[0].source_label.as_deref(), Some("Odcinek 1"));
    }

    #[test]
    fn parse_main_premieres_extracts_background_image_tiles() {
        let html = r#"
            <section class="box box-new-series">
                <a href="/series/68638-tensei-shitara-slime-datta-ken-4th-season"
                    class="img media-title-cover season-tile"
                    style="background-image:url(/res/images/225x350/435918.jpg)">
                    <span class="tile-title">
                        <span>Tensei shitara Slime Datta Ken 4th Season</span>
                    </span>
                </a>
            </section>
        "#;

        let rows = parse_main_premieres_html(html);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title_id, Some(68638));
        assert_eq!(rows[0].name, "Tensei shitara Slime Datta Ken 4th Season");
        assert_eq!(
            rows[0].image_url,
            "https://shinden.pl/res/images/225x350/435918.jpg"
        );
    }

    #[test]
    fn parses_card_ratings_without_leaking_between_cards() {
        let html = r#"
            <article data-rate="8.25">
                <a href="/series/60001-alpha"><img alt="Alpha" /></a>
            </article>
            <article>
                <a href="/series/60002-beta"><img alt="Beta" /></a>
                <span class="rate-top">7,4</span>
            </article>
            <article>
                <a href="/series/60003-gamma"><img alt="Gamma" /></a>
            </article>
        "#;

        let rows = parse_main_premieres_html(html);

        assert_eq!(
            rows.iter().map(|row| row.rating.as_str()).collect::<Vec<_>>(),
            vec!["8,25", "7,4", ""],
        );
    }

    #[test]
    fn parse_season_anime_extracts_title_rows() {
        let html = r#"
            <article>
                <h3><a href="/series/60001-jujutsu-kaisen-shimetsu-kaiyuu-zenpen">Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen</a></h3>
                <img src="https://cdn.shinden.eu/cdn1/images/genuine/60001.jpg" alt="">
                <p>TV 12ep</p>
                <strong>8,7</strong>
            </article>
        "#;

        let rows = parse_season_anime_html(html);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title_id, Some(60001));
        assert_eq!(rows[0].name, "Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen");
        assert_eq!(rows[0].anime_type, "TV");
        assert_eq!(rows[0].episodes, "12ep");
        assert_eq!(rows[0].rating, "8,7");
    }

    #[test]
    fn map_discovery_anime_results_uses_matching_status() {
        let rows = vec![DiscoveryAnimeBase {
            name: "Anime 59922".to_string(),
            url: "https://shinden.pl/series/59922-anime".to_string(),
            image_url: SHINDEN_TITLE_PLACEHOLDER.to_string(),
            anime_type: "TV".to_string(),
            rating: "8,1".to_string(),
            episodes: "12ep".to_string(),
            description: String::new(),
            title_id: Some(59922),
            total_episodes: Some(12),
            source_label: Some("Odcinek 1".to_string()),
        }];
        let watching_items = vec![WatchingListApiItem {
            title_id: 59922,
            watch_status: Some("plan".to_string()),
            is_favourite: Some(1),
            title: "Anime 59922".to_string(),
            cover_id: None,
            anime_type: Some("TV".to_string()),
            summary_rating_total: Some("8.1".to_string()),
            episodes: Some(12),
            watched_episodes_cnt: Some("0".to_string()),
            description_pl: None,
            description_en: None,
            release_date: None,
            year: None,
        }];

        let mapped = map_discovery_anime_results(rows, watching_items);

        assert_eq!(mapped[0].watch_status, "plan");
        assert_eq!(mapped[0].is_favourite, 1);
        assert_eq!(mapped[0].total_episodes, Some(12));
    }

    #[test]
    fn discovery_anime_serializes_frontend_field_names() {
        let anime = DiscoveryAnime {
            name: "Anime 59922".to_string(),
            url: "https://shinden.pl/series/59922-anime".to_string(),
            image_url: "https://cdn.shinden.eu/cdn1/images/genuine/59922.jpg".to_string(),
            anime_type: "TV".to_string(),
            rating: "8,1".to_string(),
            episodes: "12ep".to_string(),
            description: String::new(),
            title_id: Some(59922),
            watch_status: "plan".to_string(),
            is_favourite: 1,
            total_episodes: Some(12),
            source_label: Some("Odcinek 1".to_string()),
        };

        let json = serde_json::to_value(anime).expect("discovery anime should serialize");

        assert_eq!(
            json["image_url"].as_str(),
            Some("https://cdn.shinden.eu/cdn1/images/genuine/59922.jpg")
        );
        assert_eq!(json["anime_type"].as_str(), Some("TV"));
        assert!(json.get("imageUrl").is_none());
        assert!(json.get("animeType").is_none());
        assert_eq!(json["titleId"].as_u64(), Some(59922));
        assert_eq!(json["watchStatus"].as_str(), Some("plan"));
        assert_eq!(json["isFavourite"].as_u64(), Some(1));
        assert_eq!(json["totalEpisodes"].as_u64(), Some(12));
        assert_eq!(json["sourceLabel"].as_str(), Some("Odcinek 1"));
    }

    #[test]
    fn find_project_root_from_detects_repository_markers() {
        let root = unique_temp_dir("root_markers");
        let nested = root.join("src-tauri").join("target").join("release");
        fs::create_dir_all(&nested).expect("failed to create nested test directory");
        fs::write(root.join("package.json"), "{}").expect("failed to create package marker");
        fs::write(root.join("src-tauri").join("tauri.conf.json"), "{}")
            .expect("failed to create tauri marker");

        let found = find_project_root_from(&nested);

        assert_eq!(found.as_deref(), Some(root.as_path()));
        fs::remove_dir_all(root).expect("failed to remove test directory");
    }

    #[test]
    fn append_log_line_writes_exceptions_to_project_log_file() {
        let log_dir = unique_temp_dir("logs");

        let path = append_log_line(&log_dir, "ERROR", "example exception")
            .expect("failed to append log line");

        assert_eq!(path, log_dir.join("shinden-client.log"));
        let contents = fs::read_to_string(path).expect("failed to read log file");
        assert!(contents.contains("[ERROR] example exception"));
        fs::remove_dir_all(log_dir).expect("failed to remove log directory");
    }

    #[test]
    fn write_log_command_discards_log_file_path() {
        let result: Result<(), String> = discard_log_path(Ok(PathBuf::from("shinden-client.log")));

        assert_eq!(result, Ok(()));
    }

    #[test]
    fn extract_user_id_from_profile_links_finds_current_user_animelist() {
        let html = r#"
            <a href="https://lista.shinden.pl/animelist/31875-szypss">Lista Anime</a>
            <a href="/user/31875-szypss">Profil</a>
        "#;

        let user_id = extract_user_id_from_profile_html(html);

        assert_eq!(user_id.as_deref(), Some("31875"));
    }

    #[test]
    fn extract_shinden_basic_auth_reads_storage_token() {
        let html = r#"<script>_Storage.basic = "token-123";</script>"#;

        assert_eq!(
            extract_shinden_basic_auth(html).as_deref(),
            Some("token-123")
        );
    }

    #[test]
    fn shinden_watch_status_value_maps_ui_and_api_values() {
        assert_eq!(
            shinden_watch_status_value(Some("inProgress")).unwrap(),
            Some("in progress")
        );
        assert_eq!(
            shinden_watch_status_value(Some("in progress")).unwrap(),
            Some("in progress")
        );
        assert_eq!(
            shinden_watch_status_value(Some("completed")).unwrap(),
            Some("completed")
        );
        assert_eq!(
            shinden_watch_status_value(Some("skip")).unwrap(),
            Some("skip")
        );
        assert_eq!(
            shinden_watch_status_value(Some("hold")).unwrap(),
            Some("hold")
        );
        assert_eq!(
            shinden_watch_status_value(Some("dropped")).unwrap(),
            Some("dropped")
        );
        assert_eq!(
            shinden_watch_status_value(Some("plan")).unwrap(),
            Some("plan")
        );
        assert_eq!(shinden_watch_status_value(Some("no")).unwrap(), None);
        assert_eq!(shinden_watch_status_value(None).unwrap(), None);
    }

    #[test]
    fn shinden_watch_status_value_rejects_unknown_status() {
        let result = shinden_watch_status_value(Some("watching-but-weird"));

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unsupported anime status"));
    }

    #[test]
    fn title_status_matches_normalizes_status_values() {
        assert!(title_status_matches(Some("in progress"), Some("inProgress")).unwrap());
        assert!(title_status_matches(Some("completed"), Some("completed")).unwrap());
        assert!(title_status_matches(None, Some("no")).unwrap());
        assert!(!title_status_matches(Some("completed"), Some("in progress")).unwrap());
    }

    #[test]
    fn shinden_legacy_watch_status_values_include_old_aliases() {
        assert_eq!(
            shinden_legacy_watch_status_values(Some("in progress")).unwrap(),
            vec!["in progress", "in-progress", "watching"]
        );
        assert_eq!(
            shinden_legacy_watch_status_values(Some("completed")).unwrap(),
            vec!["completed", "watched"]
        );
        assert!(
            shinden_legacy_watch_status_values(Some("no"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn watch_status_list_slug_maps_shinden_values() {
        assert_eq!(watch_status_list_slug("in progress"), "in-progress");
        assert_eq!(watch_status_list_slug("completed"), "completed");
        assert_eq!(watch_status_list_slug("skip"), "skip");
        assert_eq!(watch_status_list_slug("hold"), "hold");
        assert_eq!(watch_status_list_slug("dropped"), "dropped");
        assert_eq!(watch_status_list_slug("plan"), "plan");
    }

    #[test]
    fn title_status_payload_serializes_shinden_status_change() {
        let payload = build_title_status_payload(59922, Some("completed"), Some(1))
            .expect("payload should build");
        let value = serde_json::to_value(payload).expect("payload should serialize");

        assert_eq!(value["input"][0]["titleId"], 59922);
        assert_eq!(value["input"][0]["watchStatus"], "completed");
        assert_eq!(value["input"][0]["isFavourite"], 1);
        assert_eq!(value["input"][0]["priority"], 0);
        assert_eq!(value["input"][0]["recommend"], 0);
    }

    #[test]
    fn title_status_payload_serializes_no_status_as_null() {
        let payload =
            build_title_status_payload(59922, Some("no"), None).expect("payload should build");
        let value = serde_json::to_value(payload).expect("payload should serialize");

        assert!(value["input"][0]["watchStatus"].is_null());
        assert_eq!(value["input"][0]["isFavourite"], 0);
    }

    #[test]
    fn title_status_payload_preserves_priority_and_recommendation() {
        let payload =
            build_title_status_payload_with_details(59922, Some("plan"), Some(1), -10, 25)
                .expect("payload should build");
        let value = serde_json::to_value(payload).expect("payload should serialize");

        assert_eq!(value["input"][0]["watchStatus"], "plan");
        assert_eq!(value["input"][0]["isFavourite"], 1);
        assert_eq!(value["input"][0]["priority"], -10);
        assert_eq!(value["input"][0]["recommend"], 25);
    }

    #[test]
    fn watched_episode_payload_serializes_single_episode() {
        let payload =
            build_watched_episode_payload(59922, 168519, "2026-05-03 00:45:10".to_string(), 1);
        let value = serde_json::to_value(payload).expect("payload should serialize");

        assert_eq!(value["titleId"], 59922);
        assert_eq!(value["episodes"][0]["episodeId"], 168519);
        assert_eq!(value["episodes"][0]["viewCnt"], 1);
        assert_eq!(value["episodes"][0]["createdTime"], "2026-05-03 00:45:10");
    }

    #[test]
    fn watched_episode_payload_serializes_unwatched_episode() {
        let payload =
            build_watched_episode_payload(59922, 168519, "2026-05-03 00:45:10".to_string(), 0);
        let value = serde_json::to_value(payload).expect("payload should serialize");

        assert_eq!(value["titleId"], 59922);
        assert_eq!(value["episodes"][0]["episodeId"], 168519);
        assert_eq!(value["episodes"][0]["viewCnt"], 0);
        assert_eq!(value["episodes"][0]["createdTime"], "2026-05-03 00:45:10");
    }

    #[test]
    fn map_watching_list_item_builds_series_and_cover_urls() {
        let item = WatchingListApiItem {
            title_id: 59922,
            watch_status: Some("in progress".to_string()),
            is_favourite: Some(0),
            title: "Enen no Shouboutai: San no Shou".to_string(),
            cover_id: Some(123456),
            anime_type: Some("TV".to_string()),
            summary_rating_total: Some("7.9000".to_string()),
            episodes: Some(12),
            watched_episodes_cnt: Some("3".to_string()),
            description_pl: Some("Opis".to_string()),
            description_en: None,
            release_date: None,
            year: None,
        };

        let anime = map_watching_list_item(item).expect("item should map");

        assert_eq!(anime.name, "Enen no Shouboutai: San no Shou");
        assert_eq!(anime.url, "https://shinden.pl/series/59922");
        assert_eq!(
            anime.image_url,
            "https://cdn.shinden.eu/cdn1/images/genuine/123456.jpg"
        );
        assert_eq!(anime.anime_type, "TV");
        assert_eq!(anime.rating, "7,90");
        assert_eq!(anime.episodes, "3/12");
        assert_eq!(anime.description, "Opis");
    }

    #[test]
    fn map_watching_list_item_details_preserves_status_progress_and_favourite() {
        let item = WatchingListApiItem {
            title_id: 59922,
            watch_status: Some("in progress".to_string()),
            is_favourite: Some(1),
            title: "Enen no Shouboutai: San no Shou".to_string(),
            cover_id: Some(123456),
            anime_type: Some("TV".to_string()),
            summary_rating_total: Some("7.9000".to_string()),
            episodes: Some(12),
            watched_episodes_cnt: Some("3".to_string()),
            description_pl: Some("Opis".to_string()),
            description_en: None,
            release_date: None,
            year: None,
        };

        let anime = map_watching_list_item_details(item).expect("item should map");

        assert_eq!(anime.title_id, 59922);
        assert_eq!(anime.watch_status, "in progress");
        assert_eq!(anime.is_favourite, 1);
        assert_eq!(anime.name, "Enen no Shouboutai: San no Shou");
        assert_eq!(anime.url, "https://shinden.pl/series/59922");
        assert_eq!(anime.rating, "7,90");
        assert_eq!(anime.episodes, "3/12");
        assert_eq!(anime.watched_episodes_count, 3);
        assert_eq!(anime.total_episodes, Some(12));
    }

    #[test]
    fn user_list_normal_sync_preserves_cached_metadata_and_updates_status() {
        let mut cache = UserAnimeListCache::default();
        cache.items.insert(
            "59922".to_string(),
            cached_user_anime_fixture(59922, "Old Title", "in progress", true),
        );

        let items = merge_user_anime_list_cache(
            &mut cache,
            vec![watching_item_fixture(
                59922,
                Some("completed"),
                Some(1),
                Some(24),
            )],
            false,
            10_000,
        );

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "Old Title");
        assert_eq!(items[0].watch_status, "completed");
        assert_eq!(items[0].is_favourite, 1);
        assert_eq!(items[0].total_episodes, Some(24));
        assert_eq!(items[0].updated_at_ms, 10_000);
    }

    #[test]
    fn user_list_force_refresh_overwrites_cached_metadata() {
        let mut cache = UserAnimeListCache::default();
        cache.items.insert(
            "59922".to_string(),
            cached_user_anime_fixture(59922, "Old Title", "in progress", true),
        );

        let mut incoming = watching_item_fixture(59922, Some("completed"), Some(1), Some(12));
        incoming.title = "Fresh Title".to_string();
        let items = merge_user_anime_list_cache(&mut cache, vec![incoming], true, 10_000);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "Fresh Title");
        assert_eq!(items[0].watch_status, "completed");
    }

    #[test]
    fn user_list_details_add_tags_and_age_rating_to_cached_item() {
        let mut item = cached_user_anime_fixture(59922, "Old Title", "in progress", true);
        let details = AnimeDetails {
            categories: vec![
                AnimeCategoryGroup {
                    label: "Gatunki:".to_string(),
                    items: vec!["Komedia".to_string(), "Fantasy".to_string()],
                },
                AnimeCategoryGroup {
                    label: "Pierwowzor:".to_string(),
                    items: vec!["Manga".to_string(), "Fantasy".to_string()],
                },
            ],
            information: vec![AnimeInfoRow {
                label: "Kategoria wiekowa:".to_string(),
                value: "R17+".to_string(),
            }],
            ..AnimeDetails::default()
        };

        apply_user_anime_details_to_item(&mut item, &details);

        assert_eq!(item.tags, vec!["Komedia", "Fantasy", "Manga"]);
        assert_eq!(item.age_rating.as_deref(), Some("R17+"));
    }

    #[test]
    fn anime_detail_age_rating_reads_mpaa_label() {
        let details = AnimeDetails {
            information: vec![AnimeInfoRow {
                label: "MPAA:".to_string(),
                value: "PG-13".to_string(),
            }],
            ..AnimeDetails::default()
        };

        assert_eq!(anime_detail_age_rating(&details).as_deref(), Some("PG-13"));
    }

    #[test]
    fn user_list_sync_inserts_new_titles_and_hides_removed_titles() {
        let mut cache = UserAnimeListCache::default();
        cache.items.insert(
            "1".to_string(),
            cached_user_anime_fixture(1, "Removed", "completed", true),
        );

        let items = merge_user_anime_list_cache(
            &mut cache,
            vec![watching_item_fixture(2, Some("plan"), Some(0), Some(12))],
            false,
            10_000,
        );

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title_id, 2);
        assert_eq!(items[0].watch_status, "plan");
        assert!(!cache.items.get("1").expect("cached item").active);
    }

    #[test]
    fn user_list_sync_detects_new_active_titles_for_detail_metadata() {
        let mut cache = UserAnimeListCache::default();
        cache.items.insert(
            "1".to_string(),
            cached_user_anime_fixture(1, "Existing", "completed", true),
        );
        let existing_keys = user_anime_list_cache_keys(&cache);

        merge_user_anime_list_cache(
            &mut cache,
            vec![
                watching_item_fixture(1, Some("completed"), Some(0), Some(12)),
                watching_item_fixture(2, Some("plan"), Some(0), Some(12)),
            ],
            false,
            10_000,
        );

        assert_eq!(
            new_active_user_anime_list_cache_keys(&cache, &existing_keys),
            vec!["2".to_string()]
        );
    }

    #[test]
    fn user_list_refresh_state_queues_only_active_items() {
        let mut cache = UserAnimeListCache::default();
        cache.items.insert(
            "1".to_string(),
            cached_user_anime_fixture(1, "Active completed", "completed", true),
        );
        cache.items.insert(
            "2".to_string(),
            cached_user_anime_fixture(2, "Removed completed", "completed", false),
        );
        cache.items.insert(
            "3".to_string(),
            cached_user_anime_fixture(3, "Active plan", "plan", true),
        );

        let state = build_user_anime_list_refresh_state(&cache, 20_000);
        let queued_keys: Vec<&str> = state.queue.iter().map(|item| item.key.as_str()).collect();

        assert_eq!(queued_keys, vec!["1", "3"]);
        assert_eq!(state.started_at_ms, Some(20_000));
        assert_eq!(
            user_anime_list_refresh_status_from_state(&state, false).total,
            2
        );
    }

    #[test]
    fn user_list_refresh_state_queues_only_titles_missing_detail_metadata() {
        let mut cache = UserAnimeListCache::default();
        let mut complete = cached_user_anime_fixture(1, "Complete", "completed", true);
        complete.tags = vec!["Fantasy".to_string()];
        complete.age_rating = Some("PG-13".to_string());
        cache.items.insert("1".to_string(), complete);
        cache.items.insert(
            "2".to_string(),
            cached_user_anime_fixture(2, "Missing metadata", "plan", true),
        );

        let state = build_user_anime_list_refresh_state(&cache, 20_000);
        let queued_keys: Vec<&str> = state.queue.iter().map(|item| item.key.as_str()).collect();

        assert_eq!(queued_keys, vec!["2"]);
    }

    #[test]
    fn user_list_refresh_status_counts_progress_remaining_and_current_title() {
        let state = UserAnimeListRefreshState {
            queue: vec![
                UserAnimeListRefreshQueueItem {
                    key: "1".to_string(),
                    title_id: 1,
                    title: "Done".to_string(),
                    url: "https://shinden.pl/series/1".to_string(),
                    done: true,
                    failed: false,
                },
                UserAnimeListRefreshQueueItem {
                    key: "2".to_string(),
                    title_id: 2,
                    title: "Failed".to_string(),
                    url: "https://shinden.pl/series/2".to_string(),
                    done: false,
                    failed: true,
                },
                UserAnimeListRefreshQueueItem {
                    key: "3".to_string(),
                    title_id: 3,
                    title: "Pending".to_string(),
                    url: "https://shinden.pl/series/3".to_string(),
                    done: false,
                    failed: false,
                },
            ],
            started_at_ms: Some(10_000),
            last_finished_at_ms: None,
            last_error: Some("partial".to_string()),
        };

        let status = user_anime_list_refresh_status_from_state(&state, true);

        assert!(status.running);
        assert_eq!(status.current, 2);
        assert_eq!(status.total, 3);
        assert_eq!(status.remaining, 1);
        assert_eq!(status.refreshed, 1);
        assert_eq!(status.failed, 1);
        assert_eq!(status.current_title, "Pending");
        assert_eq!(status.last_error.as_deref(), Some("partial"));
    }

    #[test]
    fn map_search_anime_results_defaults_to_no_status_and_extracts_title_id() {
        let results = map_search_anime_results(
            vec![anime_fixture(
                "https://shinden.pl/series/59922-enen-no-shouboutai",
            )],
            Vec::new(),
        );

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title_id, Some(59922));
        assert_eq!(results[0].watch_status, "no");
        assert_eq!(results[0].is_favourite, 0);
        assert_eq!(results[0].total_episodes, None);
        assert_eq!(results[0].anime.name, "Enen no Shouboutai: San no Shou");
    }

    #[test]
    fn map_search_anime_results_uses_matching_watching_status() {
        let results = map_search_anime_results(
            vec![anime_fixture(
                "https://shinden.pl/titles/59922-enen-no-shouboutai",
            )],
            vec![watching_item_fixture(
                59922,
                Some("completed"),
                Some(1),
                Some(12),
            )],
        );

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title_id, Some(59922));
        assert_eq!(results[0].watch_status, "completed");
        assert_eq!(results[0].is_favourite, 1);
        assert_eq!(results[0].total_episodes, Some(12));
    }

    #[test]
    fn anime_details_uses_fetched_title_status() {
        let details = AnimeDetails {
            title_id: Some(59211),
            watch_status: "no".to_string(),
            is_favourite: 0,
            user_status_loaded: false,
            ..AnimeDetails::default()
        };

        let details = anime_details_with_title_status(
            details,
            Some(TitleStatusApiTitle {
                watch_status: Some("completed".to_string()),
                is_favourite: Some(1),
                priority: Some(0),
                recommend: Some(0),
            }),
            true,
        );

        assert_eq!(details.watch_status, "completed");
        assert_eq!(details.is_favourite, 1);
        assert!(details.user_status_loaded);
    }

    #[test]
    fn anime_details_marks_loaded_when_title_has_no_status() {
        let details = AnimeDetails {
            title_id: Some(59211),
            watch_status: "completed".to_string(),
            is_favourite: 1,
            user_status_loaded: false,
            ..AnimeDetails::default()
        };

        let details = anime_details_with_title_status(details, None, true);

        assert_eq!(details.watch_status, "no");
        assert_eq!(details.is_favourite, 0);
        assert!(details.user_status_loaded);
    }

    #[test]
    fn watching_list_status_url_uses_in_progress_status() {
        let url = watching_list_status_url("31875", "in progress", 100, 200);

        assert_eq!(
            url,
            "https://lista.shinden.pl/api/userlist/31875/anime/in-progress?limit=100&offset=200"
        );
    }

    #[test]
    fn watching_list_status_url_uses_selected_status_slug() {
        let url = watching_list_status_url("31875", "completed", 100, 200);

        assert_eq!(
            url,
            "https://lista.shinden.pl/api/userlist/31875/anime/completed?limit=100&offset=200"
        );
    }

    #[test]
    fn title_status_url_uses_title_and_user_ids() {
        assert_eq!(
            title_status_url(59922, "31875"),
            "https://lista.shinden.pl/api/title-status/59922/31875"
        );
    }

    #[test]
    fn legacy_userlist_series_url_uses_shinden_api_path() {
        assert_eq!(
            legacy_userlist_series_url("31875", 59922),
            "https://shinden.pl/api/userlist/31875/series/59922"
        );
    }

    #[test]
    fn canonical_title_url_selects_the_result_matching_the_exact_title_id() {
        let results = vec![
            anime_fixture("https://shinden.pl/series/69862-bye-bye-earth"),
            anime_fixture("https://shinden.pl/series/68581-bye-bye-earth-2nd-season"),
        ];

        assert_eq!(
            canonical_title_url_from_search_results(68581, &results).as_deref(),
            Some("https://shinden.pl/series/68581-bye-bye-earth-2nd-season"),
        );
    }
    #[test]
    fn cached_canonical_url_is_used_without_a_new_title_search() {
        let urls = HashMap::from([(
            68581,
            "https://shinden.pl/series/68581-bye-bye-earth-2nd-season".to_string(),
        )]);

        assert_eq!(
            canonical_url_from_cache_or_fallback(68581, &urls),
            "https://shinden.pl/series/68581-bye-bye-earth-2nd-season",
        );
        assert_eq!(
            canonical_url_from_cache_or_fallback(69862, &urls),
            "https://shinden.pl/series/69862",
        );
    }

    #[test]
    fn cached_user_list_is_returned_without_live_refresh_unless_forced() {
        let item = cached_user_anime_fixture(68581, "Bye Bye, Earth 2nd Season", "in progress", true);
        let mut cache = UserAnimeListCache::default();
        cache.items.insert(user_anime_list_cache_key(item.title_id), item);

        assert!(should_return_cached_user_anime_lists(&cache, false));
        assert!(!should_return_cached_user_anime_lists(&cache, true));
        assert!(!should_return_cached_user_anime_lists(&UserAnimeListCache::default(), false));
    }


    #[test]
    fn title_id_from_series_url_extracts_numeric_id() {
        assert_eq!(
            title_id_from_series_url("https://shinden.pl/series/59922-enen-no-shouboutai")
                .as_deref(),
            Some("59922")
        );
        assert_eq!(
            title_id_from_series_url("https://shinden.pl/series/59922").as_deref(),
            Some("59922")
        );
        assert_eq!(
            title_id_from_series_url("https://shinden.pl/titles/59922-enen-no-shouboutai")
                .as_deref(),
            Some("59922")
        );
        assert_eq!(
            title_id_from_series_url("https://shinden.pl/titles/abc"),
            None
        );
    }

    #[test]
    fn true_final_episode_requires_known_total_episode_count() {
        assert!(is_true_final_episode(12, Some(12)));
        assert!(!is_true_final_episode(10, Some(12)));
        assert!(!is_true_final_episode(10, None));
    }

    #[test]
    fn true_final_episode_ignores_last_loaded_episode_when_total_is_larger() {
        let playback = vec![
            Episode {
                title: "Episode 9".to_string(),
                link: "https://shinden.pl/episode/9".to_string(),
            },
            Episode {
                title: "Episode 10".to_string(),
                link: "https://shinden.pl/episode/10".to_string(),
            },
        ];
        let progress = vec![TitleEpisodeApiItem {
            episode_id: 100,
            episode_no: 10,
            is_filer: Some(0),
            watched: None,
            title_pl: None,
            title_en: None,
            title_official: None,
        }];

        let merged = merge_episode_progress(playback, progress, Some(12));

        assert_eq!(merged[1].episode_no, 10);
        assert!(!merged[1].is_true_final_episode);
    }

    #[test]
    fn merge_episode_progress_marks_watched_rows_by_episode_number() {
        let playback = vec![
            Episode {
                title: "Playback one".to_string(),
                link: "https://shinden.pl/episode/1".to_string(),
            },
            Episode {
                title: "Playback two".to_string(),
                link: "https://shinden.pl/episode/2".to_string(),
            },
        ];
        let progress = vec![TitleEpisodeApiItem {
            episode_id: 168519,
            episode_no: 2,
            is_filer: Some(0),
            watched: Some(TitleEpisodeWatchedApiItem {
                episode_id: 168519,
                view_cnt: 1,
                created_time: Some("2022-07-28T00:33:32.000Z".to_string()),
            }),
            title_pl: Some(TitleEpisodeTitleApiItem {
                lang: "pl".to_string(),
                episode_id: 168519,
                title: "Polski tytul".to_string(),
                title_type: "national".to_string(),
            }),
            title_en: None,
            title_official: None,
        }];

        let merged = merge_episode_progress(playback, progress, Some(2));

        assert_eq!(merged[0].episode_no, 1);
        assert_eq!(merged[0].episode_id, None);
        assert!(!merged[0].watched);
        assert_eq!(merged[1].episode_id, Some(168519));
        assert_eq!(merged[1].title, "Playback two");
        assert!(merged[1].watched);
        assert_eq!(merged[1].view_count, 1);
        assert!(merged[1].is_true_final_episode);
    }

    fn watching_item(watched: Option<&str>, episodes: Option<u32>) -> WatchingListApiItem {
        WatchingListApiItem {
            title_id: 59922,
            watch_status: Some("in progress".to_string()),
            is_favourite: Some(0),
            title: "Enen no Shouboutai: San no Shou".to_string(),
            cover_id: None,
            anime_type: None,
            summary_rating_total: None,
            episodes,
            watched_episodes_cnt: watched.map(str::to_string),
            description_pl: None,
            description_en: None,
            release_date: None,
            year: None,
        }
    }

    fn watching_item_with_title(
        title_id: u64,
        watched: Option<&str>,
        episodes: Option<u32>,
    ) -> WatchingListApiItem {
        let mut item = watching_item(watched, episodes);
        item.title_id = title_id;
        item.title = format!("Anime {title_id}");
        item
    }

    #[test]
    fn fresh_cached_user_id_is_reused_until_ttl_expires() {
        let mut cache = CachedUserId::default();
        store_cached_user_id_value(&mut cache, "31875", 10_000);

        assert_eq!(
            cached_user_id_if_fresh(&cache, 10_000 + USER_ID_CACHE_TTL_MS - 1).as_deref(),
            Some("31875")
        );
        assert_eq!(
            cached_user_id_if_fresh(&cache, 10_000 + USER_ID_CACHE_TTL_MS + 1),
            None
        );
    }

    #[test]
    fn user_profile_rate_limit_error_is_transient() {
        assert!(is_transient_user_profile_error(
            "HTTP status client error (429 Too Many Requests) for url (https://shinden.pl/user)"
        ));
        assert!(!is_transient_user_profile_error("User is not logged in"));
    }

    #[test]
    fn watching_cache_language_scan_stops_after_first_playable_episode() {
        let mut availability = std::collections::HashMap::new();

        let should_stop = record_watching_cache_episode_subtitle_availability(
            ["EN"].into_iter(),
            Some("pl"),
            &mut availability,
        );

        assert!(should_stop);
        assert_eq!(availability.get("en"), Some(&true));
        assert_eq!(availability.get("pl"), None);
    }

    #[test]
    fn watching_cache_episode_scan_continues_past_empty_player_lists() {
        let mut availability = std::collections::HashMap::new();

        let should_stop = record_watching_cache_episode_subtitle_availability(
            std::iter::empty::<&str>(),
            Some("pl"),
            &mut availability,
        );

        assert!(!should_stop);
        assert!(availability.is_empty());
    }

    #[test]
    fn episode_availability_keeps_player_and_subtitle_state_per_episode() {
        let availability = watching_episode_availability(&[
            Player {
                player: "default".to_string(),
                max_res: "1080p".to_string(),
                lang_audio: "JP".to_string(),
                lang_subs: "IPL".to_string(),
                online_id: "one".to_string(),
            },
            Player {
                player: "default".to_string(),
                max_res: "1080p".to_string(),
                lang_audio: "JP".to_string(),
                lang_subs: "PL".to_string(),
                online_id: "two".to_string(),
            },
        ]);

        assert!(availability.has_players);
        assert_eq!(availability.subtitle_availability.get("pl"), Some(&true));
        assert_eq!(availability.subtitle_availability.get("pl:human"), Some(&true));
    }

    #[test]
    fn watching_cache_refresh_scans_titles_serially_to_avoid_rate_limits() {
        assert_eq!(WATCHING_CACHE_REFRESH_CONCURRENCY, 1);
    }

    #[test]
    fn watching_cache_failure_serializes_title_link_and_reason() {
        let failure = WatchingCacheFailure {
            title_id: 71632,
            title: "Kokoore".to_string(),
            series_url: "https://shinden.pl/series/71632".to_string(),
            reason: "HTTP 404".to_string(),
        };
        let value = serde_json::to_value(failure).expect("failure serializes");

        assert_eq!(value["titleId"], 71632);
        assert_eq!(value["reason"], "HTTP 404");
    }

    #[test]
    fn watching_cache_refresh_plan_queues_only_uncached_unwatched_items() {
        let uncached = watching_item_with_title(59922, Some("2"), Some(3));
        let completed = watching_item_with_title(59923, Some("3"), Some(3));
        let cached = watching_item_with_title(59924, Some("2"), Some(3));
        let stale = watching_item_with_title(59925, Some("2"), Some(3));
        let items = vec![uncached, completed, cached, stale];
        let mut cache = WatchingAvailabilityCache::default();
        let mut subtitle_availability = std::collections::HashMap::new();
        subtitle_availability.insert("pl".to_string(), true);

        cache.entries.insert(
            "59924".to_string(),
            WatchingAvailabilityCacheEntry {
                title_id: 59924,
                watched_episodes_cnt: 2,
                total_episodes: Some(3),
                has_available_unwatched_episode: true,
                subtitle_availability: subtitle_availability.clone(),
                episode_availability: [("episode-3".to_string(), WatchingEpisodeAvailability::default())].into_iter().collect(),
                checked_at_ms: 10_000,
            },
        );
        cache.entries.insert(
            "59925".to_string(),
            WatchingAvailabilityCacheEntry {
                title_id: 59925,
                watched_episodes_cnt: 2,
                total_episodes: Some(3),
                has_available_unwatched_episode: true,
                subtitle_availability,
                episode_availability: Default::default(),
                checked_at_ms: 0,
            },
        );

        let plan = collect_watching_cache_refresh_plan(&items, &cache, Some("pl"), 10_500, false);
        let queued_title_ids: Vec<u64> = plan
            .items_to_scan
            .iter()
            .map(|item| item.title_id)
            .collect();

        assert_eq!(plan.skipped, 2);
        assert_eq!(plan.processed, 2);
        assert_eq!(queued_title_ids, vec![59922, 59925]);
    }

    #[test]
    fn has_unwatched_episodes_compares_watched_count_to_total() {
        assert!(has_unwatched_episodes(&watching_item(Some("2"), Some(3))));
        assert!(!has_unwatched_episodes(&watching_item(Some("3"), Some(3))));
        assert!(has_unwatched_episodes(&watching_item(None, Some(1))));
    }

    #[test]
    fn subtitle_language_matches_common_aliases() {
        assert!(subtitle_language_matches("Polski", "PL"));
        assert!(subtitle_language_matches("Napisy PL", "polski"));
        assert!(subtitle_language_matches("iPL", "PL"));
        assert!(subtitle_language_matches("English", "EN"));
        assert!(!subtitle_language_matches("Angielski", "PL"));
    }

    #[test]
    fn subtitle_language_can_exclude_ai_translations() {
        assert!(!subtitle_language_matches_with_options("iPL", "PL", true));
        assert!(subtitle_language_matches_with_options("PL", "PL", true));
    }

    #[test]
    fn subtitle_availability_records_ai_and_human_variants_separately() {
        let mut availability = std::collections::HashMap::new();

        record_subtitle_language_availability(&mut availability, "iPL");

        assert_eq!(availability.get("pl"), Some(&true));
        assert_eq!(availability.get("pl:human"), None);

        record_subtitle_language_availability(&mut availability, "PL");

        assert_eq!(availability.get("pl"), Some(&true));
        assert_eq!(availability.get("pl:human"), Some(&true));
    }

    #[test]
    fn ai_filtered_subtitles_use_separate_cache_key() {
        let filter = WatchingAnimeFilter {
            check_subtitle_availability_online: Some(true),
            subtitle_language: Some("PL".to_string()),
            exclude_ai_subtitles: Some(true),
            ..Default::default()
        };

        assert_eq!(
            selected_subtitle_cache_key(&filter).as_deref(),
            Some("pl:human")
        );
    }

    #[test]
    fn watching_progress_filter_includes_all_items_when_disabled() {
        let filter = WatchingAnimeFilter::default();

        assert!(watching_progress_filter_matches(
            &watching_item(Some("3"), Some(3)),
            &filter
        ));
    }

    #[test]
    fn watching_progress_filter_uses_local_unwatched_counts() {
        let filter = WatchingAnimeFilter {
            only_available_unwatched: Some(true),
            ..Default::default()
        };

        assert!(watching_progress_filter_matches(
            &watching_item(Some("2"), Some(3)),
            &filter
        ));
        assert!(!watching_progress_filter_matches(
            &watching_item(Some("3"), Some(3)),
            &filter
        ));
    }

    #[test]
    fn subtitle_availability_online_check_is_opt_in() {
        assert!(!WatchingAnimeFilter::default().check_subtitle_availability_online());

        let filter = WatchingAnimeFilter {
            check_subtitle_availability_online: Some(true),
            ..Default::default()
        };

        assert!(filter.check_subtitle_availability_online());
    }

    #[test]
    fn cache_filter_hides_items_without_confirmed_available_episode() {
        let item = watching_item(Some("2"), Some(3));
        let filter = WatchingAnimeFilter {
            only_available_unwatched: Some(true),
            ..Default::default()
        };
        let mut cache = WatchingAvailabilityCache::default();

        assert!(!watching_cache_filter_matches(&item, &filter, &cache));

        cache.entries.insert(
            "59922".to_string(),
            WatchingAvailabilityCacheEntry {
                title_id: 59922,
                watched_episodes_cnt: 2,
                total_episodes: Some(3),
                has_available_unwatched_episode: false,
                subtitle_availability: Default::default(),
                episode_availability: Default::default(),
                checked_at_ms: 1000,
            },
        );

        assert!(!watching_cache_filter_matches(&item, &filter, &cache));
    }

    #[test]
    fn cache_filter_keeps_ipl_only_title_when_language_filters_are_disabled() {
        let item = watching_item(Some("2"), Some(3));
        let filter = WatchingAnimeFilter {
            only_available_unwatched: Some(true),
            ..Default::default()
        };
        let mut cache = WatchingAvailabilityCache::default();
        cache.entries.insert(
            "59922".to_string(),
            WatchingAvailabilityCacheEntry {
                title_id: 59922,
                watched_episodes_cnt: 2,
                total_episodes: Some(3),
                has_available_unwatched_episode: false,
                subtitle_availability: HashMap::from([("pl".to_string(), true)]),
                episode_availability: Default::default(),
                checked_at_ms: 1000,
            },
        );

        assert!(watching_cache_filter_matches(&item, &filter, &cache));
    }

    #[test]
    fn cache_filter_honors_subtitle_filter_without_unwatched_toggle() {
        let item = watching_item(Some("2"), Some(3));
        let filter = WatchingAnimeFilter {
            check_subtitle_availability_online: Some(true),
            subtitle_language: Some("PL".to_string()),
            ..Default::default()
        };


        assert!(!watching_cache_filter_matches(&item, &filter, &WatchingAvailabilityCache::default()));
    }
    #[test]
    fn cache_filter_uses_cached_subtitle_language_availability() {
        let item = watching_item(Some("2"), Some(3));
        let filter = WatchingAnimeFilter {
            only_available_unwatched: Some(true),
            check_subtitle_availability_online: Some(true),
            subtitle_language: Some("PL".to_string()),
            ..Default::default()
        };
        let mut subtitle_availability = std::collections::HashMap::new();
        subtitle_availability.insert("pl".to_string(), true);
        let mut cache = WatchingAvailabilityCache::default();
        cache.entries.insert(
            "59922".to_string(),
            WatchingAvailabilityCacheEntry {
                title_id: 59922,
                watched_episodes_cnt: 2,
                total_episodes: Some(3),
                has_available_unwatched_episode: true,
                subtitle_availability,
                episode_availability: Default::default(),
                checked_at_ms: 1000,
            },
        );

        assert!(watching_cache_filter_matches(&item, &filter, &cache));

        let english_filter = WatchingAnimeFilter {
            only_available_unwatched: Some(true),
            check_subtitle_availability_online: Some(true),
            subtitle_language: Some("EN".to_string()),
            ..Default::default()
        };

        assert!(!watching_cache_filter_matches(
            &item,
            &english_filter,
            &cache
        ));
    }

    #[test]
    fn cache_filter_rejects_entry_after_watched_count_changes() {
        let item_after_watching_episode = watching_item(Some("3"), Some(4));
        let filter = WatchingAnimeFilter {
            only_available_unwatched: Some(true),
            check_subtitle_availability_online: Some(true),
            subtitle_language: Some("PL".to_string()),
            ..Default::default()
        };
        let mut subtitle_availability = std::collections::HashMap::new();
        subtitle_availability.insert("pl".to_string(), true);
        let mut cache = WatchingAvailabilityCache::default();
        cache.entries.insert(
            "59922".to_string(),
            WatchingAvailabilityCacheEntry {
                title_id: 59922,
                watched_episodes_cnt: 2,
                total_episodes: Some(4),
                has_available_unwatched_episode: true,
                subtitle_availability,
                episode_availability: Default::default(),
                checked_at_ms: 1000,
            },
        );

        assert!(!watching_cache_filter_matches(
            &item_after_watching_episode,
            &filter,
            &cache
        ));
    }

    #[test]
    fn cache_refresh_plan_queues_entry_after_watched_count_changes() {
        let item_after_watching_episode = watching_item(Some("3"), Some(4));
        let mut subtitle_availability = std::collections::HashMap::new();
        subtitle_availability.insert("pl".to_string(), true);
        let mut cache = WatchingAvailabilityCache::default();
        cache.entries.insert(
            "59922".to_string(),
            WatchingAvailabilityCacheEntry {
                title_id: 59922,
                watched_episodes_cnt: 2,
                total_episodes: Some(4),
                has_available_unwatched_episode: true,
                subtitle_availability,
                episode_availability: Default::default(),
                checked_at_ms: 10_000,
            },
        );

        let plan = collect_watching_cache_refresh_plan(
            &[item_after_watching_episode],
            &cache,
            Some("pl"),
            10_500,
            false,
        );

        assert_eq!(plan.skipped, 0);
        assert_eq!(plan.processed, 0);
        assert_eq!(plan.items_to_scan.len(), 1);
    }

    #[test]
    fn cache_filter_distinguishes_ai_filtered_subtitle_availability() {
        let item = watching_item(Some("2"), Some(3));
        let filter = WatchingAnimeFilter {
            only_available_unwatched: Some(true),
            check_subtitle_availability_online: Some(true),
            subtitle_language: Some("PL".to_string()),
            exclude_ai_subtitles: Some(true),
            ..Default::default()
        };
        let mut subtitle_availability = std::collections::HashMap::new();
        subtitle_availability.insert("pl".to_string(), true);
        let mut cache = WatchingAvailabilityCache::default();
        cache.entries.insert(
            "59922".to_string(),
            WatchingAvailabilityCacheEntry {
                title_id: 59922,
                watched_episodes_cnt: 2,
                total_episodes: Some(3),
                has_available_unwatched_episode: true,
                subtitle_availability,
                episode_availability: Default::default(),
                checked_at_ms: 1000,
            },
        );

        assert!(!watching_cache_filter_matches(&item, &filter, &cache));

        cache
            .entries
            .get_mut("59922")
            .expect("cache entry should exist")
            .subtitle_availability
            .insert("pl:human".to_string(), true);

        assert!(watching_cache_filter_matches(&item, &filter, &cache));
    }

    #[test]
    fn fresh_cache_entry_skips_refresh_only_when_requested_language_is_cached() {
        let item = watching_item(Some("2"), Some(3));
        let mut subtitle_availability = std::collections::HashMap::new();
        subtitle_availability.insert("pl".to_string(), true);
        let entry = WatchingAvailabilityCacheEntry {
            title_id: 59922,
            watched_episodes_cnt: 2,
            total_episodes: Some(3),
            has_available_unwatched_episode: true,
            subtitle_availability,
            episode_availability: [("episode-3".to_string(), WatchingEpisodeAvailability::default())].into_iter().collect(),
            checked_at_ms: 10_000,
        };

        assert!(cache_entry_satisfies_refresh(
            &entry,
            &item,
            Some("pl"),
            10_500,
            false
        ));
        assert!(!cache_entry_satisfies_refresh(
            &entry,
            &item,
            Some("en"),
            10_500,
            false
        ));
        assert!(!cache_entry_satisfies_refresh(
            &entry,
            &item,
            Some("pl"),
            10_500,
            true
        ));
    }

    #[test]
    fn cache_without_episode_snapshots_is_refreshed_even_when_fresh() {
        let item = watching_item(Some("2"), Some(3));
        let mut subtitle_availability = std::collections::HashMap::new();
        subtitle_availability.insert("pl".to_string(), true);
        let entry = WatchingAvailabilityCacheEntry {
            title_id: 59922,
            watched_episodes_cnt: 2,
            total_episodes: Some(3),
            has_available_unwatched_episode: true,
            subtitle_availability,
            episode_availability: Default::default(),
            checked_at_ms: 10_000,
        };

        assert!(!cache_entry_satisfies_refresh(
            &entry,
            &item,
            Some("pl"),
            10_500,
            false
        ));
    }

    #[test]
    fn watching_cache_item_error_message_hides_technical_request_details() {
        let message = watching_cache_item_error_message("Potion Wagami wo Tasukeru");

        assert_eq!(
            message,
            "Nie udalo sie sprawdzic: Potion Wagami wo Tasukeru"
        );
        assert!(!message.contains("https://"));
    }
}
