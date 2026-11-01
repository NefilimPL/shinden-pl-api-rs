use crate::{client::ShindenAPI, models::Episode};
use anyhow::{Context, Result};
// Potrzebne do .context()
use scraper::{Html, Selector};

impl ShindenAPI {
    pub async fn get_episodes(&self, link: &str) -> Result<Vec<Episode>> {
        let series_url = if requires_series_url_resolution(link) {
            self.resolve_final_url(link).await?
        } else {
            link.trim().to_string()
        };
        let url = episode_page_url(&series_url);
        match self.get_html(&url).await {
            Ok(html) => {
                if let Some(episodes) = parse_episode_table(&html) {
                    return Ok(episodes);
                }
            }
            Err(error)
                if error.downcast_ref::<reqwest::Error>().is_some_and(|error| {
                    error.status() == Some(reqwest::StatusCode::NOT_FOUND)
                }) => {}
            Err(error) => return Err(error),
        }

        // Shinden has used both routes. A missing table is not an empty list:
        // it may be the title page returned for an unsupported episode route.
        let legacy_url = url.trim_end_matches("/episodes").to_string() + "/all-episodes";
        let html = self.get_html(&legacy_url).await?;
        parse_episode_table(&html)
            .with_context(|| format!("Could not find episode table on {url} or {legacy_url}"))
    }
}

fn parse_episode_table(html: &str) -> Option<Vec<Episode>> {
    let doc = Html::parse_document(&html);

    let tbody_selector = Selector::parse("tbody.list-episode-checkboxes").unwrap();
    let tbody_element = doc.select(&tbody_selector).next()?;

    let row_selector = Selector::parse("tr").unwrap();
    let title_selector = Selector::parse(".ep-title").unwrap();
    let button_selector = Selector::parse("a.button.active").unwrap();

    let mut episodes = Vec::new();

    for row in tbody_element.select(&row_selector) {
        let Some(title) = row.select(&title_selector).next() else {
            continue;
        };
        let link = row
            .select(&button_selector)
            .next()
            .and_then(|button| button.value().attr("href"))
            .map(str::trim)
            .filter(|href| !href.is_empty())
            .and_then(|href| {
                reqwest::Url::parse("https://shinden.pl/")
                    .unwrap()
                    .join(href)
                    .ok()
            })
            .filter(|link| link.scheme() == "https" && link.host_str() == Some("shinden.pl"))
            .map(|link| link.to_string())
            .unwrap_or_default();
        episodes.push(Episode {
            title: title.text().collect::<String>().trim().to_string(),
            link,
        });
    }

    episodes.reverse();
    Some(episodes)
}

fn episode_page_url(series_url: &str) -> String {
    let without_fragment = series_url.split('#').next().unwrap_or(series_url);
    let base = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment)
        .trim_end_matches('/')
        .trim_end_matches("/all-episodes")
        .trim_end_matches("/episodes");
    format!("{base}/episodes")
}

fn requires_series_url_resolution(link: &str) -> bool {
    let path = link
        .split_once("/series/")
        .or_else(|| link.split_once("/titles/"));
    let Some((_, path)) = path else {
        return false;
    };
    let segment = path
        .split('/')
        .next()
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or_default();
    !segment.is_empty() && segment.chars().all(|character| character.is_ascii_digit())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_current_episode_page_url_from_canonical_series_url() {
        assert_eq!(
            episode_page_url("https://shinden.pl/series/71632-kokoore/"),
            "https://shinden.pl/series/71632-kokoore/episodes"
        );
    }

    #[test]
    fn episode_page_urls_do_not_duplicate_an_existing_episode_suffix() {
        for url in [
            "https://shinden.pl/series/71632-kokoore/episodes",
            "https://shinden.pl/series/71632-kokoore/all-episodes/?page=2#list",
        ] {
            assert_eq!(
                episode_page_url(url),
                "https://shinden.pl/series/71632-kokoore/episodes"
            );
        }
    }

    #[test]
    fn resolves_slugless_series_urls_before_loading_episodes() {
        assert!(requires_series_url_resolution(
            "https://shinden.pl/series/71632"
        ));
        assert!(requires_series_url_resolution(
            "https://shinden.pl/titles/68581"
        ));
        assert!(!requires_series_url_resolution(
            "https://shinden.pl/series/71632-kokoore"
        ));
    }
}
