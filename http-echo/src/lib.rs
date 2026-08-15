use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::{StatusCode, Version},
    response::{IntoResponse, Response},
    routing::any,
};
use clap::Parser;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

/// Echo received HTTP requests as raw request text
#[derive(Parser, Debug)]
#[command(name = "http-echo", version, about = "Echo received HTTP requests as raw request text")]
pub struct Args {
    /// Port to listen on
    #[arg(short, long, default_value_t = 8081)]
    pub port: u16,

    /// Host address to bind to
    #[arg(long, default_value = "0.0.0.0")]
    pub host: String,
}

const MAX_BODY: usize = 100 * 1024 * 1024; // 100MB

fn version_str(version: Version) -> &'static str {
    match version {
        Version::HTTP_10 => "HTTP/1.0",
        Version::HTTP_11 => "HTTP/1.1",
        Version::HTTP_2 => "HTTP/2",
        Version::HTTP_3 => "HTTP/3",
        _ => "HTTP/?",
    }
}

async fn echo(request: axum::extract::Request) -> Response {
    let method = request.method().clone();
    let uri = request.uri().clone();
    tracing::info!("{} {}", method, uri);
    let version = request.version();
    let headers = request.headers().clone();

    let bytes = match axum::body::to_bytes(request.into_body(), MAX_BODY).await {
        Ok(b) => b,
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };

    let mut out = String::new();
    out.push_str(&format!("{} {} {}\r\n", method, uri, version_str(version)));

    for (name, value) in headers.iter() {
        let value_str = match value.to_str() {
            Ok(s) => s.to_string(),
            Err(_) => "<binary>".to_string(),
        };
        out.push_str(&format!("{}: {}\r\n", name, value_str));
    }

    out.push_str("\r\n");

    match std::str::from_utf8(&bytes) {
        Ok(body_text) => out.push_str(body_text),
        Err(_) => {
            let hex_preview: Vec<String> = bytes
                .iter()
                .take(32)
                .map(|b| format!("{:02x}", b))
                .collect();
            out.push_str(&format!(
                "[body: {} bytes, non-UTF-8 binary; hex preview: {}]",
                bytes.len(),
                hex_preview.join(" ")
            ));
        }
    }

    out.into_response()
}

fn get_local_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    Some(socket.local_addr().ok()?.ip())
}

pub async fn run(args: Args) {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let app = Router::new()
        .route("/", any(echo))
        .route("/{*path}", any(echo))
        .layer(DefaultBodyLimit::max(MAX_BODY))
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
    println!("🔍 HTTP echo server started!");
    println!("   Serving : {} (max body {})", actual_addr, MAX_BODY);
    println!("   Local   : http://{}", actual_addr);
    if let Some(ip) = get_local_ip() {
        println!("   Network : http://{}:{}", ip, actual_addr.port());
    }
    println!("\n   Press Ctrl+C to stop\n");

    axum::serve(listener, app)
        .await
        .expect("Server failed to start");
}
