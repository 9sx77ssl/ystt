//! TikTok page, embed and short-link resolution.
//!
//! This module owns only the resolver state machine; public `Ystt` APIs remain
//! unchanged and the download/transfer modules consume the same `Resolved` type.

use std::time::{Duration, Instant};

use http::header::LOCATION;
use tracing::debug;
use ystt_net::{BrowserHeaders, NetError, RequestKind, RequestOptions};
use ystt_waf::WafChallenge;

use crate::config::QualityPolicy;
use crate::cookie::CookieJar;
use crate::error::{Result, YsttError};
use crate::model::{MediaInfo, MediaKind};
use crate::parse::{PageParse, parse_embed_page, parse_page};
use crate::progress::{ProgressEmitter, ProgressStage};
use crate::url::{TikTokUrl, find_post_id_in_html};

use super::options::ResolvedOptions;
use super::{FetchOutcome, Resolved, Ystt, check_cancelled, millis, sleep_or_cancel};

impl Ystt {
    pub(crate) async fn resolve_internal(
        &self,
        url: &str,
        opts: &ResolvedOptions,
        emitter: &ProgressEmitter,
    ) -> Result<Resolved> {
        self.resolve_internal_mode(url, opts, self.config.resolver.use_embed, emitter)
            .await
    }

    pub(crate) async fn resolve_internal_mode(
        &self,
        url: &str,
        opts: &ResolvedOptions,
        allow_embed: bool,
        emitter: &ProgressEmitter,
    ) -> Result<Resolved> {
        let started = Instant::now();
        let parsed = TikTokUrl::parse(url)?;
        let signal = opts.signal.as_ref();

        check_cancelled(signal)?;

        let post_id = self.resolve_post_id(&parsed, signal).await?;

        let quality = opts.quality;
        let cache_key = format!("{post_id}:{}", quality.as_str());
        let metadata_enabled = opts.metadata_cache;

        if let Some(mut cached) = self.cache.get(&cache_key).await {
            cached.source_url = url.to_owned();
            let elapsed = started.elapsed();
            return Ok(Resolved {
                info: cached,
                timings: crate::download::Timings {
                    resolve_ms: millis(elapsed),
                    total_ms: millis(elapsed),
                    ..crate::download::Timings::default()
                },
                from_cache: true,
                from_embed: false,
                cache_key,
                cookie: None,
            });
        }

        if metadata_enabled && let Some(entry) = self.metadata.get(&cache_key).await {
            let mut info = entry.info;
            info.source_url = url.to_owned();
            self.cache.put(cache_key.clone(), info.clone()).await;
            let elapsed = started.elapsed();
            return Ok(Resolved {
                info,
                timings: crate::download::Timings {
                    resolve_ms: millis(elapsed),
                    total_ms: millis(elapsed),
                    ..crate::download::Timings::default()
                },
                from_cache: true,
                from_embed: false,
                cache_key,
                cookie: entry.cookie,
            });
        }

        let page_url = parsed.page_url(&post_id);
        let page_url = match &self.config.resolver.page_origin {
            Some(origin) => page_url.replacen("https://www.tiktok.com", origin, 1),
            None => page_url,
        };

        let page_started = Instant::now();
        let prefer_embed = allow_embed
            && parsed.kind() != Some(MediaKind::Photo)
            && quality != QualityPolicy::Best
            && quality != QualityPolicy::Efficient;
        let mut outcome = if prefer_embed {
            match self.fetch_embed(&post_id, signal).await? {
                Some(outcome) => outcome,
                None => {
                    self.fetch_detail(&page_url, &post_id, signal, Some(emitter))
                        .await?
                }
            }
        } else {
            match self
                .fetch_detail(&page_url, &post_id, signal, Some(emitter))
                .await
            {
                Ok(outcome) => outcome,
                Err(primary_error) if parsed.kind() != Some(MediaKind::Photo) => {
                    debug!(?primary_error, "full page failed; trying embed fallback");
                    match self.fetch_embed(&post_id, signal).await {
                        Ok(Some(outcome)) => outcome,
                        _ => return Err(primary_error),
                    }
                }
                Err(error) => return Err(error),
            }
        };
        outcome.page = page_started
            .elapsed()
            .saturating_sub(outcome.waf)
            .saturating_sub(outcome.parse);
        let from_embed = outcome.from_embed;
        let cookie = outcome.cookie.clone();
        let item = outcome.item;

        let video_url = item.best_video_url(quality).map(str::to_owned);
        let photo_entries = item.photo_entries();
        let photo_urls: Vec<String> = photo_entries.iter().map(|(url, _)| url.clone()).collect();
        let photo_dimensions: Vec<_> = photo_entries.iter().map(|(_, dims)| *dims).collect();
        let audio_url = item.audio_url().map(str::to_owned);
        let bitrates = item
            .video
            .as_ref()
            .map(|video| video.bitrates.clone())
            .unwrap_or_default();
        let video_width = item.video.as_ref().and_then(|video| video.width);
        let video_height = item.video.as_ref().and_then(|video| video.height);
        let duration_sec = item.video.as_ref().and_then(|video| video.duration_sec);
        let cover_url = item
            .video
            .as_ref()
            .and_then(|video| video.cover_url.clone());
        let is_slideshow = !item.photos.is_empty();
        // Photo posts also carry a placeholder `video` object with a relative
        // URL. If the post has gallery images, photos are the media.
        let video_url = if photo_urls.is_empty() {
            video_url
        } else {
            None
        };
        let is_age_restricted = item.is_content_classified
            && video_url.is_none()
            && photo_urls.is_empty()
            && audio_url.is_none();

        let info = MediaInfo {
            post_id,
            source_url: url.to_owned(),
            page_url,
            author: item.author,
            description: item.description,
            video_url,
            photo_urls,
            photo_dimensions,
            audio_url,
            is_slideshow,
            is_age_restricted,
            music: item.music,
            bitrates,
            video_width,
            video_height,
            duration_sec,
            cover_url,
        };

        if !info.has_media() && !info.is_age_restricted {
            return Err(YsttError::NoMedia);
        }

        if info.has_media() {
            self.cache.put(cache_key.clone(), info.clone()).await;
            if metadata_enabled
                && let Err(error) = self
                    .metadata
                    .put(&cache_key, &info, outcome.cookie.as_deref())
                    .await
            {
                tracing::warn!(?error, "failed to store metadata cache");
            }
        }

        let elapsed = started.elapsed();
        Ok(Resolved {
            info,
            timings: crate::download::Timings {
                resolve_ms: millis(elapsed),
                page_ms: millis(outcome.page),
                waf_ms: millis(outcome.waf),
                parse_ms: millis(outcome.parse),
                total_ms: millis(elapsed),
                ..crate::download::Timings::default()
            },
            from_cache: false,
            from_embed,
            cache_key,
            cookie,
        })
    }

    pub(crate) async fn resolve_post_id(
        &self,
        parsed: &TikTokUrl,
        signal: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<String> {
        match parsed.post_id() {
            Some(post_id) => Ok(post_id.to_owned()),
            None if parsed.is_short_link() => {
                let short_url = parsed
                    .short_url()
                    .unwrap_or_else(|| parsed.raw().to_owned());
                self.resolve_short_link(&short_url, signal).await
            }
            None => Err(YsttError::Parse(
                "URL does not contain a post ID or short-link code".into(),
            )),
        }
    }

    async fn resolve_short_link(
        &self,
        short_url: &str,
        signal: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<String> {
        let headers = BrowserHeaders::build(RequestKind::ShortLink, None, None, None)?;
        let options = match signal {
            Some(signal) => RequestOptions::short_link().with_signal(signal.clone()),
            None => RequestOptions::short_link(),
        };
        let response = self.http.get(short_url, headers, options).await?;

        if let Some(location) = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            && let Some(post_id) = crate::url::get_post_id(location)
        {
            return Ok(post_id);
        }

        let status = response.status_u16();
        if !response.is_success() && !(300..400).contains(&status) {
            return Err(YsttError::Network(NetError::Status {
                status,
                url: short_url.to_owned(),
            }));
        }

        let html = response
            .text(
                self.config.network.max_page_bytes,
                self.config.network.http.read_timeout,
                signal,
            )
            .await?;

        find_post_id_in_html(&html)
            .ok_or_else(|| YsttError::Parse("cannot resolve post ID from short link".into()))
    }

    async fn fetch_embed(
        &self,
        post_id: &str,
        signal: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<Option<FetchOutcome>> {
        let origin = self
            .config
            .resolver
            .page_origin
            .as_deref()
            .unwrap_or("https://www.tiktok.com")
            .trim_end_matches('/');
        let url = format!("{origin}/embed/v2/{post_id}");
        let headers = BrowserHeaders::build(RequestKind::Page, None, None, None)?;
        let options = match signal {
            Some(signal) => RequestOptions::page().with_signal(signal.clone()),
            None => RequestOptions::page(),
        };

        let started = Instant::now();
        let response = match self.http.get(&url, headers, options).await {
            Ok(response) => response,
            Err(error) => {
                debug!(?error, "embed request failed; falling back to full page");
                return Ok(None);
            }
        };
        if !response.is_success() {
            return Ok(None);
        }

        let mut cookies = CookieJar::default();
        for set_cookie in response.set_cookie_headers() {
            cookies.absorb(&set_cookie);
        }

        let body_started = Instant::now();
        let html = match response
            .text(
                self.config.network.max_page_bytes,
                self.config.network.http.read_timeout,
                signal,
            )
            .await
        {
            Ok(html) => html,
            Err(error) => {
                debug!(?error, "embed body failed; falling back to full page");
                return Ok(None);
            }
        };
        let body_elapsed = body_started.elapsed();
        let parse_started = Instant::now();
        let item = parse_embed_page(&html, post_id);
        let parse_elapsed = parse_started.elapsed();
        debug!(
            headers_ms = started
                .elapsed()
                .saturating_sub(body_elapsed + parse_elapsed)
                .as_millis() as u64,
            body_ms = body_elapsed.as_millis() as u64,
            bytes = html.len(),
            "embed fetched"
        );
        Ok(item.map(|item| FetchOutcome {
            item,
            page: started.elapsed().saturating_sub(parse_elapsed),
            waf: Duration::ZERO,
            parse: parse_elapsed,
            cookie: cookies.header(),
            from_embed: true,
        }))
    }

    async fn fetch_detail(
        &self,
        page_url: &str,
        expected_post_id: &str,
        signal: Option<&tokio_util::sync::CancellationToken>,
        progress: Option<&ProgressEmitter>,
    ) -> Result<FetchOutcome> {
        let fetch_started = Instant::now();
        let mut cookies = CookieJar::default();
        let mut waf_total = Duration::ZERO;
        let mut parse_total = Duration::ZERO;
        let mut last_error: Option<YsttError> = None;

        for attempt in 1..=3u32 {
            check_cancelled(signal)?;

            let cookie = cookies.header();
            let headers = BrowserHeaders::build(RequestKind::Page, None, cookie.as_deref(), None)?;
            let options = match signal {
                Some(signal) => RequestOptions::page().with_signal(signal.clone()),
                None => RequestOptions::page(),
            };
            let request_started = Instant::now();
            let response = self.http.get(page_url, headers, options).await?;
            let headers_elapsed = request_started.elapsed();
            let status = response.status_u16();

            for set_cookie in response.set_cookie_headers() {
                cookies.absorb(&set_cookie);
            }

            if matches!(status, 404 | 410) {
                return Err(YsttError::Unavailable(format!("HTTP {status}")));
            }
            if status == 429 || status >= 500 {
                last_error = Some(YsttError::Network(NetError::Status {
                    status,
                    url: page_url.to_owned(),
                }));
                if attempt < 3 {
                    sleep_or_cancel(Duration::from_millis(250 * u64::from(attempt)), signal)
                        .await?;
                    continue;
                }
                break;
            }

            let version = response.version();
            let body_started = Instant::now();
            let html = response
                .text(
                    self.config.network.max_page_bytes,
                    self.config.network.http.read_timeout,
                    signal,
                )
                .await?;
            debug!(
                attempt,
                ?version,
                headers_ms = headers_elapsed.as_millis() as u64,
                body_ms = body_started.elapsed().as_millis() as u64,
                bytes = html.len(),
                "page fetched"
            );

            let parse_started = Instant::now();
            let parsed = parse_page(&html, expected_post_id);
            parse_total += parse_started.elapsed();

            match parsed? {
                PageParse::Item(item) => {
                    let item = *item;
                    let page_total = fetch_started.elapsed();
                    let page = page_total
                        .saturating_sub(waf_total)
                        .saturating_sub(parse_total);
                    return Ok(FetchOutcome {
                        item,
                        page,
                        waf: waf_total,
                        parse: parse_total,
                        cookie: cookies.header(),
                        from_embed: false,
                    });
                }
                PageParse::NoItem => {
                    if let Some(challenge) =
                        WafChallenge::parse_html(&html, self.config.waf.max_iterations)?
                    {
                        debug!("solving TikTok WAF challenge");
                        let fingerprint = challenge.fingerprint();
                        if let Some(cookie) = self
                            .waf_cookies
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .get(&fingerprint)
                            .cloned()
                        {
                            cookies.set(&cookie);
                            continue;
                        }
                        if let Some(progress) = progress {
                            progress.emit(ProgressStage::WafChallenge { attempt });
                        }
                        let solver = self.waf.clone();
                        let challenge_for_worker = challenge.clone();
                        let solve_started = Instant::now();
                        let cookie = tokio::task::spawn_blocking(move || {
                            solver.solve(&challenge_for_worker)
                        })
                        .await
                        .map_err(|error| YsttError::Internal(error.to_string()))??;
                        let solve_elapsed = solve_started.elapsed();
                        waf_total += solve_elapsed;
                        if let Some(progress) = progress {
                            progress.emit(ProgressStage::WafSolved {
                                elapsed_ms: millis(solve_elapsed),
                            });
                        }
                        {
                            let mut cache = self
                                .waf_cookies
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            if cache.len() >= 64 {
                                cache.clear();
                            }
                            cache.insert(fingerprint, cookie.clone());
                        }
                        cookies.set(&cookie);
                        continue;
                    }

                    if ystt_waf::looks_like_external_bot_page(&html) {
                        return Err(YsttError::Parse(
                            "external bot/CAPTCHA page is not supported".into(),
                        ));
                    }

                    last_error = Some(if status == 200 {
                        YsttError::Parse("no item payload and no WAF challenge".into())
                    } else {
                        YsttError::Network(NetError::Status {
                            status,
                            url: page_url.to_owned(),
                        })
                    });
                    if attempt < 3 {
                        sleep_or_cancel(Duration::from_millis(250 * u64::from(attempt)), signal)
                            .await?;
                        continue;
                    }
                }
            }
        }

        Err(last_error
            .unwrap_or_else(|| YsttError::Parse("cannot parse TikTok page after retries".into())))
    }
}
