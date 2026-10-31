//! YouTube URL parsing.

use anyhow::{Result, bail};
use url::Url;

use crate::timeparse;

/// A video reference extracted from a YouTube URL.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoRef {
    pub video_id: String,
    /// Start offset from `t=` / `start=` in the URL, in seconds.
    pub start: Option<f64>,
}

fn is_video_id(s: &str) -> bool {
    s.len() == 11
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Parses a YouTube URL (watch, youtu.be, shorts, embed, live) or a bare video id.
pub fn parse(input: &str) -> Result<VideoRef> {
    let input = input.trim();
    if is_video_id(input) {
        return Ok(VideoRef {
            video_id: input.to_string(),
            start: None,
        });
    }
    let with_scheme = if input.contains("://") {
        input.to_string()
    } else {
        format!("https://{input}")
    };
    let url =
        Url::parse(&with_scheme).map_err(|e| anyhow::anyhow!("invalid URL {input:?}: {e}"))?;
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let host = host.strip_prefix("m.").unwrap_or(host);

    let mut segments = url
        .path_segments()
        .map(|s| s.collect::<Vec<_>>())
        .unwrap_or_default();
    segments.retain(|s| !s.is_empty());

    let id = match host {
        "youtu.be" => segments.first().map(|s| s.to_string()),
        "youtube.com" | "music.youtube.com" | "youtube-nocookie.com" => match segments.as_slice() {
            ["watch"] => url
                .query_pairs()
                .find(|(k, _)| k == "v")
                .map(|(_, v)| v.into_owned()),
            ["shorts" | "embed" | "live" | "v", id, ..] => Some(id.to_string()),
            _ => None,
        },
        _ => bail!("not a YouTube URL: {input}"),
    };
    let Some(video_id) = id.filter(|id| is_video_id(id)) else {
        bail!("could not find a video id in {input}");
    };

    let start = url
        .query_pairs()
        .find(|(k, _)| k == "t" || k == "start")
        .and_then(|(_, v)| timeparse::parse_duration(&v).ok());

    Ok(VideoRef { video_id, start })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> String {
        parse(s).unwrap().video_id
    }

    #[test]
    fn parses_common_forms() {
        assert_eq!(
            id("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            "dQw4w9WgXcQ"
        );
        assert_eq!(id("youtube.com/watch?list=x&v=dQw4w9WgXcQ"), "dQw4w9WgXcQ");
        assert_eq!(
            id("https://m.youtube.com/watch?v=dQw4w9WgXcQ"),
            "dQw4w9WgXcQ"
        );
        assert_eq!(
            id("https://music.youtube.com/watch?v=dQw4w9WgXcQ"),
            "dQw4w9WgXcQ"
        );
        assert_eq!(id("https://youtu.be/dQw4w9WgXcQ?si=abc"), "dQw4w9WgXcQ");
        assert_eq!(
            id("https://www.youtube.com/shorts/dQw4w9WgXcQ"),
            "dQw4w9WgXcQ"
        );
        assert_eq!(
            id("https://www.youtube.com/embed/dQw4w9WgXcQ"),
            "dQw4w9WgXcQ"
        );
        assert_eq!(
            id("https://www.youtube.com/live/dQw4w9WgXcQ"),
            "dQw4w9WgXcQ"
        );
        assert_eq!(id("dQw4w9WgXcQ"), "dQw4w9WgXcQ");
    }

    #[test]
    fn parses_start_time() {
        assert_eq!(
            parse("https://youtu.be/dQw4w9WgXcQ?t=90").unwrap().start,
            Some(90.0)
        );
        assert_eq!(
            parse("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=1m30s")
                .unwrap()
                .start,
            Some(90.0)
        );
    }

    #[test]
    fn rejects_non_youtube() {
        assert!(parse("https://vimeo.com/123").is_err());
        assert!(parse("https://www.youtube.com/feed/trending").is_err());
        assert!(parse("https://www.youtube.com/watch?v=short").is_err());
    }
}
