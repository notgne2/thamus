use std::collections::HashMap;
use std::sync::LazyLock;

const EXT_TO_MIME: &[(&str, &str)] = &[
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("png", "image/png"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("svg", "image/svg+xml"),
    ("mp3", "audio/mpeg"),
    ("wav", "audio/wav"),
    ("ogg", "audio/ogg"),
    ("flac", "audio/flac"),
    ("opus", "audio/opus"),
    ("mp4", "video/mp4"),
    ("webm", "video/webm"),
    ("mov", "video/quicktime"),
    ("avi", "video/x-msvideo"),
    ("mkv", "video/x-matroska"),
    ("pdf", "application/pdf"),
];

static MIME_TO_EXT: LazyLock<HashMap<&str, &str>> = LazyLock::new(|| {
    EXT_TO_MIME
        .iter()
        .rev()
        .map(|(ext, mime)| (*mime, *ext))
        .collect()
});

#[must_use]
pub fn mime_for_ext(ext: &str, hint: &str) -> String {
    if let Some(mime) = EXT_TO_MIME.iter().find(|(e, _)| *e == ext).map(|(_, m)| *m) {
        return mime.to_string();
    }
    match hint {
        "image" => format!("image/{ext}"),
        "audio" => format!("audio/{ext}"),
        _ => "application/octet-stream".to_string(),
    }
}

#[must_use]
pub fn ext_for_mime(mime: &str) -> Option<&str> {
    MIME_TO_EXT.get(mime).copied()
}

#[must_use]
pub fn type_for_mime(mime: &str) -> &str {
    if mime.starts_with("image/") {
        "image"
    } else if mime.starts_with("audio/") {
        "audio"
    } else if mime.starts_with("video/") {
        "video"
    } else if mime.starts_with("application/") {
        match mime {
            "application/pdf" => "pdf",
            _ => "file",
        }
    } else {
        mime.split('/').next().unwrap_or("file")
    }
}

#[must_use]
pub fn ext_format_supported(ext: &str) -> bool {
    matches!(
        ext,
        // "jpg" | "jpeg" | "png" | "gif" | "webp" | "svg" | "mp3" | "wav" | "ogg" | "flac" | "opus"
        // "jpg" | "jpeg" | "png" | "gif" | "webp" | "svg" | "pdf"
        "jpg" | "jpeg" | "png" | "gif" | "svg" | "pdf"
    )
}
