use crate::app_error::AppError;
use crate::cli::{DaemonOptions, ReconcileOptions};
use crate::reconcile::{compute_deployment_hash_for_revision, run_reconcile_once};
use crate::state::{Context, SnapshotMetadata};
use crate::util::log;
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use rustls_pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn serve(ctx: Context, daemon_opts: DaemonOptions) -> Result<(), AppError> {
    verify_daemon_config_tree_permissions(&daemon_opts.config_path)?;
    ctx.ensure_prereqs()?;
    let tls_config = match (&daemon_opts.tls_crt, &daemon_opts.tls_key) {
        (Some(crt), Some(key)) => Some(Arc::new(load_tls_config(crt, key)?)),
        (None, None) => None,
        _ => {
            return Err(AppError::msg(
                "--tls-crt and --tls-key must be provided together",
            ))
        }
    };
    let listener = TcpListener::bind(&daemon_opts.listen)
        .map_err(|err| AppError::msg(format!("failed to bind {}: {err}", daemon_opts.listen)))?;
    let scheme = if tls_config.is_some() {
        "https"
    } else {
        "http"
    };
    log(&format!(
        "Daemon listening on {scheme}://{}",
        daemon_opts.listen
    ));

    let state = Arc::new(Mutex::new(DaemonState {
        phase: "idle".to_string(),
        queued: false,
        current_hash: ctx.read_current_hash().ok().flatten(),
        active_head_hash: None,
        queued_head_hash: None,
        last_result: None,
        last_error: None,
        last_started_at: None,
        last_finished_at: None,
        compare_cache: BTreeMap::new(),
    }));
    let ctx = Arc::new(ctx);
    let reconcile = daemon_opts.reconcile.clone();

    for stream in listener.incoming() {
        let stream = stream?;
        let state = state.clone();
        let ctx = ctx.clone();
        let reconcile = reconcile.clone();
        let tls_config = tls_config.clone();
        thread::spawn(move || {
            let result = if let Some(tls_config) = tls_config {
                handle_tls_connection(stream, tls_config, state, ctx, reconcile)
            } else {
                handle_connection(stream, state, ctx, reconcile)
            };
            if let Err(err) = result {
                eprintln!("[stow][daemon][error] {err}");
            }
        });
    }
    Ok(())
}

fn verify_daemon_config_tree_permissions(path: &Path) -> Result<(), AppError> {
    let root = path.parent().ok_or_else(|| {
        AppError::msg(format!(
            "daemon config path has no parent directory: {}",
            path.display()
        ))
    })?;
    verify_root_read_only_path(root)?;
    verify_root_read_only_path(path)
}

fn verify_root_read_only_path(path: &Path) -> Result<(), AppError> {
    let metadata = fs::symlink_metadata(path).map_err(|err| {
        AppError::msg(format!(
            "failed to inspect daemon config path {}: {err}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_symlink() {
        return Err(AppError::msg(format!(
            "daemon config folder must not contain symlinks: {}",
            path.display()
        )));
    }
    if metadata.uid() != 0 {
        return Err(AppError::msg(format!(
            "daemon config folder must be owned by root throughout: {}",
            path.display()
        )));
    }

    let mode = metadata.mode() & 0o777;
    if metadata.is_dir() {
        if mode != 0o500 {
            return Err(AppError::msg(format!(
                "daemon config directories must be root-read-only/traversable (0500), got {mode:o}: {}",
                path.display()
            )));
        }
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            verify_root_read_only_path(&entry.path())?;
        }
    } else if metadata.is_file() {
        if mode != 0o400 {
            return Err(AppError::msg(format!(
                "daemon config files must be root-read-only (0400), got {mode:o}: {}",
                path.display()
            )));
        }
    } else {
        return Err(AppError::msg(format!(
            "daemon config folder must contain only regular files and directories: {}",
            path.display()
        )));
    }
    Ok(())
}

fn load_tls_config(crt_path: &Path, key_path: &Path) -> Result<ServerConfig, AppError> {
    let cert_pem = fs::read(crt_path).map_err(|err| {
        AppError::msg(format!(
            "failed to read TLS certificate {}: {err}",
            crt_path.display()
        ))
    })?;
    let key_pem = fs::read(key_path).map_err(|err| {
        AppError::msg(format!(
            "failed to read TLS private key {}: {err}",
            key_path.display()
        ))
    })?;
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| {
            AppError::msg(format!(
                "failed to parse TLS certificate PEM {}: {err}",
                crt_path.display()
            ))
        })?;
    if certs.is_empty() {
        return Err(AppError::msg(format!(
            "TLS certificate file contains no certificates: {}",
            crt_path.display()
        )));
    }
    let key = PrivateKeyDer::from_pem_slice(&key_pem).map_err(|err| {
        AppError::msg(format!(
            "failed to parse TLS private key PEM {}: {err}",
            key_path.display()
        ))
    })?;
    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|err| AppError::msg(format!("failed to build TLS server config: {err}")))
}

fn handle_tls_connection(
    stream: TcpStream,
    tls_config: Arc<ServerConfig>,
    state: Arc<Mutex<DaemonState>>,
    ctx: Arc<Context>,
    reconcile: ReconcileOptions,
) -> Result<(), AppError> {
    let connection = ServerConnection::new(tls_config)
        .map_err(|err| AppError::msg(format!("failed to create TLS connection: {err}")))?;
    let stream = StreamOwned::new(connection, stream);
    handle_connection(stream, state, ctx, reconcile)
}

fn handle_connection<S: Read + Write>(
    mut stream: S,
    state: Arc<Mutex<DaemonState>>,
    ctx: Arc<Context>,
    reconcile: ReconcileOptions,
) -> Result<(), AppError> {
    let request = read_request(&mut stream)?;
    let response = match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") | ("GET", "/status") => {
            let head_hash = request.query.get("head_hash").cloned();
            let status = snapshot_status(&state, &ctx, head_hash);
            json_response(200, &status)?
        }
        ("POST", "/trigger") => {
            let head_hash = request.query.get("head_hash").cloned();
            let status = enqueue_trigger(state, ctx, reconcile, head_hash)?;
            json_response(202, &status)?
        }
        ("GET", "/gitlab.svg") => {
            let git_hash = request.query.get("git_hash").cloned();
            let badge = gitlab_badge(&state, &ctx, git_hash)?;
            svg_response(200, &badge)?
        }
        _ => json_response(404, &ErrorResponse { error: "not found" })?,
    };
    stream.write_all(&response)?;
    Ok(())
}

fn enqueue_trigger(
    state: Arc<Mutex<DaemonState>>,
    ctx: Arc<Context>,
    reconcile: ReconcileOptions,
    head_hash: Option<String>,
) -> Result<StatusResponse, AppError> {
    let mut start_worker = None;
    {
        let mut guard = state
            .lock()
            .map_err(|_| AppError::msg("daemon state poisoned"))?;
        guard.current_hash = ctx.read_current_hash().ok().flatten();
        if guard.phase == "idle" {
            guard.phase = "starting".to_string();
            guard.active_head_hash = head_hash.clone();
            guard.last_started_at = Some(now_unix());
            start_worker = Some(head_hash);
        } else {
            guard.queued = true;
            if head_hash.is_some() {
                guard.queued_head_hash = head_hash;
            }
        }
    }

    if let Some(initial_head) = start_worker {
        let state = state.clone();
        let ctx = ctx.clone();
        thread::spawn(move || daemon_worker(state, ctx, reconcile, initial_head));
    }

    Ok(snapshot_status(&state, &ctx, None))
}

fn daemon_worker(
    state: Arc<Mutex<DaemonState>>,
    ctx: Arc<Context>,
    reconcile: ReconcileOptions,
    mut active_head_hash: Option<String>,
) {
    loop {
        {
            if let Ok(mut guard) = state.lock() {
                guard.phase = "reconciling".to_string();
                guard.active_head_hash = active_head_hash.clone();
                guard.last_started_at = Some(now_unix());
                guard.last_error = None;
            }
        }

        let result = run_reconcile_once(&ctx, &reconcile);
        let latest_hash = ctx.read_current_hash().ok().flatten();

        let mut next_head = None;
        let mut should_continue = false;
        if let Ok(mut guard) = state.lock() {
            guard.current_hash = latest_hash;
            guard.last_finished_at = Some(now_unix());
            match result {
                Ok(()) => {
                    guard.last_result = Some("ok".to_string());
                    guard.last_error = None;
                }
                Err(err) => {
                    guard.last_result = Some("error".to_string());
                    guard.last_error = Some(err.to_string());
                }
            }

            if guard.queued {
                guard.queued = false;
                next_head = guard.queued_head_hash.take();
                guard.phase = "starting".to_string();
                should_continue = true;
            } else {
                guard.phase = "idle".to_string();
                guard.active_head_hash = None;
            }
        }

        if should_continue {
            active_head_hash = next_head;
            continue;
        }
        break;
    }
}

fn snapshot_status(
    state: &Arc<Mutex<DaemonState>>,
    ctx: &Context,
    requested_head_hash: Option<String>,
) -> StatusResponse {
    let mut guard = state.lock().expect("daemon state poisoned");
    guard.current_hash = ctx.read_current_hash().ok().flatten();
    let expected_hash = requested_head_hash
        .as_deref()
        .and_then(|head| expected_hash_for_git_hash(&mut guard, ctx, head).ok());
    let label = status_label(
        &guard,
        requested_head_hash.as_deref(),
        expected_hash.as_deref(),
    );
    StatusResponse {
        phase: guard.phase.clone(),
        queued: guard.queued,
        current_hash: guard.current_hash.clone(),
        expected_hash,
        snapshot: ctx.read_current_snapshot_metadata().ok().flatten(),
        snapshot_path: ctx
            .current_snapshot_path()
            .ok()
            .flatten()
            .map(|p| p.display().to_string()),
        active_head_hash: guard.active_head_hash.clone(),
        queued_head_hash: guard.queued_head_hash.clone(),
        requested_head_hash,
        status_label: label,
        last_result: guard.last_result.clone(),
        last_error: guard.last_error.clone(),
        last_started_at: guard.last_started_at,
        last_finished_at: guard.last_finished_at,
    }
}

fn status_label(
    state: &DaemonState,
    requested_head_hash: Option<&str>,
    expected_hash: Option<&str>,
) -> String {
    let desired_hash = expected_hash.or(requested_head_hash);
    match requested_head_hash {
        Some(head) if state.active_head_hash.as_deref() == Some(head) => {
            "reconciling-requested-head".to_string()
        }
        Some(head) if state.queued && state.queued_head_hash.as_deref() == Some(head) => {
            "queued-requested-head".to_string()
        }
        Some(_) if state.current_hash.as_deref() == desired_hash && state.phase == "idle" => {
            "at-requested-head".to_string()
        }
        Some(_) if state.current_hash.as_deref() == desired_hash => {
            "at-requested-head-busy".to_string()
        }
        Some(_) if state.phase == "idle" => "different-head-idle".to_string(),
        Some(_) => "different-head-busy".to_string(),
        None if state.phase == "idle" => "idle".to_string(),
        None if state.queued => "reconciling-queued".to_string(),
        None => "reconciling".to_string(),
    }
}

fn gitlab_badge(
    state: &Arc<Mutex<DaemonState>>,
    ctx: &Context,
    git_hash: Option<String>,
) -> Result<String, AppError> {
    let Some(git_hash) = git_hash else {
        return Ok(render_badge("stow", "missing git_hash", "#9f9f9f"));
    };

    let expected_hash = {
        let mut guard = state
            .lock()
            .map_err(|_| AppError::msg("daemon state poisoned"))?;
        match expected_hash_for_git_hash(&mut guard, ctx, &git_hash) {
            Ok(expected_hash) => expected_hash,
            Err(_) => return Ok(render_badge("stow", "couldn't get it", "#9f9f9f")),
        }
    };

    let status = snapshot_status(state, ctx, Some(git_hash.clone()));
    let (message, color) = if status.current_hash.as_deref() == Some(expected_hash.as_str()) {
        if status.phase == "idle" {
            ("running", "#4c1")
        } else {
            ("running/busy", "#4c1")
        }
    } else if status.active_head_hash.as_deref() == Some(git_hash.as_str()) {
        ("reconciling", "#dfb317")
    } else if status.queued && status.queued_head_hash.as_deref() == Some(git_hash.as_str()) {
        ("queued", "#fe7d37")
    } else if status.last_result.as_deref() == Some("error") {
        ("error", "#e05d44")
    } else {
        ("different", "#fe7d37")
    };
    Ok(render_badge("stow", message, color))
}

fn expected_hash_for_git_hash(
    state: &mut DaemonState,
    ctx: &Context,
    git_hash: &str,
) -> Result<String, AppError> {
    if let Some(cached) = state.compare_cache.get(git_hash) {
        return Ok(cached.expected_hash.clone());
    }
    let hashes = compute_deployment_hash_for_revision(ctx, git_hash)?;
    let expected_hash = hashes.deployment_hash;
    state.compare_cache.insert(
        git_hash.to_string(),
        CompareCacheEntry {
            expected_hash: expected_hash.clone(),
        },
    );
    Ok(expected_hash)
}

fn read_request<S: Read>(stream: &mut S) -> Result<HttpRequest, AppError> {
    let mut buffer = [0u8; 8192];
    let read = stream.read(&mut buffer)?;
    if read == 0 {
        return Err(AppError::msg("empty HTTP request"));
    }
    let request = String::from_utf8_lossy(&buffer[..read]);
    let mut lines = request.lines();
    let request_line = lines
        .next()
        .ok_or_else(|| AppError::msg("missing HTTP request line"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| AppError::msg("missing HTTP method"))?
        .to_string();
    let target = parts
        .next()
        .ok_or_else(|| AppError::msg("missing HTTP target"))?;
    let (path, query) = split_target(target);
    Ok(HttpRequest {
        method,
        path,
        query,
    })
}

fn split_target(target: &str) -> (String, BTreeMap<String, String>) {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut params = BTreeMap::new();
    for part in query.split('&').filter(|part| !part.is_empty()) {
        let (key, value) = part.split_once('=').unwrap_or((part, ""));
        params.insert(key.to_string(), percent_decode(value));
    }
    (path.to_string(), params)
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        match bytes[idx] {
            b'+' => {
                out.push(b' ');
                idx += 1;
            }
            b'%' if idx + 2 < bytes.len() => {
                if let (Some(a), Some(b)) = (from_hex(bytes[idx + 1]), from_hex(bytes[idx + 2])) {
                    out.push((a << 4) | b);
                    idx += 3;
                } else {
                    out.push(bytes[idx]);
                    idx += 1;
                }
            }
            byte => {
                out.push(byte);
                idx += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn from_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn json_response<T: Serialize>(status: u16, payload: &T) -> Result<Vec<u8>, AppError> {
    let body = serde_json::to_vec_pretty(payload)
        .map_err(|err| AppError::msg(format!("failed to serialize daemon response: {err}")))?;
    let status_text = match status {
        200 => "OK",
        202 => "Accepted",
        404 => "Not Found",
        _ => "OK",
    };
    let headers = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut response = headers.into_bytes();
    response.extend_from_slice(&body);
    Ok(response)
}

fn svg_response(status: u16, svg: &str) -> Result<Vec<u8>, AppError> {
    let body = svg.as_bytes();
    let status_text = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "OK",
    };
    let headers = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: image/svg+xml\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut response = headers.into_bytes();
    response.extend_from_slice(body);
    Ok(response)
}

fn render_badge(label: &str, message: &str, color: &str) -> String {
    let label_width = 6 * label.len() + 20;
    let msg_width = 6 * message.len() + 20;
    let total_width = label_width + msg_width;
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{total_width}\" height=\"20\" role=\"img\" aria-label=\"{label}: {message}\"><linearGradient id=\"s\" x2=\"0\" y2=\"100%\"><stop offset=\"0\" stop-color=\"#fff\" stop-opacity=\".7\"/><stop offset=\".1\" stop-color=\"#aaa\" stop-opacity=\".1\"/><stop offset=\".9\" stop-opacity=\".3\"/><stop offset=\"1\" stop-opacity=\".5\"/></linearGradient><clipPath id=\"r\"><rect width=\"{total_width}\" height=\"20\" rx=\"3\" fill=\"#fff\"/></clipPath><g clip-path=\"url(#r)\"><rect width=\"{label_width}\" height=\"20\" fill=\"#555\"/><rect x=\"{label_width}\" width=\"{msg_width}\" height=\"20\" fill=\"{color}\"/><rect width=\"{total_width}\" height=\"20\" fill=\"url(#s)\"/></g><g fill=\"#fff\" text-anchor=\"middle\" font-family=\"Verdana,Geneva,DejaVu Sans,sans-serif\" text-rendering=\"geometricPrecision\" font-size=\"11\"><text x=\"{label_center}\" y=\"15\" fill=\"#010101\" fill-opacity=\".3\">{label}</text><text x=\"{label_center}\" y=\"14\">{label}</text><text x=\"{msg_center}\" y=\"15\" fill=\"#010101\" fill-opacity=\".3\">{message}</text><text x=\"{msg_center}\" y=\"14\">{message}</text></g></svg>",
        label_center = label_width / 2,
        msg_center = label_width + (msg_width / 2),
    )
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    query: BTreeMap<String, String>,
}

struct DaemonState {
    phase: String,
    queued: bool,
    current_hash: Option<String>,
    active_head_hash: Option<String>,
    queued_head_hash: Option<String>,
    last_result: Option<String>,
    last_error: Option<String>,
    last_started_at: Option<u64>,
    last_finished_at: Option<u64>,
    compare_cache: BTreeMap<String, CompareCacheEntry>,
}

struct CompareCacheEntry {
    expected_hash: String,
}

#[derive(Serialize)]
struct StatusResponse {
    phase: String,
    queued: bool,
    current_hash: Option<String>,
    expected_hash: Option<String>,
    snapshot: Option<SnapshotMetadata>,
    snapshot_path: Option<String>,
    active_head_hash: Option<String>,
    queued_head_hash: Option<String>,
    requested_head_hash: Option<String>,
    status_label: String,
    last_result: Option<String>,
    last_error: Option<String>,
    last_started_at: Option<u64>,
    last_finished_at: Option<u64>,
}

#[derive(Serialize)]
struct ErrorResponse<'a> {
    error: &'a str,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{
        json_response, percent_decode, read_request, render_badge, split_target, status_label,
        svg_response, verify_root_read_only_path, DaemonState,
    };
    use crate::test_support::TempDir;
    use std::collections::BTreeMap;

    fn state(
        phase: &str,
        queued: bool,
        current: Option<&str>,
        active: Option<&str>,
        queued_head: Option<&str>,
    ) -> DaemonState {
        DaemonState {
            phase: phase.to_string(),
            queued,
            current_hash: current.map(str::to_string),
            active_head_hash: active.map(str::to_string),
            queued_head_hash: queued_head.map(str::to_string),
            last_result: None,
            last_error: None,
            last_started_at: None,
            last_finished_at: None,
            compare_cache: BTreeMap::new(),
        }
    }

    #[test]
    fn target_splitting_separates_path_and_query() {
        let (path, query) = split_target("/status?head_hash=abc&x=1");
        assert_eq!(path, "/status");
        assert_eq!(query.get("head_hash").map(String::as_str), Some("abc"));
        assert_eq!(query.get("x").map(String::as_str), Some("1"));

        let (path, query) = split_target("/trigger");
        assert_eq!(path, "/trigger");
        assert!(query.is_empty());

        // key without value, empty parts
        let (_, query) = split_target("/x?flag&&a=b");
        assert_eq!(query.get("flag").map(String::as_str), Some(""));
        assert_eq!(query.get("a").map(String::as_str), Some("b"));
    }

    #[test]
    fn percent_decoding_handles_plus_hex_and_malformed_escapes() {
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("%2Fpath"), "/path");
        // malformed escapes pass through literally
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn request_parsing_extracts_method_path_and_query() {
        let raw = b"GET /status?head_hash=abc HTTP/1.1\r\nHost: x\r\n\r\n";
        let request = read_request(&mut raw.as_slice()).unwrap();
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/status");
        assert_eq!(
            request.query.get("head_hash").map(String::as_str),
            Some("abc")
        );

        assert!(read_request(&mut (&[] as &[u8])).is_err());
    }

    #[test]
    fn status_labels_cover_requested_head_states() {
        // actively reconciling the requested head
        assert_eq!(
            status_label(
                &state("reconciling", false, None, Some("h"), None),
                Some("h"),
                None
            ),
            "reconciling-requested-head"
        );
        // requested head queued behind a running reconcile
        assert_eq!(
            status_label(
                &state("reconciling", true, None, Some("other"), Some("h")),
                Some("h"),
                None
            ),
            "queued-requested-head"
        );
        // converged and idle (expected hash resolved)
        assert_eq!(
            status_label(
                &state("idle", false, Some("dep-hash"), None, None),
                Some("h"),
                Some("dep-hash")
            ),
            "at-requested-head"
        );
        // converged but busy with something else
        assert_eq!(
            status_label(
                &state("reconciling", false, Some("dep-hash"), Some("other"), None),
                Some("h"),
                Some("dep-hash")
            ),
            "at-requested-head-busy"
        );
        // without expected hash the raw head is compared against current
        assert_eq!(
            status_label(
                &state("idle", false, Some("h"), None, None),
                Some("h"),
                None
            ),
            "at-requested-head"
        );
        assert_eq!(
            status_label(
                &state("idle", false, Some("other"), None, None),
                Some("h"),
                None
            ),
            "different-head-idle"
        );
        assert_eq!(
            status_label(
                &state("reconciling", false, Some("other"), Some("x"), None),
                Some("h"),
                None
            ),
            "different-head-busy"
        );
    }

    #[test]
    fn status_labels_without_requested_head() {
        assert_eq!(
            status_label(&state("idle", false, None, None, None), None, None),
            "idle"
        );
        assert_eq!(
            status_label(&state("reconciling", true, None, None, None), None, None),
            "reconciling-queued"
        );
        assert_eq!(
            status_label(&state("reconciling", false, None, None, None), None, None),
            "reconciling"
        );
    }

    #[test]
    fn json_response_sets_status_line_and_headers() {
        let body = serde_json::json!({"ok": true});
        let response = String::from_utf8(json_response(202, &body).unwrap()).unwrap();
        assert!(response.starts_with("HTTP/1.1 202 Accepted\r\n"));
        assert!(response.contains("Content-Type: application/json\r\n"));
        assert!(response.contains("Connection: close\r\n"));
        assert!(response.ends_with("}"));

        let not_found = String::from_utf8(json_response(404, &body).unwrap()).unwrap();
        assert!(not_found.starts_with("HTTP/1.1 404 Not Found\r\n"));
    }

    #[test]
    fn svg_response_disables_caching() {
        let response = String::from_utf8(svg_response(200, "<svg/>").unwrap()).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("Content-Type: image/svg+xml\r\n"));
        assert!(response.contains("Cache-Control: no-store\r\n"));
        assert!(response.ends_with("<svg/>"));
    }

    #[test]
    fn badge_rendering_embeds_label_message_and_color() {
        let svg = render_badge("stow", "running", "#4c1");
        assert!(svg.contains(">stow</text>"));
        assert!(svg.contains(">running</text>"));
        assert!(svg.contains("fill=\"#4c1\""));
        assert!(svg.contains("aria-label=\"stow: running\""));
    }

    #[test]
    fn daemon_config_permission_check_rejects_symlinks_and_open_permissions() {
        let dir = TempDir::new("daemon-perms");
        let config = dir.write("daemon.yaml", "gitlabBase: https://x\n");

        // files written by the test runner are neither root-owned nor 0400
        assert!(verify_root_read_only_path(&config).is_err());

        let link = dir.path().join("link.yaml");
        std::os::unix::fs::symlink(&config, &link).unwrap();
        assert!(verify_root_read_only_path(&link).is_err());

        assert!(verify_root_read_only_path(&dir.path().join("missing.yaml")).is_err());
    }
}
