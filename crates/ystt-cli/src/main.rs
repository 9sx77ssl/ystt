//! Minimal `ystt` command-line interface.
//!
//! The library is the product; this binary exists for quick manual downloads and
//! metadata inspection. Only flags that change user-visible behaviour are kept.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use futures_util::{StreamExt, stream};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tracing_subscriber::EnvFilter;
use ystt_core::{
    BatchOptions, BatchResult, DownloadOptions, DownloadResult, FilenameCollision, ProgressEvent,
    ProgressReporter, QualityPolicy, SoundCloudAudio, SoundCloudCache, SoundCloudEntity,
    SoundCloudPlaylist, Ystt, YsttError, is_soundcloud_url, normalize_soundcloud_url,
    sanitize_filename,
};

#[derive(Debug, Clone, Parser)]
#[command(
    name = "ystt",
    version,
    about = "Fast TikTok + SoundCloud downloader",
    long_about = "Download public TikTok videos/galleries/music and public SoundCloud tracks/playlists. No account cookies, no proxies, no mobile API, no ffmpeg."
)]
struct Cli {
    /// TikTok or SoundCloud URL.
    #[arg(required_unless_present = "urls_file")]
    url: Option<String>,

    /// File with one TikTok/SoundCloud URL per line (batch mode).
    #[arg(long, value_name = "FILE", conflicts_with_all = ["url", "info"])]
    urls_file: Option<PathBuf>,

    /// Batch concurrency (default 4, max 64).
    #[arg(
        long,
        default_value_t = 4,
        requires = "urls_file",
        conflicts_with = "url"
    )]
    concurrency: usize,

    /// Output directory.
    #[arg(short = 'o', long, default_value = "downloads")]
    output: PathBuf,

    /// Print metadata and do not download.
    #[arg(long)]
    info: bool,

    /// Print JSON instead of human-readable text.
    #[arg(long)]
    json: bool,

    /// Emit unified progress events as JSON lines on stderr.
    #[arg(long)]
    progress: bool,

    /// Media quality policy (TikTok video / SoundCloud audio).
    #[arg(long, value_enum, default_value_t = QualityArg::Balanced)]
    quality: QualityArg,

    /// Output filename template: {author}/{artist}, {post_id}, {title},
    /// {quality}, {ext}, {kind}, {index}.
    #[arg(long, value_name = "TEMPLATE")]
    filename: Option<String>,

    /// Existing output collision policy.
    #[arg(long, value_enum, default_value_t = CollisionArg::Overwrite)]
    collision: CollisionArg,

    /// Download the post's music track too (TikTok; SoundCloud is audio-only).
    #[arg(long)]
    audio: bool,

    /// Resume interrupted videos from `.part` files.
    #[arg(long)]
    resume: bool,

    /// Do not use the content-addressed file cache or metadata cache.
    #[arg(long)]
    no_cache: bool,

    /// Shared cache root for file and metadata caches across outputs.
    #[arg(long, value_name = "DIR")]
    cache_dir: Option<PathBuf>,

    /// Total request timeout in seconds.
    #[arg(long, value_name = "SECONDS")]
    timeout: Option<u64>,

    /// HTTP and media retry budget.
    #[arg(long, value_name = "N")]
    retries: Option<u32>,

    /// Browser TLS/HTTP2 fingerprint profile.
    #[arg(long, value_name = "PROFILE")]
    browser: Option<String>,

    /// Suppress informational tracing output.
    #[arg(long)]
    quiet: bool,

    /// Optional TOML or JSON configuration file.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    output: Option<PathBuf>,
    quality: Option<QualityArg>,
    filename: Option<String>,
    collision: Option<CollisionArg>,
    audio: Option<bool>,
    resume: Option<bool>,
    no_cache: Option<bool>,
    cache_dir: Option<PathBuf>,
    concurrency: Option<usize>,
    timeout: Option<u64>,
    retries: Option<u32>,
    browser: Option<String>,
    quiet: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
enum QualityArg {
    Best,
    Compatible,
    Balanced,
    Efficient,
    Fast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
enum CollisionArg {
    Overwrite,
    Suffix,
    Fail,
}

impl From<CollisionArg> for FilenameCollision {
    fn from(value: CollisionArg) -> Self {
        match value {
            CollisionArg::Overwrite => Self::Overwrite,
            CollisionArg::Suffix => Self::Suffix,
            CollisionArg::Fail => Self::Fail,
        }
    }
}

impl From<QualityArg> for QualityPolicy {
    fn from(value: QualityArg) -> Self {
        match value {
            QualityArg::Best => Self::Best,
            QualityArg::Compatible => Self::Compatible,
            QualityArg::Balanced => Self::Balanced,
            QualityArg::Efficient => Self::Efficient,
            QualityArg::Fast => Self::Fast,
        }
    }
}

#[tokio::main]
async fn main() {
    let mut cli = Cli::parse();
    if let Some(path) = cli.config.clone()
        && let Err(error) = apply_config(&mut cli, &path)
    {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
    let json = cli.json;
    init_tracing(cli.quiet);

    if let Err(error) = run(cli).await {
        if json {
            match serde_json::to_string(&error.info()) {
                Ok(payload) => eprintln!("{payload}"),
                Err(_) => eprintln!("error: {error}"),
            }
        } else {
            eprintln!("error: {error}");
        }
        std::process::exit(1);
    }
}

fn apply_config(cli: &mut Cli, path: &std::path::Path) -> Result<(), YsttError> {
    let text = std::fs::read_to_string(path)?;
    let config: FileConfig = if path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
    {
        serde_json::from_str(&text)
            .map_err(|error| YsttError::Config(format!("invalid JSON config: {error}")))?
    } else {
        toml::from_str(&text)
            .map_err(|error| YsttError::Config(format!("invalid TOML config: {error}")))?
    };
    if cli.output == std::path::Path::new("downloads")
        && let Some(value) = config.output
    {
        cli.output = value;
    }
    if cli.quality == QualityArg::Balanced
        && let Some(value) = config.quality
    {
        cli.quality = value;
    }
    if cli.filename.is_none() {
        cli.filename = config.filename;
    }
    if cli.collision == CollisionArg::Overwrite
        && let Some(value) = config.collision
    {
        cli.collision = value;
    }
    if !cli.audio {
        cli.audio = config.audio.unwrap_or(false);
    }
    if !cli.resume {
        cli.resume = config.resume.unwrap_or(false);
    }
    if !cli.no_cache {
        cli.no_cache = config.no_cache.unwrap_or(false);
    }
    if cli.cache_dir.is_none() {
        cli.cache_dir = config.cache_dir;
    }
    if cli.concurrency == 4
        && let Some(value) = config.concurrency
    {
        cli.concurrency = value.max(1);
    }
    if cli.timeout.is_none() {
        cli.timeout = config.timeout;
    }
    if cli.retries.is_none() {
        cli.retries = config.retries;
    }
    if cli.browser.is_none() {
        cli.browser = config.browser;
    }
    if !cli.quiet {
        cli.quiet = config.quiet.unwrap_or(false);
    }
    Ok(())
}

async fn run(cli: Cli) -> Result<(), YsttError> {
    let mut builder = Ystt::builder()
        .output_dir(&cli.output)
        .quality(cli.quality.into())
        .include_audio(cli.audio)
        .resume(cli.resume)
        .file_cache(!cli.no_cache)
        .metadata_cache(!cli.no_cache)
        .filename_collision(cli.collision.into());
    if let Some(cache_dir) = &cli.cache_dir {
        builder = builder.cache_dir(cache_dir);
    }
    if let Some(template) = &cli.filename {
        builder = builder.filename_template(template);
    }
    if let Some(seconds) = cli.timeout {
        builder = builder.timeout(Duration::from_secs(seconds));
    }
    if let Some(retries) = cli.retries {
        builder = builder.retries(retries);
    }
    if let Some(browser) = &cli.browser {
        let profile = browser
            .parse::<ystt_core::BrowserProfile>()
            .map_err(YsttError::Config)?;
        builder = builder.browser(profile);
    }
    let ystt = builder.build()?;
    let soundcloud_cache = (!cli.no_cache).then(|| {
        let root = cli
            .cache_dir
            .clone()
            .unwrap_or_else(|| cli.output.join(".ystt-cache"))
            .join("soundcloud");
        SoundCloudCache::new(root, Duration::from_secs(24 * 60 * 60))
    });

    let reporter = cli_reporter(cli.progress);
    let mut options = DownloadOptions::new()
        .output_dir(&cli.output)
        .quality(cli.quality.into())
        .resume(cli.resume)
        .file_cache(!cli.no_cache)
        .metadata_cache(!cli.no_cache)
        .filename_collision(cli.collision.into())
        .media_retries(cli.retries.unwrap_or(2));
    if let Some(template) = &cli.filename {
        options = options.filename_template(template);
    }
    if let Some(reporter) = reporter.clone() {
        options = options.progress(reporter);
    }

    if let Some(path) = &cli.urls_file {
        let urls = read_urls_file(path)?;
        if urls.iter().any(|url| is_soundcloud_url(url)) {
            run_mixed_batch(
                &ystt,
                &cli,
                urls,
                options,
                reporter,
                soundcloud_cache.as_ref(),
            )
            .await?;
            return Ok(());
        }
        let mut batch_options = BatchOptions::new()
            .output_dir(&cli.output)
            .quality(cli.quality.into())
            .include_audio(cli.audio)
            .resume(cli.resume)
            .file_cache(!cli.no_cache)
            .metadata_cache(!cli.no_cache)
            .filename_collision(cli.collision.into())
            .concurrency(cli.concurrency)
            .item_retries(cli.retries.unwrap_or(2));
        if let Some(seconds) = cli.timeout {
            batch_options = batch_options.item_timeout(Duration::from_secs(seconds));
        }
        if let Some(template) = &cli.filename {
            batch_options = batch_options.filename_template(template);
        }
        if let Some(reporter) = reporter {
            batch_options = batch_options.progress(reporter);
        }
        let batch = ystt.download_many(urls, batch_options).await;
        if cli.json {
            print_json(&batch)?;
        } else {
            print_batch_human(&batch);
        }
        return Ok(());
    }

    let url = cli
        .url
        .as_deref()
        .ok_or_else(|| YsttError::InvalidUrl("missing URL".into()))?;
    if is_soundcloud_url(url) {
        if cli.info {
            let entity = ystt.resolve_soundcloud_with(url, &options).await?;
            if cli.json {
                print_json(&entity)?;
            } else {
                print_soundcloud_info(&entity);
            }
            return Ok(());
        }
        let value =
            download_soundcloud_url(&ystt, &cli, url, options, soundcloud_cache.as_ref()).await?;
        if cli.json {
            print_json(&value)?;
        } else {
            print_soundcloud_value(&value);
        }
        return Ok(());
    }
    if cli.info {
        let info = ystt.resolve_with(url, &options).await?;
        if cli.json {
            print_json(&info)?;
        } else {
            print_info(&info);
        }
        return Ok(());
    }

    let result = ystt.download_with(url, options).await?;
    if cli.json {
        print_json(&result)?;
    } else {
        print_human(&result);
    }
    Ok(())
}

async fn run_mixed_batch(
    ystt: &Ystt,
    cli: &Cli,
    urls: Vec<String>,
    options: DownloadOptions,
    reporter: Option<ProgressReporter>,
    cache: Option<&SoundCloudCache>,
) -> Result<(), YsttError> {
    let concurrency = cli.concurrency.clamp(1, 64);
    let total = urls.len();
    let mut results: Vec<Option<Result<serde_json::Value, YsttError>>> =
        (0..total).map(|_| None).collect();
    let mut downloads = stream::iter(urls.into_iter().enumerate().map(|(index, url)| {
        let ystt = ystt.clone();
        let cli = cli.clone();
        let mut item_options = options.clone();
        item_options.progress = None;
        async move {
            let result = if is_soundcloud_url(&url) {
                download_soundcloud_url(&ystt, &cli, &url, item_options, cache).await
            } else {
                ystt.download_with(&url, item_options)
                    .await
                    .and_then(|result| {
                        serde_json::to_value(&result)
                            .map_err(|error| YsttError::Parse(error.to_string()))
                    })
                    .map(|mut value| {
                        if let Some(object) = value.as_object_mut() {
                            object.insert("provider".into(), serde_json::json!("tiktok"));
                        }
                        value
                    })
            };
            (index, url, result)
        }
    }))
    .buffer_unordered(concurrency);
    while let Some((index, url, result)) = downloads.next().await {
        results[index] = Some(match result {
            Ok(value) => Ok(value),
            Err(error) => Err(match error {
                YsttError::Parse(_) => error,
                other => {
                    eprintln!("error: {url} {other}");
                    other
                }
            }),
        });
    }
    drop(downloads);

    let mut successes = 0usize;
    let mut failures = 0usize;
    let mut values = Vec::with_capacity(total);
    for result in results.into_iter().flatten() {
        match result {
            Ok(value) => {
                successes += 1;
                if !cli.json {
                    print_soundcloud_value(&value);
                }
                values.push(value);
            }
            Err(error) => {
                failures += 1;
                if cli.json {
                    values.push(serde_json::json!({
                        "provider": "error",
                        "error": error.info(),
                    }));
                }
            }
        }
    }
    if let Some(reporter) = reporter {
        let _ = reporter;
    }
    if cli.json {
        print_json(&values)?;
    } else {
        println!("batch: ok {successes} | failed {failures}");
    }
    Ok(())
}

async fn download_soundcloud_url(
    ystt: &Ystt,
    cli: &Cli,
    url: &str,
    options: DownloadOptions,
    cache: Option<&SoundCloudCache>,
) -> Result<serde_json::Value, YsttError> {
    let canonical = normalize_soundcloud_url(url)?;
    let entity = ystt.resolve_soundcloud_with(&canonical, &options).await?;
    match entity {
        SoundCloudEntity::Track(track) => {
            let audio =
                soundcloud_track_cached(ystt, &track, &options, cache, cli.quality.into()).await?;
            let (path, cover) = write_soundcloud_audio(
                &cli.output,
                cli.filename.as_deref(),
                cli.quality.into(),
                cli.collision.into(),
                &audio,
            )?;
            Ok(soundcloud_track_value(&audio, &path, cover.as_deref()))
        }
        SoundCloudEntity::Playlist(playlist) => {
            let track_count = playlist.tracks.len();
            let mut results: Vec<Option<Result<serde_json::Value, YsttError>>> =
                (0..track_count).map(|_| None).collect();
            let concurrency = cli.concurrency.clamp(1, 8);
            let output_dir = cli.output.clone();
            let template = cli.filename.clone();
            let quality = cli.quality.into();
            let collision = cli.collision.into();
            let mut downloads = stream::iter(playlist.tracks.iter().cloned().enumerate().map(
                |(index, track)| {
                    let ystt = ystt.clone();
                    let options = options.clone();
                    let output_dir = output_dir.clone();
                    let template = template.clone();
                    async move {
                        let result =
                            soundcloud_track_cached(&ystt, &track, &options, cache, quality)
                                .await
                                .and_then(|audio| {
                                    let (path, cover) = write_soundcloud_audio(
                                        &output_dir,
                                        template.as_deref(),
                                        quality,
                                        collision,
                                        &audio,
                                    )?;
                                    Ok(soundcloud_track_value(&audio, &path, cover.as_deref()))
                                });
                        (index, result)
                    }
                },
            ))
            .buffer_unordered(concurrency);
            while let Some((index, result)) = downloads.next().await {
                results[index] = Some(result);
            }
            drop(downloads);
            let mut tracks = Vec::with_capacity(track_count);
            for result in results.into_iter().flatten() {
                tracks.push(result?);
            }
            Ok(serde_json::json!({
                "provider": "soundcloud",
                "type": "playlist",
                "id": playlist.id,
                "title": playlist.title,
                "artist": playlist.artist,
                "page_url": playlist.permalink_url,
                "artwork_url": playlist.artwork(),
                "track_count": track_count,
                "tracks": tracks,
            }))
        }
    }
}

async fn soundcloud_track_cached(
    ystt: &Ystt,
    track: &ystt_core::SoundCloudTrack,
    options: &DownloadOptions,
    cache: Option<&SoundCloudCache>,
    quality: QualityPolicy,
) -> Result<SoundCloudAudio, YsttError> {
    if let Some(cache) = cache
        && let Some(audio) = cache.get(&track.id, quality).await
    {
        return Ok(audio);
    }
    let audio = ystt
        .download_soundcloud_resolved_track(track, options)
        .await?;
    if let Some(cache) = cache
        && let Err(error) = cache.put(&audio, quality).await
    {
        eprintln!("soundcloud cache write failed: {error}");
    }
    Ok(audio)
}

fn unique_soundcloud_path(
    path: PathBuf,
    collision: FilenameCollision,
) -> Result<PathBuf, YsttError> {
    if collision == FilenameCollision::Overwrite || !path.exists() {
        return Ok(path);
    }
    if collision == FilenameCollision::Fail {
        return Err(YsttError::FileExists(path));
    }
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("download");
    for counter in 1..=10_000u32 {
        let candidate = path.with_file_name(if extension.is_empty() {
            format!("{stem}_{counter}")
        } else {
            format!("{stem}_{counter}.{extension}")
        });
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(YsttError::Config(
        "could not find a free output filename".into(),
    ))
}

fn write_soundcloud_audio(
    output_dir: &std::path::Path,
    template: Option<&str>,
    quality: QualityPolicy,
    collision: FilenameCollision,
    audio: &SoundCloudAudio,
) -> Result<(PathBuf, Option<PathBuf>), YsttError> {
    std::fs::create_dir_all(output_dir)?;
    let template = template.unwrap_or("{artist} - {title}");
    let mut name = template
        .replace("{author}", &audio.track.artist)
        .replace("{artist}", &audio.track.artist)
        .replace("{post_id}", &audio.track.id)
        .replace("{title}", &audio.track.title)
        .replace("{quality}", quality.as_str());
    name = sanitize_filename(&name);
    if name.is_empty() {
        name = format!("soundcloud_{}", audio.track.id);
    }
    let path = unique_soundcloud_path(output_dir.join(format!("{name}.{}", audio.ext)), collision)?;
    std::fs::write(&path, &audio.bytes)?;
    let cover = audio.artwork.as_ref().map(|artwork| {
        let extension = match artwork.mime.as_str() {
            "image/png" => "png",
            "image/webp" => "webp",
            _ => "jpg",
        };
        (path.with_extension(extension), artwork)
    });
    let cover_path = match cover {
        Some((cover_path, artwork)) => {
            std::fs::write(&cover_path, &artwork.bytes)?;
            Some(cover_path)
        }
        None => None,
    };
    Ok((path, cover_path))
}

fn soundcloud_track_value(
    audio: &SoundCloudAudio,
    path: &std::path::Path,
    cover: Option<&std::path::Path>,
) -> serde_json::Value {
    serde_json::json!({
        "provider": "soundcloud",
        "type": "track",
        "id": audio.track.id,
        "title": audio.track.title,
        "artist": audio.track.artist,
        "duration_sec": audio.duration_sec(),
        "artwork_url": audio.artwork_url(),
        "page_url": audio.track.permalink_url,
        "artwork_path": cover.map(|path| path.display().to_string()),
        "audio": {
            "path": path.display().to_string(),
            "size": audio.bytes.len(),
            "mime": audio.mime,
            "ext": audio.ext,
            "preset": audio.preset,
            "sha256": sha256_hex(&audio.bytes),
        }
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn print_soundcloud_info(entity: &SoundCloudEntity) {
    match entity {
        SoundCloudEntity::Track(track) => {
            println!("provider: soundcloud");
            println!("type: track");
            println!("track: {} ({})", track.id, track.title);
            println!("artist: {}", track.artist);
            println!("duration: {}s", track.duration_sec().unwrap_or(0));
            println!("page: {}", track.permalink_url);
            println!("artwork: {}", track.artwork());
            println!("transcodings: {}", track.transcodings.len());
            for transcoding in &track.transcodings {
                println!(
                    "stream: preset={} protocol={} mime={}",
                    transcoding.preset.as_deref().unwrap_or("-"),
                    transcoding.protocol.as_deref().unwrap_or("-"),
                    transcoding.mime_type.as_deref().unwrap_or("-")
                );
            }
        }
        SoundCloudEntity::Playlist(playlist) => print_soundcloud_playlist(playlist),
    }
}

fn print_soundcloud_playlist(playlist: &SoundCloudPlaylist) {
    println!("provider: soundcloud");
    println!("type: playlist");
    println!("playlist: {} ({})", playlist.id, playlist.title);
    println!("artist: {}", playlist.artist);
    println!("duration: {}s", playlist.duration_sec().unwrap_or(0));
    println!("page: {}", playlist.permalink_url);
    println!("artwork: {}", playlist.artwork());
    println!("tracks: {}", playlist.tracks.len());
    for (index, track) in playlist.tracks.iter().enumerate() {
        println!(
            "  {}. {} — {} ({}s)",
            index + 1,
            track.artist,
            track.title,
            track.duration_sec().unwrap_or(0)
        );
    }
}

fn print_soundcloud_value(value: &serde_json::Value) {
    if value.get("provider").and_then(|provider| provider.as_str()) == Some("tiktok") {
        println!(
            "post: {}",
            value
                .get("post_id")
                .and_then(|value| value.as_str())
                .unwrap_or("-")
        );
        println!(
            "author: @{}",
            value
                .pointer("/author/id")
                .and_then(|value| value.as_str())
                .unwrap_or("-")
        );
        if let Some(video) = value.get("video") {
            println!(
                "video: {} ({})",
                video
                    .get("path")
                    .and_then(|value| value.as_str())
                    .unwrap_or("-"),
                human_size(
                    video
                        .get("size")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(0)
                )
            );
        }
        if let Some(audio) = value.get("audio") {
            println!(
                "audio: {} ({})",
                audio
                    .get("path")
                    .and_then(|value| value.as_str())
                    .unwrap_or("-"),
                human_size(
                    audio
                        .get("size")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(0)
                )
            );
        }
        return;
    }
    let kind = value
        .get("type")
        .and_then(|kind| kind.as_str())
        .unwrap_or("result");
    if kind == "playlist" {
        println!(
            "playlist: {} ({})",
            value
                .get("title")
                .and_then(|value| value.as_str())
                .unwrap_or("-"),
            value
                .get("id")
                .and_then(|value| value.as_str())
                .unwrap_or("-")
        );
        if let Some(tracks) = value.get("tracks").and_then(|tracks| tracks.as_array()) {
            for (index, track) in tracks.iter().enumerate() {
                let title = track
                    .get("title")
                    .and_then(|value| value.as_str())
                    .unwrap_or("-");
                let artist = track
                    .get("artist")
                    .and_then(|value| value.as_str())
                    .unwrap_or("-");
                let path = track
                    .pointer("/audio/path")
                    .and_then(|value| value.as_str())
                    .unwrap_or("-");
                let size = track
                    .pointer("/audio/size")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0);
                println!(
                    "  {}. {} — {} ({} bytes) {}",
                    index + 1,
                    artist,
                    title,
                    size,
                    path
                );
            }
        }
    } else {
        println!(
            "track: {} — {}",
            value
                .get("artist")
                .and_then(|value| value.as_str())
                .unwrap_or("-"),
            value
                .get("title")
                .and_then(|value| value.as_str())
                .unwrap_or("-")
        );
        if let Some(audio) = value.get("audio") {
            println!(
                "audio: {} ({})",
                audio
                    .get("path")
                    .and_then(|value| value.as_str())
                    .unwrap_or("-"),
                human_size(
                    audio
                        .get("size")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(0)
                )
            );
            if let Some(hash) = audio.get("sha256").and_then(|value| value.as_str()) {
                println!("sha256: {hash}");
            }
        }
    }
}

fn read_urls_file(path: &std::path::Path) -> Result<Vec<String>, YsttError> {
    const MAX_URL_FILE_BYTES: u64 = 8 * 1024 * 1024;
    const MAX_URLS: usize = 10_000;
    let metadata = std::fs::metadata(path)?;
    if metadata.len() > MAX_URL_FILE_BYTES {
        return Err(YsttError::Config(format!(
            "URL file is larger than {MAX_URL_FILE_BYTES} bytes"
        )));
    }
    let text = std::fs::read_to_string(path)?;
    let urls = parse_urls(&text);
    if urls.is_empty() {
        return Err(YsttError::Config("URL file is empty".into()));
    }
    if urls.len() > MAX_URLS {
        return Err(YsttError::Config(format!(
            "URL file contains more than {MAX_URLS} URLs"
        )));
    }
    Ok(urls)
}

fn cli_reporter(enabled: bool) -> Option<ProgressReporter> {
    enabled.then(|| {
        Arc::new(|event: ProgressEvent| match serde_json::to_string(&event) {
            Ok(line) => eprintln!("{line}"),
            Err(error) => eprintln!("progress serialization error: {error}"),
        }) as ProgressReporter
    })
}

fn parse_urls(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

fn print_batch_human(batch: &BatchResult) {
    println!(
        "batch: total {} ms | ok {} | failed {}",
        batch.total_ms,
        batch.successes(),
        batch.failures()
    );
    for item in &batch.items {
        if let Some(result) = &item.result {
            println!("ok: {} ({} ms)", item.url, result.timings.total_ms);
        } else if let Some(error) = &item.error {
            println!("error: {} {} ({})", item.url, error.code, error.message);
        }
    }
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<(), YsttError> {
    let json =
        serde_json::to_string_pretty(value).map_err(|error| YsttError::Parse(error.to_string()))?;
    println!("{json}");
    Ok(())
}

fn print_info(info: &ystt_core::MediaInfo) {
    println!("post: {}", info.post_id);
    println!("author: @{}", info.author.id);
    println!("page: {}", info.page_url);
    println!("video: {}", info.video_url.as_deref().unwrap_or("-"));
    if let Some(width) = info.video_width
        && let Some(height) = info.video_height
    {
        println!("video_size: {width}x{height}");
    }
    if let Some(duration) = info.duration_sec {
        println!("duration: {duration}s");
    }
    for bitrate in &info.bitrates {
        println!(
            "bitrate: {} bps, codec={}, size={}x{}, url={}",
            bitrate.bitrate,
            bitrate.codec.as_deref().unwrap_or("-"),
            bitrate.width.map_or("-".to_owned(), |v| v.to_string()),
            bitrate.height.map_or("-".to_owned(), |v| v.to_string()),
            bitrate.url.as_deref().unwrap_or("-")
        );
    }
    println!("photos: {}", info.photo_urls.len());
    for (index, (url, dimensions)) in info
        .photo_urls
        .iter()
        .zip(info.photo_dimensions.iter())
        .enumerate()
    {
        let size = dimensions
            .map(|d| format!("{}x{}", d.width, d.height))
            .unwrap_or_else(|| "-".into());
        println!("photo[{}]: {} {}", index + 1, size, url);
    }
    println!("audio: {}", info.audio_url.as_deref().unwrap_or("-"));
    println!("slideshow: {}", info.is_slideshow);
    println!("age_restricted: {}", info.is_age_restricted);
}

fn print_human(result: &DownloadResult) {
    println!("post: {}", result.post_id);
    println!("author: @{}", result.author.id);
    println!("page: {}", result.page_url);
    println!("cache_hit: {}", result.cache_hit);

    if let Some(video) = &result.video {
        println!(
            "video: {} ({})",
            video.path.display(),
            human_size(video.size)
        );
    }
    for photo in &result.photos {
        println!(
            "photo: {} ({})",
            photo.path.display(),
            human_size(photo.size)
        );
    }
    if let Some(audio) = &result.audio {
        println!(
            "audio: {} ({})",
            audio.path.display(),
            human_size(audio.size)
        );
    }
    for failure in &result.failures {
        println!(
            "failed: {} {} ({})",
            failure.kind.as_str(),
            failure.url,
            failure.error
        );
    }

    println!(
        "timings: total {} ms | resolve {} ms | page {} ms | waf {} ms | parse {} ms | media {} ms",
        result.timings.total_ms,
        result.timings.resolve_ms,
        result.timings.page_ms,
        result.timings.waf_ms,
        result.timings.parse_ms,
        result.timings.media_ms
    );
}

fn human_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.2} GiB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.2} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.2} KiB", bytes / KIB)
    } else {
        format!("{bytes:.0} B")
    }
}

fn init_tracing(quiet: bool) {
    let fallback = if quiet { "error" } else { "warn" };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(fallback));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_file_skips_comments_and_blank_lines() {
        let urls = parse_urls("# comment\n\nhttps://a\n  https://b  \n");
        assert_eq!(urls, vec!["https://a", "https://b"]);
    }

    #[test]
    fn toml_config_parses_typed_options() {
        let config: FileConfig = toml::from_str(
            r#"
output = "music"
quality = "efficient"
filename = "{kind}-{post_id}.{ext}"
collision = "suffix"
audio = true
timeout = 45
retries = 4
browser = "firefox151"
quiet = true
"#,
        )
        .unwrap();
        assert_eq!(config.output, Some(PathBuf::from("music")));
        assert_eq!(config.quality, Some(QualityArg::Efficient));
        assert_eq!(config.collision, Some(CollisionArg::Suffix));
        assert_eq!(config.timeout, Some(45));
        assert_eq!(config.retries, Some(4));
        assert!(config.quiet.unwrap());
    }
}
