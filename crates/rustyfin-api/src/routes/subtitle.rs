use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;
use uuid::Uuid;

use rustyfin_core::subtitle::{
    OpenSubtitlesConfig, discover_external_subtitles, download_subtitle, is_image_subtitle,
    shift_vtt, srt_to_vtt, strip_ass_blocks,
};
use rustyfin_model::media_source::{MediaSourceResponse, MediaStream};

use crate::error::ApiError;
use crate::routes::streaming::{Version, select_version};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/Videos/{id}/Subtitles/{index}", get(get_subtitle))
        .route(
            "/Videos/{id}/Subtitles/{index}/Stream",
            get(get_subtitle_stream),
        )
        .route(
            "/Videos/{id}/{media_source_id}/Subtitles/{index}/Stream.{format}",
            get(source_subtitle_vtt),
        )
        .route(
            "/Videos/{id}/{media_source_id}/Subtitles/{index}/{start_ticks}/Stream.{format}",
            get(source_subtitle_vtt_from),
        )
}

/// Administrator-only subtitle routes. `routes::build_router` wraps them in `require_admin`.
pub fn admin_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/Items/{id}/RemoteSearch/Subtitles",
            get(search_remote_subtitles),
        )
        .route(
            "/Videos/{id}/Subtitles/{index}/Download",
            post(download_remote_subtitle),
        )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SubtitleQuery {
    /// If set to "vtt", SRT subtitles will be converted to WebVTT.
    pub format: Option<String>,
}

async fn get_subtitle(
    State(state): State<AppState>,
    Path((id, index)): Path<(Uuid, usize)>,
    Query(query): Query<SubtitleQuery>,
) -> Result<Response, ApiError> {
    let item = rustyfin_db::repo_media::get_item_by_id(&state.db_ro, id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("Item {id} not found")))?;

    let media_path = PathBuf::from(&item.path);
    let subs = discover_external_subtitles(&media_path);

    let sub = subs
        .into_iter()
        .find(|s| s.index == index)
        .ok_or_else(|| ApiError::NotFound(format!("Subtitle index {index} not found")))?;

    let content = tokio::fs::read_to_string(&sub.path)
        .await
        .map_err(|e| ApiError::Internal(format!("Cannot read subtitle: {e}")))?;

    let want_vtt = query
        .format
        .as_deref()
        .map(|f| f.eq_ignore_ascii_case("vtt"))
        .unwrap_or(false);

    if want_vtt && sub.format == "srt" {
        let vtt = srt_to_vtt(&content);
        Ok((
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/vtt; charset=utf-8")],
            vtt,
        )
            .into_response())
    } else {
        let ct = match sub.format.as_str() {
            "vtt" => "text/vtt; charset=utf-8",
            "srt" => "application/x-subrip; charset=utf-8",
            "ass" | "ssa" => "text/x-ssa; charset=utf-8",
            _ => "text/plain; charset=utf-8",
        };
        Ok((StatusCode::OK, [(header::CONTENT_TYPE, ct)], content).into_response())
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
#[allow(dead_code)]
struct RemoteSubtitleResult {
    pub id: String,
    pub name: String,
    pub language: String,
    pub format: String,
    pub download_count: Option<i64>,
}

/// GET /Videos/{id}/Subtitles/{index}/Stream – Stream subtitle file directly
async fn get_subtitle_stream(
    State(state): State<AppState>,
    Path((id, index)): Path<(Uuid, usize)>,
) -> Result<Response, ApiError> {
    let item = rustyfin_db::repo_media::get_item_by_id(&state.db_ro, id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("Item {id} not found")))?;

    let media_path = PathBuf::from(&item.path);
    let subs = discover_external_subtitles(&media_path);

    let sub = subs
        .into_iter()
        .find(|s| s.index == index)
        .ok_or_else(|| ApiError::NotFound(format!("Subtitle index {index} not found")))?;

    let content = tokio::fs::read(&sub.path)
        .await
        .map_err(|e| ApiError::Internal(format!("Cannot read subtitle: {e}")))?;

    let ct = match sub.format.as_str() {
        "vtt" => "text/vtt; charset=utf-8",
        "srt" => "application/x-subrip; charset=utf-8",
        "ass" | "ssa" => "text/x-ssa; charset=utf-8",
        _ => "text/plain; charset=utf-8",
    };

    let filename = sub
        .path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_default();

    let disposition = format!("attachment; filename=\"{}\"", filename);
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, ct),
            (header::CONTENT_DISPOSITION, &disposition),
        ],
        content,
    )
        .into_response())
}

/// GET /Videos/{id}/{media_source_id}/Subtitles/{index}/Stream.{format} – A subtitle as WebVTT
async fn source_subtitle_vtt(
    State(state): State<AppState>,
    Path((id, media_source_id, index, format)): Path<(Uuid, String, i32, String)>,
) -> Result<Response, ApiError> {
    subtitle_vtt(&state, id, &media_source_id, index, 0, &format).await
}

/// GET /Videos/{id}/{media_source_id}/Subtitles/{index}/{start_ticks}/Stream.{format} – The same
/// WebVTT, its cues moved back by `start_ticks`
async fn source_subtitle_vtt_from(
    State(state): State<AppState>,
    Path((id, media_source_id, index, start_ticks, format)): Path<(Uuid, String, i32, i64, String)>,
) -> Result<Response, ApiError> {
    subtitle_vtt(&state, id, &media_source_id, index, start_ticks, &format).await
}

/// The external subtitle files of `source` that its streams do not list, as subtitle streams
/// numbered after the highest stream index, in file-name order.
pub(crate) fn external_streams(source: &MediaSourceResponse) -> Vec<MediaStream> {
    let listed: Vec<PathBuf> = source
        .media_streams
        .iter()
        .filter_map(|stream| stream.path.as_deref().map(PathBuf::from))
        .collect();
    let first = source
        .media_streams
        .iter()
        .map(|stream| stream.index)
        .max()
        .unwrap_or(-1)
        + 1;
    let media_path = PathBuf::from(&source.path);
    discover_external_subtitles(&media_path)
        .into_iter()
        .filter(|file| !listed.contains(&file.path))
        .enumerate()
        .map(|(k, file)| MediaStream {
            index: first + k as i32,
            codec: Some(file.format),
            codec_type: "subtitle".to_string(),
            language: file.language,
            is_external: true,
            path: Some(file.path.to_string_lossy().into_owned()),
            r#type: Some("External".to_string()),
            ..Default::default()
        })
        .collect()
}

/// The WebVTT of subtitle stream `index` of media source `media_source_id` of item `id`, from
/// `start_ticks` on. An embedded stream is converted from the source's file, an external one from
/// its own file. The cache keeps the whole text; the cues move back after it is read.
async fn subtitle_vtt(
    state: &AppState,
    id: Uuid,
    media_source_id: &str,
    index: i32,
    start_ticks: i64,
    format: &str,
) -> Result<Response, ApiError> {
    if !format.eq_ignore_ascii_case("vtt") {
        return Err(ApiError::BadRequest(format!(
            "Subtitles are served as vtt, not {format}"
        )));
    }

    let sources =
        rustyfin_db::repo_media_source::get_media_sources_with_item_check(&state.db_ro, &id)
            .await
            .ok_or_else(|| ApiError::NotFound(format!("Item {id} not found")))?;
    let source = match select_version(id, &sources, Some(media_source_id))? {
        Version::Primary => sources.first(),
        Version::Source(i) => sources.get(i),
    }
    .ok_or_else(|| {
        ApiError::NotFound(format!(
            "Media source {media_source_id} not found for item {id}"
        ))
    })?;

    // The stream: an embedded one, or an external one, which must name its file
    let external = external_streams(source);
    let stream = source
        .media_streams
        .iter()
        .chain(&external)
        .find(|s| s.index == index && s.codec_type.eq_ignore_ascii_case("subtitle"))
        .filter(|s| !s.is_external || s.path.is_some())
        .ok_or_else(|| ApiError::NotFound(format!("Subtitle stream {index} not found")))?;

    if is_image_subtitle(stream.codec.as_deref().unwrap_or("")) {
        return Err(ApiError::BadRequest(format!(
            "Subtitle stream {index} is a picture subtitle, which has no text to convert"
        )));
    }

    // The input: the external file, or the source's file with the stream picked by its index
    let (input, map) = if stream.is_external {
        let path = stream.path.as_deref().unwrap_or_default();
        (PathBuf::from(path), "0:s:0".to_string())
    } else {
        (PathBuf::from(&source.path), format!("0:{index}"))
    };
    let meta = tokio::fs::metadata(&input).await.map_err(|e| {
        tracing::warn!("Cannot read the subtitle source {}: {e}", input.display());
        ApiError::Internal("Cannot read the subtitle source".to_string())
    })?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    let source_key: String = source
        .id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cache_dir = state.data_dir.join("cache").join("subtitle-vtt");
    let cache_file = cache_dir.join(format!("{source_key}-{index}-{}-{mtime}.vtt", meta.len()));

    // The whole WebVTT text, from the cache or from ffmpeg
    let text = match tokio::fs::read_to_string(&cache_file).await {
        Ok(text) => text,
        Err(_) => {
            let cp1252 = stream.is_external && is_cp1252(&input).await;
            let text = ffmpeg_vtt(&input, &map, cp1252).await?;
            // ffmpeg keeps the ASS override and comment blocks as text; no ASS renderer shows them
            let ass = stream.codec.as_deref().is_some_and(|codec| {
                codec.eq_ignore_ascii_case("ass") || codec.eq_ignore_ascii_case("ssa")
            });
            let text = if ass { strip_ass_blocks(&text) } else { text };
            if let Err(e) = write_cache(&cache_dir, &cache_file, &text).await {
                tracing::warn!("Cannot cache the subtitle at {}: {e}", cache_file.display());
            }
            text
        }
    };

    let vtt = if start_ticks > 0 {
        shift_vtt(&text, start_ticks)
    } else {
        text
    };
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/vtt; charset=utf-8")],
        vtt,
    )
        .into_response())
}

/// Runs ffmpeg to write subtitle stream `map` of `input` as WebVTT, reading the file as
/// Windows-1252 when `cp1252`. A failure is logged with ffmpeg's stderr; the 500 does not name
/// the file.
async fn ffmpeg_vtt(input: &std::path::Path, map: &str, cp1252: bool) -> Result<String, ApiError> {
    let mut command = tokio::process::Command::new("ffmpeg");
    command.args(["-nostdin", "-v", "error"]);
    if cp1252 {
        command.args(["-sub_charenc", "CP1252"]);
    }
    command
        .arg("-i")
        .arg(input)
        .args(["-map", map, "-f", "webvtt", "-"])
        .kill_on_drop(true);
    let stderr = match command.output().await {
        Ok(out) if out.status.success() => match String::from_utf8(out.stdout) {
            Ok(vtt) => return Ok(vtt),
            Err(_) => "ffmpeg wrote text that is not UTF-8".to_string(),
        },
        Ok(out) => String::from_utf8_lossy(&out.stderr).trim().to_string(),
        Err(e) => e.to_string(),
    };
    tracing::warn!("Cannot write {} as WebVTT: {stderr}", input.display());
    Err(ApiError::Internal(
        "Cannot convert the subtitle to WebVTT".to_string(),
    ))
}

/// Whether an external subtitle file is Windows-1252: its bytes are neither UTF-8 nor UTF-16,
/// which a byte order mark starts. A file that cannot be read is not.
async fn is_cp1252(file: &std::path::Path) -> bool {
    let Ok(bytes) = tokio::fs::read(file).await else {
        return false;
    };
    let utf16 = bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]);
    !utf16 && std::str::from_utf8(&bytes).is_err()
}

/// Writes a cache file through a temporary file beside it and a rename, so that a reader never
/// sees a part-written file.
async fn write_cache(
    dir: &std::path::Path,
    file: &std::path::Path,
    text: &str,
) -> std::io::Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    // A name of its own beside the file, so that concurrent writers do not collide
    let tmp = file.with_extension(format!("{}.tmp", Uuid::new_v4().simple()));
    let written = match tokio::fs::write(&tmp, text).await {
        Ok(()) => tokio::fs::rename(&tmp, file).await,
        Err(e) => Err(e),
    };
    if written.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    written
}

async fn search_remote_subtitles(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(query): Query<RemoteSubtitleQuery>,
) -> Result<Response, ApiError> {
    // Search for subtitles using OpenSubtitles or local cache
    let item = rustyfin_db::repo_media::get_item_by_id(&state.db_ro, id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("Item {id} not found")))?;

    let results = rustyfin_core::subtitle::search_remote_subtitles(
        &state.db_ro,
        &item.name,
        query.language.as_deref(),
        item.production_year,
    )
    .await?;

    Ok((StatusCode::OK, axum::Json(results)).into_response())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RemoteSubtitleQuery {
    #[serde(default)]
    pub language: Option<String>,
}

/// Request body for downloading a remote subtitle
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DownloadSubtitleRequest {
    /// OpenSubtitles subtitle ID
    pub subtitle_id: String,
}

/// POST /Videos/{id}/Subtitles/{index}/Download – Download and cache a remote subtitle
async fn download_remote_subtitle(
    State(state): State<AppState>,
    Path((_id, _index)): Path<(Uuid, usize)>,
    Json(req): Json<DownloadSubtitleRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let cache_dir = state.data_dir.join("cache").join("subtitles");

    let config = OpenSubtitlesConfig::default();
    match download_subtitle(&config, &req.subtitle_id, &cache_dir).await {
        Ok(path) => {
            tracing::info!("Subtitle downloaded to {}", path.display());
            Ok(Json(json!({
                "status": "ok",
                "path": path.to_string_lossy().to_string()
            })))
        }
        Err(e) => {
            tracing::warn!("Subtitle download failed: {}", e);
            Err(ApiError::Internal(e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustyfin_model::media_source::{MediaSourceResponse, MediaStream};

    /// A folder under the temp dir with empty files, removed when dropped.
    struct Folder(PathBuf);

    impl Folder {
        fn with(names: &[&str]) -> Folder {
            let dir =
                std::env::temp_dir().join(format!("rustyfin-b22-{}", Uuid::new_v4().simple()));
            std::fs::create_dir_all(&dir).unwrap();
            for name in names {
                std::fs::write(dir.join(name), "").unwrap();
            }
            Folder(dir)
        }

        fn path(&self, name: &str) -> String {
            self.0.join(name).to_str().unwrap().to_string()
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A source for `Film.mkv` in `folder` with `streams` (MediaStream JSON).
    fn source(folder: &Folder, streams: serde_json::Value) -> MediaSourceResponse {
        serde_json::from_value(json!({
            "Id": "b22-source",
            "Uid": "b22-source",
            "Protocol": "File",
            "Path": folder.path("Film.mkv"),
            "MediaStreams": streams,
        }))
        .unwrap()
    }

    /// A stream's index, codec, language and path.
    type Summary<'a> = (i32, Option<&'a str>, Option<&'a str>, Option<&'a str>);

    fn summary(streams: &[MediaStream]) -> Vec<Summary<'_>> {
        streams
            .iter()
            .map(|s| {
                (
                    s.index,
                    s.codec.as_deref(),
                    s.language.as_deref(),
                    s.path.as_deref(),
                )
            })
            .collect()
    }

    const FILES: [&str; 5] = [
        "Film.mkv",
        "Film.eng.srt",
        "Film.ger.ass",
        "Film.srt",
        "Other.srt",
    ];

    #[test]
    fn test_external_streams_follow_the_highest_index() {
        let folder = Folder::with(&FILES);
        let source = source(
            &folder,
            json!([
                { "Index": 0, "CodecType": "video", "Codec": "h264" },
                { "Index": 2, "CodecType": "subtitle", "Codec": "subrip" },
                { "Index": 1, "CodecType": "audio", "Codec": "aac" },
            ]),
        );
        let streams = external_streams(&source);
        let (eng, ger, plain) = (
            folder.path("Film.eng.srt"),
            folder.path("Film.ger.ass"),
            folder.path("Film.srt"),
        );
        assert_eq!(
            summary(&streams),
            vec![
                (3, Some("srt"), Some("eng"), Some(eng.as_str())),
                (4, Some("ass"), Some("ger"), Some(ger.as_str())),
                (5, Some("srt"), None, Some(plain.as_str())),
            ]
        );
        for stream in &streams {
            assert_eq!(stream.codec_type, "subtitle");
            assert_eq!(stream.r#type.as_deref(), Some("External"));
            assert!(stream.is_external);
            assert!(!stream.is_default && !stream.is_forced);
            assert_eq!(stream.title, None);
        }
    }

    #[test]
    fn test_external_streams_skip_files_the_source_lists() {
        let folder = Folder::with(&FILES);
        let source = source(
            &folder,
            json!([
                { "Index": 0, "CodecType": "video", "Codec": "h264" },
                {
                    "Index": 7,
                    "CodecType": "subtitle",
                    "Codec": "srt",
                    "IsExternal": true,
                    "Path": folder.path("Film.eng.srt"),
                },
            ]),
        );
        let (ger, plain) = (folder.path("Film.ger.ass"), folder.path("Film.srt"));
        assert_eq!(
            summary(&external_streams(&source)),
            vec![
                (8, Some("ass"), Some("ger"), Some(ger.as_str())),
                (9, Some("srt"), None, Some(plain.as_str())),
            ]
        );
    }

    #[test]
    fn test_external_streams_of_a_source_without_streams_start_at_zero() {
        let folder = Folder::with(&FILES);
        let indexes: Vec<i32> = external_streams(&source(&folder, json!([])))
            .iter()
            .map(|s| s.index)
            .collect();
        assert_eq!(indexes, vec![0, 1, 2]);

        // No subtitle files: nothing
        let bare = Folder::with(&["Film.mkv"]);
        assert!(external_streams(&source(&bare, json!([]))).is_empty());
    }
}
