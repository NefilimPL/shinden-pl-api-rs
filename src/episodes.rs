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

        let html = self.get_html(&url).await?;
        let doc = Html::parse_document(&html);

        let tbody_selector = Selector::parse("tbody.list-episode-checkboxes").unwrap();
        let tbody_element = doc.select(&tbody_selector)
            .next()
            .context("Could not find tbody element on the page")?;

        let title_selector = Selector::parse(".ep-title").unwrap();
        let button_selector = Selector::parse("a.button.active").unwrap();

        let mut episodes = Vec::new();

        for el in tbody_element.select(&title_selector) {
            episodes.push(Episode {
                title: el.text().collect::<String>(),
                link: String::new(),
            });
        }

        for (i, el) in tbody_element.select(&button_selector).enumerate() {
            if let Some(href) = el.value().attr("href") {
                if let Some(ep) = episodes.get_mut(i) {
                    ep.link = format!("https://shinden.pl{}", href);
                }
            }
        }

        episodes.reverse();
        Ok(episodes)
    }
}

fn episode_page_url(series_url: &str) -> String {
    let without_fragment = series_url.split('#').next().unwrap_or(series_url);
    let base = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment)
        .trim_end_matches('/');
    format!("{base}/episodes")
}

fn requires_series_url_resolution(link: &str) -> bool {
    let Some((_, path)) = link.split_once("/series/") else {
        return false;
    };
    let segment = path.split('/').next().unwrap_or_default().split('?').next().unwrap_or_default();
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
    fn resolves_slugless_series_urls_before_loading_episodes() {
        assert!(requires_series_url_resolution("https://shinden.pl/series/71632"));
        assert!(!requires_series_url_resolution("https://shinden.pl/series/71632-kokoore"));
    }
}
