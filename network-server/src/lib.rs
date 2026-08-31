use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, State,
    },
    http::{HeaderMap, Request, StatusCode, Version},
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse, Json, Response,
    },
    routing::{any, get},
    Router,
};
use clap::Parser;
use futures_util::stream::{self, Stream};
use serde::Serialize;
use std::{
    convert::Infallible,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tower_http::{cors::CorsLayer, trace::TraceLayer};

const DEFAULT_PORT: u16 = 8090;
const MAX_BODY: usize = 10 * 1024 * 1024;

/// Start a local HTTP server with SSE and WebSocket test endpoints.
#[derive(Parser, Debug, Clone)]
#[command(
    name = "network-server",
    version,
    about = "Start an HTTP, SSE, and WebSocket test server"
)]
pub struct Args {
    /// Port to listen on
    #[arg(short, long, default_value_t = DEFAULT_PORT)]
    pub port: u16,

    /// Host address to bind to
    #[arg(long, default_value = "0.0.0.0")]
    pub host: String,

    /// Delay between SSE events in milliseconds
    #[arg(
        long,
        default_value_t = 1000,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    pub sse_interval: u64,

    /// Number of SSE events to send; 0 keeps the stream open indefinitely
    #[arg(long, default_value_t = 0)]
    pub sse_count: u64,

    /// Message prefix used by the SSE endpoint
    #[arg(long, default_value = "network-server event")]
    pub sse_message: String,
}

#[derive(Clone)]
struct AppState {
    sse_interval: Duration,
    sse_count: Option<u64>,
    sse_message: Arc<str>,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    service: &'static str,
}

#[derive(Serialize)]
struct RequestInfo {
    method: String,
    uri: String,
    version: &'static str,
    headers: Vec<(String, String)>,
    body: Option<String>,
    body_bytes: usize,
}

fn version_str(version: Version) -> &'static str {
    match version {
        Version::HTTP_09 => "HTTP/0.9",
        Version::HTTP_10 => "HTTP/1.0",
        Version::HTTP_11 => "HTTP/1.1",
        Version::HTTP_2 => "HTTP/2",
        Version::HTTP_3 => "HTTP/3",
        _ => "HTTP/?",
    }
}

/// Build the application router. Kept public so endpoint behavior can be tested without
/// starting a real listener.
pub fn app(args: &Args) -> Router {
    let state = AppState {
        sse_interval: Duration::from_millis(args.sse_interval),
        sse_count: (args.sse_count > 0).then_some(args.sse_count),
        sse_message: Arc::from(args.sse_message.as_str()),
    };

    Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/sse", get(sse))
        .route("/ws", get(websocket_upgrade))
        .fallback(any(request_info))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(Arc::new(state))
}

async fn index() -> Html<&'static str> {
    Html(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>network-server</title>
  <style>
    :root { color-scheme: light dark; font-family: system-ui, sans-serif; }
    body { max-width: 760px; margin: 40px auto; padding: 0 20px; line-height: 1.5; }
    code { padding: 2px 5px; border-radius: 4px; background: #8883; }
    pre { padding: 14px; overflow-x: auto; border-radius: 8px; background: #8882; }
    button { padding: 7px 12px; cursor: pointer; }
    #events { min-height: 4em; white-space: pre-wrap; }
  </style>
</head>
<body>
  <h1>network-server</h1>
  <p>A small server for testing HTTP, SSE, and WebSocket clients.</p>
  <h2>Endpoints</h2>
  <ul>
    <li><code>GET /healthz</code> — health check</li>
    <li><code>GET /sse</code> — server-sent events</li>
    <li><code>GET /ws</code> — WebSocket echo</li>
    <li><code>ANY /anything</code> — request inspection JSON</li>
  </ul>
  <h2>Browser smoke test</h2>
  <button id="connect-sse">Connect SSE</button>
  <button id="connect-ws">Connect WebSocket</button>
  <pre id="events">No events yet.</pre>
  <script>
    const output = document.querySelector('#events');
    const log = message => { output.textContent += '\n' + message; };
    document.querySelector('#connect-sse').onclick = () => {
      const source = new EventSource('/sse');
      source.onmessage = event => log('SSE: ' + event.data);
      source.onerror = () => { log('SSE: connection closed'); source.close(); };
      log('SSE: connecting');
    };
    document.querySelector('#connect-ws').onclick = () => {
      const protocol = location.protocol === 'https:' ? 'wss' : 'ws';
      const socket = new WebSocket(protocol + '://' + location.host + '/ws');
      socket.onopen = () => { log('WebSocket: connected'); socket.send('hello'); };
      socket.onmessage = event => log('WebSocket: ' + event.data);
      socket.onclose = () => log('WebSocket: closed');
      socket.onerror = () => log('WebSocket: error');
    };
  </script>
</body>
</html>"#,
    )
}

async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        service: "network-server",
    })
}

fn sse_stream(
    interval: Duration,
    count: Option<u64>,
    message: Arc<str>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    let interval = tokio::time::interval(interval);
    stream::unfold((interval, 0_u64), move |(mut interval, index)| {
        let message = Arc::clone(&message);
        async move {
            if count.is_some_and(|limit| index >= limit) {
                return None;
            }

            interval.tick().await;
            let number = index + 1;
            let event = Event::default()
                .event("message")
                .id(number.to_string())
                .data(format!("{message} #{number}"));
            Some((Ok(event), (interval, number)))
        }
    })
}

async fn sse(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    Sse::new(sse_stream(
        state.sse_interval,
        state.sse_count,
        Arc::clone(&state.sse_message),
    ))
    .keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

async fn websocket_upgrade(
    ws: WebSocketUpgrade,
    ConnectInfo(client): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| websocket(socket, client))
}

async fn websocket(mut socket: WebSocket, client: SocketAddr) {
    tracing::info!(%client, "WebSocket connected");

    if socket
        .send(Message::Text(format!("connected from {client}").into()))
        .await
        .is_err()
    {
        return;
    }

    while let Some(result) = socket.recv().await {
        let Ok(message) = result else {
            tracing::debug!(%client, "WebSocket receive error");
            break;
        };

        let response = match message {
            Message::Text(text) => Message::Text(format!("echo: {text}").into()),
            Message::Binary(bytes) => Message::Binary(bytes),
            Message::Ping(bytes) => Message::Pong(bytes),
            Message::Pong(_) => continue,
            Message::Close(frame) => {
                let _ = socket.send(Message::Close(frame)).await;
                break;
            }
        };

        if socket.send(response).await.is_err() {
            break;
        }
    }

    tracing::info!(%client, "WebSocket disconnected");
}

async fn request_info(request: Request<axum::body::Body>) -> Response {
    let method = request.method().to_string();
    let uri = request.uri().to_string();
    let version = version_str(request.version());
    let headers = headers_to_vec(request.headers());
    let bytes = match axum::body::to_bytes(request.into_body(), MAX_BODY).await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };
    let body_bytes = bytes.len();
    let body = if body_bytes == 0 {
        None
    } else {
        Some(String::from_utf8_lossy(&bytes).into_owned())
    };

    Json(RequestInfo {
        method,
        uri,
        version,
        headers,
        body,
        body_bytes,
    })
    .into_response()
}

fn headers_to_vec(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            (
                name.to_string(),
                value.to_str().unwrap_or("<binary>").to_string(),
            )
        })
        .collect()
}

fn get_local_ip() -> Option<IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    Some(socket.local_addr().ok()?.ip())
}

pub async fn run(args: Args) {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let app = app(&args);
    let addr = format!("{}:{}", args.host, args.port);
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("Error: failed to bind to {addr}: {error}");
            std::process::exit(1);
        }
    };

    let actual_addr = listener
        .local_addr()
        .expect("failed to get local listener address");
    println!("🌐 Network test server started!");
    println!("   Listen    : {actual_addr}");
    println!("   Browser   : http://{actual_addr}");
    println!("   SSE       : http://{actual_addr}/sse");
    println!("   WebSocket : ws://{actual_addr}/ws");
    if let Some(ip) = get_local_ip() {
        println!("   Network   : http://{ip}:{}", actual_addr.port());
    }
    println!("\n   Press Ctrl+C to stop\n");

    if let Err(error) = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    {
        eprintln!("Error: network server stopped: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_defaults() {
        let args = Args::try_parse_from(["network-server"]).unwrap();
        assert_eq!(args.port, DEFAULT_PORT);
        assert_eq!(args.sse_interval, 1000);
        assert_eq!(args.sse_count, 0);
    }

    #[test]
    fn rejects_zero_sse_interval() {
        assert!(Args::try_parse_from(["network-server", "--sse-interval", "0"]).is_err());
    }

    #[test]
    fn maps_zero_count_to_an_unlimited_stream() {
        let args = Args::try_parse_from(["network-server", "--sse-count", "0"]).unwrap();
        let state = AppState {
            sse_interval: Duration::from_millis(args.sse_interval),
            sse_count: (args.sse_count > 0).then_some(args.sse_count),
            sse_message: Arc::from(args.sse_message.as_str()),
        };
        assert_eq!(state.sse_count, None);
    }
}
