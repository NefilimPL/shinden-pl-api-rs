use crate::headers::{RequestType, get_headers_for_type};
use anyhow::{Context, Result};
use cookie_store::CookieStore;
use cookie_store::serde;
use reqwest::header::{HeaderValue, USER_AGENT};
use reqwest::{Client, IntoUrl, Method, RequestBuilder, Response};
use reqwest_cookie_store::CookieStoreMutex;
use std::fs::{File, create_dir_all};
use std::io::{BufReader, BufWriter};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

pub const BROWSER_VERIFICATION_REQUIRED: &str = "SHINDEN_BROWSER_VERIFICATION_REQUIRED";

/// Inspect the challenge header before discarding the response in error_for_status.
pub fn check_response(response: Response) -> Result<Response> {
    if response
        .headers()
        .get("cf-mitigated")
        .is_some_and(|value| value == "challenge")
    {
        anyhow::bail!(
            "{BROWSER_VERIFICATION_REQUIRED}: Shinden wymaga weryfikacji Cloudflare w oknie przeglądarki aplikacji."
        );
    }
    Ok(response.error_for_status()?)
}

#[derive(Clone)]

pub struct ShindenAPI {
    pub client: Client,
    pub(crate) image_client: Client,
    jar: Arc<CookieStoreMutex>,
    cookie_path: PathBuf,
    browser_user_agent: Arc<RwLock<Option<HeaderValue>>>,
}

impl ShindenAPI {
    pub fn new() -> Result<Self> {
        let mut cookie_path =
            dirs::data_local_dir().context("Could not find user local data directory")?;
        cookie_path.push("shinden_api");
        create_dir_all(&cookie_path)?;
        cookie_path.push("cookies.json");

        Self::with_cookie_path(cookie_path)
    }

    fn with_cookie_path(cookie_path: PathBuf) -> Result<Self> {
        let store = load_cookies(&cookie_path)?;
        let jar = Arc::new(CookieStoreMutex::new(store));
        let agent_path = cookie_path.with_file_name("browser-user-agent.json");
        let browser_user_agent = if agent_path.exists() {
            let value: String = serde_json::from_reader(BufReader::new(File::open(agent_path)?))?;
            Some(HeaderValue::from_str(&value).context("Invalid saved browser user agent")?)
        } else {
            None
        };

        let client = Client::builder()
            .cookie_provider(jar.clone())
            .redirect(reqwest::redirect::Policy::default())
            .gzip(true)
            .deflate(true)
            .brotli(true)
            .build()?;

        let image_client = Client::builder()
            .cookie_provider(jar.clone())
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if !crate::images::is_trusted_image_url(attempt.url()) {
                    attempt.error("Unsupported Shinden image redirect")
                } else if attempt.previous().len() >= 5 {
                    attempt.error("Too many Shinden image redirects")
                } else {
                    attempt.follow()
                }
            }))
            .build()?;

        Ok(Self {
            client,
            image_client,
            jar,
            cookie_path,
            browser_user_agent: Arc::new(RwLock::new(browser_user_agent)),
        })
    }

    /// All requests using browser clearance must present that browser's user agent.
    pub fn request<U: IntoUrl>(&self, method: Method, url: U) -> RequestBuilder {
        let mut request = self.client.request(method, url);
        let agent = self
            .browser_user_agent
            .read()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(agent) = agent.as_ref() {
            request = request.header(USER_AGENT, agent.clone());
        }
        request
    }

    pub(crate) fn headers(
        &self,
        request_type: RequestType,
        url: &str,
    ) -> Result<reqwest::header::HeaderMap> {
        let mut headers = get_headers_for_type(request_type, Some(url))?;
        let agent = self
            .browser_user_agent
            .read()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(agent) = agent.as_ref() {
            headers.insert(USER_AGENT, agent.clone());
        }
        Ok(headers)
    }

    /// Import only clearance, leaving the application's login session untouched.
    /// The host application obtains this cookie from its own trusted browser window.
    pub fn import_browser_clearance(&self, user_agent: &str, cookie: &str) -> Result<()> {
        anyhow::ensure!(!user_agent.trim().is_empty(), "Missing browser user agent");
        let user_agent_header =
            HeaderValue::from_str(user_agent).context("Invalid browser user agent")?;
        let cookie = cookie_store::RawCookie::parse(cookie.to_owned())
            .context("Invalid clearance cookie")?;
        anyhow::ensure!(
            cookie.name() == "cf_clearance" && !cookie.value().is_empty(),
            "Missing Cloudflare clearance"
        );
        let url = reqwest::Url::parse("https://shinden.pl/")?;
        let mut store = self
            .jar
            .lock()
            .map_err(|_| anyhow::anyhow!("Cookie store unavailable"))?;
        let mut updated = store.clone();
        updated
            .insert_raw(&cookie, &url)
            .map_err(|_| anyhow::anyhow!("Invalid or expired Shinden clearance cookie"))?;
        let mut agent = self
            .browser_user_agent
            .write()
            .unwrap_or_else(|error| error.into_inner());
        serde_json::to_writer(
            BufWriter::new(File::create(
                self.cookie_path.with_file_name("browser-user-agent.json"),
            )?),
            user_agent,
        )?;
        save_cookies(&updated, &self.cookie_path)?;
        *store = updated;
        *agent = Some(user_agent_header);
        Ok(())
    }

    pub async fn get_html(&self, url: &str) -> Result<String> {
        let headers = self.headers(RequestType::Frontend, url)?;
        let response = self
            .request(Method::GET, url)
            .headers(headers)
            .send()
            .await?;
        let response = check_response(response)?;

        let body = response.text().await?;

        save_cookies(&self.jar.lock().unwrap(), &self.cookie_path)?;
        Ok(body)
    }
    pub async fn resolve_final_url(&self, url: &str) -> Result<String> {
        let headers = self.headers(RequestType::Frontend, url)?;
        let response = self
            .request(Method::GET, url)
            .headers(headers)
            .send()
            .await?;
        let response = check_response(response)?;

        let resolved_url = response.url().to_string();
        save_cookies(&self.jar.lock().unwrap(), &self.cookie_path)?;
        Ok(resolved_url)
    }

    pub async fn post_form(
        &self,
        url: &str,
        form: &[(String, String)],
        custom_headers: Option<reqwest::header::HeaderMap>,
    ) -> Result<String> {
        let mut headers = self.headers(RequestType::Login, url)?;

        if let Some(ch) = custom_headers {
            headers.extend(ch);
        }

        let response = self
            .request(Method::POST, url)
            .form(form)
            .headers(headers)
            .send()
            .await?;
        let response = check_response(response)?;

        let body = response.text().await?;
        save_cookies(&self.jar.lock().unwrap(), &self.cookie_path)?;
        Ok(body)
    }
}

fn load_cookies(path: &std::path::Path) -> Result<CookieStore> {
    if path.exists() {
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        serde::json::load(reader).map_err(|e| anyhow::anyhow!(e.to_string()))
    } else {
        Ok(CookieStore::default())
    }
}

fn save_cookies(store: &CookieStore, path: &std::path::Path) -> Result<()> {
    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);

    serde::json::save(store, &mut writer).map_err(|e| anyhow::anyhow!(e.to_string()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestStorage(PathBuf);

    impl TestStorage {
        fn new() -> Self {
            let suffix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir()
                .join(format!("shinden-clearance-{}-{suffix}", std::process::id()));
            create_dir_all(&directory).unwrap();
            Self(directory)
        }

        fn api(&self) -> ShindenAPI {
            ShindenAPI::with_cookie_path(self.0.join("cookies.json")).unwrap()
        }
    }

    impl Drop for TestStorage {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.0.join("cookies.json"));
            let _ = std::fs::remove_file(self.0.join("browser-user-agent.json"));
            let _ = std::fs::remove_dir(&self.0);
        }
    }

    #[test]
    fn clearance_and_browser_identity_survive_restart_without_replacing_login() {
        let storage = TestStorage::new();
        let api = storage.api();
        let url = reqwest::Url::parse("https://shinden.pl/").unwrap();
        api.jar
            .lock()
            .unwrap()
            .insert_raw(
                &cookie_store::RawCookie::parse("login=existing-session; Secure; Max-Age=3600")
                    .unwrap(),
                &url,
            )
            .unwrap();
        api.import_browser_clearance(
            "Browser/123",
            "cf_clearance=verified; Domain=.shinden.pl; Path=/; Secure; HttpOnly; Max-Age=3600",
        )
        .unwrap();
        drop(api);

        let restored = storage.api();
        let cookies: Vec<_> = restored
            .jar
            .lock()
            .unwrap()
            .get_request_values(&url)
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        assert!(cookies.contains(&("cf_clearance".into(), "verified".into())));
        assert!(cookies.contains(&("login".into(), "existing-session".into())));
        let request = restored.request(Method::GET, url.clone()).build().unwrap();
        assert_eq!(request.headers()[USER_AGENT], "Browser/123");
        assert_eq!(
            restored
                .headers(RequestType::Frontend, url.as_str())
                .unwrap()[USER_AGENT],
            "Browser/123"
        );
        assert_eq!(
            restored.headers(RequestType::Login, url.as_str()).unwrap()[USER_AGENT],
            "Browser/123"
        );
        let other_url = reqwest::Url::parse("https://example.com/").unwrap();
        assert_eq!(
            restored
                .jar
                .lock()
                .unwrap()
                .get_request_values(&other_url)
                .count(),
            0
        );
    }

    #[test]
    fn invalid_clearance_never_changes_or_persists_the_session() {
        let storage = TestStorage::new();
        let api = storage.api();
        for (agent, cookie) in [
            ("", "cf_clearance=verified"),
            ("Browser\r\nInjected: value", "cf_clearance=verified"),
            ("Browser/123", "login=untrusted"),
            ("Browser/123", "cf_clearance="),
            ("Browser/123", "cf_clearance=verified; Domain=evil.test"),
            ("Browser/123", "cf_clearance=expired; Max-Age=0"),
        ] {
            assert!(api.import_browser_clearance(agent, cookie).is_err());
        }
        assert!(!storage.0.join("cookies.json").exists());
        assert!(!storage.0.join("browser-user-agent.json").exists());
        assert!(api.browser_user_agent.read().unwrap().is_none());
    }
}
