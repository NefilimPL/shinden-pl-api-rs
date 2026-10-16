use shinden_pl_api::client::ShindenAPI;
use std::io::{Read, Write};
use std::net::TcpListener;

fn server(status: &str, headers: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let response = format!(
        "HTTP/1.1 {status}\r\n{headers}Content-Length: 9\r\nConnection: close\r\n\r\nchallenge"
    );
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0; 4096];
        stream.read(&mut request).unwrap();
        stream.write_all(response.as_bytes()).unwrap();
    });
    url
}

#[test]
fn cloudflare_challenge_is_actionable_instead_of_a_generic_403() {
    let url = server("403 Forbidden", "cf-mitigated: challenge\r\n");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let api = ShindenAPI::new().unwrap();
    let error = runtime
        .block_on(api.get_html(&url))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("SHINDEN_BROWSER_VERIFICATION_REQUIRED"),
        "{error}"
    );
}

#[test]
fn challenge_with_success_status_is_never_parsed_as_shinden_content() {
    let url = server("200 OK", "cf-mitigated: challenge\r\n");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let api = ShindenAPI::new().unwrap();
    let result = runtime.block_on(api.get_html(&url));
    assert!(
        result.is_err(),
        "Challenge HTML must not be treated as content"
    );
}

#[test]
fn ordinary_forbidden_response_does_not_request_cloudflare_verification() {
    let url = server("403 Forbidden", "");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let api = ShindenAPI::new().unwrap();
    let error = runtime
        .block_on(api.get_html(&url))
        .unwrap_err()
        .to_string();
    assert!(error.contains("403"));
    assert!(!error.contains("SHINDEN_BROWSER_VERIFICATION_REQUIRED"));
}

#[test]
fn login_post_also_reports_a_challenge_instead_of_success() {
    let url = server("403 Forbidden", "cf-mitigated: challenge\r\n");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let api = ShindenAPI::new().unwrap();
    let error = runtime
        .block_on(api.post_form(&url, &[], None))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("SHINDEN_BROWSER_VERIFICATION_REQUIRED"),
        "{error}"
    );
}
