use crate::{
    client::ShindenAPI,
    models::{
        Anime, SearchFilterCatalog, SearchFilterRequest, SearchResultsPage, SearchTagGroup, SearchTagOption,
        SearchTagSelection, SearchTagSelectionMode,
    },
};
use anyhow::Result;
use reqwest::Url;
use scraper::{Html, Selector};

impl ShindenAPI {
    pub async fn search_anime(&self, name: &str) -> Result<Vec<Anime>> {
        self.search_anime_with_filters(&SearchFilterRequest {
            query: name.to_string(),
            ..Default::default()
        }).await.map(|page| page.items)
    }

    pub async fn get_search_filter_catalog(&self) -> Result<SearchFilterCatalog> {
        let html = self.get_html("https://shinden.pl/series?").await?;
        Ok(parse_search_filter_catalog_html(&html))
    }

    pub async fn search_anime_with_filters(&self, request: &SearchFilterRequest) -> Result<SearchResultsPage> {
        if !request.tags.is_empty() {
            let catalog = self.get_search_filter_catalog().await?;
            let available_tags = catalog
                .groups
                .iter()
                .flat_map(|group| group.options.iter().map(|option| option.id))
                .collect::<std::collections::HashSet<_>>();
            if request
                .tags
                .iter()
                .any(|tag| !available_tags.contains(&tag.tag_id))
            {
                anyhow::bail!("Search request contains a tag that is not available in Shinden filters");
            }
        }

        if let Some(letter) = request.letter.as_deref() {
            let catalog = self.get_search_filter_catalog().await?;
            if !catalog.letters.iter().any(|available| available == letter) {
                anyhow::bail!("Search request contains a letter that is not available in Shinden filters");
            }
        }

        let mut search_url = Url::parse("https://shinden.pl/series")?;
        {
            let mut query = search_url.query_pairs_mut();
            query.append_pair("type", "contains");
            query.append_pair("search", request.query.trim());
            query.append_pair("page", &request.page.max(1).to_string());
            if !request.tags.is_empty() {
                query.append_pair("genres-type", search_genres_type(&request.genres_type));
                query.append_pair("genres", &encode_search_genres(&request.tags));
            }
            if let Some(letter) = request.letter.as_deref() {
                query.append_pair("letter", letter);
            }
        }
        let html = self.get_html(search_url.as_str()).await?;

        Ok(parse_search_results_page_html(&html, request.page))
    }

}

fn parse_search_results_page_html(html: &str, requested_page: u32) -> SearchResultsPage {
    let doc = Html::parse_document(html);
    let page_links = Selector::parse("a[href*='page=']").expect("valid page selector");
    let current_page = requested_page.max(1);
    let total_pages = doc
        .select(&page_links)
        .filter_map(|link| page_number_from_href(link.value().attr("href")?))
        .max()
        .unwrap_or(current_page)
        .max(current_page);

    SearchResultsPage {
        items: parse_search_results_html(html),
        current_page,
        total_pages,
    }
}

fn page_number_from_href(href: &str) -> Option<u32> {
    Url::parse("https://shinden.pl/")
        .ok()?
        .join(href)
        .ok()?
        .query_pairs()
        .find_map(|(key, value)| (key == "page").then(|| value.parse().ok()))?
}

fn parse_search_results_html(html: &str) -> Vec<Anime> {

        let doc = Html::parse_document(html);
        let div_row = Selector::parse(".div-row").unwrap();
        let h3 = Selector::parse("h3").unwrap();
        let a = Selector::parse("a").unwrap();
        let cover = Selector::parse(".cover-col a").unwrap();
        let kind = Selector::parse(".title-kind-col").unwrap();
        let episodes = Selector::parse(".episodes-col").unwrap();
        let rating = Selector::parse(".rate-top").unwrap();

        let mut result = Vec::new();

        for div in doc.select(&div_row) {
            let name_elem = div.select(&h3).next().and_then(|h| h.select(&a).next());
            let name = name_elem
                .map(|el| el.text().collect::<String>())
                .unwrap_or_default();
            let url = name_elem
                .and_then(|el| el.value().attr("href"))
                .unwrap_or("")
                .to_string();
            let img_href = div
                .select(&cover)
                .next()
                .and_then(|el| el.value().attr("href"))
                .unwrap_or("/res/other/placeholders/title/100x100.jpg");

            let full_url = format!("https://shinden.pl{}", url);
            let img_url = format!("https://shinden.pl{}", img_href);
            let anime_type = div
                .select(&kind)
                .next()
                .map(|k| k.text().collect::<String>())
                .unwrap_or_default();
            let ep_count = div
                .select(&episodes)
                .next()
                .map(|e| e.text().collect::<String>())
                .unwrap_or_default()
                .trim()
                .to_string();
            let rate = div
                .select(&rating)
                .next()
                .map(|r| r.text().collect::<String>())
                .unwrap_or_default();

            if !name.is_empty() {
                result.push(Anime {
                    name,
                    url: full_url,
                    image_url: img_url,
                    anime_type,
                    rating: rate,
                    episodes: ep_count,
                    description: String::new(),
                });
            }
        }

        result
}

fn parse_search_filter_catalog_html(html: &str) -> SearchFilterCatalog {
    let doc = Html::parse_document(html);
    let tabs = Selector::parse(".search-items-tabs a[id]").expect("valid tab selector");
    let options = Selector::parse("a.genre-item[data-id]").expect("valid tag option selector");
    let letters = Selector::parse("#TabLetters a[href*='letter=']").expect("valid letter selector");

    let groups = doc.select(&tabs).filter_map(|tab| {
        let tab_id = tab.value().attr("id")?;
        let group_id = tab_id.strip_prefix("goTab")?.to_ascii_lowercase();
        let group_selector = Selector::parse(&format!("#Tab{}", &tab_id[5..])).ok()?;
        let group = doc.select(&group_selector).next()?;
        let options = group.select(&options).filter_map(|option| {
            let id = option.value().attr("data-id")?.parse::<u64>().ok()?;
            let label = option.text().collect::<String>().trim().to_string();
            (!label.is_empty()).then_some(SearchTagOption { id, label })
        }).collect::<Vec<_>>();

        (!options.is_empty()).then_some(SearchTagGroup {
            id: group_id,
            label: tab.text().collect::<String>().trim().to_string(),
            options,
        })
    }).collect();

    let letters = doc
        .select(&letters)
        .filter_map(|letter| {
            let href = letter.value().attr("href")?;
            let value = Url::parse("https://shinden.pl/").ok()?.join(href).ok()?
                .query_pairs()
                .find_map(|(key, value)| (key == "letter").then_some(value.into_owned()))?;
            (!value.is_empty()).then_some(value)
        })
        .collect();

    SearchFilterCatalog { groups, letters }
}

fn encode_search_genres(tags: &[SearchTagSelection]) -> String {
    tags.iter().map(|tag| {
        let mode = match tag.mode {
            SearchTagSelectionMode::Include => 'i',
            SearchTagSelectionMode::Exclude => 'e',
        };
        format!("{mode}{}", tag.tag_id)
    }).collect::<Vec<_>>().join(";")
}

fn search_genres_type(value: &str) -> &str {
    if value == "one" { "one" } else { "all" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_included_and_excluded_tags_for_shinden() {
        let genres = encode_search_genres(&[
            SearchTagSelection::include(5),
            SearchTagSelection::include(1741),
            SearchTagSelection::include(92),
            SearchTagSelection::exclude(39),
        ]);

        assert_eq!(genres, "i5;i1741;i92;e39");
    }

    #[test]
    fn parses_tag_groups_from_the_series_search_form() {
        let html = r#"
            <form method="GET" action="/series" class="search-form">
                <ul class="tabs search-items-tabs"><li><a id="goTabGenres">Gatunki</a></li><li><a id="goTabtarget_group">Grupy docelowe</a></li></ul>
                <ul id="TabLetters"><li><a href="?letter=A&amp;page=1">A</a></li><li><a href="?letter=1&amp;page=1">#</a></li></ul>
                <div id="TabGenres"><ul class="genre-list"><li><a class="genre-item" data-id="5">Akcja</a></li></ul></div>
                <div id="Tabtarget_group"><ul class="genre-list"><li><a class="genre-item" data-id="39">Josei</a></li></ul></div>
            </form>
        "#;

        let catalog = parse_search_filter_catalog_html(html);

        assert_eq!(catalog.groups.len(), 2);
        assert_eq!(catalog.groups[0].id, "genres");
        assert_eq!(catalog.groups[0].label, "Gatunki");
        assert_eq!(catalog.groups[0].options[0].id, 5);
        assert_eq!(catalog.groups[1].id, "target_group");
        assert_eq!(catalog.groups[1].options[0].label, "Josei");
        assert_eq!(catalog.letters, vec!["A", "1"]);
    }

    #[test]
    fn parses_the_requested_page_and_last_page_from_search_pagination() {
        let html = r#"
            <div class="div-row"><h3><a href="/series/1-alpha">Alpha</a></h3></div>
            <nav class="pagination">
                <a href="/series?search=alpha&amp;page=2">2</a>
                <span class="active">3</span>
                <a href="/series?search=alpha&amp;page=8">8</a>
            </nav>
        "#;

        let page = parse_search_results_page_html(html, 3);

        assert_eq!(page.current_page, 3);
        assert_eq!(page.total_pages, 8);
        assert_eq!(page.items.len(), 1);
    }
}


