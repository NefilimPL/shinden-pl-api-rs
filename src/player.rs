use crate::{client::ShindenAPI, models::Player};
use anyhow::Result;
use scraper::{Html, Selector};
use serde_json::Value;

impl ShindenAPI {
    pub async fn get_players(&self, episode_link: &str) -> Result<Vec<Player>> {
        let html = self.get_html(episode_link).await?;
        parse_players_html(&html)
    }
}

fn parse_players_html(html: &str) -> Result<Vec<Player>> {
    let doc = Html::parse_document(&html);
    if let Some(data) = doc
        .select(&Selector::parse("script#episode-players-data").unwrap())
        .next()
    {
        let payload: Value = serde_json::from_str(&data.inner_html())?;
        let players = payload
            .get("players")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("Missing Shinden player data"))?;
        anyhow::ensure!(
            !players.is_empty() || payload.get("zalogowany").and_then(Value::as_bool) == Some(true),
            "Zaloguj się w aplikacji, aby sprawdzić dostępność odtwarzaczy Shinden."
        );
        let field = |player: &Value, key: &str| {
            player
                .get(key)
                .map(|value| {
                    value.as_str().map(str::to_owned).unwrap_or_else(|| {
                        if value.is_number() {
                            value.to_string()
                        } else {
                            String::new()
                        }
                    })
                })
                .unwrap_or_default()
        };
        return Ok(players
            .iter()
            .map(|player| Player {
                player: field(player, "player"),
                online_id: field(player, "online_id"),
                max_res: field(player, "res"),
                lang_audio: field(player, "audio"),
                lang_subs: field(player, "sub"),
            })
            .collect());
    }
    let buttons_selector = Selector::parse(".ep-buttons a").unwrap();

    let mut result = Vec::new();
    for el in doc.select(&buttons_selector) {
        if let Some(data) = el.value().attr("data-episode") {
            if let Ok(json) = serde_json::from_str::<Value>(data) {
                let player = Player {
                    player: json["player"].as_str().unwrap_or("").to_string(),
                    max_res: json["max_res"].as_str().unwrap_or("").to_string(),
                    lang_audio: json["lang_audio"].as_str().unwrap_or("").to_string(),
                    lang_subs: json["lang_subs"].as_str().unwrap_or("").to_string(),
                    online_id: json["online_id"].as_str().unwrap_or("").to_string(),
                };
                result.push(player);
            }
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_players_from_the_current_json_island() {
        let players = super::parse_players_html(r#"<script id="episode-players-data" type="application/json">{"players":[{"player":"CDA","online_id":123,"audio":"JP","sub":"PL","res":1080}],"zalogowany":true}</script>"#).unwrap();
        assert_eq!(players.len(), 1);
        assert_eq!(players[0].online_id, "123");
        assert_eq!(players[0].lang_subs, "PL");
        assert_eq!(players[0].max_res, "1080");
        assert!(
            super::parse_players_html(
                r#"<script id="episode-players-data">{"players":[],"zalogowany":false}</script>"#
            )
            .is_err()
        );
        assert!(
            super::parse_players_html(
                r#"<script id="episode-players-data">{"players":[],"zalogowany":true}</script>"#
            )
            .unwrap()
            .is_empty()
        );
    }
}
