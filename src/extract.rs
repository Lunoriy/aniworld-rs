//! Hoster extraction. VOE is the primary hoster on AniWorld, and its source
//! URL is hidden behind an obfuscated JSON blob; the steps below mirror the
//! original decoder so the same streams resolve.

use anyhow::{anyhow, bail, Result};
use regex::Regex;

use crate::net::Http;

/// Resolve a provider page to a playable (usually HLS) URL.
pub async fn stream_url(http: &Http, provider_url: &str, provider: &str) -> Result<String> {
    if provider_url.contains(".m3u8") {
        return Ok(provider_url.to_string());
    }

    let is_voe = provider.eq_ignore_ascii_case("voe")
        || provider_url.contains("voe.sx")
        || provider_url.contains("/e/");

    if is_voe {
        return voe_source(http, provider_url).await;
    }

    // Unknown hoster: best effort, look for a playlist or a direct file.
    let html = http.text(provider_url, &[]).await?;
    if let Some(source) = extract_source_from_html(&html) {
        return Ok(source);
    }
    find_playlist(&html).ok_or_else(|| anyhow!("{provider} returned no playable stream"))
}

async fn voe_source(http: &Http, embed_url: &str) -> Result<String> {
    let html = http
        .text(
            embed_url,
            &[
                ("Referer", "https://aniworld.to/"),
                ("Accept-Language", "en-US,en;q=0.5"),
            ],
        )
        .await?;

    if let Some(source) = extract_source_from_html(&html) {
        return Ok(source);
    }

    // VOE rotates embed domains and sometimes answers with a JS redirect.
    if let Some(next) = redirect_target(&html) {
        if next != embed_url {
            let html = http.text(&next, &[]).await?;
            if let Some(source) = extract_source_from_html(&html) {
                return Ok(source);
            }
        }
    }

    Err(anyhow!(
        "VOE page contained no playable source (CAPTCHA or removed video?)"
    ))
}

/// Decode the obfuscated VOE payload back into its JSON object.
pub fn decode_voe_string(encoded: &str) -> Result<serde_json::Value> {
    let step1 = rot13(encoded);
    let step2: String = step1
        .replace("@$", "_")
        .replace("^^", "_")
        .replace("~@", "_")
        .replace("%?", "_")
        .replace("*~", "_")
        .replace("!!", "_")
        .replace("#&", "_")
        .chars()
        .filter(|c| *c != '_')
        .collect();

    let step3 = base64_decode(&step2)?;
    let shifted: Vec<u8> = step3.iter().map(|b| b.wrapping_sub(3)).collect();
    let reversed: Vec<u8> = shifted.iter().rev().copied().collect();
    let json = base64_decode_bytes(&reversed)?;
    serde_json::from_slice(&json).map_err(|e| anyhow!("VOE payload is not JSON: {e}"))
}

fn rot13(input: &str) -> String {
    input
        .chars()
        .map(|c| match c {
            'A'..='Z' => (((c as u8 - b'A' + 13) % 26) + b'A') as char,
            'a'..='z' => (((c as u8 - b'a' + 13) % 26) + b'a') as char,
            other => other,
        })
        .collect()
}

fn base64_decode(value: &str) -> Result<Vec<u8>> {
    base64_decode_bytes(value.as_bytes())
}

fn base64_decode_bytes(value: &[u8]) -> Result<Vec<u8>> {
    use base64::Engine as _;
    let engine = base64::engine::general_purpose::STANDARD;
    engine
        .decode(value)
        .map_err(|e| anyhow!("base64 decode failed: {e}"))
}

/// Scan a VOE page for a media source.
pub fn extract_source_from_html(html: &str) -> Option<String> {
    // 1. <script type="application/json"> payloads (string or array of blobs).
    let script_re = Regex::new(
        r#"(?is)<script\b[^>]*\btype\s*=\s*["']application/json["'][^>]*>(.*?)</script>"#,
    )
    .ok()?;
    for script in script_re.captures_iter(html) {
        let payload = script.get(1)?.as_str().trim();
        let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
            continue;
        };
        let candidates: Vec<&str> = match &value {
            serde_json::Value::String(s) => vec![s.as_str()],
            serde_json::Value::Array(items) => items.iter().filter_map(|v| v.as_str()).collect(),
            _ => continue,
        };
        for candidate in candidates {
            if let Ok(decoded) = decode_voe_string(candidate) {
                if let Some(source) = decoded.get("source").and_then(|s| s.as_str()) {
                    if !source.is_empty() {
                        return Some(source.to_string());
                    }
                }
            }
        }
    }

    // 2. var a168c='<blob>'
    if let Ok(re) = Regex::new(r"var a168c='([^']+)'") {
        if let Some(c) = re.captures(html) {
            if let Ok(decoded) = decode_voe_string(&c[1]) {
                if let Some(source) = decoded.get("source").and_then(|s| s.as_str()) {
                    return Some(source.to_string());
                }
            }
        }
    }

    // 3. plain 'hls': '<url>' or a bare .m3u8 anywhere in the markup.
    if let Ok(re) = Regex::new(r#"'hls':\s*'(?P<hls>[^']+)'"#) {
        if let Some(c) = re.captures(html) {
            return Some(c["hls"].to_string());
        }
    }
    find_playlist(html)
}

fn find_playlist(html: &str) -> Option<String> {
    let re = Regex::new(r#"https?://[^\s'"<>\\]+?\.m3u8[^\s'"<>\\]*"#).ok()?;
    re.find(html)
        .map(|m| html_escape::decode_html_entities(m.as_str()).to_string())
}

fn redirect_target(html: &str) -> Option<String> {
    let re = Regex::new(
        r#"(?:https?://[^\s'"<>]+/e/[^\s'"<>]+|location\.href\s*=\s*['"](https?://[^'"]+)['"])"#,
    )
    .ok()?;
    let caps = re.captures(html)?;
    let url = caps
        .get(1)
        .or_else(|| caps.get(0))
        .map(|m| m.as_str().to_string())?;
    if url.starts_with("http") {
        Some(url)
    } else {
        None
    }
}

/// Follow the site's `/redirect/<id>` hop and return the hoster URL.
pub async fn provider_url(http: &Http, redirect_url: &str, site_host: &str) -> Result<String> {
    let final_url = http.final_url(redirect_url).await?;
    let host = url::Url::parse(&final_url)
        .map(|u| u.host_str().unwrap_or_default().to_string())
        .unwrap_or_default();
    if host.is_empty() || host.ends_with(site_host) {
        bail!("redirect {redirect_url} did not leave {site_host}");
    }
    Ok(final_url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(json: &str) -> String {
        use base64::Engine as _;
        let engine = base64::engine::general_purpose::STANDARD;
        // inverse of decode: b64(json) -> reverse -> shift +3 -> b64
        let step5 = engine.encode(json.as_bytes());
        let reversed: Vec<u8> = step5.bytes().rev().collect();
        let shifted: Vec<u8> = reversed.iter().map(|b| b.wrapping_add(3)).collect();
        let step2 = engine.encode(shifted);
        // re-insert junk and rotate back
        rot13(&format!("{step2}"))
    }

    #[test]
    fn decodes_round_trip_payload() {
        let encoded = encode(r#"{"source":"https://cdn.example/master.m3u8","direct_access_url":""}"#);
        let decoded = decode_voe_string(&encoded).unwrap();
        assert_eq!(decoded["source"], "https://cdn.example/master.m3u8");
    }

    #[test]
    fn extracts_source_from_script_tag() {
        let blob = encode(r#"{"source":"https://cdn.example/hls/master.m3u8?t=1"}"#);
        let html = format!(
            r#"<html><script type="application/json">["{blob}"]</script></html>"#
        );
        assert_eq!(
            extract_source_from_html(&html).as_deref(),
            Some("https://cdn.example/hls/master.m3u8?t=1")
        );
    }

    #[test]
    fn falls_back_to_plain_hls_key() {
        let html = r#"<script>var player = {'hls': 'https://cdn.example/a/master.m3u8'};</script>"#;
        assert_eq!(
            extract_source_from_html(html).as_deref(),
            Some("https://cdn.example/a/master.m3u8")
        );
    }

    #[test]
    fn returns_none_for_empty_page() {
        assert!(extract_source_from_html("<html>blocked</html>").is_none());
    }
}
