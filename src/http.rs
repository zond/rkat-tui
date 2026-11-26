//! rkat-tui's own HTTP requests (model lists, LiteLLM model info, the
//! community price list, the agents' `web_fetch`): the one client
//! builder, bounded body reads (the body is read chunk by chunk and
//! refused once it passes a cap, so a missing or lying `Content-Length`
//! cannot exhaust memory), and the lenient reading of fields of documents
//! rkat-tui does not own.

use std::time::Duration;

use serde::Deserialize as _;

/// A client for requests to `url` (the base URL when known), timing out
/// after `timeout` (the connect too). Like `meerkat-client`, a loopback
/// URL never goes through a proxy.
pub fn client(url: Option<&str>, timeout: Duration) -> reqwest::Result<reqwest::Client> {
    builder(url, timeout).build()
}

/// The user agent of `web_fetch`'s requests: the program and its version,
/// nothing of the user or the project.
pub const USER_AGENT: &str = concat!("rkat-tui/", env!("CARGO_PKG_VERSION"));

/// [`client`] for `web_fetch` (`crate::tools::web_fetch`) of `url`: the
/// same proxy and TLS roots, a generic [`USER_AGENT`], and no redirect
/// followed by the client (the tool follows a redirect within the same
/// origin itself and reports any other).
pub fn fetch_client(url: &str, timeout: Duration) -> reqwest::Result<reqwest::Client> {
    builder(Some(url), timeout)
        .user_agent(USER_AGENT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

/// The one client builder: `timeout` for the request and the connect, no
/// proxy for a loopback `url`.
fn builder(url: Option<&str>, timeout: Duration) -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(timeout);
    if url.is_some_and(is_loopback_url) {
        builder.no_proxy()
    } else {
        builder
    }
}

/// Whether `url` points at this machine (`localhost`, `127.0.0.0/8`, `::1`).
pub(crate) fn is_loopback_url(url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(url) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_matches(|c| c == '[' || c == ']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// A field of a JSON document rkat-tui does not own: a value of type `T`,
/// else nothing (`null` or any other shape is "not reported", never a
/// refusal of the whole document).
///
/// # Errors
///
/// Only what the deserializer itself fails with (not a value's type).
pub(crate) fn lenient<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    Ok(serde_json::from_value(serde_json::Value::deserialize(deserializer)?).ok())
}

/// A body read that failed or passed its cap.
#[derive(Debug, thiserror::Error)]
pub enum CappedReadError {
    /// Transport failure (the URL is stripped).
    #[error("network error: {0}")]
    Http(#[source] reqwest::Error),
    #[error("response larger than {cap} bytes")]
    TooLarge {
        /// The cap in bytes.
        cap: usize,
    },
}

impl From<reqwest::Error> for CappedReadError {
    fn from(err: reqwest::Error) -> Self {
        Self::Http(err.without_url())
    }
}

/// Read `response`'s body, at most `cap` bytes.
///
/// # Errors
///
/// [`CappedReadError::TooLarge`] when the declared or the actual length
/// passes `cap`, [`CappedReadError::Http`] on transport failures.
pub async fn read_capped(
    mut response: reqwest::Response,
    cap: usize,
) -> Result<Vec<u8>, CappedReadError> {
    if response
        .content_length()
        .is_some_and(|len| len > cap as u64)
    {
        return Err(CappedReadError::TooLarge { cap });
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > cap {
            return Err(CappedReadError::TooLarge { cap });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_detection() {
        assert!(is_loopback_url("http://127.0.0.1:8080/"));
        assert!(is_loopback_url("http://localhost:1"));
        assert!(is_loopback_url("http://[::1]:1"));
        assert!(!is_loopback_url("https://api.anthropic.com"));
        assert!(!is_loopback_url("not a url"));
    }

    #[test]
    fn a_lenient_field_of_another_type_is_absent() {
        #[derive(serde::Deserialize)]
        struct Fields {
            #[serde(default, deserialize_with = "lenient")]
            number: Option<f64>,
            #[serde(default, deserialize_with = "lenient")]
            flag: Option<bool>,
        }
        let read = |text: &str| {
            let fields: Fields = serde_json::from_str(text).unwrap_or_else(|_| Fields {
                number: Some(-1.0),
                flag: None,
            });
            (fields.number, fields.flag)
        };
        assert_eq!(
            read(r#"{"number": 3, "flag": true}"#),
            (Some(3.0), Some(true))
        );
        assert_eq!(read(r#"{"number": "3", "flag": 1}"#), (None, None));
        assert_eq!(read(r#"{"number": null}"#), (None, None));
        assert_eq!(read("{}"), (None, None));
    }
}
