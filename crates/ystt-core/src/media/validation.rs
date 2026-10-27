//! Media signature validation.

use std::path::Path;

use tokio::fs::File;
use tokio::io::AsyncReadExt;

use crate::error::{Result, YsttError};
use crate::model::MediaKind;

pub(crate) async fn validate_download(path: &Path, kind: MediaKind) -> Result<()> {
    let mut file = File::open(path).await?;
    let mut head = [0u8; 16];
    let read = file.read(&mut head).await?;
    let head = &head[..read];

    if head.is_empty() {
        return Err(YsttError::Parse("empty media file".into()));
    }
    if head[0] == b'<'
        || head.starts_with(b"<!DOCTYPE")
        || head.starts_with(b"<html")
        || head.starts_with(b"<HTML")
    {
        return Err(YsttError::Parse(
            "server returned an HTML page instead of media".into(),
        ));
    }

    let valid = match kind {
        MediaKind::Video => is_mp4(head) || is_webm(head) || is_riff_avi(head),
        MediaKind::Photo => is_jpeg(head) || is_png(head) || is_webp(head) || is_gif(head),
        MediaKind::Audio => is_mp3(head) || is_mp4(head) || is_ogg(head) || is_wav(head),
    };

    if !valid {
        return Err(YsttError::Parse(format!(
            "media signature mismatch for {}",
            kind.as_str()
        )));
    }

    Ok(())
}

pub(crate) fn is_mp4(head: &[u8]) -> bool {
    head.len() >= 8 && &head[4..8] == b"ftyp"
}

pub(crate) fn is_webm(head: &[u8]) -> bool {
    head.starts_with(&[0x1A, 0x45, 0xDF, 0xA3])
}

pub(crate) fn is_riff_avi(head: &[u8]) -> bool {
    head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"AVI "
}

pub(crate) fn is_jpeg(head: &[u8]) -> bool {
    head.starts_with(&[0xFF, 0xD8, 0xFF])
}

pub(crate) fn is_png(head: &[u8]) -> bool {
    head.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])
}

pub(crate) fn is_webp(head: &[u8]) -> bool {
    head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP"
}

pub(crate) fn is_gif(head: &[u8]) -> bool {
    head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a")
}

pub(crate) fn is_mp3(head: &[u8]) -> bool {
    head.starts_with(b"ID3") || (head.len() >= 2 && head[0] == 0xFF && (head[1] & 0xE0) == 0xE0)
}

pub(crate) fn is_ogg(head: &[u8]) -> bool {
    head.starts_with(b"OggS")
}

pub(crate) fn is_wav(head: &[u8]) -> bool {
    head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WAVE"
}
