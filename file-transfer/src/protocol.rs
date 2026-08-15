use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAGIC: &[u8; 8] = b"UTFT\x01\0\0\0";
pub const PROTOCOL_MAJOR: u16 = 1;
pub const PROTOCOL_MINOR: u16 = 0;
pub const DEFAULT_PORT: u16 = 9417;
pub const CHUNK_SIZE: usize = 4 * 1024 * 1024;
pub const SYNC_INTERVAL: u64 = 64 * 1024 * 1024;
pub const SPACE_RESERVE: u64 = 100 * 1024 * 1024;
const MAX_CONTROL_FRAME: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileInfo {
    pub path: String,
    pub size: u64,
    pub modified_ms: u64,
    pub executable: bool,
    pub file_id: String,
    pub blake3: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ListEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified_ms: u64,
    pub executable: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SortBy {
    #[default]
    Name,
    Size,
    Modified,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello {
        major: u16,
        minor: u16,
        token: Option<String>,
    },
    List {
        path: String,
        cursor: usize,
        limit: usize,
        sort: SortBy,
    },
    Stat {
        path: String,
    },
    Inspect {
        path: String,
    },
    Download {
        path: String,
        offset: u64,
        expected_file_id: Option<String>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Hello {
        major: u16,
        minor: u16,
        auth_required: bool,
    },
    List {
        entries: Vec<ListEntry>,
        next_cursor: Option<usize>,
    },
    Stat {
        file: FileInfo,
    },
    Inspect {
        entry: ListEntry,
    },
    Download {
        file: FileInfo,
        offset: u64,
        chunk_size: usize,
    },
    Complete {
        source_unchanged: bool,
    },
    Error {
        code: String,
        message: String,
    },
}

pub async fn write_magic<W: AsyncWrite + Unpin>(writer: &mut W) -> io::Result<()> {
    writer.write_all(MAGIC).await
}

pub async fn read_magic<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<()> {
    let mut magic = [0u8; 8];
    reader.read_exact(&mut magic).await?;
    if &magic != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid file-transfer protocol magic",
        ));
    }
    Ok(())
}

pub async fn write_message<W, T>(writer: &mut W, value: &T) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = serde_json::to_vec(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if bytes.len() > MAX_CONTROL_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control frame is too large",
        ));
    }
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await
}

pub async fn read_message<R, T>(reader: &mut R) -> io::Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let length = reader.read_u32().await? as usize;
    if length > MAX_CONTROL_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control frame exceeds the configured limit",
        ));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub fn error_message(code: impl Into<String>, message: impl Into<String>) -> ServerMessage {
    ServerMessage::Error {
        code: code.into(),
        message: message.into(),
    }
}

pub fn server_error(message: ServerMessage) -> io::Error {
    match message {
        ServerMessage::Error { code, message } => io::Error::other(format!("{code}: {message}")),
        _ => io::Error::new(io::ErrorKind::InvalidData, "unexpected server response"),
    }
}
