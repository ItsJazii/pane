//! DeepSeek Harness desktop app: sessions live under
//! `%USERPROFILE%\.dsh\sessions\<project>\<session-<uuid>>\` as
//! zstd-framed JSONL (`session.v4.jsonl.zstd`, older `session.jsonl.zstd`,
//! plain `session*.jsonl` tolerated). Written live, so the last frame can
//! be truncated mid-write — a partial decode still counts, it just never
//! caches. All strictly read-only.

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use super::minimax::FileStamp;

/// DSH_HOME relocates the whole Harness home (portable installs).
pub(crate) fn home() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("DSH_HOME") {
        let dir = dir.trim().to_string();
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    dirs::home_dir().map(|h| h.join(".dsh"))
}

/// One billed turn: the route (`request/header`'s config, else
/// `request/context`) plus its usage counters. `input` is UNCACHED input
/// — DeepSeek's usage shape bills cache reads separately.
#[derive(Clone, Debug)]
pub struct DshEvent {
    pub ts_ms: i64,
    pub provider: String,
    pub model: String,
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

/// Compressed session files stay small; refuse absurd blobs anyway so a
/// hostile file can't stall the scan or balloon RAM on decode.
const MAX_SESSION_BYTES: u64 = 256 * 1024 * 1024;
/// Same guard after decode — a zip-bomb-shaped file must not grow it.
const MAX_DECODED_BYTES: usize = 512 * 1024 * 1024;
/// A single JSONL line this long is a transcript dump, not an event.
const SESSION_LINE_CAP: usize = 8 * 1024 * 1024;

/// `session*.jsonl.zstd` / `session*.jsonl` under `root`, recursive —
/// Harness nests sessions one project dir deep. The bool reports a
/// complete listing: a read_dir/entry/file_type error (other than a
/// missing dir, which is normal) means some subtree went unread, so
/// callers must not treat an absent path as deleted.
fn session_files(root: &Path) -> (Vec<PathBuf>, bool) {
    let mut out = Vec::new();
    let mut listed = true;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                listed = false;
                continue;
            }
        };
        for entry in entries {
            let Ok(e) = entry else {
                listed = false;
                continue;
            };
            let p = e.path();
            let Ok(ft) = e.file_type() else {
                listed = false;
                continue;
            };
            if ft.is_dir() {
                stack.push(p);
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.starts_with("session")
                && (name.ends_with(".jsonl.zstd") || name.ends_with(".jsonl"))
            {
                out.push(p);
            }
        }
    }
    out.sort();
    (out, listed)
}

/// Whether any Harness session file exists — enough for the keyless card.
pub(crate) fn sessions_present() -> bool {
    home().is_some_and(|h| !session_files(&h.join("sessions")).0.is_empty())
}

/// Decode a session blob into UTF-8 JSONL bytes. zstd's stream decoder
/// walks concatenated frames to EOF; a truncated trailing frame (Harness
/// appends while we read) errors mid-stream, so whatever decoded first is
/// kept and the second tuple slot reports `complete: false`. Plain
/// (uncompressed) `.jsonl` passes through untouched.
fn decode(bytes: &[u8], zstd_framed: bool) -> (Vec<u8>, bool) {
    if !zstd_framed {
        return (bytes.to_vec(), true);
    }
    use std::io::Read;
    let mut decoder = match zstd::stream::read::Decoder::new(bytes) {
        Ok(d) => d,
        Err(_) => return (Vec::new(), false),
    };
    let mut out = Vec::new();
    let mut buf = [0u8; 128 * 1024];
    loop {
        match decoder.read(&mut buf) {
            Ok(0) => return (out, true),
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if out.len() > MAX_DECODED_BYTES {
                    return (out, false);
                }
            }
            Err(_) => return (out, false),
        }
    }
}

/// Route for the request in flight. `request/header`'s config is
/// authoritative, so once a header lands `header_seen` locks the route
/// against the `request/context` line that follows it; a context only
/// fills in when no header arrived. The lock clears on each assistant
/// reply — a new request starts there.
#[derive(Default)]
struct RouteState {
    route: Option<(String, String)>,
    header_seen: bool,
}

/// One JSONL line → possibly an event; also feeds the running route.
/// `route` is the (provider, model) the following assistant messages
/// bill to: `request/header`'s `data.header.config` is authoritative,
/// `request/context` repeats it as a fallback. `model/selection` is the
/// UI pick only — a session can select one provider and request through
/// another, so it never moves the route.
fn push_line(bytes: &[u8], route: &mut RouteState, out: &mut Vec<DshEvent>) {
    let mut b = bytes;
    while matches!(b.last(), Some(b'\n') | Some(b'\r')) {
        b = &b[..b.len() - 1];
    }
    let Ok(text) = std::str::from_utf8(b) else {
        return;
    };
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return;
    };
    let data = v.get("data");
    match v.get("type").and_then(Value::as_str) {
        Some("request/header") => {
            route.header_seen = true;
            let cfg = data.and_then(|d| d.get("header")).and_then(|h| h.get("config"));
            if let Some(r) = route_from(cfg) {
                route.route = Some(r);
            }
        }
        Some("request/context") => {
            if !route.header_seen {
                if let Some(r) = route_from(data) {
                    route.route = Some(r);
                }
            }
        }
        Some("assistant/message") => {
            route.header_seen = false; // a new request starts after replies
            let Some(u) = data.and_then(|d| d.get("usage")) else {
                return;
            };
            let Some((provider, model)) = route.route.clone() else {
                return;
            };
            let Some(ts_ms) = v.get("time").and_then(Value::as_f64) else {
                return;
            };
            let num = |k: &str| u.get(k).and_then(Value::as_f64).unwrap_or(0.0);
            out.push(DshEvent {
                ts_ms: ts_ms as i64,
                provider,
                model,
                input: num("inputTokens"),
                output: num("outputTokens"),
                cache_read: num("cacheReadTokens"),
                cache_write: num("cacheWriteTokens"),
            });
        }
        _ => {}
    }
}

fn route_from(v: Option<&Value>) -> Option<(String, String)> {
    let v = v?;
    let provider = v.get("provider").and_then(Value::as_str)?.to_string();
    let model = v.get("model").and_then(Value::as_str).unwrap_or_default().to_string();
    Some((provider, model))
}

/// (events, complete) for one session file. `None` = unreadable right
/// now (av lock, mid-rotate) — transient, the caller keeps last-good.
fn session_events(path: &Path) -> Option<(Vec<DshEvent>, bool)> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_SESSION_BYTES {
        return Some((Vec::new(), true)); // oversized: skip for good
    }
    let bytes = std::fs::read(path).ok()?;
    let framed = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(".zstd"));
    let (decoded, complete) = decode(&bytes, framed);
    let mut route = RouteState::default();
    let mut out = Vec::new();
    for line in decoded.split(|b| *b == b'\n') {
        if line.len() > SESSION_LINE_CAP {
            continue;
        }
        push_line(line, &mut route, &mut out);
    }
    Some((out, complete))
}

type SessionCache = HashMap<PathBuf, (FileStamp, bool, Vec<DshEvent>)>;

/// Token events across every Harness session under `home`, newest file
/// state each call. Each file is cached on its (mtime, len) stamp; a file
/// untouched since `cutoff` is skipped without opening it. An incomplete
/// decode serves what it got but never marks the stamp complete — a live
/// session is retried whole next scan. Returns (events, complete) where
/// `complete: false` means something readable-on-disk didn't fully make
/// it this pass.
pub(crate) fn collect_from(
    home: &Path,
    cutoff: SystemTime,
    cache: &mut SessionCache,
) -> (Vec<DshEvent>, bool) {
    let (files, listed) = session_files(&home.join("sessions"));
    collect_files(files, listed, cutoff, cache)
}

/// The per-file half of `collect_from`, split out so a listing gap can
/// be exercised without OS permission tricks. `listed: false` means the
/// session tree didn't fully enumerate: absent paths are then NOT
/// pruned from the cache (a missing entry may just live under the
/// unreadable subtree) and their last-good events still serve.
fn collect_files(
    files: Vec<PathBuf>,
    listed: bool,
    cutoff: SystemTime,
    cache: &mut SessionCache,
) -> (Vec<DshEvent>, bool) {
    let live: HashSet<&PathBuf> = files.iter().collect();
    if listed {
        cache.retain(|k, _| live.contains(k));
    }
    let cutoff_ms = cutoff
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let mut out = Vec::new();
    let mut complete = listed;
    for file in &files {
        let meta = match std::fs::metadata(file) {
            Ok(meta) => meta,
            // Vanished between listing and stat — genuinely gone.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                cache.remove(file);
                continue;
            }
            // Unstattable: serve last-good and flag the gap instead of
            // letting an epoch stamp skip it below the cutoff.
            Err(_) => {
                complete = false;
                if let Some((_, _, events)) = cache.get(file) {
                    out.extend(events.iter().filter(|e| e.ts_ms >= cutoff_ms).cloned());
                }
                continue;
            }
        };
        let stamp: FileStamp = (meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), meta.len());
        if stamp.0 < cutoff {
            continue; // nothing newer than its last write can be inside
        }
        let entry = cache
            .entry(file.clone())
            .or_insert_with(|| ((SystemTime::UNIX_EPOCH, 0), false, Vec::new()));
        if entry.0 != stamp || !entry.1 {
            match session_events(file) {
                Some((events, done)) => {
                    entry.2 = events;
                    if done {
                        entry.0 = stamp;
                        entry.1 = true;
                    } else {
                        complete = false;
                    }
                }
                // Unreadable: keep previous events AND the old stamp so
                // the next scan retries instead of caching a gap.
                None => complete = false,
            }
        }
        out.extend(entry.2.iter().filter(|e| e.ts_ms >= cutoff_ms).cloned());
    }
    if !listed {
        // Entries under the unreadable subtree never made `files` —
        // serve their last-good events rather than dropping them.
        for (path, (_, _, events)) in cache.iter() {
            if live.contains(path) {
                continue;
            }
            out.extend(events.iter().filter(|e| e.ts_ms >= cutoff_ms).cloned());
        }
    }
    (out, complete)
}

/// Dated token events from Harness' local session files. See
/// `collect_from` for caching and the completeness contract.
pub fn collect_usage_events(cutoff: SystemTime) -> (Vec<DshEvent>, bool) {
    static CACHE: Mutex<Option<SessionCache>> = Mutex::new(None);
    let Some(home) = home() else {
        return (Vec::new(), true);
    };
    match CACHE.lock() {
        Ok(mut guard) => collect_from(&home, cutoff, guard.get_or_insert_with(HashMap::new)),
        Err(_) => (Vec::new(), false),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    /// Serializes tests that point DSH_HOME at a fixture dir —
    /// deepseek.rs's keyless-snapshot test shares this lock.
    pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn header(provider: &str, model: &str) -> String {
        json!({"type": "request/header", "seq": 1, "time": 0,
            "data": {"header": {"config": {"provider": provider, "model": model}}}})
        .to_string()
    }

    fn context(provider: &str, model: &str) -> String {
        json!({"type": "request/context", "seq": 2, "time": 0,
            "data": {"provider": provider, "model": model}})
        .to_string()
    }

    fn message(ts_ms: i64, input: f64, output: f64, cache_read: f64) -> String {
        json!({"type": "assistant/message", "seq": 3, "time": ts_ms,
            "data": {"usage": {"inputTokens": input, "outputTokens": output,
                "cacheReadTokens": cache_read, "cacheWriteTokens": 0.0,
                "totalTokens": input + output + cache_read}}})
        .to_string()
    }

    /// Trailing newline on purpose: concatenated frames decode back to
    /// back-to-back text, and Harness always terminates a line — without
    /// it the last line of frame N fuses with the first of frame N+1.
    fn zstd_file(lines: &[String]) -> Vec<u8> {
        zstd::encode_all(format!("{}\n", lines.join("\n")).as_bytes(), 0).unwrap()
    }

    #[test]
    fn concatenated_frames_decode_and_route() {
        // Frame 1: deepseek-account route + a message; frame 2: an
        // aihubmix header + a message, and a model/selection that must
        // NOT move attribution before the final deepseek message.
        let frame1 = zstd_file(&[
            header("deepseek-account", "deepseek-flash"),
            message(1_760_000_000_000, 100.0, 50.0, 10.0),
        ]);
        let frame2 = zstd_file(&[
            header("aihubmix", "deepseek-v4.1-flash-1017"),
            message(1_760_000_100_000, 200.0, 60.0, 0.0),
            json!({"type": "model/selection", "seq": 9, "time": 0,
                "data": {"provider": "deepseek-account", "model": "deepseek-flash"}})
            .to_string(),
            message(1_760_000_200_000, 5.0, 5.0, 0.0),
        ]);
        let mut bytes = frame1;
        bytes.extend_from_slice(&frame2);
        let (decoded, complete) = decode(&bytes, true);
        assert!(complete);
        let mut route = RouteState::default();
        let mut events = Vec::new();
        for line in decoded.split(|b| *b == b'\n') {
            push_line(line, &mut route, &mut events);
        }
        assert_eq!(events.len(), 3);
        assert_eq!(
            (events[0].provider.as_str(), events[0].model.as_str()),
            ("deepseek-account", "deepseek-flash")
        );
        assert_eq!(
            (events[1].provider.as_str(), events[1].model.as_str()),
            ("aihubmix", "deepseek-v4.1-flash-1017")
        );
        // model/selection never rewrote the route — the last message
        // still bills aihubmix (the header's route).
        assert_eq!(events[2].provider, "aihubmix");
    }

    #[test]
    fn request_context_is_the_fallback_route() {
        let mut route = RouteState::default();
        let mut events = Vec::new();
        push_line(context("deepseek-official", "deepseek-v4-pro").as_bytes(), &mut route, &mut events);
        push_line(
            message(1_760_000_000_000, 1.0, 1.0, 0.0).as_bytes(),
            &mut route,
            &mut events,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].provider, "deepseek-official");
    }

    #[test]
    fn request_header_wins_over_later_context() {
        let mut route = RouteState::default();
        let mut events = Vec::new();
        for line in [
            header("deepseek-account", "deepseek-flash"),
            context("aihubmix", "deepseek-v4.1-flash-1017"),
            message(1_760_000_000_000, 1.0, 1.0, 0.0),
        ] {
            push_line(line.as_bytes(), &mut route, &mut events);
        }
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].provider, "deepseek-account");
        assert_eq!(events[0].model, "deepseek-flash");
    }

    #[test]
    fn context_routes_the_next_request_after_a_reply() {
        let mut route = RouteState::default();
        let mut events = Vec::new();
        for line in [
            header("deepseek-account", "deepseek-flash"),
            message(1_760_000_000_000, 1.0, 1.0, 0.0),
            context("aihubmix", "deepseek-v4.1-flash-1017"),
            message(1_760_000_100_000, 2.0, 2.0, 0.0),
        ] {
            push_line(line.as_bytes(), &mut route, &mut events);
        }
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].provider, "deepseek-account");
        assert_eq!(events[1].provider, "aihubmix");
    }

    #[test]
    fn message_without_a_route_is_skipped() {
        let mut route = RouteState::default();
        let mut events = Vec::new();
        push_line(
            message(1_760_000_000_000, 1.0, 1.0, 0.0).as_bytes(),
            &mut route,
            &mut events,
        );
        assert!(events.is_empty());
    }

    #[test]
    fn truncated_last_frame_keeps_earlier_events_incomplete() {
        let frame1 = zstd_file(&[
            header("deepseek-account", "deepseek-flash"),
            message(1_760_000_000_000, 100.0, 50.0, 10.0),
        ]);
        let frame2 = zstd_file(&[message(1_760_000_100_000, 9.0, 9.0, 0.0)]);
        let mut bytes = frame1;
        bytes.extend_from_slice(&frame2[..frame2.len() - 4]);
        let (decoded, complete) = decode(&bytes, true);
        assert!(!complete);
        let mut route = RouteState::default();
        let mut events = Vec::new();
        for line in decoded.split(|b| *b == b'\n') {
            push_line(line, &mut route, &mut events);
        }
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].input, 100.0);
    }

    #[test]
    fn plain_jsonl_sessions_parse_without_decode() {
        let lines = [
            header("deepseek-account", "deepseek-flash"),
            message(1_760_000_000_000, 7.0, 3.0, 0.0),
        ]
        .join("\n");
        let (decoded, complete) = decode(lines.as_bytes(), false);
        assert!(complete);
        let mut route = RouteState::default();
        let mut events = Vec::new();
        for line in decoded.split(|b| *b == b'\n') {
            push_line(line, &mut route, &mut events);
        }
        assert_eq!(events.len(), 1);
    }

    fn dsh_fixture(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pane-dsh-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn collect_from_reads_a_fixture_tree_once() {
        let dir = dsh_fixture("collect");
        let sess = dir.join("sessions").join("proj").join("session-abc");
        std::fs::create_dir_all(&sess).unwrap();
        let file = sess.join("session.v4.jsonl.zstd");
        let mut f = std::fs::File::create(&file).unwrap();
        f.write_all(&zstd_file(&[
            header("deepseek-account", "deepseek-flash"),
            message(1_800_000_000_000, 10.0, 4.0, 0.0),
        ]))
        .unwrap();
        let mut cache = SessionCache::new();
        let (events, complete) =
            collect_from(&dir, SystemTime::UNIX_EPOCH, &mut cache);
        assert!(complete);
        assert_eq!(events.len(), 1);
        // Second pass on the same stamp serves the cache, no re-decode.
        let (events2, complete2) =
            collect_from(&dir, SystemTime::UNIX_EPOCH, &mut cache);
        assert!(complete2 && events2.len() == 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sessions_path_as_a_file_is_a_listing_gap() {
        let dir = dsh_fixture("filegap");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("sessions"), b"not a dir").unwrap();
        let (_, listed) = session_files(&dir.join("sessions"));
        assert!(!listed);
        let mut cache = SessionCache::new();
        let (_, complete) = collect_from(&dir, SystemTime::UNIX_EPOCH, &mut cache);
        assert!(!complete);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unlisted_subtree_serves_last_good_instead_of_vanishing() {
        let dir = dsh_fixture("gap");
        let sess = dir.join("sessions").join("proj").join("session-abc");
        std::fs::create_dir_all(&sess).unwrap();
        let file = sess.join("session.v4.jsonl.zstd");
        std::fs::write(
            &file,
            zstd_file(&[
                header("deepseek-account", "deepseek-flash"),
                message(1_800_000_000_000, 10.0, 4.0, 0.0),
            ]),
        )
        .unwrap();
        let mut cache = SessionCache::new();
        let (events, complete) = collect_from(&dir, SystemTime::UNIX_EPOCH, &mut cache);
        assert!(complete && events.len() == 1);

        // A listing gap: the file isn't in `files`, yet last-good
        // events still serve and the cache entry survives.
        let (events, complete) =
            collect_files(Vec::new(), false, SystemTime::UNIX_EPOCH, &mut cache);
        assert!(!complete);
        assert_eq!(events.len(), 1);
        assert!(cache.contains_key(&file));

        // A complete listing that genuinely lacks the file prunes it.
        let (events, complete) =
            collect_files(Vec::new(), true, SystemTime::UNIX_EPOCH, &mut cache);
        assert!(complete);
        assert!(events.is_empty());
        assert!(!cache.contains_key(&file));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_vanishing_after_listing_is_not_a_gap() {
        let ghost = dsh_fixture("ghost")
            .join("sessions")
            .join("proj")
            .join("session-abc")
            .join("session.v4.jsonl.zstd");
        let mut cache = SessionCache::new();
        let (events, complete) =
            collect_files(vec![ghost], true, SystemTime::UNIX_EPOCH, &mut cache);
        assert!(complete);
        assert!(events.is_empty());
    }

    #[test]
    fn sessions_present_follows_dsh_home() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("DSH_HOME").ok();
        let dir = dsh_fixture("home");
        std::env::set_var("DSH_HOME", &dir);
        assert!(!sessions_present());
        let sess = dir.join("sessions").join("proj").join("session-x");
        std::fs::create_dir_all(&sess).unwrap();
        std::fs::write(sess.join("session.v4.jsonl.zstd"), b"anything").unwrap();
        assert!(sessions_present());
        match prev {
            Some(v) => std::env::set_var("DSH_HOME", v),
            None => std::env::remove_var("DSH_HOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
