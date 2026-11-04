use shinden_pl_api::{
    client_backend::ShindenClientBackend,
    local_library::{LibraryTitle, LocalLibrary},
};

#[test]
fn fresh_application_session_starts_as_guest_without_reusing_a_previous_account() {
    let path = std::env::temp_dir().join(format!(
        "shinden-startup-{}-{}.sqlite3",
        std::process::id(),
        shinden_pl_api::local_library::now()
    ));
    {
        let db = LocalLibrary::open(&path).unwrap();
        db.activate_account(7, "Other").unwrap();
        db.save_title(
            "user:7",
            &LibraryTitle::new(42, "Private title", "https://shinden.pl/titles/42-test"),
        )
        .unwrap();
        db.set_status("user:7", 42, "plan", None).unwrap();
    }
    {
        let backend = ShindenClientBackend::with_library_path(&path).unwrap();
        let state = backend.get_library_state().unwrap();
        assert_eq!(state.active_profile.id, "guest");
        assert!(!state.authenticated);
        assert!(state.profiles.iter().any(|p| p.id == "user:7"));
    }
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn startup_and_local_writes_do_not_require_shinden_or_a_session() {
    let backend = ShindenClientBackend::with_library_path(":memory:").unwrap();
    assert!(!backend.get_library_state().unwrap().network_enabled);
    assert_eq!(backend.get_user_name().await.unwrap(), None);
    assert!(
        backend
            .get_user_anime_lists(Some(true))
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert!(backend.search("hello".into()).await.unwrap().is_empty());
    assert!(
        backend
            .get_anime_details("https://shinden.pl/titles/42-test".into())
            .await
            .unwrap_err()
            .contains("Tryb lokalny")
    );
    assert!(
        backend
            .search_public_users("Test".into())
            .await
            .unwrap_err()
            .contains("import")
    );
    backend.logout().await.unwrap();
    assert_eq!(
        backend.get_library_state().unwrap().active_profile.name,
        "Guest"
    );
}

#[tokio::test]
async fn backend_changes_only_the_selected_profile_without_remote_writes() {
    let path = std::env::temp_dir().join(format!(
        "shinden-backend-{}-{}.sqlite3",
        std::process::id(),
        shinden_pl_api::local_library::now()
    ));
    {
        let db = LocalLibrary::open(&path).unwrap();
        db.save_title(
            "guest",
            &LibraryTitle::new(42, "Title", "https://shinden.pl/titles/42-test"),
        )
        .unwrap();
        db.activate_account(7, "Other").unwrap();
        db.save_title(
            "user:7",
            &LibraryTitle::new(42, "Title", "https://shinden.pl/titles/42-test"),
        )
        .unwrap();
        db.set_status("user:7", 42, "plan", None).unwrap();
        db.select_profile("guest").unwrap();
    }
    {
        let backend = ShindenClientBackend::with_library_path(&path).unwrap();
        backend
            .update_anime_status(42, Some("completed".into()), None)
            .await
            .unwrap();
        assert_eq!(
            backend
                .get_user_anime_lists(None)
                .await
                .unwrap()
                .counts
                .completed,
            1
        );
        backend.select_library_profile("user:7".into()).unwrap();
        let payload = backend.get_user_anime_lists(None).await.unwrap();
        assert_eq!(payload.counts.plan, 1);
        assert_eq!(payload.counts.completed, 0);
        assert_eq!(backend.get_user_name().await.unwrap(), None);
    }
    std::fs::remove_file(path).unwrap();
}
