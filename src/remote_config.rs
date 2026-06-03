// Remote configuration loader.
//
// Fetches a small JSON document from a remote URL and applies the ID server,
// relay server, key and (optionally) API server to the running client. The
// values are written into `DEFAULT_SETTINGS`, i.e. they act as defaults only:
// the user can still override them from the settings dialog (see
// `Config::get_option` resolution order: OVERWRITE > user config > DEFAULT).
//
// The fetch runs once (blocking, short timeout) at startup so the first
// connection already uses the remote server, then keeps polling in the
// background so the servers can be rotated centrally while the app is running.
// When a server-affecting field changes the rendezvous connection is
// restarted so the change takes effect immediately ("hot switch").
//
// The last successful document is cached on disk so the client still works
// (with the previous servers) when the network or the config host is down.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use hbb_common::{config, log};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Configuration knobs — edit these for your deployment.
// ---------------------------------------------------------------------------

/// URLs that return the config JSON, in priority order. On every fetch they
/// are tried top to bottom and the first one that is alive (reachable and
/// returns valid JSON) wins — i.e. liveness check + failover across mirrors.
/// Can be overridden at runtime with the `RUSTDESK_REMOTE_CONFIG_URL`
/// environment variable (comma- or whitespace-separated list).
///
/// Expected JSON (all fields optional), e.g.:
/// {
///   "id-server":    "ycsv.haojiahuo.link",
///   "relay-server": "ycsv.haojiahuo.link",
///   "api-server":   "",
///   "key":          "W0tcF0JJfuXwDeumwYPhxzKLUmpO4UUIvt..."
/// }
const REMOTE_CONFIG_URLS: &[&str] = &[
    "http://144.22.37.226:21111/yuancheng.json",
    "http://144.22.52.5:21111/yuancheng.json",
];

/// How often to re-fetch while the app is running.
const POLL_INTERVAL: Duration = Duration::from_secs(300); // 5 minutes

/// Timeout for the blocking startup fetch (kept short so startup is not
/// blocked on a bad network; subsequent polls use the same timeout).
const FETCH_TIMEOUT: Duration = Duration::from_secs(8);

/// Cache file name (stored in the app config directory).
const CACHE_FILE: &str = "remote_config_cache.json";

// ---------------------------------------------------------------------------

fn config_urls() -> Vec<String> {
    if let Ok(v) = std::env::var("RUSTDESK_REMOTE_CONFIG_URL") {
        let list: Vec<String> = v
            .split(|c| c == ',' || c == ' ' || c == '\n' || c == '\t')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_owned())
            .collect();
        if !list.is_empty() {
            return list;
        }
    }
    REMOTE_CONFIG_URLS.iter().map(|s| s.to_string()).collect()
}

fn cache_path() -> std::path::PathBuf {
    config::Config::path(CACHE_FILE)
}

/// Map a JSON document to the internal option keys we care about.
/// Accepts both friendly aliases and the internal option names.
fn extract_options(doc: &Value) -> HashMap<String, String> {
    let obj = match doc.as_object() {
        Some(o) => o,
        None => return HashMap::new(),
    };
    let get = |keys: &[&str]| -> Option<String> {
        for k in keys {
            if let Some(v) = obj.get(*k).and_then(|v| v.as_str()) {
                return Some(v.trim().to_owned());
            }
        }
        None
    };
    let mut out = HashMap::new();
    if let Some(v) = get(&["id-server", "custom-rendezvous-server"]) {
        out.insert("custom-rendezvous-server".to_owned(), v);
    }
    if let Some(v) = get(&["relay-server", "relay"]) {
        out.insert("relay-server".to_owned(), v);
    }
    if let Some(v) = get(&["api-server", "api"]) {
        out.insert("api-server".to_owned(), v);
    }
    if let Some(v) = get(&["key"]) {
        out.insert("key".to_owned(), v);
    }
    out
}

/// Write the options into `DEFAULT_SETTINGS` (defaults layer). Returns true if
/// any server-affecting value (id/relay/key) actually changed.
fn apply_options(opts: &HashMap<String, String>) -> bool {
    if opts.is_empty() {
        return false;
    }
    let mut changed = false;
    let mut settings = config::DEFAULT_SETTINGS.write().unwrap();
    for (k, v) in opts {
        let prev = settings.get(k).cloned().unwrap_or_default();
        if &prev != v {
            if k == "custom-rendezvous-server" || k == "relay-server" || k == "key" {
                changed = true;
            }
            settings.insert(k.clone(), v.clone());
        }
    }
    changed
}

fn read_cache() -> Option<Value> {
    let data = std::fs::read_to_string(cache_path()).ok()?;
    serde_json::from_str(&data).ok()
}

fn write_cache(doc: &Value) {
    if let Ok(s) = serde_json::to_string(doc) {
        if let Err(e) = std::fs::write(cache_path(), s) {
            log::warn!("Failed to cache remote config: {e}");
        }
    }
}

/// Try each configured URL in order; return the JSON from the first live one.
fn fetch() -> Option<Value> {
    let client = reqwest::blocking::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .build()
        .ok()?;
    for url in config_urls() {
        match fetch_one(&client, &url) {
            Some(v) => {
                log::info!("Remote config: fetched from {url}");
                return Some(v);
            }
            None => {
                log::warn!("Remote config: {url} is down, trying next");
            }
        }
    }
    log::warn!("Remote config: all URLs are down");
    None
}

fn fetch_one(client: &reqwest::blocking::Client, url: &str) -> Option<Value> {
    let resp = client.get(url).send().ok()?;
    let resp = resp.error_for_status().ok()?;
    resp.json::<Value>().ok()
}

/// Entry point. Runs the cache-apply + first fetch on a dedicated thread (so
/// the blocking HTTP client never collides with a Tokio runtime), then keeps
/// polling. The caller blocks for at most `FETCH_TIMEOUT` waiting for the first
/// attempt so the first connection already uses the remote servers.
static STARTED: AtomicBool = AtomicBool::new(false);

pub fn init_and_start() {
    // Idempotent: tolerate being called from more than one entry point
    // (desktop `core_main`, Android `initialize`) within the same process.
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    // Apply the cached document immediately (instant, works offline).
    let have_cache = match read_cache() {
        Some(doc) => {
            apply_options(&extract_options(&doc));
            log::info!("Remote config: applied cached document");
            true
        }
        None => false,
    };

    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let started = std::thread::Builder::new()
        .name("remote-config".into())
        .spawn(move || {
            // First fetch at startup. Don't restart here: the rendezvous
            // mediator hasn't connected yet.
            if let Some(doc) = fetch() {
                apply_options(&extract_options(&doc));
                write_cache(&doc);
                log::info!("Remote config: applied fresh document at startup");
            }
            let _ = tx.send(());
            // Keep polling for live updates.
            poll_loop();
        })
        .is_ok();

    // Only block startup on the very first run (no cached servers yet), so the
    // first connection already uses the remote servers. On later launches the
    // cache is applied instantly and the fetch refreshes in the background.
    if started && !have_cache {
        let _ = rx.recv_timeout(FETCH_TIMEOUT + Duration::from_secs(2));
    }
}

fn poll_loop() {
    loop {
        std::thread::sleep(POLL_INTERVAL);
        if let Some(doc) = fetch() {
            let changed = apply_options(&extract_options(&doc));
            write_cache(&doc);
            if changed {
                log::info!("Remote config: servers changed, restarting rendezvous");
                // Hot-switch: reconnect to the new ID/relay server immediately.
                crate::RendezvousMediator::restart();
            }
        }
    }
}
