use crate::{client::ShindenAPI, details::basic_auth_token};
use anyhow::{anyhow, Result};
use reqwest::Url;
use scraper::{Html, Selector};
use std::time::Duration;
use tokio::time::sleep;


impl ShindenAPI {
    pub async fn get_player_iframe(&self, online_id: &str) -> Result<String> {
        let main_html = self.get_html("https://shinden.pl/main").await?;
        let auth = basic_auth_token(&main_html)
            .ok_or_else(|| anyhow!("Shinden player auth token missing"))?;
        let url1 = player_load_url(online_id, &auth);
        let url2 = player_show_url(online_id, &auth);

        let _ = self.get_html(&url1).await?;
        sleep(Duration::from_secs(5)).await;
        let html = self.get_html(&url2).await?;

        let doc = Html::parse_document(&html);
        let iframe = doc.select(&Selector::parse("iframe").unwrap()).next();

        Ok(iframe.map(|i| i.html()).unwrap_or_default())
    }
}

fn player_load_url(online_id: &str, auth: &str) -> String {
    player_request_url(online_id, "player_load", auth, false)
}

fn player_show_url(online_id: &str, auth: &str) -> String {
    player_request_url(online_id, "player_show", auth, true)
}

fn player_request_url(online_id: &str, action: &str, auth: &str, include_dimensions: bool) -> String {
    let mut url = Url::parse(&format!("https://api4.shinden.pl/xhr/{online_id}/{action}"))
        .expect("static Shinden player URL is valid");
    let mut query = url.query_pairs_mut();
    query.append_pair("auth", auth);
    if include_dimensions {
        query.append_pair("width", "0");
        query.append_pair("height", "-1");
    }
    drop(query);
    url.into()
}

#[cfg(test)]
mod tests {
    use super::player_show_url;

    #[test]
    fn builds_the_player_show_request_with_the_fresh_auth_token() {
        assert_eq!(
            player_show_url("1895906", "current-token"),
            "https://api4.shinden.pl/xhr/1895906/player_show?auth=current-token&width=0&height=-1",
        );
    }
}

