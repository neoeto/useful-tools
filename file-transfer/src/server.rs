use crate::{
    pathing::resolve_server_path,
    protocol::{
        self, error_message, ClientMessage, FileInfo, ListEntry, ServerMessage, SortBy, CHUNK_SIZE,
        PROTOCOL_MAJOR, PROTOCOL_MINOR,
    },
};
use chrono::{DateTime, Local};
use clap::Args as ClapArgs;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::File,
    future::Future,
    io::{self, IsTerminal, Read, Write},
    net::{IpAddr, SocketAddr, UdpSocket},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::{watch, Semaphore},
    task::JoinSet,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use uuid::Uuid;

const SERVER_FILENAME_WIDTH: usize = 32;
const MARQUEE_PAUSE: Duration = Duration::from_millis(700);
const MARQUEE_STEP: Duration = Duration::from_millis(160);

#[derive(ClapArgs, Debug, Clone)]
pub struct ServerArgs {
    /// Directory containing files clients may download
    #[arg(short, long, default_value = ".")]
    pub dir: PathBuf,

    /// Address and port to listen on
    #[arg(long, default_value = "0.0.0.0:9417")]
    pub bind: String,

    /// Require clients to provide a shared token
    #[arg(long)]
    pub auth: bool,

    /// Read the shared token from this file
    #[arg(long)]
    pub token_file: Option<PathBuf>,

    /// Maximum number of simultaneously connected clients
    #[arg(long, default_value_t = 8)]
    pub max_connections: usize,

    /// Disconnect clients that send no protocol data for this many seconds
    #[arg(long, default_value_t = 60)]
    pub idle_timeout: u64,
}

#[derive(Clone)]
struct ServerState {
    root: Arc<PathBuf>,
    token: Option<Arc<String>>,
    hashes: HashCache,
    transfers: TransferHub,
    idle_timeout: Duration,
}

#[derive(Clone)]
struct TransferHub {
    inner: Arc<Mutex<HashMap<Uuid, TransferStatus>>>,
    interactive: bool,
}

#[derive(Clone)]
struct TransferStatus {
    peer: SocketAddr,
    path: String,
    offset: u64,
    sent: u64,
    total: u64,
    started: Instant,
    result: Option<String>,
    finished_at: Option<Instant>,
}

struct ActiveTransfer {
    hub: TransferHub,
    id: Uuid,
    finished: bool,
}

impl ActiveTransfer {
    fn update(&self, sent: u64) {
        self.hub.update(self.id, sent);
    }

    fn finish(mut self, result: &str) {
        self.hub.finish(self.id, result);
        self.finished = true;
    }
}

impl Drop for ActiveTransfer {
    fn drop(&mut self) {
        if !self.finished {
            self.hub.finish(self.id, "failed");
        }
    }
}

impl TransferHub {
    fn new(interactive: bool) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            interactive,
        }
    }

    fn start(&self, peer: SocketAddr, path: String, offset: u64, total: u64) -> Uuid {
        let id = Uuid::new_v4();
        let state = TransferStatus {
            peer,
            path,
            offset,
            sent: offset,
            total,
            started: Instant::now(),
            result: None,
            finished_at: None,
        };
        self.inner
            .lock()
            .expect("transfer status lock")
            .insert(id, state);
        id
    }

    fn update(&self, id: Uuid, sent: u64) {
        if let Some(state) = self
            .inner
            .lock()
            .expect("transfer status lock")
            .get_mut(&id)
        {
            state.sent = sent;
        }
    }

    fn finish(&self, id: Uuid, result: &str) {
        let completed = {
            let mut states = self.inner.lock().expect("transfer status lock");
            let Some(state) = states.get_mut(&id) else {
                return;
            };
            if state.result.is_some() {
                return;
            }
            state.result = Some(result.to_string());
            state.finished_at = Some(Instant::now());
            Some((
                state.peer,
                state.path.clone(),
                state.sent,
                state.total,
                result.to_string(),
            ))
        };
        if !self.interactive {
            if let Some((peer, path, sent, total, result)) = completed {
                eprintln!(
                    "[server] {peer} | {} | {} / {} | {result}",
                    sanitize_display_text(&path),
                    format_bytes(sent),
                    format_bytes(total),
                );
            }
        }
    }

    async fn display(self, mut shutdown: watch::Receiver<bool>) {
        const FINISHED_DISPLAY: Duration = Duration::from_secs(3);
        let interactive = self.interactive;
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        let mut rendered_lines = 0usize;
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let now = Instant::now();
                    let mut states = {
                        let mut states = self.inner.lock().expect("transfer status lock");
                        states.retain(|_, state| {
                            state.finished_at.is_none_or(|finished| {
                                now.duration_since(finished) < FINISHED_DISPLAY
                            })
                        });
                        states
                            .iter()
                            .map(|(id, state)| (*id, state.clone()))
                            .collect::<Vec<_>>()
                    };
                    states.sort_by_key(|(id, _)| *id.as_bytes());
                    if interactive {
                        rendered_lines = render_dashboard(&states, rendered_lines);
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        if interactive {
                            let _ = render_dashboard(&[], rendered_lines);
                        }
                        break;
                    }
                }
            }
        }
    }
}

fn render_dashboard(states: &[(Uuid, TransferStatus)], previous_lines: usize) -> usize {
    let slots = previous_lines.max(states.len());
    if slots == 0 {
        return 0;
    }

    let mut stderr = io::stderr();
    let now = Instant::now();
    if previous_lines > 0 {
        let _ = write!(stderr, "\x1b[{}A", previous_lines);
    }
    for index in 0..slots {
        let _ = write!(stderr, "\r\x1b[2K");
        if let Some((_, state)) = states.get(index) {
            let elapsed = now
                .checked_duration_since(state.started)
                .unwrap_or_default();
            if let Some(result) = state.result.as_deref() {
                let _ = write!(
                    stderr,
                    "[server] {} | {} | {} / {} | {}",
                    state.peer,
                    fit_server_filename(&state.path, None),
                    format_bytes(state.sent),
                    format_bytes(state.total),
                    result,
                );
            } else {
                let elapsed_seconds = elapsed.as_secs_f64().max(0.001);
                let rate = state.sent.saturating_sub(state.offset) as f64 / elapsed_seconds;
                let _ = write!(
                    stderr,
                    "[server] {} | {} | {} / {} | {}/s | {} | sending",
                    state.peer,
                    fit_server_filename(&state.path, Some(elapsed)),
                    format_bytes(state.sent),
                    format_bytes(state.total),
                    format_bytes(rate as u64),
                    format_completion_time(estimate_completion(state, elapsed)),
                );
            }
        }
        let _ = writeln!(stderr);
    }
    let _ = stderr.flush();
    if states.is_empty() {
        0
    } else {
        slots
    }
}

fn estimate_completion(state: &TransferStatus, elapsed: Duration) -> Option<SystemTime> {
    let transferred = state.sent.saturating_sub(state.offset);
    let remaining = state.total.saturating_sub(state.sent);
    if state.result.is_some() || transferred == 0 || remaining == 0 {
        return None;
    }
    let seconds = elapsed.as_secs_f64();
    if seconds <= 0.0 {
        return None;
    }
    let rate = (transferred as f64 / seconds).round() as u64;
    if rate == 0 {
        return None;
    }
    let remaining_seconds = remaining.saturating_add(rate.saturating_sub(1)) / rate;
    SystemTime::now().checked_add(Duration::from_secs(remaining_seconds))
}

fn format_completion_time(completion_at: Option<SystemTime>) -> String {
    completion_at
        .map(DateTime::<Local>::from)
        .map(|timestamp| format!("ETA {}", timestamp.format("%H:%M:%S")))
        .unwrap_or_else(|| "ETA --:--:--".to_string())
}

fn fit_server_filename(path: &str, elapsed: Option<Duration>) -> String {
    let value = sanitize_display_text(path);
    let name = match elapsed {
        Some(elapsed) => scrolling_name(&value, SERVER_FILENAME_WIDTH, elapsed),
        None => truncate_name(&value, SERVER_FILENAME_WIDTH),
    };
    let used = UnicodeWidthStr::width(name.as_str());
    if used >= SERVER_FILENAME_WIDTH {
        return name;
    }
    format!("{name}{}", " ".repeat(SERVER_FILENAME_WIDTH - used))
}

fn sanitize_display_text(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                '�'
            } else {
                character
            }
        })
        .collect()
}

fn truncate_name(name: &str, width: usize) -> String {
    if UnicodeWidthStr::width(name) <= width {
        return name.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let content_width = width.saturating_sub(1);
    let mut used = 0;
    let mut output = String::new();
    for character in name.chars() {
        let character_width = character.width().unwrap_or(0);
        if used + character_width > content_width {
            break;
        }
        output.push(character);
        used += character_width;
    }
    output.push('…');
    output
}

fn scrolling_name(name: &str, width: usize, elapsed: Duration) -> String {
    if UnicodeWidthStr::width(name) <= width || elapsed < MARQUEE_PAUSE {
        return truncate_name(name, width);
    }
    let characters = format!("{name}   ").chars().collect::<Vec<_>>();
    let offset = ((elapsed - MARQUEE_PAUSE).as_millis() / MARQUEE_STEP.as_millis()) as usize
        % characters.len();
    let mut output = String::new();
    let mut used = 0;
    for character in characters.iter().cycle().skip(offset) {
        let character_width = character.width().unwrap_or(0);
        if used + character_width > width {
            break;
        }
        output.push(*character);
        used += character_width;
        if used == width {
            break;
        }
    }
    output
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedHash {
    size: u64,
    modified_ms: u64,
    blake3: String,
}

#[derive(Clone)]
struct HashCache {
    path: Arc<PathBuf>,
    values: Arc<Mutex<HashMap<String, CachedHash>>>,
}

impl HashCache {
    fn load() -> Self {
        #[cfg(not(test))]
        let path = config_dir().join("hash-cache.json");
        #[cfg(test)]
        let path = std::env::temp_dir().join(format!("ut-hash-cache-{}.json", Uuid::new_v4()));
        let values = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            path: Arc::new(path),
            values: Arc::new(Mutex::new(values)),
        }
    }

    async fn file_info(&self, path: PathBuf, relative: String) -> io::Result<FileInfo> {
        let metadata = fs::metadata(&path).await?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        let size = metadata.len();
        let source_modified_ms = modified_ms(&metadata);
        let key = path.to_string_lossy().to_string();
        if let Some(cached) = self
            .values
            .lock()
            .expect("hash cache lock")
            .get(&key)
            .cloned()
        {
            if cached.size == size && cached.modified_ms == source_modified_ms {
                return Ok(make_file_info(relative, &metadata, cached.blake3));
            }
        }

        let hash_path = path.clone();
        let hash = tokio::task::spawn_blocking(move || hash_file(&hash_path))
            .await
            .map_err(io::Error::other)??;
        let after = fs::metadata(&path).await?;
        if after.len() != size || modified_ms(&after) != source_modified_ms {
            return Err(io::Error::other("source file changed while hashing"));
        }
        self.values.lock().expect("hash cache lock").insert(
            key,
            CachedHash {
                size,
                modified_ms: source_modified_ms,
                blake3: hash.clone(),
            },
        );
        self.save().await;
        Ok(make_file_info(relative, &metadata, hash))
    }

    async fn save(&self) {
        let Some(parent) = self.path.parent() else {
            return;
        };
        if fs::create_dir_all(parent).await.is_err() {
            return;
        }
        let bytes = {
            let values = self.values.lock().expect("hash cache lock");
            match serde_json::to_vec(&*values) {
                Ok(bytes) => bytes,
                Err(_) => return,
            }
        };
        let _ = fs::write(&*self.path, bytes).await;
    }
}

pub async fn run(args: ServerArgs) -> io::Result<()> {
    let control_stdin = std::env::var_os("UT_FILE_TRANSFER_CONTROL_STDIN").is_some();
    let root = args.dir.canonicalize()?;
    if !root.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a directory", root.display()),
        ));
    }
    let token = load_server_token(&args)?;
    if args.bind.starts_with("0.0.0.0:") || args.bind.starts_with("[::]:") {
        eprintln!("WARNING: file-transfer uses plaintext TCP; file contents and tokens can be observed on the network.");
    }
    let listener = TcpListener::bind(&args.bind).await?;
    let local = listener.local_addr()?;
    println!("File transfer server started");
    println!("  Shared : {}", root.display());
    println!("  Listen : {local}");
    match server_local_ip(local) {
        Some(ip) => {
            println!("  Local IP: {ip}");
            println!("  Connect : {}", SocketAddr::new(ip, local.port()));
        }
        None => println!("  Local IP: unavailable (check network configuration)"),
    }
    println!(
        "  Auth   : {}",
        if token.is_some() {
            "required"
        } else {
            "disabled"
        }
    );
    println!("  Clients: max {}", args.max_connections);
    println!("  Press Ctrl+C to stop");

    let state = ServerState {
        root: Arc::new(root),
        token: token.map(Arc::new),
        hashes: HashCache::load(),
        transfers: TransferHub::new(io::stderr().is_terminal()),
        idle_timeout: Duration::from_secs(args.idle_timeout),
    };
    let permits = Arc::new(Semaphore::new(args.max_connections.max(1)));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(state.transfers.clone().display(shutdown_rx.clone()));
    let mut connections = JoinSet::new();

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, peer) = result?;
                let permit = match Arc::clone(&permits).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        tokio::spawn(reject_busy(stream));
                        continue;
                    }
                };
                let state = state.clone();
                let shutdown = shutdown_rx.clone();
                connections.spawn(async move {
                    let _permit = permit;
                    if let Err(error) = handle_connection(stream, peer, state, shutdown).await {
                        eprintln!("[server] {peer} | error: {error}");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("[server] graceful shutdown requested");
                break;
            }
            _ = control_shutdown(control_stdin), if control_stdin => {
                eprintln!("[server] graceful shutdown requested by parent TUI");
                break;
            }
        }
    }

    let _ = shutdown_tx.send(true);
    let graceful = async { while connections.join_next().await.is_some() {} };
    if tokio::time::timeout(Duration::from_secs(10), graceful)
        .await
        .is_err()
    {
        connections.abort_all();
    }
    Ok(())
}

fn server_local_ip(listener: SocketAddr) -> Option<IpAddr> {
    if !listener.ip().is_unspecified() {
        return Some(listener.ip());
    }
    let (bind, probe) = if listener.is_ipv4() {
        ("0.0.0.0:0", "192.0.2.1:9")
    } else {
        ("[::]:0", "[2001:db8::1]:9")
    };
    let socket = UdpSocket::bind(bind).ok()?;
    socket.connect(probe).ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then_some(ip)
}

async fn control_shutdown(enabled: bool) {
    if !enabled {
        std::future::pending::<()>().await;
        return;
    }
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().eq_ignore_ascii_case("shutdown") {
            return;
        }
    }
}

async fn reject_busy(mut stream: TcpStream) {
    if protocol::read_magic(&mut stream).await.is_ok() {
        let _ = protocol::write_message(
            &mut stream,
            &error_message("server_busy", "too many connected clients"),
        )
        .await;
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    state: ServerState,
    mut shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    with_idle_timeout(state.idle_timeout, protocol::read_magic(&mut stream)).await?;
    let hello: ClientMessage =
        with_idle_timeout(state.idle_timeout, protocol::read_message(&mut stream)).await?;
    let ClientMessage::Hello { major, token, .. } = hello else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "hello required"));
    };
    if major != PROTOCOL_MAJOR {
        protocol::write_message(
            &mut stream,
            &error_message(
                "protocol_version",
                format!("server requires protocol {PROTOCOL_MAJOR}.x"),
            ),
        )
        .await?;
        return Ok(());
    }
    if state.token.as_deref().map(String::as_str) != token.as_deref() && state.token.is_some() {
        protocol::write_message(
            &mut stream,
            &error_message("authentication", "invalid or missing token"),
        )
        .await?;
        return Ok(());
    }
    protocol::write_message(
        &mut stream,
        &ServerMessage::Hello {
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MINOR,
            auth_required: state.token.is_some(),
        },
    )
    .await?;

    let request: ClientMessage =
        with_idle_timeout(state.idle_timeout, protocol::read_message(&mut stream)).await?;
    match request {
        ClientMessage::List {
            path,
            cursor,
            limit,
            sort,
        } => match list_directory(&state.root, &path, cursor, limit, sort).await {
            Ok((entries, next_cursor)) => {
                protocol::write_message(
                    &mut stream,
                    &ServerMessage::List {
                        entries,
                        next_cursor,
                    },
                )
                .await?
            }
            Err(error) => {
                protocol::write_message(&mut stream, &error_message("list", error.to_string()))
                    .await?
            }
        },
        ClientMessage::Stat { path } => match stat_file(&state, &path).await {
            Ok(file) => protocol::write_message(&mut stream, &ServerMessage::Stat { file }).await?,
            Err(error) => {
                protocol::write_message(&mut stream, &error_message("stat", error.to_string()))
                    .await?
            }
        },
        ClientMessage::Inspect { path } => match inspect_path(&state.root, &path).await {
            Ok(entry) => {
                protocol::write_message(&mut stream, &ServerMessage::Inspect { entry }).await?
            }
            Err(error) => {
                protocol::write_message(&mut stream, &error_message("inspect", error.to_string()))
                    .await?
            }
        },
        ClientMessage::Download {
            path,
            offset,
            expected_file_id,
        } => {
            send_file(
                &mut stream,
                peer,
                &state,
                &path,
                offset,
                expected_file_id,
                &mut shutdown,
            )
            .await?;
        }
        ClientMessage::Hello { .. } => {
            protocol::write_message(&mut stream, &error_message("protocol", "unexpected hello"))
                .await?;
        }
    }
    Ok(())
}

async fn with_idle_timeout<T>(
    timeout: Duration,
    future: impl Future<Output = io::Result<T>>,
) -> io::Result<T> {
    tokio::time::timeout(timeout, future)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "client was idle for too long"))?
}

async fn list_directory(
    root: &Path,
    relative: &str,
    cursor: usize,
    limit: usize,
    sort: SortBy,
) -> io::Result<(Vec<ListEntry>, Option<usize>)> {
    let directory = if relative.is_empty() {
        root.to_path_buf()
    } else {
        resolve_server_path(root, relative)?
    };
    if !directory.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a directory",
        ));
    }
    let mut reader = fs::read_dir(&directory).await?;
    let mut entries = Vec::new();
    while let Some(entry) = reader.next_entry().await? {
        let name = match entry.file_name().into_string() {
            Ok(name) => name,
            Err(_) => continue,
        };
        let file_type = entry.file_type().await?;
        if file_type.is_symlink() || !(file_type.is_file() || file_type.is_dir()) {
            continue;
        }
        let metadata = entry.metadata().await?;
        let path = if relative.is_empty() {
            name.clone()
        } else {
            format!("{relative}/{name}")
        };
        entries.push(ListEntry {
            name,
            path,
            is_dir: file_type.is_dir(),
            size: if file_type.is_file() {
                metadata.len()
            } else {
                0
            },
            modified_ms: modified_ms(&metadata),
            executable: executable(&metadata),
        });
    }
    entries.sort_by(|left, right| {
        right.is_dir.cmp(&left.is_dir).then_with(|| match sort {
            SortBy::Name => left.name.to_lowercase().cmp(&right.name.to_lowercase()),
            SortBy::Size => left.size.cmp(&right.size).then(left.name.cmp(&right.name)),
            SortBy::Modified => left
                .modified_ms
                .cmp(&right.modified_ms)
                .then(left.name.cmp(&right.name)),
        })
    });
    let limit = limit.clamp(1, 500);
    let end = (cursor + limit).min(entries.len());
    let page = if cursor < entries.len() {
        entries[cursor..end].to_vec()
    } else {
        Vec::new()
    };
    let next = (end < entries.len()).then_some(end);
    Ok((page, next))
}

async fn stat_file(state: &ServerState, relative: &str) -> io::Result<FileInfo> {
    let path = resolve_server_path(&state.root, relative)?;
    state.hashes.file_info(path, relative.to_string()).await
}

async fn inspect_path(root: &Path, relative: &str) -> io::Result<ListEntry> {
    let path = if relative.is_empty() {
        root.to_path_buf()
    } else {
        resolve_server_path(root, relative)?
    };
    let metadata = fs::metadata(&path).await?;
    if !(metadata.is_file() || metadata.is_dir()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file or directory",
        ));
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_string();
    Ok(ListEntry {
        name,
        path: relative.to_string(),
        is_dir: metadata.is_dir(),
        size: if metadata.is_file() {
            metadata.len()
        } else {
            0
        },
        modified_ms: modified_ms(&metadata),
        executable: executable(&metadata),
    })
}

async fn send_file(
    stream: &mut TcpStream,
    peer: SocketAddr,
    state: &ServerState,
    relative: &str,
    offset: u64,
    expected_file_id: Option<String>,
    shutdown: &mut watch::Receiver<bool>,
) -> io::Result<()> {
    let path = resolve_server_path(&state.root, relative)?;
    let info = match state
        .hashes
        .file_info(path.clone(), relative.to_string())
        .await
    {
        Ok(info) => info,
        Err(error) => {
            protocol::write_message(stream, &error_message("download", error.to_string())).await?;
            return Ok(());
        }
    };
    if expected_file_id
        .as_deref()
        .is_some_and(|id| id != info.file_id)
    {
        protocol::write_message(
            stream,
            &error_message("source_changed", "remote file changed; restart is required"),
        )
        .await?;
        return Ok(());
    }
    if offset > info.size || (offset != info.size && !offset.is_multiple_of(CHUNK_SIZE as u64)) {
        protocol::write_message(stream, &error_message("offset", "invalid resume offset")).await?;
        return Ok(());
    }
    protocol::write_message(
        stream,
        &ServerMessage::Download {
            file: info.clone(),
            offset,
            chunk_size: CHUNK_SIZE,
        },
    )
    .await?;

    let before = fs::metadata(&path).await?;
    let mut file = fs::File::open(&path).await?;
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    let transfer = ActiveTransfer {
        id: state
            .transfers
            .start(peer, relative.to_string(), offset, info.size),
        hub: state.transfers.clone(),
        finished: false,
    };
    let mut sent = offset;
    let mut buffer = vec![0u8; CHUNK_SIZE];
    let mut interrupted = false;
    while sent < info.size {
        if *shutdown.borrow() {
            interrupted = true;
            break;
        }
        let wanted = (info.size - sent).min(CHUNK_SIZE as u64) as usize;
        file.read_exact(&mut buffer[..wanted]).await?;
        let hash = blake3::hash(&buffer[..wanted]);
        stream.write_u32(wanted as u32).await?;
        stream.write_all(hash.as_bytes()).await?;
        stream.write_all(&buffer[..wanted]).await?;
        sent += wanted as u64;
        transfer.update(sent);
    }
    stream.write_u32(0).await?;
    let after = fs::metadata(&path).await?;
    let unchanged =
        !interrupted && before.len() == after.len() && modified_ms(&before) == modified_ms(&after);
    protocol::write_message(
        stream,
        &ServerMessage::Complete {
            source_unchanged: unchanged,
        },
    )
    .await?;
    transfer.finish(if unchanged { "complete" } else { "interrupted" });
    Ok(())
}

fn load_server_token(args: &ServerArgs) -> io::Result<Option<String>> {
    if !args.auth {
        return Ok(None);
    }
    if let Ok(token) = std::env::var("UT_FILE_TRANSFER_TOKEN") {
        if !token.trim().is_empty() {
            return Ok(Some(token.trim().to_string()));
        }
    }
    if let Some(path) = &args.token_file {
        return Ok(Some(std::fs::read_to_string(path)?.trim().to_string()));
    }
    let default_path = config_dir().join("server-token");
    if let Ok(token) = std::fs::read_to_string(&default_path) {
        if !token.trim().is_empty() {
            return Ok(Some(token.trim().to_string()));
        }
    }

    let token = Uuid::new_v4().simple().to_string();
    eprintln!("Generated access token: {token}");
    if io::stdin().is_terminal() {
        eprint!("Save token to {}? [y/N] ", default_path.display());
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if answer.trim().eq_ignore_ascii_case("y") {
            if let Some(parent) = default_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&default_path, format!("{token}\n"))?;
            eprintln!("Token saved to {}", default_path.display());
        }
    }
    Ok(Some(token))
}

fn make_file_info(relative: String, metadata: &std::fs::Metadata, hash: String) -> FileInfo {
    let size = metadata.len();
    let modified_ms = modified_ms(metadata);
    let file_id = format!("{size}:{modified_ms}:{hash}");
    FileInfo {
        path: relative,
        size,
        modified_ms,
        executable: executable(metadata),
        file_id,
        blake3: hash,
    }
}

fn hash_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn modified_ms(metadata: &std::fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(unix)]
fn executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_metadata: &std::fs::Metadata) -> bool {
    false
}

fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ut")
        .join("file-transfer")
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_filename_field_is_fixed_width_and_scrolls() {
        let path = "a-very-long-file-name-that-needs-scrolling.bin";
        let initial = fit_server_filename(path, Some(Duration::ZERO));
        let scrolled = fit_server_filename(path, Some(MARQUEE_PAUSE + MARQUEE_STEP));
        assert_eq!(
            UnicodeWidthStr::width(initial.as_str()),
            SERVER_FILENAME_WIDTH
        );
        assert_eq!(
            UnicodeWidthStr::width(scrolled.as_str()),
            SERVER_FILENAME_WIDTH
        );
        assert_ne!(initial, scrolled);
    }

    #[test]
    fn explicit_listener_address_is_reported() {
        let listener: SocketAddr = "127.0.0.1:9417".parse().unwrap();
        assert_eq!(server_local_ip(listener), Some(listener.ip()));
    }
}
