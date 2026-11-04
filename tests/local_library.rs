use shinden_pl_api::client_backend::EpisodeProgress;
use shinden_pl_api::local_library::{LibraryTitle, LocalLibrary};

fn title(id: u64) -> LibraryTitle {
    LibraryTitle::new(id, "Test", "https://shinden.pl/titles/42")
}

#[test]
fn profiles_isolate_lists_and_guest_is_not_an_account_named_guest() {
    let db = LocalLibrary::open(":memory:").unwrap();
    let guest = db.state().unwrap().active_profile.id;
    db.save_title(&guest, &title(42)).unwrap();
    db.set_status(&guest, 42, "plan", None).unwrap();
    let user = db.activate_account(123, "Guest").unwrap();
    assert_ne!(guest, user.id);
    assert!(db.titles(&user.id, true).unwrap().is_empty());
    db.set_status(&guest, 42, "completed", None).unwrap();
    assert_eq!(
        db.titles(&guest, true).unwrap()[0].item.watch_status,
        "completed"
    );
    assert!(db.titles(&user.id, true).unwrap().is_empty());
}

#[test]
fn episode_progress_is_idempotent_and_preserves_unknown_totals() {
    let db = LocalLibrary::open(":memory:").unwrap();
    let p = db.state().unwrap().active_profile.id;
    db.save_title(&p, &title(42)).unwrap();
    db.save_episodes(
        &p,
        42,
        &[EpisodeProgress {
            title: "1".into(),
            link: "https://shinden.pl/epek/42-test/view/7".into(),
            episode_id: Some(7),
            episode_no: 1,
            watched: false,
            view_count: 0,
            total_episodes: None,
            is_true_final_episode: false,
        }],
    )
    .unwrap();
    db.mark_episode(&p, 42, 7, true).unwrap();
    db.mark_episode(&p, 42, 7, true).unwrap();
    let t = db.title(&p, 42).unwrap().unwrap();
    assert_eq!(t.item.watched_episodes_count, 1);
    assert_eq!(t.item.total_episodes, None);
    assert_ne!(t.item.watch_status, "completed");
    db.save_episodes(
        &p,
        42,
        &[EpisodeProgress {
            title: "1 changed".into(),
            link: "https://shinden.pl/epek/42-test/view/7".into(),
            episode_id: Some(7),
            episode_no: 1,
            watched: false,
            view_count: 0,
            total_episodes: None,
            is_true_final_episode: false,
        }],
    )
    .unwrap();
    assert!(db.episodes(&p, 42).unwrap().unwrap()[0].watched);
    db.mark_episode(&p, 42, 7, false).unwrap();
    assert_eq!(
        db.title(&p, 42)
            .unwrap()
            .unwrap()
            .item
            .watched_episodes_count,
        0
    );
    assert!(db.mark_episode(&p, 42, 999, true).is_err());
}

#[test]
fn import_adds_missing_titles_and_preserves_local_changes() {
    let db = LocalLibrary::open(":memory:").unwrap();
    let p = db.state().unwrap().active_profile.id;
    let mut incoming = title(42);
    incoming.item.watch_status = "plan".into();
    incoming.item.active = true;
    assert_eq!(db.import_titles(&p, &[incoming.clone()]).unwrap(), 1);
    db.set_status(&p, 42, "completed", Some(1)).unwrap();
    assert_eq!(db.import_titles(&p, &[incoming]).unwrap(), 0);
    assert_eq!(
        db.title(&p, 42).unwrap().unwrap().item.watch_status,
        "completed"
    );
}

#[test]
fn settings_and_data_survive_reopening_but_network_is_off_by_default() {
    let path = std::env::temp_dir().join(format!(
        "shinden-library-test-{}-{}.sqlite3",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let db = LocalLibrary::open(&path).unwrap();
        let state = db.state().unwrap();
        assert!(!state.network_enabled);
        assert!(!state.import_enabled);
        db.save_title(&state.active_profile.id, &title(42)).unwrap();
        db.set_preferences(true, true).unwrap();
    }
    {
        let db = LocalLibrary::open(&path).unwrap();
        let state = db.state().unwrap();
        assert!(state.network_enabled);
        assert!(state.import_enabled);
        assert!(db.title(&state.active_profile.id, 42).unwrap().is_some());
        assert!(db.select_profile("../../secret").is_err());
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn a_failed_import_rolls_back_every_insert() {
    let db = LocalLibrary::open(":memory:").unwrap();
    let invalid = title(u64::MAX);
    assert!(db.import_titles("guest", &[title(42), invalid]).is_err());
    assert!(db.titles("guest", false).unwrap().is_empty());
}

#[test]
fn review_metadata_refresh_preserves_release_dates_and_loaded_details() {
    let db = LocalLibrary::open(":memory:").unwrap();
    let mut imported = title(42);
    imported.item.release_year = Some(2025);
    imported.item.release_date = Some("2025-10-01".into());
    imported.item.detail_metadata_loaded = true;
    db.save_title("guest", &imported).unwrap();
    db.save_title("guest", &title(42)).unwrap();
    let saved = db.title("guest", 42).unwrap().unwrap();
    assert_eq!(saved.item.release_year, Some(2025));
    assert_eq!(saved.item.release_date.as_deref(), Some("2025-10-01"));
    assert!(saved.item.detail_metadata_loaded);
}
