# Shinden.pl API Client (Rust)

A simple and blazingly fast API client for the Shinden.pl service, written in Rust. 
This project enables programmatic interaction with the service, 
such as logging in, fetching episode lists, searching for anime, and retrieving video player links, 
all while leveraging Rust's performance and safety guarantees.

## Features
- User Authentication: Handles session management and user login.
- Episode Listing: Extracts titles and links to individual episode pages.
- Player Retrieval: Obtains data about available video players for a given episode (e.g., Mega.nz, CDA.pl iframes).
- Cookie Management: Persistently stores and manages session cookies.

## Getting Started

To use this library in your Rust project, add it as a dependency in your Cargo.toml file:

```toml
[dependencies]
shinden-pl-api = "0.1.5"
```

Alternatively, you can add it directly via the Cargo CLI:
Bash

```sh
cargo add shinden-pl-api
```

### Example
```rust
use anyhow::Result;
use shinden_pl_api::client::ShindenAPI;
use shinden_pl_api::models::{Episode, Player};

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Initialize the API client.
    // This client manages cookies and sets appropriate default headers.
    let client = ShindenAPI::new()?;

    // 2. Log in to the service.
    // Replace with your actual login credentials.
    let _ = client.login("example@example.com", "example"); // Placeholder credentials

    let series_url = "https://shinden.pl/series/12434-hunter-x-hunter-2011";

    println!("Fetching episodes for series: {}", series_url);

    // 3. Fetch the list of episodes for the specified series.
    let episodes: Vec<Episode> = client.get_episodes(series_url).await?;
    println!("Found {} episodes:", episodes.len());

    for ep in &episodes {
        println!(" - {} [{}]", ep.title, ep.link);
    }

    // 4. If the episode list isn't empty, fetch players for the first episode.
    if let Some(first_episode) = episodes.first() {
        println!("\nFetching players for first episode: {}", first_episode.link);
        // `get_players` returns a list of available players on the episode page.
        let players: Vec<Player> = client.get_players(&first_episode.link).await?;
        println!("Found {} players:", players.len());

        // 5. Retrieve the specific player iframe (e.g., the first one from the list).
        // `online_id` is the player identifier you need to pass.
        let player_iframe_html = client.get_player_iframe(&players[0].online_id).await?;

        // Print the full <iframe> tag, which you can then embed in your frontend.
        println!("{}", player_iframe_html);
    }

    Ok(())
}
```

## Local application backend

`client_backend::ShindenClientBackend` uses a SQLite library as its source of
truth. `new()` opens the OS local-data file
`shinden-client/library.sqlite3`; `with_library_path(path)` supports a custom
file or `:memory:` for tests. `SHINDEN_LIBRARY_PATH` overrides the default.
SQLite is bundled; no database service is needed.

Every new application session starts in Guest. Account profiles have stable
internal keys derived from their public numeric profile IDs and display their
nicknames. Every query and resource belongs to a profile. Local status, rating
and watched-episode updates never POST to Shinden. Existing profiles can be
selected locally without authenticating. Logout selects Guest.

Network access and public-list import are disabled by default. Use
`set_library_preferences(network_enabled, import_enabled)` to opt in, then
`search_public_users(query)` and `import_public_user(profile_url)` to import
public lists into the profile captured when the import starts. Import validates
all pages and commits additions in one transaction; existing local progress is
preserved. Metadata and playback links are saved when fetched. Explicit library
refresh downloads descriptions, categories and episodes; explicit availability
refresh checks players. There are no automatic background scans.

Public requests use a separate ephemeral anonymous `ShindenAPI` client. Explicit
browser login activates an ephemeral account client for protected playback.
Disabling networking invalidates permission shared by all clients, and replacing
an account session revokes its clones. The request checks also apply after a
player preparation delay and before redirects. Requests already in flight may
finish. HTTP 403/429 and the known Shinden block page disable subsequent requests.

These guarantees apply to `ShindenClientBackend`. Direct `ShindenAPI` methods
remain remote operations; `ShindenAPI::new()` retains its existing persistent
cookie behavior for library consumers. `ShindenAPI::anonymous()` neither loads
nor writes account cookies. Neither local storage nor anonymous HTTP requests
guarantee that a remote service cannot recognize an IP or a client.

## Security scanning

Shinden's list API requires the public numeric profile identifier from
`/animelist/<id>-<slug>` links in its HTTPS request paths. Internally this value is
named `public_profile_id` to distinguish it from credentials and session tokens.
CodeQL's `rust/cleartext-transmission` rule uses name-based sensitive-data
heuristics and can flag generic `user_id` values in URLs even over HTTPS.
See the [CodeQL rule documentation](https://codeql.github.com/codeql-query-help/rust/rust-cleartext-transmission/).

# LICENSE
This project is licensed under the MIT License.
