use crate::client::ShindenAPI;
use anyhow::Result;
use reqwest::header::{HeaderMap, HeaderValue};
use scraper::{Html, Selector};

impl ShindenAPI {
    pub async fn login(&self, email: &str, password: &str) -> Result<()> {
        let url = "https://shinden.pl/main/login";
        let _get_response_body = self.get_html(url).await?;

        let form = vec![
            ("username".into(), email.into()),
            ("password".into(), password.into()),
            ("remember".into(), "on".into()),
        ];

        let mut post_specific_headers = HeaderMap::new();
        post_specific_headers.insert(
            reqwest::header::REFERER,
            HeaderValue::from_static("https://shinden.pl/main/login"),
        ); // Często wymagane

        self.post_form(url, &form, Some(post_specific_headers))
            .await?;

        Ok(())
    }

    pub async fn logout(&self) -> Result<()> {
        let url = "https://shinden.pl/main/logout";
        self.get_html(url).await?;
        Ok(())
    }

    pub async fn get_user_name(&self) -> Result<Option<String>> {
        let html = self.get_html("https://shinden.pl/user").await?;
        let doc = Html::parse_document(&html);
        let title = doc
            .root_element()
            .select(&Selector::parse("title").unwrap())
            .next();
        if let Some(t) = title {
            let text = t.text().collect::<String>();
            let username = text.split("(użytkownik)").next().unwrap_or("").trim();
            return Ok(Some(username.to_string()));
        }
        Ok(None)
    }

    pub async fn get_user_profile_image(&self) -> Result<Option<String>> {
        let html = self.get_html("https://shinden.pl/user").await?;
        Ok(user_profile_image_from_html(&html))
    }
}

fn user_profile_image_from_html(html: &str) -> Option<String> {
    let doc = Html::parse_document(html);
    let img = doc
        .select(&Selector::parse(".info-aside-img").unwrap())
        .next();
    if let Some(img_el) = img {
        if let Some(src) = img_el.value().attr("src") {
            return crate::images::normalize_image_url(src).map(|url| url.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::user_profile_image_from_html;

    #[test]
    fn profile_avatar_resolves_absolute_relative_and_cdn_sources() {
        for (source, expected) in [
            (
                "/res/avatars/42.jpg",
                "https://shinden.pl/res/avatars/42.jpg",
            ),
            (
                "https://shinden.pl/res/avatars/42.jpg",
                "https://shinden.pl/res/avatars/42.jpg",
            ),
            (
                "//cdn.shinden.eu/avatar.jpg",
                "https://cdn.shinden.eu/avatar.jpg",
            ),
        ] {
            let html = format!(r#"<img class="info-aside-img" src="{source}">"#);
            assert_eq!(
                user_profile_image_from_html(&html).as_deref(),
                Some(expected)
            );
        }
    }

    #[test]
    fn profile_without_an_avatar_does_not_invent_an_image_address() {
        assert_eq!(user_profile_image_from_html("<html></html>"), None);
        assert_eq!(
            user_profile_image_from_html(r#"<img class="info-aside-img" src="">"#),
            None
        );
    }
}
