//! Helpers shared between the crate's snapshot tests.

/// Redactions applied to a rendered diagnostic before it reaches its snapshot.
///
/// Paths and OS error strings differ per machine, so a snapshot holding them could never match on
/// every platform CI runs on.
pub(crate) const SNAPSHOT_FILTERS: &[(&str, &str)] = &[
    (
        r#"(?:\\\\[?.]\\)?(?:[A-Za-z]:)?(?:[/\\][^/\\\s:"'\]]+){2,}"#,
        "<redacted-path>",
    ),
    (
        r"(?m)^(\s*[`|].*?-> ).+ \(os error \d+\)$",
        "${1}OS error <redacted>",
    ),
];

/// Render `error` the way the snapshot tests compare it.
///
/// The graphical handler is pinned to an unlimited width and an empty theme so the output does not
/// depend on the terminal the tests run in.
pub(crate) fn render_error(error: &miette::Report) -> String {
    let mut rendered = String::new();
    let _ = miette::GraphicalReportHandler::default()
        .with_width(usize::MAX)
        .with_theme(miette::GraphicalTheme::none())
        .render_report(&mut rendered, &**error);
    rendered
}

/// Assert that `error` renders to the snapshot stored under `name`.
///
/// A macro rather than a function so `insta` resolves the snapshot against the calling test's file
/// and module rather than this one.
macro_rules! assert_error_snapshot {
    ($name:expr, $error:expr) => {
        insta::with_settings! { {
            filters => $crate::test_support::SNAPSHOT_FILTERS.to_vec(),
        }, {
            insta::assert_snapshot!($name, $crate::test_support::render_error(&$error));
        }}
    };
}

pub(crate) use assert_error_snapshot;
