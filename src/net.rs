//! Shared HTTP client. One pooled client for the whole process keeps TLS
//! handshakes and connections cheap; retries and backoff live here so the
//! scraper and the segment engine behave the same way.

use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use bytes::Bytes;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, RETRY_AFTER, USER_AGENT};
use reqwest::StatusCode;

#[derive(Clone)]
pub struct Http {
    client: reqwest::Client,
    user_agent: String,
    retries: u32,
}

impl Http {
    pub fn new(user_agent: &str, retries: u32) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(user_agent)
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(15))
            .pool_max_idle_per_host(16)
            .tcp_keepalive(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            client,
            user_agent: user_agent.to_string(),
            retries: retries.max(1),
        })
    }

    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    fn headers(&self, extra: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        map.insert(USER_AGENT, HeaderValue::from_str(&self.user_agent).unwrap());
        for (name, value) in extra {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                map.insert(name, value);
            }
        }
        map
    }

    /// GET returning the decoded body text.
    pub async fn text(&self, url: &str, extra: &[(&str, &str)]) -> Result<String> {
        let mut last: Option<anyhow::Error> = None;
        for attempt in 0..self.retries {
            match self
                .client
                .get(url)
                .headers(self.headers(extra))
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return resp.text().await.context("reading response body");
                    }
                    let retry_after = retry_delay(&resp);
                    let err = anyhow::anyhow!("HTTP {status} for {url}");
                    if !is_retryable(status) || attempt + 1 == self.retries {
                        return Err(err);
                    }
                    last = Some(err);
                    sleep_backoff(attempt, retry_after).await;
                }
                Err(err) => {
                    if attempt + 1 == self.retries {
                        return Err(err).with_context(|| format!("GET {url}"));
                    }
                    last = Some(err.into());
                    sleep_backoff(attempt, None).await;
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("GET {url} failed")))
    }

    /// GET a binary body, used for HLS segments and keys.
    pub async fn bytes(&self, url: &str, extra: &[(&str, &str)]) -> Result<Bytes> {
        let mut last: Option<anyhow::Error> = None;
        for attempt in 0..self.retries {
            match self
                .client
                .get(url)
                .headers(self.headers(extra))
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        let body = resp.bytes().await.with_context(|| format!("reading {url}"))?;
                        if body.is_empty() {
                            last = Some(anyhow::anyhow!("empty body from {url}"));
                        } else {
                            return Ok(body);
                        }
                    } else if is_retryable(status) {
                        last = Some(anyhow::anyhow!("HTTP {status} for {url}"));
                    } else {
                        bail!("HTTP {status} for {url}");
                    }
                }
                Err(err) => last = Some(err.into()),
            }
            if attempt + 1 < self.retries {
                sleep_backoff(attempt, None).await;
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("GET {url} failed")))
    }

    /// Open a streaming response for a direct file download.
    pub async fn stream(&self, url: &str, extra: &[(&str, &str)]) -> Result<reqwest::Response> {
        let resp = self
            .client
            .get(url)
            .headers(self.headers(extra))
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            bail!("HTTP {status} for {url}");
        }
        Ok(resp)
    }

    /// Follow a site redirect (`/redirect/<id>`) and return the final URL.
    pub async fn final_url(&self, url: &str) -> Result<String> {
        let resp = self
            .client
            .get(url)
            .headers(self.headers(&[]))
            .send()
            .await
            .with_context(|| format!("resolving redirect {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            bail!("redirect {url} answered HTTP {status}");
        }
        Ok(resp.url().to_string())
    }

    /// POST a form and parse the JSON reply (site search endpoint).
    pub async fn post_form_json(
        &self,
        url: &str,
        form: &[(&str, &str)],
    ) -> Result<serde_json::Value> {
        let resp = self
            .client
            .post(url)
            .headers(self.headers(&[("X-Requested-With", "XMLHttpRequest")]))
            .form(form)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            bail!("POST {url} answered HTTP {status}");
        }
        resp.json::<serde_json::Value>()
            .await
            .with_context(|| format!("decoding JSON from {url}"))
    }
}

fn is_retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::REQUEST_TIMEOUT
        || status.is_server_error()
}

fn retry_delay(resp: &reqwest::Response) -> Option<Duration> {
    resp.headers()
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

async fn sleep_backoff(attempt: u32, retry_after: Option<Duration>) {
    let base = retry_after.unwrap_or_else(|| {
        let millis = 400u64.saturating_mul(1 << attempt.min(5));
        Duration::from_millis(millis)
    });
    let jitter = {
        use rand::Rng;
        rand::thread_rng().gen_range(0..250)
    };
    tokio::time::sleep(base + Duration::from_millis(jitter)).await;
}
