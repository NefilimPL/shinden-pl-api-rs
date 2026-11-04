use reqwest::Url;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PublicUser {
    pub id: u64,
    pub name: String,
    pub url: String,
    pub image_url: Option<String>,
}

pub fn profile_from_url(source: &str) -> Result<PublicUser, String> {
    let url = Url::parse(source.trim())
        .map_err(|_| "Wklej pełny adres https://shinden.pl/user/ID-nick")?;
    if url.scheme() != "https"
        || url.host_str() != Some("shinden.pl")
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "Dozwolone są tylko bezpośrednie adresy profili https://shinden.pl/user/ID-nick".into(),
        );
    }
    let path = url
        .path()
        .trim_end_matches('/')
        .strip_prefix("/user/")
        .ok_or("Nieprawidłowy adres profilu Shinden")?;
    if path.contains('/') {
        return Err("Nieprawidłowy adres profilu Shinden".into());
    }
    let (number, slug) = path
        .split_once('-')
        .ok_or("Adres profilu musi zawierać ID i nick")?;
    let id = number
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0 && *id <= i64::MAX as u64)
        .ok_or("Nieprawidłowe ID profilu")?;
    if slug.is_empty() {
        return Err("Brak nicku w adresie profilu".into());
    }
    Ok(PublicUser {
        id,
        name: slug.into(),
        url: format!("https://shinden.pl/user/{path}"),
        image_url: None,
    })
}

pub fn parse_public_users(html: &str) -> Vec<PublicUser> {
    let doc = Html::parse_document(html);
    let mut users = BTreeMap::new();
    for link in doc.select(&Selector::parse("a[href]").unwrap()) {
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        let Ok(url) = Url::parse("https://shinden.pl/").unwrap().join(href) else {
            continue;
        };
        let Ok(mut user) = profile_from_url(url.as_str()) else {
            continue;
        };
        let name = link
            .text()
            .collect::<Vec<_>>()
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if !name.is_empty() {
            user.name = name;
        }
        user.image_url = link
            .select(&Selector::parse("img[src]").unwrap())
            .next()
            .and_then(|img| img.value().attr("src"))
            .and_then(crate::images::normalize_image_url)
            .map(|u| u.to_string());
        users
            .entry(user.id)
            .and_modify(|old: &mut PublicUser| {
                if old.name == old.url.rsplit('-').next().unwrap_or_default()
                    || user.name != user.url.rsplit('-').next().unwrap_or_default()
                {
                    old.name = user.name.clone();
                }
                if old.image_url.is_none() {
                    old.image_url = user.image_url.clone();
                }
            })
            .or_insert(user);
    }
    users.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn profile_urls_cannot_target_other_origins_or_routes() {
        assert_eq!(
            profile_from_url("https://shinden.pl/user/458357-nefilimpl")
                .unwrap()
                .id,
            458357
        );
        for url in [
            "http://shinden.pl/user/1-a",
            "https://shinden.pl.evil.test/user/1-a",
            "https://user:pass@shinden.pl/user/1-a",
            "https://shinden.pl:444/user/1-a",
            "https://shinden.pl/user/1-a/extra",
            "https://shinden.pl/user/0-a",
            "https://shinden.pl/user/1-a?url=http://evil.test",
        ] {
            assert!(profile_from_url(url).is_err(), "{url}");
        }
    }
    #[test]
    fn user_search_reads_links_and_deduplicates_profiles() {
        let users = parse_public_users(
            r#"<a href="/user/458357-nefilimpl"><img src="/avatar.jpg"></a><a href="/user/458357-nefilimpl">NefilimPL</a><a href="https://evil.test/user/42-fake">Fake</a>"#,
        );
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].name, "NefilimPL");
        assert_eq!(
            users[0].image_url.as_deref(),
            Some("https://shinden.pl/avatar.jpg")
        );
    }
}
