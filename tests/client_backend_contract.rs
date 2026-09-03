use shinden_pl_api::client_backend::{
    DiscoveryAnime, EpisodeProgress, ShindenClientBackend, UserAnimeListCounts, UserAnimeListItem,
    UserAnimeListRefreshStatus, UserAnimeListRefreshSummary, UserAnimeListsPayload, WatchingAnime,
    WatchingAnimeFilter,
};
use shinden_pl_api::models::{SearchFilterRequest, SearchTagSelection};

#[test]
fn backend_can_be_constructed_without_network_access() {
    let backend = ShindenClientBackend::new();

    assert!(backend.is_ok());
}

#[test]
fn frontend_contract_types_keep_expected_json_shape() {
    let watching = WatchingAnime {
        title_id: 59922,
        name: "Enen no Shouboutai".to_string(),
        url: "https://shinden.pl/series/59922".to_string(),
        image_url: "https://cdn.shinden.eu/cdn1/images/genuine/59922.jpg".to_string(),
        anime_type: "TV".to_string(),
        rating: "8.10".to_string(),
        episodes: "2 / 12".to_string(),
        description: "Fire force".to_string(),
        watch_status: "in progress".to_string(),
        is_favourite: 1,
        watched_episodes_count: 2,
        total_episodes: Some(12),
    };
    let watching_json = serde_json::to_value(watching).expect("watching anime serializes");

    assert_eq!(watching_json["titleId"], 59922);
    assert_eq!(watching_json["watchStatus"], "in progress");
    assert_eq!(watching_json["isFavourite"], 1);
    assert_eq!(watching_json["watchedEpisodesCount"], 2);
    assert_eq!(watching_json["totalEpisodes"], 12);

    let discovery = DiscoveryAnime {
        name: "Season show".to_string(),
        url: "https://shinden.pl/series/60001".to_string(),
        image_url: "https://cdn.shinden.eu/cdn1/images/genuine/60001.jpg".to_string(),
        anime_type: "TV".to_string(),
        rating: "7.50".to_string(),
        episodes: "12".to_string(),
        description: "Season entry".to_string(),
        title_id: Some(60001),
        watch_status: "completed".to_string(),
        is_favourite: 0,
        total_episodes: Some(12),
        source_label: Some("Nowe".to_string()),
    };
    let discovery_json = serde_json::to_value(discovery).expect("discovery anime serializes");

    assert_eq!(discovery_json["titleId"], 60001);
    assert_eq!(discovery_json["watchStatus"], "completed");
    assert_eq!(discovery_json["isFavourite"], 0);
    assert_eq!(discovery_json["totalEpisodes"], 12);
    assert_eq!(discovery_json["sourceLabel"], "Nowe");

    let progress = EpisodeProgress {
        title: "Episode 2".to_string(),
        link: "https://shinden.pl/episode/2".to_string(),
        episode_id: Some(168519),
        episode_no: 2,
        watched: true,
        view_count: 1,
        total_episodes: Some(12),
        is_true_final_episode: false,
    };
    let progress_json = serde_json::to_value(progress).expect("episode progress serializes");

    assert_eq!(progress_json["episodeId"], 168519);
    assert_eq!(progress_json["episodeNo"], 2);
    assert_eq!(progress_json["viewCount"], 1);
    assert_eq!(progress_json["totalEpisodes"], 12);
    assert_eq!(progress_json["isTrueFinalEpisode"], false);

    let filter = WatchingAnimeFilter {
        only_available_unwatched: Some(true),
        subtitle_language: Some("PL".to_string()),
        check_subtitle_availability_online: Some(true),
        exclude_ai_subtitles: Some(true),
    };
    let filter_json = serde_json::to_value(filter).expect("watching filter serializes");

    assert_eq!(filter_json["onlyAvailableUnwatched"], true);
    assert_eq!(filter_json["subtitleLanguage"], "PL");
    assert_eq!(filter_json["checkSubtitleAvailabilityOnline"], true);
    assert_eq!(filter_json["excludeAiSubtitles"], true);

    let user_list_item = UserAnimeListItem {
        title_id: 59922,
        name: "Enen no Shouboutai".to_string(),
        url: "https://shinden.pl/series/59922".to_string(),
        image_url: "https://cdn.shinden.eu/cdn1/images/genuine/59922.jpg".to_string(),
        anime_type: "TV".to_string(),
        rating: "8.10".to_string(),
        episodes: "2/12".to_string(),
        description: "Fire force".to_string(),
        watch_status: "in progress".to_string(),
        is_favourite: 1,
        watched_episodes_count: 2,
        total_episodes: Some(12),
        release_year: Some(2025),
        tags: vec!["Komedia".to_string(), "Fantasy".to_string()],
        age_rating: Some("R17+".to_string()),
        detail_metadata_loaded: true,
        active: true,
        updated_at_ms: 10,
    };
    let payload = UserAnimeListsPayload {
        items: vec![user_list_item],
        counts: UserAnimeListCounts {
            in_progress: 1,
            completed: 0,
            skip: 0,
            hold: 0,
            dropped: 0,
            plan: 0,
            all: 1,
        },
        refreshed_at_ms: Some(10),
        sync_error: None,
    };
    let json = serde_json::to_value(payload).expect("payload serializes");

    assert_eq!(json["items"][0]["titleId"], 59922);
    assert_eq!(json["items"][0]["watchStatus"], "in progress");
    assert_eq!(json["items"][0]["releaseYear"], 2025);
    assert_eq!(json["items"][0]["tags"][0], "Komedia");
    assert_eq!(json["items"][0]["ageRating"], "R17+");
    assert_eq!(json["counts"]["inProgress"], 1);
    assert_eq!(json["counts"]["all"], 1);

    let refresh_status = UserAnimeListRefreshStatus {
        running: true,
        current: 2,
        total: 5,
        remaining: 3,
        refreshed: 2,
        failed: 0,
        current_title: "Season show".to_string(),
        last_finished_at_ms: None,
        last_error: None,
    };
    let refresh_summary = UserAnimeListRefreshSummary {
        status: refresh_status,
        already_running: true,
    };
    let refresh_json =
        serde_json::to_value(refresh_summary).expect("user list refresh summary serializes");

    assert_eq!(refresh_json["alreadyRunning"], true);
    assert_eq!(refresh_json["status"]["running"], true);
    assert_eq!(refresh_json["status"]["current"], 2);
    assert_eq!(refresh_json["status"]["total"], 5);
    assert_eq!(refresh_json["status"]["remaining"], 3);
    assert_eq!(refresh_json["status"]["currentTitle"], "Season show");
}

#[test]
fn filtered_search_request_keeps_only_public_tag_selection_data() {
    let request = SearchFilterRequest {
        query: "Cowboy Bebop".to_string(),
        tags: vec![SearchTagSelection::include(5), SearchTagSelection::exclude(39)],
        genres_type: "all".to_string(),
        letter: None,
        page: 1,
    };

    let json = serde_json::to_value(request).expect("filtered search request serializes");

    assert_eq!(json["query"], "Cowboy Bebop");
    assert_eq!(json["genresType"], "all");
    assert_eq!(json["page"], 1);
    assert_eq!(json["tags"][0]["tagId"], 5);
    assert_eq!(json["tags"][0]["mode"], "include");
    assert_eq!(json["tags"][1]["mode"], "exclude");
    assert!(json["tags"][0].get("formName").is_none());
}
