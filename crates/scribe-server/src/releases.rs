//! Release-catalog cache and GitHub fetcher.
//!
//! Backs the `ClientMessage::ListReleases` IPC: an in-memory cache of the
//! 30 most recent published Scribe releases (rendered + sanitized), refreshed
//! on demand with stale-while-revalidate semantics. See `data-model.md` for
//! the Fresh / Stale / Failed transition diagram and `research.md` §R3 / §R4
//! for the endpoint and TTL choices.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::{info, warn};

use scribe_common::error::ScribeError;
use scribe_common::protocol::{Release, ReleaseListResultState};

/// Multi-release endpoint used by [`GithubReleaseFetcher`]. `per_page=30`
/// matches GitHub's default and is the cap research R3 settled on; pagination
/// beyond the first page is intentionally out of scope for v1.
const GITHUB_RELEASES_URL: &str =
    "https://api.github.com/repos/sharaf-nassar/scribe/releases?per_page=30";

/// Defensive cap on how many releases we keep before serializing them across
/// the IPC boundary. The endpoint already returns at most 30; this guards
/// against a future schema change that would otherwise leak unbounded data.
const MAX_RELEASES: usize = 30;

/// Boxed future returned by [`ReleaseFetcher::fetch_releases`]. Aliased so
/// the trait signature stays inside clippy's type-complexity budget.
pub type FetchReleasesFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<Release>, ScribeError>> + Send + 'a>>;

/// Trait the [`handle_list_releases`] function uses to fetch fresh release
/// data, designed so tests can inject deterministic implementations without
/// going through the network.
///
/// The boxed-future return shape (rather than `async fn` in trait) keeps the
/// trait dyn-compatible — `Arc<dyn ReleaseFetcher>` is what the dispatcher
/// hands to the handler.
pub trait ReleaseFetcher: Send + Sync {
    fn fetch_releases(&self) -> FetchReleasesFuture<'_>;
}

/// Real implementation that calls the GitHub releases endpoint, drops drafts,
/// keeps pre-releases, and runs each release's `body` through pulldown-cmark
/// + ammonia to produce the wire-shape `body_html`.
pub struct GithubReleaseFetcher {
    client: &'static reqwest::Client,
}

impl GithubReleaseFetcher {
    /// Build a fetcher backed by the process-wide shared HTTP client from
    /// [`crate::updater::http_client`] (research R7) so connection pooling,
    /// DNS, and TLS sessions are shared across the updater and the catalog.
    #[must_use]
    pub fn new() -> Self {
        Self { client: crate::updater::http_client() }
    }
}

impl Default for GithubReleaseFetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl ReleaseFetcher for GithubReleaseFetcher {
    fn fetch_releases(&self) -> FetchReleasesFuture<'_> {
        Box::pin(async move {
            let listed: Vec<GhListedRelease> = self
                .client
                .get(GITHUB_RELEASES_URL)
                .send()
                .await
                .map_err(|e| ScribeError::UpdateCheckFailed { reason: format!("{e}") })?
                .error_for_status()
                .map_err(|e| ScribeError::UpdateCheckFailed { reason: format!("{e}") })?
                .json()
                .await
                .map_err(|e| ScribeError::UpdateCheckFailed { reason: format!("{e}") })?;

            Ok(listed
                .into_iter()
                .filter(|r| !r.draft)
                .take(MAX_RELEASES)
                .map(into_release)
                .collect())
        })
    }
}

/// Private deserialization shape mirroring the subset of GitHub's
/// `/releases` payload we consume. Mirrors `GhRelease` in `updater.rs`
/// but adds the fields the catalog needs (`name`, `published_at`, `body`).
#[derive(Deserialize)]
struct GhListedRelease {
    tag_name: String,
    name: Option<String>,
    published_at: Option<String>,
    body: Option<String>,
    html_url: String,
    draft: bool,
    prerelease: bool,
}

fn into_release(raw: GhListedRelease) -> Release {
    let version = raw.tag_name.trim_start_matches('v').to_owned();
    let body_html = render_release_body(raw.body.as_deref().unwrap_or(""));
    Release {
        version,
        name: raw.name,
        published_at: raw.published_at.unwrap_or_default(),
        body_html,
        prerelease: raw.prerelease,
        html_url: raw.html_url,
    }
}

/// Markdown → `CommonMark` + GFM HTML → ammonia-sanitized HTML pipeline.
///
/// Extracted as a standalone fn so the test module can exercise the render
/// path against fixtures (plain text, code blocks, lists, links, tables,
/// `<script>` injection) without needing a live HTTP fetcher.
fn render_release_body(markdown: &str) -> String {
    use pulldown_cmark::{Options, Parser, html};

    let mut options = Options::empty();
    options.insert(
        Options::ENABLE_TABLES
            | Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_FOOTNOTES
            | Options::ENABLE_TASKLISTS
            | Options::ENABLE_GFM,
    );
    let parser = Parser::new_ext(markdown, options);
    let mut html_buf = String::with_capacity(markdown.len());
    html::push_html(&mut html_buf, parser);
    ammonia::clean(&html_buf)
}

/// In-memory cache keyed by `()` — one per `scribe-server` process — that
/// the `ListReleases` handler reads and writes. See `data-model.md` for the
/// invariants.
pub struct ReleaseCatalog {
    /// `Some` after the first successful fetch; remains `Some` thereafter
    /// even when subsequent refreshes fail (FR-013 stale-while-revalidate).
    pub last_fetched_at: Option<Instant>,
    /// Tracks whether the most recent attempt succeeded. Lets the handler
    /// distinguish "we have current data" from "we have data but the latest
    /// refresh failed".
    pub last_fetch_was_success: bool,
    /// The cached release vector. `None` until the first successful fetch
    /// ever.
    pub value: Option<Vec<Release>>,
    /// Time-to-live before the cache is treated as stale and a background
    /// refresh is kicked off. Configurable so tests can use shorter values.
    pub ttl: Duration,
    /// True iff a background refresh task is currently running. Used to
    /// avoid a thundering herd when many `ListReleases` requests arrive
    /// while a single refresh is already underway.
    pub inflight_refresh: bool,
    /// User-presentable reason carried with `Stale` responses when the most
    /// recent refresh attempt failed. `None` until at least one refresh has
    /// been attempted; cleared on success.
    pub last_refresh_error: Option<String>,
}

impl ReleaseCatalog {
    /// Default TTL per research R4 (1 hour balances rate-limit safety and
    /// post-release freshness).
    pub const DEFAULT_TTL: Duration = Duration::from_secs(60 * 60);

    /// Construct an empty catalog with the given TTL.
    #[must_use]
    pub fn new(ttl: Duration) -> Self {
        Self {
            last_fetched_at: None,
            last_fetch_was_success: false,
            value: None,
            ttl,
            inflight_refresh: false,
            last_refresh_error: None,
        }
    }
}

impl Default for ReleaseCatalog {
    fn default() -> Self {
        Self::new(Self::DEFAULT_TTL)
    }
}

/// Outcome of inspecting the catalog under the lock; returned to the handler
/// so it can decide whether to do a synchronous fetch, kick a background
/// refresh, or return the cache directly.
enum CacheLookup {
    /// Cache is within TTL — return it as-is.
    Fresh(Vec<Release>),
    /// Cache exists but is past TTL. We have already flipped
    /// `inflight_refresh` to `true` if `kick_refresh` is set; the caller is
    /// responsible for spawning the background fetch.
    Stale { releases: Vec<Release>, reason: String, kick_refresh: bool },
    /// No cached value — handler must do a synchronous fetch.
    Empty,
}

fn inspect_locked(catalog: &mut ReleaseCatalog) -> CacheLookup {
    let Some(ref value) = catalog.value else {
        return CacheLookup::Empty;
    };
    let releases = value.clone();
    let elapsed = catalog.last_fetched_at.map_or(Duration::MAX, |t| t.elapsed());

    if elapsed <= catalog.ttl {
        CacheLookup::Fresh(releases)
    } else {
        let reason = stale_reason(catalog);
        let kick_refresh = !catalog.inflight_refresh;
        if kick_refresh {
            catalog.inflight_refresh = true;
        }
        CacheLookup::Stale { releases, reason, kick_refresh }
    }
}

fn stale_reason(catalog: &ReleaseCatalog) -> String {
    catalog.last_refresh_error.as_ref().map_or_else(
        || {
            let minutes = catalog.ttl.as_secs() / 60;
            format!("stale (more than {minutes} minutes since last fetch)")
        },
        |err| format!("last refresh attempt failed: {err}"),
    )
}

/// Handle a `ListReleases` request, applying the Fresh / Stale / Failed
/// transition rules from `data-model.md`.
///
/// - Hit within TTL: returns `Fresh` from the cache.
/// - Hit past TTL: returns `Stale` and (if no refresh is already running)
///   spawns a background refresh that updates the cache when it completes.
/// - Miss: synchronously calls the fetcher; on success populates the cache
///   and returns `Fresh`; on failure returns `Failed` without populating.
///
/// `fetcher` is shared as an `Arc` so the handler can both await it inline
/// (cold-cache path) and clone it into a background `tokio::spawn` (stale
/// path) without requiring `'static` borrow gymnastics. Tests inject
/// panic / fixture / failing implementations the same way.
pub async fn handle_list_releases(
    catalog: &Arc<Mutex<ReleaseCatalog>>,
    fetcher: &Arc<dyn ReleaseFetcher>,
) -> ReleaseListResultState {
    // Phase 1: look at cache state under the lock and decide what to do.
    let lookup = {
        let mut guard = catalog.lock().await;
        inspect_locked(&mut guard)
    };

    match lookup {
        CacheLookup::Fresh(releases) => ReleaseListResultState::Fresh { releases },
        CacheLookup::Stale { releases, reason, kick_refresh } => {
            if kick_refresh {
                spawn_background_refresh(Arc::clone(catalog), Arc::clone(fetcher));
            }
            ReleaseListResultState::Stale { releases, reason }
        }
        CacheLookup::Empty => synchronous_first_fetch(catalog, fetcher.as_ref()).await,
    }
}

/// Synchronous fallback for the cold-cache case: await the fetcher inline,
/// populate the cache on success, return `Fresh`; on failure leave the cache
/// empty and return `Failed { reason }`.
async fn synchronous_first_fetch(
    catalog: &Arc<Mutex<ReleaseCatalog>>,
    fetcher: &dyn ReleaseFetcher,
) -> ReleaseListResultState {
    match fetcher.fetch_releases().await {
        Ok(releases) => {
            let mut guard = catalog.lock().await;
            guard.value = Some(releases.clone());
            guard.last_fetched_at = Some(Instant::now());
            guard.last_fetch_was_success = true;
            guard.last_refresh_error = None;
            ReleaseListResultState::Fresh { releases }
        }
        Err(e) => {
            let reason = format!("{e}");
            let mut guard = catalog.lock().await;
            guard.last_fetch_was_success = false;
            guard.last_refresh_error = Some(reason.clone());
            ReleaseListResultState::Failed { reason }
        }
    }
}

/// Kick a background refresh task. The caller has already flipped
/// `inflight_refresh` to `true` under the lock; this spawn clears the flag
/// when the fetch completes (success or failure).
fn spawn_background_refresh(catalog: Arc<Mutex<ReleaseCatalog>>, fetcher: Arc<dyn ReleaseFetcher>) {
    tokio::spawn(async move {
        match fetcher.fetch_releases().await {
            Ok(releases) => {
                info!(count = releases.len(), "background release refresh succeeded");
                let mut guard = catalog.lock().await;
                guard.value = Some(releases);
                guard.last_fetched_at = Some(Instant::now());
                guard.last_fetch_was_success = true;
                guard.last_refresh_error = None;
                guard.inflight_refresh = false;
            }
            Err(e) => {
                warn!("background release refresh failed: {e}");
                let mut guard = catalog.lock().await;
                guard.last_fetch_was_success = false;
                guard.last_refresh_error = Some(format!("{e}"));
                guard.inflight_refresh = false;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── render_release_body tests (T004) ─────────────────────────────

    #[test]
    fn renders_plain_text() {
        let html = render_release_body("hello world");
        assert!(
            html.contains("<p>hello world</p>"),
            "plain text should render as a <p>; got: {html}"
        );
    }

    #[test]
    fn renders_fenced_code_block() {
        let html = render_release_body("```rust\nfn main() {}\n```");
        assert!(html.contains("<pre>"), "fenced code should produce <pre>; got: {html}");
        assert!(html.contains("<code"), "fenced code should produce <code>; got: {html}");
        assert!(html.contains("fn main()"), "fenced body should be preserved; got: {html}");
    }

    #[test]
    fn renders_unordered_list() {
        let html = render_release_body("- a\n- b\n- c");
        assert!(html.contains("<ul>"), "list should produce <ul>; got: {html}");
        assert!(html.contains("<li>a</li>"), "list items should produce <li>; got: {html}");
    }

    #[test]
    fn renders_link_with_external_href() {
        let html = render_release_body("[text](https://example.com)");
        assert!(
            html.contains("href=\"https://example.com\""),
            "link href must survive sanitization; got: {html}"
        );
        assert!(html.contains(">text</a>"), "link text must survive; got: {html}");
    }

    #[test]
    fn renders_table() {
        // GFM table syntax — confirms ENABLE_TABLES is set.
        let md = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let html = render_release_body(md);
        assert!(html.contains("<table>"), "GFM table must render to <table>; got: {html}");
        assert!(html.contains("<thead>") || html.contains("<th"), "header row missing: {html}");
        assert!(html.contains("<td>1</td>"), "cell missing: {html}");
    }

    #[test]
    fn strips_script_injection() {
        let html = render_release_body("hello <script>alert(1)</script> world");
        assert!(!html.contains("<script"), "ammonia must strip <script> tags; got: {html}");
    }

    // ── Cache state machine tests (T005 + T020) ──────────────────────

    fn fixture_release() -> Release {
        Release {
            version: "0.4.2".to_owned(),
            name: Some("0.4.2".to_owned()),
            published_at: "2026-05-09T10:00:00Z".to_owned(),
            body_html: "<p>fixture</p>".to_owned(),
            prerelease: false,
            html_url: "https://github.com/sharaf-nassar/scribe/releases/tag/v0.4.2".to_owned(),
        }
    }

    /// Fetcher whose `fetch_releases` panics — used to confirm the handler
    /// did not reach for the network when the cache should have been
    /// sufficient.
    struct PanicFetcher;

    impl ReleaseFetcher for PanicFetcher {
        fn fetch_releases(&self) -> FetchReleasesFuture<'_> {
            Box::pin(async { panic!("PanicFetcher::fetch_releases must not be called") })
        }
    }

    /// Fetcher that yields a deterministic fixed result (cloned each call).
    struct StaticFetcher {
        result: Result<Vec<Release>, ScribeError>,
    }

    impl ReleaseFetcher for StaticFetcher {
        fn fetch_releases(&self) -> FetchReleasesFuture<'_> {
            let cloned = match &self.result {
                Ok(releases) => Ok(releases.clone()),
                Err(ScribeError::UpdateCheckFailed { reason }) => {
                    Err(ScribeError::UpdateCheckFailed { reason: reason.clone() })
                }
                Err(other) => Err(ScribeError::UpdateCheckFailed { reason: format!("{other}") }),
            };
            Box::pin(async move { cloned })
        }
    }

    fn make_catalog_arc(catalog: ReleaseCatalog) -> Arc<Mutex<ReleaseCatalog>> {
        Arc::new(Mutex::new(catalog))
    }

    fn panic_fetcher() -> Arc<dyn ReleaseFetcher> {
        Arc::new(PanicFetcher)
    }

    fn static_fetcher(result: Result<Vec<Release>, ScribeError>) -> Arc<dyn ReleaseFetcher> {
        Arc::new(StaticFetcher { result })
    }

    #[tokio::test]
    async fn returns_fresh_within_ttl() {
        let mut catalog = ReleaseCatalog::new(Duration::from_secs(3600));
        catalog.value = Some(vec![fixture_release()]);
        catalog.last_fetched_at = Some(Instant::now());
        catalog.last_fetch_was_success = true;
        let catalog = make_catalog_arc(catalog);
        let fetcher = panic_fetcher();

        let state = handle_list_releases(&catalog, &fetcher).await;
        match state {
            ReleaseListResultState::Fresh { releases } => {
                assert_eq!(releases, vec![fixture_release()]);
            }
            other => panic!("expected Fresh, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transitions_to_failed_when_no_cache_and_fetch_errs() {
        let catalog = make_catalog_arc(ReleaseCatalog::new(Duration::from_secs(3600)));
        let fetcher =
            static_fetcher(Err(ScribeError::UpdateCheckFailed { reason: "boom".to_owned() }));

        let state = handle_list_releases(&catalog, &fetcher).await;
        match state {
            ReleaseListResultState::Failed { reason } => {
                assert!(reason.contains("boom"), "reason must surface fetcher error; got {reason}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }

        // Cache must remain empty so future calls can retry cleanly.
        let guard = catalog.lock().await;
        assert!(guard.value.is_none(), "Failed must not populate the cache");
    }

    #[tokio::test]
    async fn populates_cache_on_first_successful_fetch() {
        let catalog = make_catalog_arc(ReleaseCatalog::new(Duration::from_secs(3600)));
        let success_fetcher = static_fetcher(Ok(vec![fixture_release()]));

        let first = handle_list_releases(&catalog, &success_fetcher).await;
        match first {
            ReleaseListResultState::Fresh { releases } => {
                assert_eq!(releases, vec![fixture_release()]);
            }
            other => panic!("expected Fresh on first call, got {other:?}"),
        }

        // Second call must hit the cache; fetcher must not be invoked.
        let panicker = panic_fetcher();
        let second = handle_list_releases(&catalog, &panicker).await;
        match second {
            ReleaseListResultState::Fresh { releases } => {
                assert_eq!(releases, vec![fixture_release()]);
            }
            other => panic!("expected Fresh on second call, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn does_not_thundering_herd_during_inflight_refresh() {
        let mut catalog = ReleaseCatalog::new(Duration::from_secs(60));
        catalog.value = Some(vec![fixture_release()]);
        // Last fetch is intentionally well past TTL so the path is "stale".
        catalog.last_fetched_at = Instant::now().checked_sub(Duration::from_secs(3600));
        catalog.last_fetch_was_success = false;
        catalog.inflight_refresh = true; // simulate an already-running refresh
        let catalog = make_catalog_arc(catalog);
        let fetcher = panic_fetcher();

        let state = handle_list_releases(&catalog, &fetcher).await;
        match state {
            ReleaseListResultState::Stale { releases, .. } => {
                assert_eq!(releases, vec![fixture_release()]);
            }
            other => panic!("expected Stale, got {other:?}"),
        }

        // The inflight flag must still be set — we did not start a second one.
        let guard = catalog.lock().await;
        assert!(
            guard.inflight_refresh,
            "concurrent caller must not clear or re-arm the inflight flag"
        );
    }

    /// FR-013: when a cached value exists but the cache has aged past TTL and
    /// the refresh fetch would fail, the handler must return `Stale` (with
    /// the cached releases preserved) and NEVER downgrade to `Failed`. T020.
    #[tokio::test]
    async fn returns_stale_when_cache_exists_but_refresh_fails() {
        let mut catalog = ReleaseCatalog::new(Duration::from_secs(60));
        catalog.value = Some(vec![fixture_release()]);
        catalog.last_fetched_at = Instant::now().checked_sub(Duration::from_secs(3600));
        catalog.last_fetch_was_success = true;
        catalog.last_refresh_error = Some("network down".to_owned());
        let catalog = make_catalog_arc(catalog);

        // The fetcher is configured to fail. The handler returns the cached
        // vector synchronously (the failure only writes back to the catalog
        // when the spawned refresh runs). FR-013's contract is the
        // synchronous return: cached data plus a `Stale` reason — never
        // `Failed` while a cached value is available.
        let fetcher = static_fetcher(Err(ScribeError::UpdateCheckFailed {
            reason: "still failing".to_owned(),
        }));

        let state = handle_list_releases(&catalog, &fetcher).await;
        match state {
            ReleaseListResultState::Stale { releases, reason } => {
                assert_eq!(releases, vec![fixture_release()]);
                assert!(!reason.is_empty(), "Stale reason must be non-empty");
                assert!(
                    reason.contains("network down") || reason.contains("stale"),
                    "Stale reason should reflect the last error or staleness; got {reason}"
                );
            }
            other => panic!("FR-013: expected Stale, got {other:?}"),
        }
    }
}
