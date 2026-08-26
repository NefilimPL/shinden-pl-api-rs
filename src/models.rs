use serde::{Deserialize, Serialize};
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Anime {
    pub name: String,
    pub url: String,
    pub image_url: String,
    pub anime_type: String,
    pub rating: String,
    pub episodes: String,
    pub description: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct SearchFilterCatalog {
    pub groups: Vec<SearchTagGroup>,
    pub letters: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SearchTagGroup {
    pub id: String,
    pub label: String,
    pub options: Vec<SearchTagOption>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SearchTagOption {
    pub id: u64,
    pub label: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SearchTagSelection {
    pub tag_id: u64,
    pub mode: SearchTagSelectionMode,
}

impl SearchTagSelection {
    pub fn include(tag_id: u64) -> Self {
        Self { tag_id, mode: SearchTagSelectionMode::Include }
    }

    pub fn exclude(tag_id: u64) -> Self {
        Self { tag_id, mode: SearchTagSelectionMode::Exclude }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SearchTagSelectionMode {
    Include,
    Exclude,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct SearchFilterRequest {
    pub query: String,
    #[serde(default)]
    pub tags: Vec<SearchTagSelection>,
    #[serde(default = "default_search_genres_type")]
    pub genres_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub letter: Option<String>,
    #[serde(default = "default_search_page")]
    pub page: u32,
}

fn default_search_genres_type() -> String {
    "all".to_string()
}

fn default_search_page() -> u32 {
    1
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SearchResultsPage {
    pub items: Vec<Anime>,
    pub current_page: u32,
    pub total_pages: u32,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Episode {
    pub title: String,
    pub link: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Player {
    pub player: String,
    pub max_res: String,
    pub lang_audio: String,
    pub lang_subs: String,
    pub online_id: String,
}
