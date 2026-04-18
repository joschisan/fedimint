use std::time::Duration;

use assert_matches::assert_matches;
use futures::FutureExt;
use tokio::time::error::Elapsed;
use tokio::time::timeout;

use super::{NextOrPending, SafeUrl};

#[test]
fn test_safe_url() {
    let test_cases = vec![
        (
            "http://1.2.3.4:80/foo",
            "http://1.2.3.4/foo",
            "SafeUrl(http://1.2.3.4/foo)",
            "http://1.2.3.4/foo",
        ),
        (
            "http://1.2.3.4:81/foo",
            "http://1.2.3.4:81/foo",
            "SafeUrl(http://1.2.3.4:81/foo)",
            "http://1.2.3.4:81/foo",
        ),
        (
            "picomint://1.2.3.4:1000/foo",
            "picomint://1.2.3.4:1000/foo",
            "SafeUrl(picomint://1.2.3.4:1000/foo)",
            "picomint://1.2.3.4:1000/foo",
        ),
        (
            "picomint://foo:bar@domain.com:1000/foo",
            "picomint://REDACTEDUSER:REDACTEDPASS@domain.com:1000/foo",
            "SafeUrl(picomint://REDACTEDUSER:REDACTEDPASS@domain.com:1000/foo)",
            "picomint://domain.com:1000/foo",
        ),
        (
            "picomint://foo@1.2.3.4:1000/foo",
            "picomint://REDACTEDUSER@1.2.3.4:1000/foo",
            "SafeUrl(picomint://REDACTEDUSER@1.2.3.4:1000/foo)",
            "picomint://1.2.3.4:1000/foo",
        ),
    ];

    for (url_str, safe_display_expected, safe_debug_expected, without_auth_expected) in test_cases {
        let safe_url = SafeUrl::parse(url_str).unwrap();

        let safe_display = format!("{safe_url}");
        assert_eq!(
            safe_display, safe_display_expected,
            "Display implementation out of spec"
        );

        let safe_debug = format!("{safe_url:?}");
        assert_eq!(
            safe_debug, safe_debug_expected,
            "Debug implementation out of spec"
        );

        let without_auth = safe_url.without_auth().unwrap();
        assert_eq!(
            without_auth.as_str(),
            without_auth_expected,
            "Without auth implementation out of spec"
        );
    }

    // Exercise `From`-trait via `Into`
    let _: SafeUrl = url::Url::parse("http://1.2.3.4:80/foo").unwrap().into();
}

#[tokio::test]
async fn test_next_or_pending() {
    let mut stream = futures::stream::iter(vec![1, 2]);
    assert_eq!(stream.next_or_pending().now_or_never(), Some(1));
    assert_eq!(stream.next_or_pending().now_or_never(), Some(2));
    assert_matches!(
        timeout(Duration::from_millis(100), stream.next_or_pending()).await,
        Err(Elapsed { .. })
    );
}

