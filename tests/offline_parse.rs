//! Offline page-parser tests using sanitized fixtures.

use ystt::{PageParse, QualityPolicy, YsttError, parse_page};

const VIDEO_PAGE: &str = include_str!("fixtures/video_page.html");
const SLIDESHOW_PAGE: &str = include_str!("fixtures/slideshow_page.html");
const UNAVAILABLE_PAGE: &str = include_str!("fixtures/unavailable_page.html");

#[test]
fn video_fixture_is_parsed() {
    let PageParse::Item(item) = parse_page(VIDEO_PAGE, "7684902068791872798").unwrap() else {
        panic!("expected an item");
    };

    assert_eq!(item.post_id, "7684902068791872798");
    assert_eq!(item.author.id, "fixture_user");
    assert_eq!(item.description.as_deref(), Some("fixture video"));
    assert_eq!(
        item.best_video_url(QualityPolicy::Best),
        Some("https://cdn.example/1080p.mp4")
    );
    assert_eq!(
        item.best_video_url(QualityPolicy::Compatible),
        Some("https://cdn.example/720p.mp4")
    );
    assert_eq!(
        item.best_video_url(QualityPolicy::Fast),
        Some("https://cdn.example/720p.mp4")
    );
    assert_eq!(item.audio_url(), Some("https://cdn.example/music.mp3"));
}

#[test]
fn slideshow_fixture_preserves_gallery_order_and_prefers_jpeg() {
    let PageParse::Item(item) = parse_page(SLIDESHOW_PAGE, "7674697655909600532").unwrap() else {
        panic!("expected an item");
    };

    assert_eq!(item.photos.len(), 2);
    assert_eq!(
        item.photo_urls(),
        vec![
            "https://cdn.example/p1.jpeg?x=1",
            "https://cdn.example/p2.jpeg?x=1"
        ]
    );
}

#[test]
fn unavailable_fixture_is_typed() {
    let error = parse_page(UNAVAILABLE_PAGE, "1").unwrap_err();
    assert!(matches!(error, YsttError::Unavailable(_)));
}

#[test]
fn missing_marker_is_not_an_item() {
    assert!(matches!(
        parse_page("<html>no data</html>", "1").unwrap(),
        PageParse::NoItem
    ));
}
