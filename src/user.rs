use crate::client::ShindenAPI;
use anyhow::Result;
use base64::Engine;
use reqwest::header::{HeaderMap, HeaderValue};
use scraper::{Html, Selector};

#[derive(Debug)]
pub struct CurrentUser {
    pub id: u64,
    pub name: String,
}

fn current_user_from_token(token: &str) -> Result<CurrentUser> {
    let payload = token
        .split('.')
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("Invalid Shinden identity response"))?;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload)?;
    let claims: serde_json::Value = serde_json::from_slice(&payload)?;
    let id = claims
        .get("uid")
        .or_else(|| claims.get("sub"))
        .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
        .filter(|id| *id > 0)
        .ok_or_else(|| anyhow::anyhow!("Missing Shinden user ID"))?;
    let name = claims
        .get("username")
        .or_else(|| claims.get("name"))
        .and_then(|value| value.as_str())
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("Missing Shinden user name"))?
        .to_string();
    Ok(CurrentUser { id, name })
}

impl ShindenAPI {
    pub async fn login(&self, email: &str, password: &str) -> Result<()> {
        let url = "https://shinden.pl/main/login";
        let login_page = self.get_html(url).await?;
        anyhow::ensure!(
            !login_page.contains("data-login-api=\"/v1/auth\""),
            "Shinden wymaga logowania w oknie przeglądarki aplikacji."
        );

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
        Ok(self.get_current_user().await?.map(|user| user.name))
    }

    pub async fn get_current_user(&self) -> Result<Option<CurrentUser>> {
        let response = match self.get_json("https://shinden.pl/api/user/jwt").await {
            Ok(response) => response,
            Err(error)
                if error.downcast_ref::<reqwest::Error>().is_some_and(|error| {
                    error.status() == Some(reqwest::StatusCode::UNAUTHORIZED)
                }) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        response
            .get("jwt")
            .and_then(|value| value.as_str())
            .filter(|token| !token.is_empty())
            .map(current_user_from_token)
            .transpose()
    }

    pub async fn get_user_profile_image(&self) -> Result<Option<String>> {
        let html = self.get_html("https://shinden.pl/").await?;
        Ok(user_profile_image_from_html(&html))
    }
}

fn user_profile_image_from_html(html: &str) -> Option<String> {
    let doc = Html::parse_document(html);
    let img = doc
        .select(&Selector::parse(".np-awatar img").unwrap())
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
    fn current_user_comes_from_the_authenticated_jwt_instead_of_the_removed_user_page() {
        use base64::Engine;
        let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"uid":31875,"username":"Test User","exp":2000000000}"#);
        let user = super::current_user_from_token(&format!("header.{claims}.signature")).unwrap();
        assert_eq!(user.id, 31875);
        assert_eq!(user.name, "Test User");
        assert!(super::current_user_from_token("invalid").is_err());
    }

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
            let html = format!(r#"<a class="np-awatar"><img src="{source}"></a>"#);
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
