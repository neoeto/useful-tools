use crate::{
    client_tui,
    pathing::safe_local_path,
    protocol::{
        self, server_error, ClientMessage, FileInfo, ListEntry, ServerMessage, SortBy, CHUNK_SIZE,
        DEFAULT_PORT, PROTOCOL_MAJOR, PROTOCOL_MINOR, SPACE_RESERVE, SYNC_INTERVAL,
    },
};
use clap::{ArgAction, Args as ClapArgs};
use filetime::FileTime;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::OpenOptions,
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    net::TcpStream,
};

#[derive(ClapArgs, Debug, Clone)]
pub struct ClientArgs {
    /// Server hostname or IP address, optionally followed by a port
    pub server: Option<String>,

    /// Remote files or directories to download; opens the browser when omitted
    #[arg(short, long, value_name = "PATH")]
    pub remote: Vec<String>,

    /// Directory in which downloaded files are stored
    #[arg(short, long, default_value = ".")]
    pub output: PathBuf,

    /// Read the shared access token from this file
    #[arg(long)]
    pub token_file: Option<PathBuf>,

    /// Save an interactively entered token with this recent Server
    #[arg(long)]
    pub save_token: bool,

    /// Replace a different destination or reset an invalid partial file
    #[arg(long)]
    pub overwrite: bool,

    /// Disable use of existing partial downloads
    #[arg(long = "no-resume", action = ArgAction::SetFalse, default_value_t = true)]
    pub resume: bool,

    /// Number of automatic reconnection attempts
    #[arg(long, default_value_t = 3)]
    pub retries: u32,

    /// Connection timeout in seconds
    #[arg(long, default_value_t = 10)]
    pub connect_timeout: u64,

    /// Idle I/O timeout in seconds
    #[arg(long, default_value_t = 60)]
    pub idle_timeout: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressState {
    Preparing,
    Downloading,
    Verifying,
    Complete,
    Skipped,
    Failed,
}

#[derive(Debug, Clone)]
pub struct Progress {
    pub path: String,
    pub transferred: u64,
    pub total: u64,
    pub resumed_from: u64,
    pub bytes_per_second: u64,
    pub eta: Option<Duration>,
    pub state: ProgressState,
    pub file_index: usize,
    pub file_count: usize,
    pub batch_transferred: u64,
    pub batch_total: u64,
}

pub type ProgressCallback = Arc<dyn Fn(Progress) + Send + Sync>;

#[derive(Clone)]
pub(crate) struct Api {
    address: String,
    token: Option<String>,
    connect_timeout: Duration,
    idle_timeout: Duration,
}

pub(crate) struct ListPage {
    pub entries: Vec<ListEntry>,
    pub next_cursor: Option<usize>,
}

#[derive(Debug, Clone)]
pub(crate) enum PlanItem {
    Directory { path: String, modified_ms: u64 },
    File(String),
}

#[derive(Debug, Serialize, Deserialize)]
struct PartialMetadata {
    version: u8,
    server: String,
    remote_path: String,
    file_id: String,
    size: u64,
    verified_offset: u64,
    chunk_size: usize,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct RecentConfig {
    servers: Vec<RecentServer>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RecentServer {
    address: String,
    output: PathBuf,
    last_used_ms: u64,
    #[serde(default)]
    token: Option<String>,
}

pub async fn run(mut args: ClientArgs) -> io::Result<()> {
    if !args.output.exists() {
        fs::create_dir_all(&args.output).await?;
    }
    args.output = args.output.canonicalize()?;
    let server = match args.server.clone() {
        Some(server) => server,
        None if io::stdin().is_terminal() => choose_server(&args.output)?,
        None => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "SERVER is required in non-interactive mode",
            ));
        }
    };
    let address = normalize_address(&server);
    let mut token =
        load_client_token(args.token_file.as_deref())?.or_else(|| saved_token(&address));
    let mut api = Api::new(
        address.clone(),
        token.clone(),
        Duration::from_secs(args.connect_timeout),
        Duration::from_secs(args.idle_timeout),
    );

    if let Err(error) = api.list_page("", 0, SortBy::Name).await {
        if error.to_string().contains("authentication")
            && io::stdin().is_terminal()
            && token.is_none()
        {
            token = Some(prompt_token()?);
            if !args.save_token {
                args.save_token = prompt_yes_no("Save this token with the recent Server? [y/N] ")?;
            }
            api = Api::new(
                address.clone(),
                token.clone(),
                Duration::from_secs(args.connect_timeout),
                Duration::from_secs(args.idle_timeout),
            );
            api.list_page("", 0, SortBy::Name).await?;
        } else {
            return Err(error);
        }
    }
    if std::env::var_os("UT_FILE_TRANSFER_NO_HISTORY").is_none() {
        remember_server(
            &address,
            &args.output,
            args.save_token.then(|| token.clone()).flatten(),
        );
    }

    if args.remote.is_empty() {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "the file browser requires an interactive terminal; use --remote for scripted downloads",
            ));
        }
        return client_tui::run(api, args).await;
    }

    let plan = expand_paths(&api, &args.remote).await?;
    preflight_collisions(&args.output, &plan)?;
    let callback = console_progress();
    let summary = download_plan(&api, &args, plan, callback).await;
    if summary.failed > 0 {
        return Err(io::Error::other(format!(
            "{} file(s) failed; {} completed, {} skipped",
            summary.failed, summary.completed, summary.skipped
        )));
    }
    Ok(())
}

impl Api {
    pub(crate) fn new(
        address: String,
        token: Option<String>,
        connect_timeout: Duration,
        idle_timeout: Duration,
    ) -> Self {
        Self {
            address,
            token,
            connect_timeout,
            idle_timeout,
        }
    }

    async fn connect(&self) -> io::Result<TcpStream> {
        let mut stream =
            tokio::time::timeout(self.connect_timeout, TcpStream::connect(&self.address))
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connection timed out"))??;
        protocol::write_magic(&mut stream).await?;
        protocol::write_message(
            &mut stream,
            &ClientMessage::Hello {
                major: PROTOCOL_MAJOR,
                minor: PROTOCOL_MINOR,
                token: self.token.clone(),
            },
        )
        .await?;
        let response: ServerMessage = self.read_message(&mut stream).await?;
        match response {
            ServerMessage::Hello { major, .. } if major == PROTOCOL_MAJOR => Ok(stream),
            message => Err(server_error(message)),
        }
    }

    async fn read_message(&self, stream: &mut TcpStream) -> io::Result<ServerMessage> {
        tokio::time::timeout(self.idle_timeout, protocol::read_message(stream))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "server response timed out"))?
    }

    pub(crate) async fn list_page(
        &self,
        path: &str,
        cursor: usize,
        sort: SortBy,
    ) -> io::Result<ListPage> {
        let mut stream = self.connect().await?;
        protocol::write_message(
            &mut stream,
            &ClientMessage::List {
                path: path.to_string(),
                cursor,
                limit: 500,
                sort,
            },
        )
        .await?;
        match self.read_message(&mut stream).await? {
            ServerMessage::List {
                entries,
                next_cursor,
            } => Ok(ListPage {
                entries,
                next_cursor,
            }),
            message => Err(server_error(message)),
        }
    }

    async fn stat(&self, path: &str) -> io::Result<FileInfo> {
        let mut stream = self.connect().await?;
        protocol::write_message(
            &mut stream,
            &ClientMessage::Stat {
                path: path.to_string(),
            },
        )
        .await?;
        match self.read_message(&mut stream).await? {
            ServerMessage::Stat { file } => Ok(file),
            message => Err(server_error(message)),
        }
    }

    async fn inspect(&self, path: &str) -> io::Result<ListEntry> {
        let mut stream = self.connect().await?;
        protocol::write_message(
            &mut stream,
            &ClientMessage::Inspect {
                path: path.to_string(),
            },
        )
        .await?;
        match self.read_message(&mut stream).await? {
            ServerMessage::Inspect { entry } => Ok(entry),
            message => Err(server_error(message)),
        }
    }

    async fn open_download(
        &self,
        path: &str,
        offset: u64,
        file_id: &str,
    ) -> io::Result<(TcpStream, FileInfo)> {
        let mut stream = self.connect().await?;
        protocol::write_message(
            &mut stream,
            &ClientMessage::Download {
                path: path.to_string(),
                offset,
                expected_file_id: Some(file_id.to_string()),
            },
        )
        .await?;
        match self.read_message(&mut stream).await? {
            ServerMessage::Download {
                file,
                offset: accepted,
                chunk_size,
            } => {
                if accepted != offset || chunk_size != CHUNK_SIZE {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "server changed resume parameters",
                    ));
                }
                Ok((stream, file))
            }
            message => Err(server_error(message)),
        }
    }
}

#[derive(Default)]
pub(crate) struct DownloadSummary {
    pub completed: usize,
    pub skipped: usize,
    pub failed: usize,
}

pub(crate) async fn download_plan(
    api: &Api,
    args: &ClientArgs,
    plan: Vec<PlanItem>,
    callback: ProgressCallback,
) -> DownloadSummary {
    let mut summary = DownloadSummary::default();
    let file_paths = plan
        .iter()
        .filter_map(|item| match item {
            PlanItem::File(path) => Some(path.clone()),
            PlanItem::Directory { .. } => None,
        })
        .collect::<Vec<_>>();
    let mut sizes = HashMap::new();
    for path in &file_paths {
        if let Ok(info) = api.stat(path).await {
            sizes.insert(path.clone(), info.size);
        }
    }
    let batch_total = sizes.values().copied().sum::<u64>();
    let file_count = file_paths.len();
    let mut file_index = 0usize;
    let mut batch_completed = 0u64;
    let mut directories = Vec::new();
    for item in plan {
        match item {
            PlanItem::Directory { path, modified_ms } => match safe_local_path(&args.output, &path)
            {
                Ok(target) => {
                    if let Err(error) = fs::create_dir_all(&target).await {
                        eprintln!("file-transfer: {path}: {error}");
                        summary.failed += 1;
                    } else {
                        directories.push((target, modified_ms));
                    }
                }
                Err(error) => {
                    eprintln!("file-transfer: {path}: {error}");
                    summary.failed += 1;
                }
            },
            PlanItem::File(path) => {
                file_index += 1;
                let size = sizes.get(&path).copied().unwrap_or(0);
                let outer = Arc::clone(&callback);
                let progress_callback: ProgressCallback = Arc::new({
                    let base = batch_completed;
                    move |mut progress| {
                        progress.file_index = file_index;
                        progress.file_count = file_count;
                        progress.batch_transferred =
                            base.saturating_add(progress.transferred.min(size));
                        progress.batch_total = batch_total;
                        outer(progress);
                    }
                });
                match download_file(api, args, &path, progress_callback).await {
                    Ok(DownloadOutcome::Complete) => summary.completed += 1,
                    Ok(DownloadOutcome::Skipped) => summary.skipped += 1,
                    Err(error) => {
                        callback(Progress {
                            path: path.clone(),
                            transferred: 0,
                            total: 0,
                            resumed_from: 0,
                            bytes_per_second: 0,
                            eta: None,
                            state: ProgressState::Failed,
                            file_index,
                            file_count,
                            batch_transferred: batch_completed,
                            batch_total,
                        });
                        eprintln!("file-transfer: {path}: {error}");
                        summary.failed += 1;
                    }
                }
                batch_completed = batch_completed.saturating_add(size);
            }
        }
    }
    for (path, modified_ms) in directories.into_iter().rev() {
        let _ = filetime::set_file_mtime(
            path,
            FileTime::from_unix_time((modified_ms / 1000) as i64, 0),
        );
    }
    summary
}

enum DownloadOutcome {
    Complete,
    Skipped,
}

async fn download_file(
    api: &Api,
    args: &ClientArgs,
    remote: &str,
    callback: ProgressCallback,
) -> io::Result<DownloadOutcome> {
    callback(progress(remote, 0, 0, 0, 0, ProgressState::Preparing));
    let info = api.stat(remote).await?;
    let final_path = safe_local_path(&args.output, remote)?;
    let parent = final_path
        .parent()
        .ok_or_else(|| io::Error::other("invalid destination"))?;
    fs::create_dir_all(parent).await?;
    let part_path = append_suffix(&final_path, ".utpart");
    let meta_path = append_suffix(&part_path, ".meta.json");

    if final_path.exists() {
        callback(progress(
            remote,
            0,
            info.size,
            0,
            0,
            ProgressState::Verifying,
        ));
        if hash_path(final_path.clone()).await? == info.blake3 {
            callback(progress(
                remote,
                info.size,
                info.size,
                0,
                0,
                ProgressState::Skipped,
            ));
            return Ok(DownloadOutcome::Skipped);
        }
        if !args.overwrite {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "destination differs; rerun with --overwrite or enable overwrite in the browser",
            ));
        }
        if part_path.exists() {
            fs::remove_file(&part_path).await?;
        }
        if meta_path.exists() {
            fs::remove_file(&meta_path).await?;
        }
        fs::rename(&final_path, &part_path).await?;
    }

    let existing_meta = read_metadata(&meta_path).await.ok();
    let mut offset = 0u64;
    if args.resume && part_path.exists() {
        match existing_meta {
            Some(meta)
                if meta.version == 1
                    && meta.server == api.address
                    && meta.remote_path == remote
                    && meta.file_id == info.file_id
                    && meta.chunk_size == CHUNK_SIZE
                    && meta.verified_offset <= info.size =>
            {
                offset = meta.verified_offset;
            }
            _ if !args.overwrite => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "partial metadata is missing, damaged, or belongs to a changed source; explicit overwrite is required",
                ));
            }
            _ => {}
        }
    }

    let std_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&part_path)?;
    std_file.try_lock_exclusive().map_err(|_| {
        io::Error::new(
            io::ErrorKind::WouldBlock,
            "another client is downloading this file",
        )
    })?;
    std_file.set_len(offset)?;
    let mut file = fs::File::from_std(std_file);

    let remaining = info.size.saturating_sub(offset);
    let available = fs2::available_space(parent)?;
    if available < remaining.saturating_add(SPACE_RESERVE) {
        return Err(io::Error::other(format!(
            "insufficient disk space: need {} plus 100 MiB reserve, available {}",
            format_bytes(remaining),
            format_bytes(available)
        )));
    }
    let mut metadata = PartialMetadata {
        version: 1,
        server: api.address.clone(),
        remote_path: remote.to_string(),
        file_id: info.file_id.clone(),
        size: info.size,
        verified_offset: offset,
        chunk_size: CHUNK_SIZE,
    };
    write_metadata(&meta_path, &metadata).await?;

    let resumed_from = offset;
    let started = Instant::now();
    let mut retries_left = args.retries;
    loop {
        file.set_len(metadata.verified_offset).await?;
        let result = transfer_once(
            api,
            &info,
            &mut file,
            &mut metadata,
            &meta_path,
            resumed_from,
            started,
            Arc::clone(&callback),
        )
        .await;
        match result {
            Ok(()) => break,
            Err(error) if retries_left > 0 => {
                retries_left -= 1;
                eprintln!(
                    "file-transfer: {remote}: {error}; reconnecting ({} retries left)",
                    retries_left
                );
                tokio::time::sleep(Duration::from_secs(
                    (args.retries - retries_left).min(5) as u64
                ))
                .await;
            }
            Err(error) => return Err(error),
        }
    }

    file.sync_all().await?;
    drop(file);
    callback(progress(
        remote,
        info.size,
        info.size,
        resumed_from,
        0,
        ProgressState::Verifying,
    ));
    if hash_path(part_path.clone()).await? != info.blake3 {
        metadata.version = 0;
        let _ = write_metadata(&meta_path, &metadata).await;
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "final BLAKE3 verification failed",
        ));
    }
    set_attributes(&part_path, &info)?;
    fs::rename(&part_path, &final_path).await?;
    if meta_path.exists() {
        fs::remove_file(&meta_path).await?;
    }
    callback(progress(
        remote,
        info.size,
        info.size,
        resumed_from,
        0,
        ProgressState::Complete,
    ));
    Ok(DownloadOutcome::Complete)
}

#[allow(clippy::too_many_arguments)]
async fn transfer_once(
    api: &Api,
    info: &FileInfo,
    file: &mut fs::File,
    metadata: &mut PartialMetadata,
    meta_path: &Path,
    resumed_from: u64,
    started: Instant,
    callback: ProgressCallback,
) -> io::Result<()> {
    let (mut stream, current) = api
        .open_download(&info.path, metadata.verified_offset, &info.file_id)
        .await?;
    if current.file_id != info.file_id {
        return Err(io::Error::other("source file changed before transfer"));
    }
    file.seek(std::io::SeekFrom::Start(metadata.verified_offset))
        .await?;
    let mut written = metadata.verified_offset;
    let mut last_sync = written;
    loop {
        let length = tokio::time::timeout(api.idle_timeout, stream.read_u32())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "download timed out"))??
            as usize;
        if length == 0 {
            break;
        }
        if length > CHUNK_SIZE || written + length as u64 > info.size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid chunk length",
            ));
        }
        let mut expected = [0u8; 32];
        let mut bytes = vec![0u8; length];
        tokio::time::timeout(api.idle_timeout, async {
            stream.read_exact(&mut expected).await?;
            stream.read_exact(&mut bytes).await?;
            io::Result::Ok(())
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "download timed out"))??;
        if blake3::hash(&bytes).as_bytes() != &expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "chunk BLAKE3 verification failed",
            ));
        }
        file.write_all(&bytes).await?;
        written += length as u64;
        if written.saturating_sub(last_sync) >= SYNC_INTERVAL || written == info.size {
            file.sync_data().await?;
            metadata.verified_offset = written;
            write_metadata(meta_path, metadata).await?;
            last_sync = written;
        }
        let elapsed = started.elapsed().as_secs_f64().max(0.001);
        let rate = written.saturating_sub(resumed_from) as f64 / elapsed;
        callback(progress(
            &info.path,
            written,
            info.size,
            resumed_from,
            rate as u64,
            ProgressState::Downloading,
        ));
        tokio::task::yield_now().await;
    }
    match api.read_message(&mut stream).await? {
        ServerMessage::Complete {
            source_unchanged: true,
        } if written == info.size => Ok(()),
        ServerMessage::Complete { .. } => Err(io::Error::other(
            "source file changed or server stopped during transfer",
        )),
        message => Err(server_error(message)),
    }
}

pub(crate) async fn expand_paths(api: &Api, roots: &[String]) -> io::Result<Vec<PlanItem>> {
    let mut plan = Vec::new();
    let mut stack = roots.iter().rev().cloned().collect::<Vec<_>>();
    while let Some(path) = stack.pop() {
        let inspected = api.inspect(&path).await?;
        if !inspected.is_dir {
            plan.push(PlanItem::File(path));
            continue;
        }
        plan.push(PlanItem::Directory {
            path: path.clone(),
            modified_ms: inspected.modified_ms,
        });
        let mut cursor = 0;
        loop {
            let page = api.list_page(&path, cursor, SortBy::Name).await?;
            for entry in page.entries.into_iter().rev() {
                stack.push(entry.path);
            }
            match page.next_cursor {
                Some(next) => cursor = next,
                None => break,
            }
        }
    }
    Ok(plan)
}

pub(crate) fn preflight_collisions(root: &Path, plan: &[PlanItem]) -> io::Result<()> {
    let insensitive = is_case_insensitive(root);
    let mut targets = HashMap::<String, String>::new();
    let mut mappings = HashMap::<String, String>::new();
    for item in plan {
        let remote = match item {
            PlanItem::Directory { path, .. } | PlanItem::File(path) => path,
        };
        let target = safe_local_path(root, remote)?;
        let mapped = target
            .strip_prefix(root)
            .unwrap_or(&target)
            .components()
            .filter_map(|component| component.as_os_str().to_str())
            .collect::<Vec<_>>()
            .join("/");
        if mapped != *remote {
            mappings.insert(remote.clone(), mapped);
        }
        let key = if insensitive {
            target.to_string_lossy().to_lowercase()
        } else {
            target.to_string_lossy().to_string()
        };
        if let Some(previous) = targets.insert(key, remote.clone()) {
            if previous != *remote {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("target path collision between {previous:?} and {remote:?}"),
                ));
            }
        }
    }
    if !mappings.is_empty() {
        let path = root.join(".ut-file-transfer-name-map.json");
        let mut existing: HashMap<String, String> = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        existing.extend(mappings);
        let bytes = serde_json::to_vec_pretty(&existing)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        std::fs::write(path, bytes)?;
    }
    Ok(())
}

fn is_case_insensitive(root: &Path) -> bool {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let lower = root.join(format!(".ut-case-probe-{id}-a"));
    let upper = root.join(format!(".ut-case-probe-{id}-A"));
    match OpenOptions::new().write(true).create_new(true).open(&lower) {
        Ok(file) => {
            drop(file);
            let insensitive = upper.exists();
            let _ = std::fs::remove_file(lower);
            insensitive
        }
        Err(_) => cfg!(windows) || cfg!(target_os = "macos"),
    }
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

async fn read_metadata(path: &Path) -> io::Result<PartialMetadata> {
    let bytes = fs::read(path).await?;
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

async fn write_metadata(path: &Path, metadata: &PartialMetadata) -> io::Result<()> {
    let bytes = serde_json::to_vec(metadata)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    fs::write(path, bytes).await
}

async fn hash_path(path: PathBuf) -> io::Result<String> {
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(path)?;
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
    })
    .await
    .map_err(io::Error::other)?
}

fn set_attributes(path: &Path, info: &FileInfo) -> io::Result<()> {
    filetime::set_file_mtime(
        path,
        FileTime::from_unix_time((info.modified_ms / 1000) as i64, 0),
    )?;
    #[cfg(unix)]
    if info.executable {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_mode(permissions.mode() | 0o111);
        std::fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

fn progress(
    path: &str,
    transferred: u64,
    total: u64,
    resumed_from: u64,
    rate: u64,
    state: ProgressState,
) -> Progress {
    let eta = (rate > 0 && transferred < total)
        .then(|| Duration::from_secs(total.saturating_sub(transferred) / rate));
    Progress {
        path: path.to_string(),
        transferred,
        total,
        resumed_from,
        bytes_per_second: rate,
        eta,
        state,
        file_index: 0,
        file_count: 0,
        batch_transferred: 0,
        batch_total: 0,
    }
}

fn console_progress() -> ProgressCallback {
    let interactive = io::stderr().is_terminal();
    let last = Arc::new(Mutex::new(Instant::now() - Duration::from_secs(2)));
    Arc::new(move |progress| {
        let terminal_state = matches!(
            progress.state,
            ProgressState::Complete | ProgressState::Skipped | ProgressState::Failed
        );
        let mut last = last.lock().expect("progress lock");
        if !terminal_state && last.elapsed() < Duration::from_millis(500) {
            return;
        }
        *last = Instant::now();
        let percent = if progress.total > 0 {
            progress.transferred as f64 / progress.total as f64 * 100.0
        } else {
            0.0
        };
        let line = format!(
            "[{}/{}] {} | {} / {} | {:5.1}% | {}/s | overall {} / {} | {:?}",
            progress.file_index,
            progress.file_count,
            progress.path,
            format_bytes(progress.transferred),
            format_bytes(progress.total),
            percent,
            format_bytes(progress.bytes_per_second),
            format_bytes(progress.batch_transferred),
            format_bytes(progress.batch_total),
            progress.state
        );
        if interactive && !terminal_state {
            eprint!("\r\x1b[2K{line}");
            let _ = io::stderr().flush();
        } else {
            eprintln!("{line}");
        }
    })
}

fn load_client_token(token_file: Option<&Path>) -> io::Result<Option<String>> {
    if let Ok(token) = std::env::var("UT_FILE_TRANSFER_TOKEN") {
        if !token.trim().is_empty() {
            return Ok(Some(token.trim().to_string()));
        }
    }
    token_file
        .map(|path| std::fs::read_to_string(path).map(|token| token.trim().to_string()))
        .transpose()
}

fn prompt_token() -> io::Result<String> {
    eprint!("Access token: ");
    io::stderr().flush()?;
    let mut token = String::new();
    io::stdin().read_line(&mut token)?;
    Ok(token.trim().to_string())
}

fn prompt_yes_no(prompt: &str) -> io::Result<bool> {
    eprint!("{prompt}");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(answer.trim().eq_ignore_ascii_case("y"))
}

fn choose_server(output: &Path) -> io::Result<String> {
    let config = load_recent();
    if !config.servers.is_empty() {
        eprintln!("Recent servers:");
        for (index, server) in config.servers.iter().take(9).enumerate() {
            eprintln!("  {}. {}", index + 1, server.address);
        }
    }
    eprint!("Server address [127.0.0.1:{DEFAULT_PORT}]: ");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    let answer = answer.trim();
    if let Ok(index) = answer.parse::<usize>() {
        if let Some(server) = config.servers.get(index.saturating_sub(1)) {
            return Ok(server.address.clone());
        }
    }
    let _ = output;
    Ok(if answer.is_empty() {
        format!("127.0.0.1:{DEFAULT_PORT}")
    } else {
        answer.to_string()
    })
}

fn normalize_address(server: &str) -> String {
    if server.starts_with('[') {
        if server.contains("]:") {
            server.to_string()
        } else {
            format!("{server}:{DEFAULT_PORT}")
        }
    } else if server.matches(':').count() == 0 {
        format!("{server}:{DEFAULT_PORT}")
    } else {
        server.to_string()
    }
}

fn recent_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ut")
        .join("file-transfer")
        .join("recent.json")
}

fn load_recent() -> RecentConfig {
    std::fs::read(recent_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn saved_token(address: &str) -> Option<String> {
    load_recent()
        .servers
        .into_iter()
        .find(|server| server.address == address)
        .and_then(|server| server.token)
}

fn remember_server(address: &str, output: &Path, token: Option<String>) {
    let mut config = load_recent();
    let existing_token = config
        .servers
        .iter()
        .find(|server| server.address == address)
        .and_then(|server| server.token.clone());
    config.servers.retain(|server| server.address != address);
    config.servers.insert(
        0,
        RecentServer {
            address: address.to_string(),
            output: output.to_path_buf(),
            last_used_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or(0),
            token: token.or(existing_token),
        },
    );
    config.servers.truncate(20);
    let path = recent_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(&config) {
        let _ = std::fs::write(path, bytes);
    }
}

pub(crate) fn format_bytes(bytes: u64) -> String {
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
    use crate::server;

    #[test]
    fn appends_default_port() {
        assert_eq!(normalize_address("example.com"), "example.com:9417");
        assert_eq!(normalize_address("127.0.0.1:1000"), "127.0.0.1:1000");
        assert_eq!(normalize_address("[::1]"), "[::1]:9417");
    }

    #[test]
    fn partial_metadata_is_tiny_for_large_files() {
        let metadata = PartialMetadata {
            version: 1,
            server: "server:9417".into(),
            remote_path: "large.bin".into(),
            file_id: "id".into(),
            size: 10 * 1024 * 1024 * 1024,
            verified_offset: 0,
            chunk_size: CHUNK_SIZE,
        };
        assert!(serde_json::to_vec(&metadata).unwrap().len() < 1024 * 1024);
    }

    #[tokio::test]
    async fn authenticated_resume_downloads_only_the_suffix() {
        let root = std::env::temp_dir().join(format!("ut-ft-server-{}", uuid::Uuid::new_v4()));
        let output = std::env::temp_dir().join(format!("ut-ft-client-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&output).unwrap();
        let mut content = vec![0u8; CHUNK_SIZE * 17 + 137];
        for (index, byte) in content.iter_mut().enumerate() {
            *byte = (index % 251) as u8;
        }
        std::fs::write(root.join("large.bin"), &content).unwrap();
        let token_path = root.join("token.txt");
        std::fs::write(&token_path, "test-token\n").unwrap();

        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let address = format!("127.0.0.1:{port}");
        let server_task = tokio::spawn(server::run(server::ServerArgs {
            dir: root.clone(),
            bind: address.clone(),
            auth: true,
            token_file: Some(token_path),
            max_connections: 2,
            idle_timeout: 5,
        }));

        for _ in 0..100 {
            if TcpStream::connect(&address).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let wrong = Api::new(
            address.clone(),
            Some("wrong".to_string()),
            Duration::from_secs(2),
            Duration::from_secs(2),
        );
        assert!(wrong.list_page("", 0, SortBy::Name).await.is_err());

        let api = Api::new(
            address.clone(),
            Some("test-token".to_string()),
            Duration::from_secs(2),
            Duration::from_secs(5),
        );
        let args = ClientArgs {
            server: None,
            remote: Vec::new(),
            output: output.clone(),
            token_file: None,
            save_token: false,
            overwrite: false,
            resume: true,
            retries: 0,
            connect_timeout: 2,
            idle_timeout: 5,
        };
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        let callback: ProgressCallback = Arc::new(move |progress| {
            let _ = progress_tx.send(progress);
        });
        let first_api = api.clone();
        let first_args = args.clone();
        let first = tokio::spawn(async move {
            download_file(&first_api, &first_args, "large.bin", callback).await
        });
        while let Some(progress) = progress_rx.recv().await {
            if progress.transferred >= SYNC_INTERVAL && progress.transferred < progress.total {
                break;
            }
        }
        first.abort();
        let _ = first.await;
        let partial_meta = read_metadata(&output.join("large.bin.utpart.meta.json"))
            .await
            .unwrap();
        assert!(partial_meta.verified_offset >= SYNC_INTERVAL);

        let resumed_from = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let resumed_value = Arc::clone(&resumed_from);
        let callback: ProgressCallback = Arc::new(move |progress| {
            resumed_value.store(progress.resumed_from, std::sync::atomic::Ordering::Relaxed);
        });
        let outcome = download_file(&api, &args, "large.bin", callback)
            .await
            .unwrap();
        assert!(matches!(outcome, DownloadOutcome::Complete));
        assert!(resumed_from.load(std::sync::atomic::Ordering::Relaxed) >= SYNC_INTERVAL);
        assert_eq!(
            hash_path(output.join("large.bin")).await.unwrap(),
            blake3::hash(&content).to_hex().to_string()
        );
        assert!(!output.join("large.bin.utpart").exists());

        server_task.abort();
        let _ = server_task.await;
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(output).unwrap();
    }
}
