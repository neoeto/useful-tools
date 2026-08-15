use axum::{
    body::Body,
    body::Bytes,
    extract::{ConnectInfo, DefaultBodyLimit, Multipart, Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
    Router,
};
use clap::Parser;
use percent_encoding::utf8_percent_encode;
use serde::Serialize;
use std::{
    io::IsTerminal,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll},
    time::Instant,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

struct TransferProgress {
    id: u64,
    display: Arc<TransferDisplay>,
    transferred: Arc<AtomicU64>,
    finished: Arc<AtomicBool>,
}

impl TransferProgress {
    fn add_bytes(&self, bytes: u64) {
        self.transferred.fetch_add(bytes, Ordering::Relaxed);
    }

    fn set_file(&self, file: String) {
        self.display.set_file(self.id, file);
    }

    fn finish(&self, succeeded: bool) {
        if !self.finished.swap(true, Ordering::AcqRel) {
            self.display.finish(self.id, succeeded);
        }
    }
}

struct TransferDisplay {
    state: Mutex<TransferDisplayState>,
    interactive: bool,
}

struct TransferDisplayState {
    next_id: u64,
    active: Vec<TransferEntry>,
    rendered_active: usize,
}

struct TransferEntry {
    id: u64,
    source_ip: IpAddr,
    operation: &'static str,
    file: String,
    total: Option<u64>,
    transferred: Arc<AtomicU64>,
    last_transferred: u64,
    last_updated: Instant,
}

impl TransferDisplay {
    fn new() -> Self {
        Self {
            state: Mutex::new(TransferDisplayState {
                next_id: 0,
                active: Vec::new(),
                rendered_active: 0,
            }),
            interactive: std::io::stderr().is_terminal(),
        }
    }

    fn start(
        self: &Arc<Self>,
        source_ip: IpAddr,
        operation: &'static str,
        file: String,
        total: Option<u64>,
    ) -> TransferProgress {
        let transferred = Arc::new(AtomicU64::new(0));
        let mut state = self.state.lock().expect("transfer display lock poisoned");
        let id = state.next_id;
        state.next_id += 1;
        state.active.push(TransferEntry {
            id,
            source_ip,
            operation,
            file,
            total,
            transferred: Arc::clone(&transferred),
            last_transferred: 0,
            last_updated: Instant::now(),
        });
        let entry = state.active.last().expect("transfer was just inserted");
        self.write_line(&format_transfer(entry, 0.0, "transferring"));
        state.rendered_active += 1;

        TransferProgress {
            id,
            display: Arc::clone(self),
            transferred,
            finished: Arc::new(AtomicBool::new(false)),
        }
    }

    fn set_file(&self, id: u64, file: String) {
        let mut state = self.state.lock().expect("transfer display lock poisoned");
        if let Some(entry) = state.active.iter_mut().find(|entry| entry.id == id) {
            entry.file = file;
            self.render_active(&mut state);
        }
    }

    fn refresh(&self) {
        let mut state = self.state.lock().expect("transfer display lock poisoned");
        if !state.active.is_empty() {
            self.render_active(&mut state);
        }
    }

    fn finish(&self, id: u64, succeeded: bool) {
        let mut state = self.state.lock().expect("transfer display lock poisoned");
        let Some(index) = state.active.iter().position(|entry| entry.id == id) else {
            return;
        };

        self.move_to_active_start(state.rendered_active);
        let entry = state.active.remove(index);
        self.write_line(&format_transfer(
            &entry,
            0.0,
            if succeeded { "complete" } else { "failed" },
        ));
        for entry in &state.active {
            self.write_line(&format_transfer(entry, 0.0, "transferring"));
        }
        state.rendered_active = state.active.len();
    }

    fn render_active(&self, state: &mut TransferDisplayState) {
        self.move_to_active_start(state.rendered_active);
        let now = Instant::now();
        for entry in &mut state.active {
            let transferred = entry.transferred.load(Ordering::Relaxed);
            let elapsed = now.duration_since(entry.last_updated).as_secs_f64();
            let speed = if elapsed > 0.0 {
                (transferred.saturating_sub(entry.last_transferred)) as f64 / elapsed
            } else {
                0.0
            };
            entry.last_transferred = transferred;
            entry.last_updated = now;
            self.write_line(&format_transfer(entry, speed, "transferring"));
        }
        state.rendered_active = state.active.len();
    }

    fn move_to_active_start(&self, line_count: usize) {
        if self.interactive && line_count > 0 {
            eprint!("\x1b[{}A", line_count);
        }
    }

    fn write_line(&self, line: &str) {
        if self.interactive {
            eprintln!("\r\x1b[2K{line}");
        } else {
            eprintln!("{line}");
        }
    }
}

fn format_transfer(entry: &TransferEntry, speed: f64, status: &str) -> String {
    let transferred = entry.transferred.load(Ordering::Relaxed);
    let progress = match entry.total {
        Some(total) if total > 0 => format!(
            "{} / {} ({:.0}%)",
            format_size(transferred),
            format_size(total),
            transferred as f64 / total as f64 * 100.0
        ),
        _ => format_size(transferred),
    };

    format!(
        "[{}] {} | {} | {} | {}/s | {}",
        entry.operation,
        entry.source_ip,
        entry.file,
        progress,
        format_size_float(speed),
        status
    )
}

fn format_size_float(size: f64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;

    if size >= GB {
        format!("{size:.1} GB")
    } else if size >= MB {
        format!("{:.1} MB", size / MB)
    } else if size >= KB {
        format!("{:.1} KB", size / KB)
    } else {
        format!("{size:.0} B")
    }
}

struct ProgressStream<S> {
    inner: S,
    progress: TransferProgress,
    expected_bytes: u64,
}

impl<S, E> futures_core::Stream for ProgressStream<S>
where
    S: futures_core::Stream<Item = Result<Bytes, E>> + Unpin,
{
    type Item = Result<Bytes, E>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                self.progress.add_bytes(bytes.len() as u64);
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(error))) => {
                self.progress.finish(false);
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                self.progress.finish(true);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S> Drop for ProgressStream<S> {
    fn drop(&mut self) {
        let completed = self.progress.transferred.load(Ordering::Relaxed) >= self.expected_bytes;
        self.progress.finish(completed);
    }
}

/// A simple file server CLI tool
#[derive(Parser, Debug)]
#[command(
    name = "file-server",
    version,
    about = "A simple file server that serves files from a directory"
)]
pub struct Args {
    /// Directory to serve files from
    #[arg(short, long, default_value = ".")]
    pub dir: PathBuf,

    /// Port to listen on
    #[arg(short, long, default_value_t = 8080)]
    pub port: u16,

    /// Host address to bind to
    #[arg(long, default_value = "0.0.0.0")]
    pub host: String,
}

#[derive(Clone)]
struct AppState {
    root_dir: PathBuf,
    transfers: Arc<TransferDisplay>,
}

#[derive(Serialize)]
struct FileEntry {
    name: String,
    is_dir: bool,
    size: u64,
    modified: String,
}

fn format_size(size: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if size >= GB {
        format!("{:.1} GB", size as f64 / GB as f64)
    } else if size >= MB {
        format!("{:.1} MB", size as f64 / MB as f64)
    } else if size >= KB {
        format!("{:.1} KB", size as f64 / KB as f64)
    } else {
        format!("{} B", size)
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

async fn handle_root(
    state: State<Arc<AppState>>,
    ConnectInfo(client_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    handle_request_inner(&state, PathBuf::new(), headers, client_addr.ip()).await
}

async fn handle_request(
    State(state): State<Arc<AppState>>,
    Path(path): Path<String>,
    ConnectInfo(client_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let decoded = match percent_encoding::percent_decode_str(&path).decode_utf8() {
        Ok(d) => d.into_owned(),
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    handle_request_inner(
        &State(state),
        PathBuf::from(decoded),
        headers,
        client_addr.ip(),
    )
    .await
}

async fn handle_request_inner(
    state: &Arc<AppState>,
    relative: PathBuf,
    headers: HeaderMap,
    client_ip: IpAddr,
) -> Response {
    // Prevent path traversal
    let canonical_root = match std::fs::canonicalize(&state.root_dir) {
        Ok(p) => p,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    let full_path = canonical_root.join(&relative);

    // Verify the path is still under root
    let canonical_path = match std::fs::canonicalize(&full_path) {
        Ok(p) => p,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    if !canonical_path.starts_with(&canonical_root) {
        return StatusCode::FORBIDDEN.into_response();
    }

    if canonical_path.is_dir() {
        serve_directory(&state, &canonical_root, &canonical_path, &relative).await
    } else {
        serve_file(canonical_path, headers, client_ip, &state.transfers).await
    }
}

async fn serve_directory(
    _state: &Arc<AppState>,
    _canonical_root: &PathBuf,
    dir_path: &PathBuf,
    relative: &PathBuf,
) -> Response {
    let mut entries: Vec<FileEntry> = Vec::new();

    let read_dir = match std::fs::read_dir(dir_path) {
        Ok(rd) => rd,
        Err(e) => {
            tracing::error!("Failed to read directory: {}", e);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    for entry in read_dir.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue; // Skip hidden files
        }

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        let modified = metadata
            .modified()
            .ok()
            .and_then(|t| {
                let datetime: chrono::DateTime<chrono::Local> = t.into();
                Some(datetime.format("%Y-%m-%d %H:%M:%S").to_string())
            })
            .unwrap_or_else(|| "-".to_string());

        entries.push(FileEntry {
            name,
            is_dir: metadata.is_dir(),
            size: metadata.len(),
            modified,
        });
    }

    // Sort: directories first, then by name
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    let relative_str = relative.to_string_lossy();
    let display_path = if relative_str.is_empty() {
        "/"
    } else {
        &*format!("/{}", relative_str)
    };

    // Build parent link
    let parent_link = relative
        .parent()
        .map(|p| {
            let s = p.to_string_lossy().to_string();
            if s.is_empty() {
                "/".to_string()
            } else {
                format!("/{}", s)
            }
        })
        .filter(|_| !relative.as_os_str().is_empty());

    let mut html = String::new();
    html.push_str(&format!(
        "<!DOCTYPE html>\
        <html><head>\
        <meta charset=\"utf-8\">\
        <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
        <title>Index of {}</title>\
        <style>\
          body {{ font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; \
                 margin: 0; padding: 20px; background: #f5f5f5; color: #333; }}\
          h1 {{ font-size: 1.4em; margin-bottom: 16px; color: #1a1a1a; }}\
          table {{ width: 100%; border-collapse: collapse; background: white; border-radius: 8px; \
                   box-shadow: 0 1px 3px rgba(0,0,0,0.1); overflow: hidden; }}\
          th {{ background: #fafafa; text-align: left; padding: 12px 16px; \
                border-bottom: 2px solid #eee; font-weight: 600; font-size: 0.85em; \
                color: #666; text-transform: uppercase; }}\
          td {{ padding: 10px 16px; border-bottom: 1px solid #f0f0f0; }}\
          tr:hover {{ background: #f8f9ff; }}\
          a {{ color: #0066cc; text-decoration: none; }}\
          a:hover {{ text-decoration: underline; }}\
          .icon {{ margin-right: 8px; }}\
          .size {{ color: #888; font-size: 0.9em; }}\
          .modified {{ color: #888; font-size: 0.9em; }}\
          .footer {{ margin-top: 20px; font-size: 0.8em; color: #999; }}\
          .upload-area {{ margin-top: 16px; padding: 16px; background: white; border-radius: 8px; \
                          box-shadow: 0 1px 3px rgba(0,0,0,0.1); }}\
          .upload-area input[type=\"file\"] {{ font-size: 0.9em; }}\
          .upload-area button {{ padding: 8px 20px; background: #0066cc; color: white; border: none; \
                                 border-radius: 6px; font-size: 0.9em; cursor: pointer; }}\
          .upload-area button:hover {{ background: #0052a3; }}\
          .upload-area button:disabled {{ background: #999; cursor: not-allowed; }}\
          .upload-row {{ display: flex; align-items: center; gap: 12px; }}\
          .progress-wrap {{ display: none; margin-top: 12px; }}\
          .progress-bar {{ width: 100%; height: 8px; background: #e9ecef; border-radius: 4px; overflow: hidden; }}\
          .progress-fill {{ height: 100%; width: 0%; background: #0066cc; border-radius: 4px; transition: width 0.2s; }}\
          .progress-text {{ margin-top: 4px; font-size: 0.85em; color: #666; }}\
        </style>\
        </head><body>\
        <h1>Index of {}</h1>\
        <table>\
        <thead><tr><th>Name</th><th>Size</th><th>Modified</th></tr></thead>\
        <tbody>",
        html_escape(display_path),
        html_escape(display_path),
    ));

    if let Some(link) = parent_link {
        html.push_str(&format!(
            "<tr><td colspan=\"3\"><a href=\"{}\">📂 ..</a></td></tr>",
            link
        ));
    }

    for entry in &entries {
        let icon = if entry.is_dir { "📁" } else { "📄" };
        let link_path = if relative.as_os_str().is_empty() {
            format!(
                "/{}",
                utf8_percent_encode(&entry.name, percent_encoding::NON_ALPHANUMERIC)
            )
        } else {
            format!(
                "/{}/{}",
                utf8_percent_encode(
                    &relative.to_string_lossy(),
                    percent_encoding::NON_ALPHANUMERIC
                ),
                utf8_percent_encode(&entry.name, percent_encoding::NON_ALPHANUMERIC)
            )
        };

        let size_str = if entry.is_dir {
            "-".to_string()
        } else {
            format_size(entry.size)
        };

        html.push_str(&format!(
            "<tr><td><span class=\"icon\">{}</span><a href=\"{}\">{}</a></td>\
             <td class=\"size\">{}</td><td class=\"modified\">{}</td></tr>",
            icon,
            link_path,
            html_escape(&entry.name),
            size_str,
            html_escape(&entry.modified),
        ));
    }

    let upload_action = if relative.as_os_str().is_empty() {
        "/".to_string()
    } else {
        format!(
            "/{}",
            utf8_percent_encode(
                &relative.to_string_lossy(),
                percent_encoding::NON_ALPHANUMERIC
            )
        )
    };

    html.push_str(
        "</tbody></table>\
         <div class=\"upload-area\">\
         <form id=\"upload-form\" action=\"",
    );
    html.push_str(&html_escape(&upload_action));
    html.push_str(
         "\" method=\"post\" enctype=\"multipart/form-data\" class=\"upload-row\">\
          <input type=\"file\" name=\"files\" id=\"file-input\" multiple required>\
          <button type=\"submit\" id=\"upload-btn\">Upload</button>\
         </form>\
         <div class=\"progress-wrap\" id=\"progress-wrap\">\
          <div class=\"progress-bar\"><div class=\"progress-fill\" id=\"progress-fill\"></div></div>\
          <div class=\"progress-text\" id=\"progress-text\"></div>\
         </div>\
         </div>\
         <p class=\"footer\">Served by file-server</p>\
         <script>\
         (function() {\
           var form = document.getElementById('upload-form');\
           var btn = document.getElementById('upload-btn');\
           var wrap = document.getElementById('progress-wrap');\
           var fill = document.getElementById('progress-fill');\
           var text = document.getElementById('progress-text');\
           function fmtSize(b) {\
             if (b >= 1073741824) return (b/1073741824).toFixed(1) + ' GB';\
             if (b >= 1048576) return (b/1048576).toFixed(1) + ' MB';\
             if (b >= 1024) return (b/1024).toFixed(1) + ' KB';\
             return b + ' B';\
           }\
           form.addEventListener('submit', function(e) {\
             e.preventDefault();\
             var files = document.getElementById('file-input').files;\
             if (!files.length) return;\
             var fd = new FormData(form);\
             var xhr = new XMLHttpRequest();\
             var totalSize = 0;\
             for (var i = 0; i < files.length; i++) totalSize += files[i].size;\
             btn.disabled = true;\
             btn.textContent = 'Uploading...';\
             wrap.style.display = 'block';\
             fill.style.width = '0%';\
             text.textContent = '0%';\
             xhr.upload.addEventListener('progress', function(ev) {\
               if (!ev.lengthComputable) return;\
               var pct = Math.round(ev.loaded / ev.total * 100);\
               fill.style.width = pct + '%';\
               text.textContent = fmtSize(ev.loaded) + ' / ' + fmtSize(ev.total) + ' (' + pct + '%)';\
             });\
             xhr.addEventListener('load', function() {\
               if (xhr.status >= 200 && xhr.status < 400) {\
                 fill.style.width = '100%';\
                 fill.style.background = '#28a745';\
                 text.textContent = 'Upload complete, redirecting...';\
                 setTimeout(function() { window.location.reload(); }, 500);\
               } else {\
                 fill.style.background = '#dc3545';\
                 text.textContent = 'Upload failed (HTTP ' + xhr.status + ')';\
                 btn.disabled = false;\
                 btn.textContent = 'Upload';\
               }\
             });\
             xhr.addEventListener('error', function() {\
               fill.style.background = '#dc3545';\
               text.textContent = 'Upload failed (network error)';\
               btn.disabled = false;\
               btn.textContent = 'Upload';\
             });\
             xhr.open('POST', form.action);\
             xhr.send(fd);\
           });\
         })();\
         </script>\
         </body></html>",
    );

    Html(html).into_response()
}

async fn serve_file(
    file_path: PathBuf,
    headers: HeaderMap,
    client_ip: IpAddr,
    transfers: &Arc<TransferDisplay>,
) -> Response {
    let file = match tokio::fs::File::open(&file_path).await {
        Ok(f) => f,
        Err(e) => {
            tracing::error!("Failed to open file {}: {}", file_path.display(), e);
            return StatusCode::NOT_FOUND.into_response();
        }
    };

    let metadata = match file.metadata().await {
        Ok(m) => m,
        Err(e) => {
            tracing::error!("Failed to read metadata for {}: {}", file_path.display(), e);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let file_size = metadata.len();
    let mime_type = mime_guess::from_path(&file_path)
        .first_or_octet_stream()
        .to_string();
    let file_name = file_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    let content_disposition: header::HeaderValue =
        format!("attachment; filename=\"{}\"", file_name)
            .parse()
            .unwrap();

    // Parse Range header
    let range_header = headers.get(header::RANGE).and_then(|v| v.to_str().ok());

    if let Some(range) = range_header {
        if let Some((start, end)) = parse_range(range, file_size) {
            let content_length = end - start + 1;
            let content_range = format!("bytes {}-{}/{}", start, end, file_size);

            let mut file = file;
            if let Err(e) = file.seek(std::io::SeekFrom::Start(start)).await {
                tracing::error!("Failed to seek in {}: {}", file_path.display(), e);
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }

            let stream = tokio_util::io::ReaderStream::new(file.take(content_length));
            let body = Body::from_stream(ProgressStream {
                inner: stream,
                progress: transfers.start(
                    client_ip,
                    "download",
                    file_name.clone(),
                    Some(content_length),
                ),
                expected_bytes: content_length,
            });

            return (
                StatusCode::PARTIAL_CONTENT,
                [
                    (header::CONTENT_TYPE, mime_type.parse().unwrap()),
                    (header::CONTENT_DISPOSITION, content_disposition),
                    (header::ACCEPT_RANGES, "bytes".parse().unwrap()),
                    (
                        header::CONTENT_LENGTH,
                        content_length.to_string().parse().unwrap(),
                    ),
                    (
                        header::HeaderName::from_static("content-range"),
                        content_range.parse().unwrap(),
                    ),
                ],
                body,
            )
                .into_response();
        } else {
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(
                    header::HeaderName::from_static("content-range"),
                    header::HeaderValue::from_str(&format!("bytes */{}", file_size)).unwrap(),
                )],
            )
                .into_response();
        }
    }

    // Full file stream (no Range header)
    let stream = tokio_util::io::ReaderStream::new(file);
    let body = Body::from_stream(ProgressStream {
        inner: stream,
        progress: transfers.start(client_ip, "download", file_name.clone(), Some(file_size)),
        expected_bytes: file_size,
    });

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime_type.parse().unwrap()),
            (header::CONTENT_DISPOSITION, content_disposition),
            (header::ACCEPT_RANGES, "bytes".parse().unwrap()),
            (
                header::CONTENT_LENGTH,
                file_size.to_string().parse().unwrap(),
            ),
        ],
        body,
    )
        .into_response()
}

/// Parse a Range header value like "bytes=0-499" or "bytes=500-" or "bytes=-500".
/// Returns (start, end) inclusive range, or None if invalid.
fn parse_range(range: &str, file_size: u64) -> Option<(u64, u64)> {
    let range = range.strip_prefix("bytes=")?;
    let range = range.trim();

    if range.starts_with('-') {
        // Suffix range: "bytes=-500" means last 500 bytes
        let suffix_len: u64 = range[1..].parse().ok()?;
        if suffix_len == 0 || suffix_len > file_size {
            return None;
        }
        Some((file_size - suffix_len, file_size - 1))
    } else if let Some(dash_pos) = range.find('-') {
        let start: u64 = range[..dash_pos].parse().ok()?;
        let end_str = &range[dash_pos + 1..];
        if start >= file_size {
            return None;
        }
        let end = if end_str.is_empty() {
            file_size - 1
        } else {
            let end: u64 = end_str.parse().ok()?;
            end.min(file_size - 1)
        };
        if start > end {
            return None;
        }
        Some((start, end))
    } else {
        None
    }
}

async fn handle_upload_root(
    state: State<Arc<AppState>>,
    ConnectInfo(client_addr): ConnectInfo<SocketAddr>,
    multipart: Multipart,
) -> Response {
    handle_upload_inner(&state, PathBuf::new(), multipart, client_addr.ip()).await
}

async fn handle_upload(
    State(state): State<Arc<AppState>>,
    Path(path): Path<String>,
    ConnectInfo(client_addr): ConnectInfo<SocketAddr>,
    multipart: Multipart,
) -> Response {
    let decoded = match percent_encoding::percent_decode_str(&path).decode_utf8() {
        Ok(d) => d.into_owned(),
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    handle_upload_inner(
        &State(state),
        PathBuf::from(decoded),
        multipart,
        client_addr.ip(),
    )
    .await
}

async fn handle_upload_inner(
    state: &Arc<AppState>,
    relative: PathBuf,
    mut multipart: Multipart,
    client_ip: IpAddr,
) -> Response {
    let canonical_root = match std::fs::canonicalize(&state.root_dir) {
        Ok(p) => p,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    let target_dir = if relative.as_os_str().is_empty() {
        canonical_root.clone()
    } else {
        let full_path = canonical_root.join(&relative);
        let canonical_path = match std::fs::canonicalize(&full_path) {
            Ok(p) => p,
            Err(_) => return StatusCode::NOT_FOUND.into_response(),
        };
        if !canonical_path.starts_with(&canonical_root) {
            return StatusCode::FORBIDDEN.into_response();
        }
        if !canonical_path.is_dir() {
            return StatusCode::BAD_REQUEST.into_response();
        }
        canonical_path
    };

    let progress = state.transfers.start(
        client_ip,
        "upload",
        relative.to_string_lossy().to_string(),
        None,
    );
    let mut succeeded = true;

    while let Some(mut field) = multipart.next_field().await.unwrap_or(None) {
        let file_name = match field.file_name() {
            Some(name) => name.to_string(),
            None => continue,
        };

        let safe_name = std::path::Path::new(&file_name)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        if safe_name.is_empty() || safe_name.starts_with('.') {
            continue;
        }

        let dest = target_dir.join(&safe_name);
        progress.set_file(safe_name);
        let mut output = match tokio::fs::File::create(&dest).await {
            Ok(file) => file,
            Err(e) => {
                tracing::error!("Failed to create upload {}: {}", file_name, e);
                succeeded = false;
                continue;
            }
        };

        loop {
            let chunk = match field.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(e) => {
                    tracing::error!("Failed to read upload {}: {}", file_name, e);
                    succeeded = false;
                    break;
                }
            };

            if let Err(e) = output.write_all(&chunk).await {
                tracing::error!("Failed to write {}: {}", dest.display(), e);
                succeeded = false;
                break;
            }
            progress.add_bytes(chunk.len() as u64);
        }
    }
    progress.finish(succeeded);

    let redirect_to = if relative.as_os_str().is_empty() {
        "/".to_string()
    } else {
        format!(
            "/{}",
            utf8_percent_encode(
                &relative.to_string_lossy(),
                percent_encoding::NON_ALPHANUMERIC
            )
        )
    };

    Redirect::to(&redirect_to).into_response()
}

fn get_local_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    Some(socket.local_addr().ok()?.ip())
}

pub async fn run(args: Args) {
    // Validate directory exists
    let dir = if args.dir.is_absolute() {
        args.dir
    } else {
        std::env::current_dir()
            .expect("Failed to get current directory")
            .join(&args.dir)
    };

    if !dir.exists() || !dir.is_dir() {
        eprintln!("Error: '{}' is not a valid directory", dir.display());
        std::process::exit(1);
    }

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let transfers = Arc::new(TransferDisplay::new());
    let refresh_transfers = Arc::clone(&transfers);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            interval.tick().await;
            refresh_transfers.refresh();
        }
    });

    let state = Arc::new(AppState {
        root_dir: dir.clone(),
        transfers,
    });

    let app = Router::new()
        .route("/", get(handle_root).post(handle_upload_root))
        .route("/{*path}", get(handle_request).post(handle_upload))
        .with_state(state)
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024 * 1024))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http());

    let addr = format!("{}:{}", args.host, args.port);
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Error: Failed to bind to {}: {}", addr, e);
            std::process::exit(1);
        }
    };

    let actual_addr = listener.local_addr().expect("Failed to get local address");
    println!("📁 File server started!");
    println!("   Serving : {}", dir.display());
    println!("   Local   : http://{}", actual_addr);
    if let Some(ip) = get_local_ip() {
        println!("   Network : http://{}:{}", ip, actual_addr.port());
    }
    println!("\n   Press Ctrl+C to stop\n");

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .expect("Server failed to start");
}
