use crate::client::{ShindenAPI, check_response};
use crate::headers::RequestType;
use anyhow::{Result, ensure};
use reqwest::Url;
use reqwest::header::{ACCEPT, CONTENT_TYPE, REFERER};

const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

#[derive(Debug)]
pub struct ShindenImage {
    pub content_type: String,
    pub bytes: Vec<u8>,
}

pub(crate) fn is_trusted_image_url(url: &Url) -> bool {
    let host = url.host_str().unwrap_or_default();
    url.scheme() == "https"
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none()
        && (host == "shinden.pl" || host.ends_with(".shinden.pl") || host == "cdn.shinden.eu")
}

pub(crate) fn normalize_image_url(source: &str) -> Option<Url> {
    if source.trim().is_empty() {
        return None;
    }
    let mut url = Url::parse("https://shinden.pl/")
        .ok()?
        .join(source.trim())
        .ok()?;
    if url.scheme() == "http" {
        url.set_scheme("https").ok()?;
    }
    is_trusted_image_url(&url).then_some(url)
}

impl ShindenAPI {
    /// Fetch images through the same cookie store and browser identity as HTML requests.
    pub async fn get_image(&self, source: &str) -> Result<ShindenImage> {
        let url = normalize_image_url(source)
            .ok_or_else(|| anyhow::anyhow!("Unsupported Shinden image URL"))?;
        let headers = self.headers(RequestType::Frontend, url.as_str())?;
        let response = self
            .image_client
            .get(url)
            .headers(headers)
            .header(ACCEPT, "image/avif,image/webp,image/*,*/*;q=0.8")
            .header(REFERER, "https://shinden.pl/")
            .send()
            .await?;
        read_image_response(response).await
    }
}

async fn read_image_response(response: reqwest::Response) -> Result<ShindenImage> {
    let mut response = check_response(response)?;
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    ensure!(
        content_type.starts_with("image/"),
        "Shinden did not return an image"
    );
    ensure!(
        response.content_length().unwrap_or_default() <= MAX_IMAGE_BYTES as u64,
        "Shinden image too large"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            bytes.len() + chunk.len() <= MAX_IMAGE_BYTES,
            "Shinden image too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    ensure!(!bytes.is_empty(), "Shinden returned an empty image");
    Ok(ShindenImage {
        content_type,
        bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn image_requests_are_limited_to_the_expected_https_hosts() {
        for url in [
            "https://shinden.pl/avatar.jpg",
            "https://cdn.shinden.eu/cover.jpg",
            "https://api4.shinden.pl/image.jpg",
        ] {
            assert!(is_trusted_image_url(&Url::parse(url).unwrap()));
        }
        for url in [
            "https://shinden.pl.evil.test/a",
            "https://example.com/a",
            "http://shinden.pl/a",
            "https://shinden.pl:8443/a",
            "https://user:secret@shinden.pl/a",
            "file:///etc/passwd",
        ] {
            assert!(!is_trusted_image_url(&Url::parse(url).unwrap()));
        }
    }

    fn response(status: &str, headers: &str, body: &[u8]) -> reqwest::Response {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let mut payload =
            format!("HTTP/1.1 {status}\r\n{headers}Connection: close\r\n\r\n").into_bytes();
        payload.extend_from_slice(body);
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            stream.read(&mut request).unwrap();
            stream.write_all(&payload).unwrap();
        });
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(reqwest::Client::new().get(url).send())
            .unwrap()
    }

    #[test]
    fn binary_image_bytes_are_preserved_and_html_is_never_served_as_an_image() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // Read headers and body on the same runtime: response bodies may still need its IO driver.
        runtime.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 4096]; stream.read(&mut request).unwrap();
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 8\r\nConnection: close\r\n\r\n\x89PNG\r\n\x1a\n").unwrap();
            });
            let result = read_image_response(reqwest::Client::new().get(url).send().await.unwrap()).await.unwrap();
            assert_eq!(result.content_type, "image/png");
            assert_eq!(result.bytes, b"\x89PNG\r\n\x1a\n");
        });
        let html = response("200 OK", "Content-Type: text/html\r\n", b"challenge");
        assert!(runtime.block_on(read_image_response(html)).is_err());
        let challenge = response("403 Forbidden", "cf-mitigated: challenge\r\n", b"challenge");
        assert!(
            runtime
                .block_on(read_image_response(challenge))
                .unwrap_err()
                .to_string()
                .contains(crate::client::BROWSER_VERIFICATION_REQUIRED)
        );
        let oversized = response(
            "200 OK",
            "Content-Type: image/jpeg\r\nContent-Length: 10485761\r\n",
            b"",
        );
        assert!(runtime.block_on(read_image_response(oversized)).is_err());
    }
}
