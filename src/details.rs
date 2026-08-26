use crate::client::ShindenAPI;
use anyhow::Result;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AnimeDetails {
    pub title_id: Option<u64>,
    pub title_type: String,
    pub name: String,
    pub alternative_titles: Vec<String>,
    pub image_url: String,
    pub description: String,
    pub information: Vec<AnimeInfoRow>,
    pub categories: Vec<AnimeCategoryGroup>,
    pub related_series: Vec<RelatedSeries>,
    pub community_rating: AnimeCommunityRating,
    pub user_ratings: AnimeUserRatings,
    pub watch_status: String,
    pub is_favourite: u8,
    pub user_status_loaded: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AnimeInfoRow {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AnimeCategoryGroup {
    pub label: String,
    pub items: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RelatedSeries {
    pub name: String,
    pub url: String,
    pub image_url: String,
    pub title_type: String,
    pub relation: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AnimeCommunityRating {
    pub overall: String,
    pub votes: String,
    pub story: String,
    pub graphics: String,
    pub music: String,
    pub characters: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AnimeUserRatings {
    pub story: u8,
    pub graphics: u8,
    pub music: u8,
    pub characters: u8,
    pub overall: u8,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AnimeRatingUpdate {
    pub title_id: u64,
    pub title_type: String,
    pub rating_type: String,
    pub value: u8,
}

impl ShindenAPI {
    pub async fn get_anime_details(&self, url: &str) -> Result<AnimeDetails> {
        let html = self.get_html(url).await?;
        Ok(parse_anime_details_html(&html, url))
    }
}

pub fn parse_anime_details_html(html: &str, page_url: &str) -> AnimeDetails {
    let doc = Html::parse_document(html);

    AnimeDetails {
        title_id: title_id_from_url(page_url),
        title_type: page_media_type(&doc).unwrap_or_else(|| "anime".to_string()),
        name: page_title(&doc),
        alternative_titles: alternative_titles(&doc),
        image_url: cover_image(&doc),
        description: description(&doc),
        information: information_rows(&doc),
        categories: category_groups(&doc),
        related_series: related_series(&doc),
        community_rating: community_rating(&doc),
        user_ratings: user_ratings(&doc),
        watch_status: "no".to_string(),
        is_favourite: 0,
        user_status_loaded: false,
    }
}

fn page_title(doc: &Html) -> String {
    text_from_selector(doc, ".page-title .title")
}

fn page_media_type(doc: &Html) -> Option<String> {
    let selector = Selector::parse(".page-title .kind").ok()?;
    doc.select(&selector)
        .next()
        .map(|item| clean_text(item.text()).to_ascii_lowercase())
        .filter(|value| !value.is_empty())
}

fn alternative_titles(doc: &Html) -> Vec<String> {
    text_from_selector(doc, ".title-other")
        .split(',')
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

fn cover_image(doc: &Html) -> String {
    let selector = Selector::parse(".title-cover img.info-aside-img").unwrap();
    doc.select(&selector)
        .next()
        .and_then(|image| image.value().attr("src"))
        .map(absolute_shinden_url)
        .unwrap_or_default()
}

fn description(doc: &Html) -> String {
    text_from_selector(doc, "#description")
}

fn information_rows(doc: &Html) -> Vec<AnimeInfoRow> {
    let row_selector = Selector::parse(".title-small-info dl.info-aside-list").unwrap();
    let term_selector = Selector::parse("dt").unwrap();
    let value_selector = Selector::parse("dd").unwrap();
    let mut rows = Vec::new();

    for list in doc.select(&row_selector) {
        let labels: Vec<String> = list
            .select(&term_selector)
            .map(|term| clean_text(term.text()))
            .collect();
        let values: Vec<String> = list
            .select(&value_selector)
            .map(|value| clean_text(value.text()))
            .collect();

        for (label, value) in labels.into_iter().zip(values.into_iter()) {
            if !label.is_empty() || !value.is_empty() {
                rows.push(AnimeInfoRow { label, value });
            }
        }
    }

    rows
}

fn category_groups(doc: &Html) -> Vec<AnimeCategoryGroup> {
    let row_selector = Selector::parse(".info-top-table-highlight tr").unwrap();
    let cell_selector = Selector::parse("td").unwrap();
    let tag_selector = Selector::parse("ul.tags a").unwrap();
    let mut groups = Vec::new();

    for row in doc.select(&row_selector) {
        let cells: Vec<_> = row.select(&cell_selector).collect();
        if cells.len() < 2 {
            continue;
        }

        let label = clean_text(cells[0].text());
        let items: Vec<String> = cells[1]
            .select(&tag_selector)
            .map(|tag| clean_text(tag.text()))
            .filter(|tag| !tag.is_empty())
            .collect();

        if !label.is_empty() && !items.is_empty() {
            groups.push(AnimeCategoryGroup { label, items });
        }
    }

    groups
}

fn related_series(doc: &Html) -> Vec<RelatedSeries> {
    let item_selector = Selector::parse("li.relation_t2t figure").unwrap();
    let caption_selector = Selector::parse("figcaption").unwrap();
    let link_selector = Selector::parse("figcaption a").unwrap();
    let image_selector = Selector::parse("img").unwrap();
    let mut series = Vec::new();

    for item in doc.select(&item_selector) {
        let Some(link) = item.select(&link_selector).next() else {
            continue;
        };
        let name = clean_text(link.text());
        let url = link
            .value()
            .attr("href")
            .map(absolute_shinden_url)
            .unwrap_or_default();
        let image_url = item
            .select(&image_selector)
            .next()
            .and_then(|image| image.value().attr("src"))
            .map(absolute_shinden_url)
            .unwrap_or_default();
        let captions: Vec<String> = item
            .select(&caption_selector)
            .map(|caption| clean_text(caption.text()))
            .filter(|value| !value.is_empty() && *value != name)
            .collect();

        if !name.is_empty() {
            series.push(RelatedSeries {
                name,
                url,
                image_url,
                title_type: captions.first().cloned().unwrap_or_default(),
                relation: captions.get(1).cloned().unwrap_or_default(),
            });
        }
    }

    series
}

fn community_rating(doc: &Html) -> AnimeCommunityRating {
    let mut rating = AnimeCommunityRating {
        overall: text_from_selector(doc, ".info-aside-rating-user"),
        votes: String::new(),
        story: String::new(),
        graphics: String::new(),
        music: String::new(),
        characters: String::new(),
    };

    let votes_selector = Selector::parse(".info-aside-rating .h6").unwrap();
    rating.votes = doc
        .select(&votes_selector)
        .next()
        .map(|item| clean_text(item.text()))
        .unwrap_or_default();

    let item_selector = Selector::parse(".info-aside-overall-rating li").unwrap();
    for item in doc.select(&item_selector) {
        let text = clean_text(item.text());
        let value = text
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string();
        if text.contains("Fabu") {
            rating.story = value;
        } else if text.contains("Grafika") {
            rating.graphics = value;
        } else if text.contains("Muzyka") {
            rating.music = value;
        } else if text.contains("Postacie") {
            rating.characters = value;
        }
    }

    rating
}

fn user_ratings(doc: &Html) -> AnimeUserRatings {
    let selector = Selector::parse("#title_rate_edit_container .rateit, x-star-rating").unwrap();
    let mut ratings = AnimeUserRatings::default();

    for item in doc.select(&selector) {
        let rating_type = item.value().attr("data-type").unwrap_or_default();
        let value = item
            .value()
            .attr("data-rateit-value")
            .or_else(|| item.value().attr("value"))
            .or_else(|| item.value().attr("data-value"))
            .and_then(|value| value.parse::<u8>().ok())
            .unwrap_or_default();

        match normalize_rating_type(rating_type).as_deref() {
            Some("story") => ratings.story = value,
            Some("graphics") => ratings.graphics = value,
            Some("music") => ratings.music = value,
            Some("characters") => ratings.characters = value,
            Some("overall") => ratings.overall = value,
            _ => {}
        }
    }

    ratings
}

pub fn normalize_rating_type(rating_type: &str) -> Option<String> {
    let normalized = rating_type.trim().to_ascii_lowercase().replace('-', "_");
    match normalized.as_str() {
        "story" | "plot" | "fabula" | "fabuła" | "fabu" => Some("story".to_string()),
        "graphics" | "graphic" | "grafika" | "visual" => Some("graphics".to_string()),
        "music" | "muzyka" => Some("music".to_string()),
        "characters" | "character" | "postacie" => Some("characters".to_string()),
        "overall" | "general" | "ogolna" | "ogólna" | "rating" => Some("overall".to_string()),
        _ => None,
    }
}

pub fn rating_update_form(update: &AnimeRatingUpdate, auth: &str) -> Vec<(String, String)> {
    vec![
        ("type".to_string(), update.rating_type.clone()),
        ("value".to_string(), update.value.min(10).to_string()),
        ("auth".to_string(), auth.to_string()),
    ]
}

pub fn anime_rating_url(title_type: &str, title_id: u64) -> String {
    format!("https://shinden.pl/api/{title_type}/{title_id}/rate")
}

pub fn basic_auth_token(html: &str) -> Option<String> {
    ["_Storage.basic = \"", "_Storage.basic=\"", "_Storage.basic = '", "_Storage.basic='"]
        .iter()
        .find_map(|marker| {
            let value = html
                .split_once(marker)
                .map(|(_, value)| value)?;
            let quote = marker.chars().last()?;
            let token = value.split(quote).next()?.trim();
            (!token.is_empty()).then(|| token.to_string())
        })
}

fn text_from_selector(doc: &Html, selector: &str) -> String {
    let selector = Selector::parse(selector).unwrap();
    doc.select(&selector)
        .next()
        .map(|item| clean_text(item.text()))
        .unwrap_or_default()
}

fn clean_text<'a>(parts: impl Iterator<Item = &'a str>) -> String {
    parts
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

fn absolute_shinden_url(url: &str) -> String {
    let trimmed = url.trim();
    if trimmed.starts_with("https://") || trimmed.starts_with("http://") {
        trimmed.to_string()
    } else if trimmed.starts_with("//") {
        format!("https:{trimmed}")
    } else if trimmed.starts_with('/') {
        format!("https://shinden.pl{trimmed}")
    } else {
        format!("https://shinden.pl/{trimmed}")
    }
}

fn title_id_from_url(url: &str) -> Option<u64> {
    let marker = "/series/";
    let start = url.find(marker)? + marker.len();
    let id = url[start..]
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect::<String>();
    id.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_anime_details_from_shinden_title_html() {
        let html = r#"
            <h1 class="page-title anime kind-tv" data-t="Himekishi" data-tt="tv">
                <span class="kind">Anime</span><span class="title">Himekishi wa Barbaroi no Yome</span>
            </h1>
            <div class="title-other parent-on-hover">姫騎士は蛮族の嫁, The Barbarian's Bride,</div>
            <section class="title-cover">
                <a href="/res/images/genuine/123.jpg"><img class="info-aside-img" src="/res/images/225x350/123.jpg" /></a>
            </section>
            <section class="title-rates">
                <div class="media info-aside-rating">
                    <div class="bd"><h3><span class="info-aside-rating-user">7,96</span>/10</h3><span class="h6">114 głosów</span></div>
                </div>
                <ul class="info-aside-overall-rating">
                    <li>7,4 <span>Fabuła</span></li>
                    <li>8,1 <span>Grafika</span></li>
                    <li>7,8 <span>Muzyka</span></li>
                    <li>8,0 <span>Postacie</span></li>
                </ul>
            </section>
            <section class="title-small-info">
                <dl class="info-aside-list"><dt>Typ:</dt><dd>TV</dd><dt>Status:</dt><dd>Emitowane</dd></dl>
            </section>
            <article class="page-content page-anime-index">
                <section class="info-top">
                    <div id="description" class="title-full-description"><p>Serafina de Lavillant została wysłana na Wschód.</p></div>
                    <table><tbody class="info-top-table-highlight">
                        <tr><td>Gatunki:</td><td><ul class="tags"><li><a href="/genre/1">Komedia</a></li><li><a>Fantasy</a></li></ul></td></tr>
                        <tr><td>Pierwowzór:</td><td><ul class="tags"><li><a>Manga</a></li></ul></td></tr>
                    </tbody></table>
                </section>
                <section class="box">
                    <h2>Powiązane Serie</h2>
                    <ul><li class="relation_t2t"><figure>
                        <figcaption><a href="/series/111-related" title="Related">Related</a></figcaption>
                        <figcaption class="figure-type">Anime</figcaption>
                        <img src="/res/images/225x350/555.jpg" />
                        <figcaption class="figure-type">Prequel</figcaption>
                    </figure></li></ul>
                </section>
                <div id="title_rate_edit_container">
                    <div class="rateit" data-type="story" data-rateit-value="8"></div>
                    <div class="rateit" data-type="graphics" data-rateit-value="7"></div>
                    <div class="rateit" data-type="music" data-rateit-value="6"></div>
                    <div class="rateit" data-type="characters" data-rateit-value="9"></div>
                    <div class="rateit" data-type="overall" data-rateit-value="8"></div>
                </div>
            </article>
        "#;

        let details = parse_anime_details_html(
            html,
            "https://shinden.pl/series/68452-himekishi-wa-barbaroi-no-yome",
        );

        assert_eq!(details.title_id, Some(68452));
        assert_eq!(details.title_type, "anime");
        assert_eq!(details.name, "Himekishi wa Barbaroi no Yome");
        assert_eq!(
            details.alternative_titles,
            vec!["姫騎士は蛮族の嫁", "The Barbarian's Bride"]
        );
        assert_eq!(
            details.image_url,
            "https://shinden.pl/res/images/225x350/123.jpg"
        );
        assert_eq!(
            details.description,
            "Serafina de Lavillant została wysłana na Wschód."
        );
        assert_eq!(details.information[1].label, "Status:");
        assert_eq!(details.categories[0].label, "Gatunki:");
        assert_eq!(details.categories[0].items, vec!["Komedia", "Fantasy"]);
        assert_eq!(details.related_series[0].relation, "Prequel");
        assert_eq!(details.community_rating.overall, "7,96");
        assert_eq!(details.community_rating.story, "7,4");
        assert_eq!(details.user_ratings.story, 8);
        assert_eq!(details.user_ratings.overall, 8);
    }

    #[test]
    fn rating_update_form_clamps_values_and_keeps_category() {
        let update = AnimeRatingUpdate {
            title_id: 68452,
            title_type: "anime".to_string(),
            rating_type: "graphics".to_string(),
            value: 12,
        };

        assert_eq!(
            rating_update_form(&update, "token"),
            vec![
                ("type".to_string(), "graphics".to_string()),
                ("value".to_string(), "10".to_string()),
                ("auth".to_string(), "token".to_string()),
            ]
        );
        assert_eq!(
            anime_rating_url(&update.title_type, update.title_id),
            "https://shinden.pl/api/anime/68452/rate"
        );
    }

    #[test]
    fn basic_auth_token_extracts_storage_value() {
        let html = "<script>_Storage.basic = 'abc123';</script>";

        assert_eq!(basic_auth_token(html).as_deref(), Some("abc123"));
    }

    #[test]
    fn basic_auth_token_extracts_double_quoted_storage_value() {
        let html = r#"<script>_Storage.basic = "current-token";</script>"#;

        assert_eq!(basic_auth_token(html).as_deref(), Some("current-token"));
    }

    #[test]
    fn anime_details_serializes_default_user_status() {
        let details = parse_anime_details_html(
            r#"
                <h1 class="page-title anime kind-tv" data-tt="tv">
                    <span class="kind">Anime</span><span class="title">Mobseka</span>
                </h1>
            "#,
            "https://shinden.pl/series/59211-otomege-sekai-wa-mob-ni-kibishii-sekai-desu",
        );

        let value = serde_json::to_value(details).unwrap();

        assert_eq!(value["watchStatus"].as_str(), Some("no"));
        assert_eq!(value["isFavourite"].as_u64(), Some(0));
        assert_eq!(value["userStatusLoaded"].as_bool(), Some(false));
    }
}
