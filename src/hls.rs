//! HLS playlist parsing and AES-128 decryption.
//!
//! The Python version delegated this partly to ffmpeg, which fetches segments
//! strictly one at a time. Parsing playlists ourselves lets the engine fetch a
//! bounded window of segments in parallel while still writing them in order.

use std::collections::HashMap;

use anyhow::{anyhow, bail, Result};

use crate::config::Language;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variant {
    pub uri: String,
    pub bandwidth: u64,
    pub audio_group: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendition {
    pub uri: Option<String>,
    pub group_id: String,
    pub language: String,
    pub name: String,
    pub is_default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub method: KeyMethod,
    pub uri: Option<String>,
    pub iv: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyMethod {
    None,
    Aes128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub uri: String,
    pub key: Option<Key>,
    pub sequence: u64,
}

#[derive(Debug, Clone, Default)]
pub struct MediaPlaylist {
    pub segments: Vec<Segment>,
    pub init: Option<String>,
    /// `.mp4` when an `EXT-X-MAP` init segment is present, else `.ts`.
    pub container: &'static str,
}

pub fn is_master(text: &str) -> bool {
    text.contains("#EXT-X-STREAM-INF")
}

pub fn parse_attributes(line: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let tag = line.splitn(2, ':').nth(1).unwrap_or("");
    let mut key_part = String::new();
    let mut value = String::new();
    let mut in_quotes = false;
    let mut parsing_value = false;

    for ch in tag.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                if parsing_value {
                    value.push(ch);
                }
            }
            '=' if !in_quotes && !parsing_value => {
                parsing_value = true;
            }
            ',' if !in_quotes && parsing_value => {
                map.insert(key_part.trim().to_ascii_uppercase(), value.trim().trim_matches('"').to_string());
                key_part.clear();
                value.clear();
                parsing_value = false;
            }
            c => {
                if parsing_value {
                    value.push(c)
                } else {
                    key_part.push(c)
                }
            }
        }
    }
    if !key_part.trim().is_empty() {
        map.insert(
            key_part.trim().to_ascii_uppercase(),
            value.trim().trim_matches('"').to_string(),
        );
    }
    map
}

pub fn resolve(base: &str, link: &str) -> String {
    match url::Url::parse(base) {
        Ok(base_url) => base_url
            .join(link)
            .map(|u| u.to_string())
            .unwrap_or_else(|_| link.to_string()),
        Err(_) => link.to_string(),
    }
}

pub fn parse_master(text: &str, base_url: &str) -> Result<(Vec<Variant>, Vec<Rendition>)> {
    let lines: Vec<&str> = text.lines().map(|l| l.trim()).collect();
    let mut variants = Vec::new();
    let mut renditions = Vec::new();

    for (index, line) in lines.iter().enumerate() {
        if let Some(_rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            let attrs = parse_attributes(line);
            let uri = lines[index + 1..]
                .iter()
                .find(|candidate| !candidate.is_empty() && !candidate.starts_with('#'))
                .map(|u| resolve(base_url, u));
            let Some(uri) = uri else { continue };
            variants.push(Variant {
                uri,
                bandwidth: attrs
                    .get("BANDWIDTH")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0),
                audio_group: attrs.get("AUDIO").cloned().unwrap_or_default(),
            });
        } else if line.starts_with("#EXT-X-MEDIA:") {
            let attrs = parse_attributes(line);
            if attrs.get("TYPE").map(|v| v != "AUDIO").unwrap_or(true) {
                continue;
            }
            renditions.push(Rendition {
                uri: attrs.get("URI").map(|u| resolve(base_url, u)),
                group_id: attrs.get("GROUP-ID").cloned().unwrap_or_default(),
                language: attrs.get("LANGUAGE").cloned().unwrap_or_default(),
                name: attrs.get("NAME").cloned().unwrap_or_default(),
                is_default: attrs
                    .get("DEFAULT")
                    .map(|v| v.eq_ignore_ascii_case("YES"))
                    .unwrap_or(false),
            });
        }
    }

    if variants.is_empty() {
        bail!("master playlist has no variants");
    }
    Ok((variants, renditions))
}

/// Highest bandwidth wins, matching the original behaviour (VOE serves 1080p
/// as the top variant).
pub fn best_variant(variants: &[Variant]) -> Option<&Variant> {
    variants.iter().max_by_key(|v| v.bandwidth)
}

pub fn select_audio<'a>(
    renditions: &'a [Rendition],
    audio_group: &str,
    language: Language,
) -> Option<&'a Rendition> {
    if audio_group.is_empty() {
        return None;
    }
    let candidates: Vec<&Rendition> = renditions
        .iter()
        .filter(|r| r.uri.is_some() && r.group_id == audio_group)
        .collect();
    if candidates.is_empty() {
        return None;
    }

    let prefixes: &[&str] = match language.hls_audio() {
        "deu" => &["de", "ger", "deu"],
        "eng" => &["en", "eng"],
        _ => &["ja", "jp", "jpn"],
    };
    let hints: &[&str] = match language.hls_audio() {
        "deu" => &["german", "deutsch"],
        "eng" => &["english"],
        _ => &["japanese", "japanisch"],
    };

    for candidate in &candidates {
        let lang = candidate.language.to_ascii_lowercase();
        if !lang.is_empty() && prefixes.iter().any(|p| lang.starts_with(p)) {
            return Some(candidate);
        }
    }
    for candidate in &candidates {
        let name = candidate.name.to_ascii_lowercase();
        if hints.iter().any(|h| name.contains(h)) {
            return Some(candidate);
        }
    }
    candidates
        .iter()
        .find(|r| r.is_default)
        .copied()
        .or_else(|| candidates.first().copied())
}

pub fn parse_media(text: &str, base_url: &str) -> Result<MediaPlaylist> {
    if !text.contains("#EXT-X-ENDLIST") {
        bail!("live playlist (no EXT-X-ENDLIST)");
    }

    let mut segments = Vec::new();
    let mut init = None;
    let mut current_key: Option<Key> = None;
    let mut sequence: u64 = 0;

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            sequence = rest.trim().parse().unwrap_or(0);
        } else if line.starts_with("#EXT-X-BYTERANGE") {
            bail!("byte-range segments are not supported");
        } else if line.starts_with("#EXT-X-MAP:") {
            let attrs = parse_attributes(line);
            let Some(uri) = attrs.get("URI") else {
                bail!("EXT-X-MAP without URI");
            };
            if attrs.contains_key("BYTERANGE") {
                bail!("byte-range init segment");
            }
            init = Some(resolve(base_url, uri));
        } else if line.starts_with("#EXT-X-KEY:") {
            let attrs = parse_attributes(line);
            let method = attrs
                .get("METHOD")
                .map(|m| m.to_ascii_uppercase())
                .unwrap_or_else(|| "NONE".to_string());
            current_key = match method.as_str() {
                "NONE" => None,
                "AES-128" => {
                    let Some(uri) = attrs.get("URI") else {
                        bail!("AES-128 key without URI");
                    };
                    Some(Key {
                        method: KeyMethod::Aes128,
                        uri: Some(resolve(base_url, uri)),
                        iv: attrs.get("IV").cloned(),
                    })
                }
                other => bail!("unsupported HLS encryption method {other}"),
            };
        } else if !line.starts_with('#') {
            segments.push(Segment {
                uri: resolve(base_url, line),
                key: current_key.clone(),
                sequence,
            });
            sequence += 1;
        }
    }

    if segments.is_empty() {
        bail!("playlist contains no segments");
    }

    Ok(MediaPlaylist {
        container: if init.is_some() { ".mp4" } else { ".ts" },
        init,
        segments,
    })
}

/// AES-128-CBC with the HLS default IV (segment sequence number) unless the
/// playlist provides one. Padding is optional in the wild.
pub fn decrypt_aes128(data: &[u8], key: &[u8], iv: &[u8; 16]) -> Result<Vec<u8>> {
    use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};

    if key.len() != 16 {
        bail!("AES key has {} bytes, expected 16", key.len());
    }
    if data.is_empty() || data.len() % 16 != 0 {
        bail!("ciphertext length {} is not a multiple of 16", data.len());
    }

    type Decryptor = cbc::Decryptor<aes::Aes128>;
    let key_arr: [u8; 16] = key.try_into().expect("checked length");

    let mut buffer = data.to_vec();
    match Decryptor::new(&key_arr.into(), iv.into()).decrypt_padded_mut::<Pkcs7>(&mut buffer) {
        Ok(plain) => Ok(plain.to_vec()),
        Err(_) => {
            let mut buffer = data.to_vec();
            let plain = Decryptor::new(&key_arr.into(), iv.into())
                .decrypt_padded_mut::<aes::cipher::block_padding::NoPadding>(&mut buffer)
                .map_err(|e| anyhow!("AES-128 decryption failed: {e}"))?;
            Ok(plain.to_vec())
        }
    }
}

pub fn resolve_iv(iv: Option<&str>, sequence: u64) -> [u8; 16] {
    if let Some(value) = iv {
        let hex = value.trim().trim_start_matches("0x").trim_start_matches("0X");
        let padded = format!("{hex:0>32}");
        if let Some(bytes) = parse_hex_16(&padded) {
            return bytes;
        }
    }
    let mut out = [0u8; 16];
    out[8..].copy_from_slice(&sequence.to_be_bytes());
    out
}

fn parse_hex_16(value: &str) -> Option<[u8; 16]> {
    if value.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: &str = r#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",LANGUAGE="de",NAME="German",DEFAULT=YES,URI="audio/de.m3u8"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",LANGUAGE="ja",NAME="Japanese",URI="audio/ja.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=900000,AUDIO="aud"
low/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=3600000,AUDIO="aud"
high/index.m3u8
"#;

    #[test]
    fn parses_master_and_picks_best() {
        let (variants, renditions) = parse_master(MASTER, "https://cdn.example/p/master.m3u8").unwrap();
        assert_eq!(variants.len(), 2);
        let best = best_variant(&variants).unwrap();
        assert_eq!(best.uri, "https://cdn.example/p/high/index.m3u8");
        let audio = select_audio(&renditions, "aud", Language::GermanDub).unwrap();
        assert_eq!(audio.uri.as_deref(), Some("https://cdn.example/p/audio/de.m3u8"));
        let audio = select_audio(&renditions, "aud", Language::EnglishSub).unwrap();
        assert_eq!(audio.language, "de"); // falls back to the default rendition
    }

    #[test]
    fn parses_media_playlist_with_keys_and_init() {
        let text = r#"#EXTM3U
#EXT-X-MEDIA-SEQUENCE:7
#EXT-X-MAP:URI="init.mp4"
#EXT-X-KEY:METHOD=AES-128,URI="k.bin",IV=0x00000000000000000000000000000007
#EXTINF:4.0,
seg0.m4s
#EXTINF:4.0,
seg1.m4s
#EXT-X-ENDLIST
"#;
        let playlist = parse_media(text, "https://cdn.example/p/v/index.m3u8").unwrap();
        assert_eq!(playlist.container, ".mp4");
        assert_eq!(
            playlist.init.as_deref(),
            Some("https://cdn.example/p/v/init.mp4")
        );
        assert_eq!(playlist.segments.len(), 2);
        assert_eq!(playlist.segments[0].sequence, 7);
        assert_eq!(
            playlist.segments[0].key.as_ref().unwrap().uri.as_deref(),
            Some("https://cdn.example/p/v/k.bin")
        );
    }

    #[test]
    fn rejects_live_and_byte_range_playlists() {
        let live = "#EXTM3U\n#EXTINF:4,\na.ts\n";
        assert!(parse_media(live, "https://x/y.m3u8").is_err());
        let ranges = "#EXTM3U\n#EXT-X-BYTERANGE:1000@0\n#EXTINF:4,\na.ts\n#EXT-X-ENDLIST\n";
        assert!(parse_media(ranges, "https://x/y.m3u8").is_err());
    }

    #[test]
    fn iv_defaults_to_sequence_number() {
        let iv = resolve_iv(None, 42);
        assert_eq!(&iv[8..], &42u64.to_be_bytes());
        let iv = resolve_iv(Some("0x0f"), 1);
        assert_eq!(iv[15], 0x0f);
    }

    #[test]
    fn decrypts_pkcs7_and_unpadded() {
        use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
        type Encryptor = cbc::Encryptor<aes::Aes128>;
        let key = [7u8; 16];
        let iv = [1u8; 16];
        let plain = b"hello hls segment";

        let mut buf = vec![0u8; 32];
        buf[..plain.len()].copy_from_slice(plain);
        let cipher = Encryptor::new(&key.into(), &iv.into())
            .encrypt_padded_mut::<Pkcs7>(&mut buf, plain.len())
            .unwrap()
            .to_vec();
        assert_eq!(decrypt_aes128(&cipher, &key, &iv).unwrap(), plain);
    }
}
