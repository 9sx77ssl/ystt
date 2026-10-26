//! Offline URL parsing tests.

use ystt::{MediaKind, TikTokUrl, find_post_id_in_html, get_post_id};

#[test]
fn direct_video_url() {
    let url = TikTokUrl::parse(
        "https://www.tiktok.com/@some.user/video/7684902068791872798?is_from_webapp=1&sender_device=pc",
    )
    .unwrap();
    assert_eq!(url.post_id(), Some("7684902068791872798"));
    assert_eq!(url.user(), Some("some.user"));
    assert_eq!(url.kind(), Some(MediaKind::Video));
    assert!(!url.is_short_link());
    assert_eq!(
        url.page_url("7684902068791872798"),
        "https://www.tiktok.com/@some.user/video/7684902068791872798"
    );
}

#[test]
fn direct_photo_url() {
    let url = TikTokUrl::parse("https://www.tiktok.com/@ilqqxz/photo/7674697655909600532").unwrap();
    assert_eq!(url.post_id(), Some("7674697655909600532"));
    assert_eq!(url.kind(), Some(MediaKind::Photo));
}

#[test]
fn short_links() {
    let vm = TikTokUrl::parse("https://vm.tiktok.com/ZMabcDEF/").unwrap();
    assert_eq!(vm.short_code(), Some("ZMabcDEF"));
    assert_eq!(
        vm.short_url().as_deref(),
        Some("https://vm.tiktok.com/ZMabcDEF")
    );

    let vt = TikTokUrl::parse("https://vt.tiktok.com/ZMabcDEF/").unwrap();
    assert_eq!(vt.short_code(), Some("ZMabcDEF"));

    let t = TikTokUrl::parse("https://www.tiktok.com/t/ZTabc123/").unwrap();
    assert_eq!(t.short_code(), Some("ZTabc123"));
    assert_eq!(
        t.short_url().as_deref(),
        Some("https://www.tiktok.com/t/ZTabc123")
    );
}

#[test]
fn query_and_bare_ids() {
    let query =
        TikTokUrl::parse("https://www.tiktok.com/@u?is_from_webapp=1&item_id=7684902068791872798")
            .unwrap();
    assert_eq!(query.post_id(), Some("7684902068791872798"));

    assert_eq!(
        get_post_id("https://www.tiktok.com/7684902068791872798").as_deref(),
        Some("7684902068791872798")
    );
}

#[test]
fn rejects_non_tiktok_and_garbage() {
    assert!(TikTokUrl::parse("https://example.com/@u/video/1").is_err());
    assert!(TikTokUrl::parse("definitely not a url").is_err());
}

#[test]
fn html_redirect_discovery() {
    let canonical =
        r#"<link rel="canonical" href="https://www.tiktok.com/@u/video/7684902068791872798">"#;
    assert_eq!(
        find_post_id_in_html(canonical).as_deref(),
        Some("7684902068791872798")
    );

    let anchor = r#"<a href="https://www.tiktok.com/@u/photo/7674697655909600532">open</a>"#;
    assert_eq!(
        find_post_id_in_html(anchor).as_deref(),
        Some("7674697655909600532")
    );
}
