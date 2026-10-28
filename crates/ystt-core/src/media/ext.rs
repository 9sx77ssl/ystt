//! Filename, extension and MIME helpers for downloaded media.

use crate::model::MediaKind;

/// Lowercase extension from a URL path, if it is safe.
pub(crate) fn ext_from_url(url: &str, fallback: &str) -> String {
    let Ok(url) = url::Url::parse(url) else {
        return fallback.to_owned();
    };
    let Some(segment) = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
    else {
        return fallback.to_owned();
    };
    let Some((_, ext)) = segment.rsplit_once('.') else {
        return fallback.to_owned();
    };
    let ext = ext.to_ascii_lowercase();
    if !ext.is_empty()
        && ext
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
    {
        ext
    } else {
        fallback.to_owned()
    }
}

/// Extension for a video URL.
pub(crate) fn video_ext(url: &str) -> String {
    match ext_from_url(url, "mp4").as_str() {
        "mp4" | "webm" | "mov" | "m4v" => ext_from_url(url, "mp4"),
        _ => "mp4".into(),
    }
}

/// Extension for a photo URL.
pub(crate) fn photo_ext(url: &str) -> String {
    match ext_from_url(url, "jpg").as_str() {
        "png" => "png".into(),
        "webp" => "webp".into(),
        "gif" => "gif".into(),
        "jpeg" | "jpg" => "jpg".into(),
        _ => "jpg".into(),
    }
}

/// Extension for an audio URL.
pub(crate) fn audio_ext(url: &str) -> String {
    match ext_from_url(url, "mp3").as_str() {
        "mp3" | "m4a" | "aac" | "ogg" | "opus" => ext_from_url(url, "mp3"),
        _ => "mp3".into(),
    }
}

/// MIME type for an extension and media kind.
pub(crate) fn mime_for_ext(ext: &str, kind: MediaKind) -> String {
    let mime = match ext {
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "mp3" => "audio/mpeg",
        "m4a" | "aac" => "audio/mp4",
        "ogg" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        _ => match kind {
            MediaKind::Video => "video/mp4",
            MediaKind::Photo => "image/jpeg",
            MediaKind::Audio => "audio/mpeg",
        },
    };
    mime.to_owned()
}

/// Sanitize a generated filename for all supported operating systems.
pub fn sanitize_filename(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_control()
            || matches!(
                character,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
            )
        {
            sanitized.push('_');
        } else {
            sanitized.push(character);
        }
    }

    let sanitized = sanitized.trim().trim_matches('.').trim();
    let mut sanitized = if sanitized.is_empty() {
        "tiktok".to_owned()
    } else {
        sanitized.chars().take(180).collect()
    };

    let base = sanitized
        .split('.')
        .next()
        .unwrap_or("tiktok")
        .to_ascii_uppercase();
    if matches!(
        base.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    ) {
        sanitized.insert(0, '_');
    }

    sanitized
}

/// Render a per-media filename. `{author}`, `{post_id}`, `{quality}`, `{ext}`,
/// `{kind}` and `{index}` are supported. The extension is appended once unless
/// the template contains `{ext}`.
pub(crate) fn media_filename(
    template: Option<&str>,
    author_id: &str,
    post_id: &str,
    quality: &str,
    kind: MediaKind,
    index: Option<usize>,
    ext: &str,
) -> String {
    let default = match kind {
        MediaKind::Video => "tiktok_{author}_{post_id}",
        MediaKind::Photo => "tiktok_{author}_{post_id}_photo_{index}",
        MediaKind::Audio => "tiktok_{author}_{post_id}_audio",
    };
    let template = template
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(default);
    let has_kind = template.contains("{kind}");
    let has_index = template.contains("{index}");
    let has_ext = template.contains("{ext}");
    let index = index.unwrap_or(1).to_string();
    let mut rendered = template
        .replace("{author}", author_id)
        .replace("{post_id}", post_id)
        .replace("{quality}", quality)
        .replace("{kind}", kind.as_str())
        .replace("{index}", &index);
    if kind == MediaKind::Photo && !has_kind && !has_index {
        rendered.push_str(&format!("_photo_{index}"));
    }
    if kind == MediaKind::Audio && !has_kind && !has_index {
        rendered.push_str("_audio");
    }
    if has_ext {
        rendered = rendered.replace("{ext}", ext);
    } else {
        rendered.push('.');
        rendered.push_str(ext);
    }
    sanitize_filename(&rendered)
}
