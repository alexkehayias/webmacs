use axum::{
    extract::{ws::WebSocket, ws::Message, Query, WebSocketUpgrade},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use portable_pty::{
    native_pty_system,
    CommandBuilder, PtySize,
};
use serde::Deserialize;
use std::{
    env,
    io::{self, Read, Write},
    net::SocketAddr,
    path::PathBuf,
};
use tower_http::services::ServeDir;
use tracing::{error, info, warn};

/// WebSocket connection query parameters
#[derive(Debug, Deserialize)]
struct WsParams {
    cols: Option<u16>,
    rows: Option<u16>,
}

/// Resize message from client
#[derive(Debug, Deserialize)]
struct ResizeMessage {
    #[serde(rename = "type")]
    msg_type: String,
    cols: u16,
    rows: u16,
}

/// Get the default shell for the current platform
fn get_shell_command() -> CommandBuilder {
    let (shell, args) = if cfg!(target_os = "windows") {
        (
            env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string()),
            Vec::<String>::new(),
        )
    } else {
        (
            env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string()),
            vec!["-l".to_string()], // Login shell
        )
    };

    let mut cmd = CommandBuilder::new(shell);
    for arg in args {
        cmd.arg(&arg);
    }
    cmd
}

/// Create a new PTY session
fn create_pty(cols: u16, rows: u16) -> io::Result<(Box<dyn Read + Send>, Box<dyn Write + Send>)> {
    let pty_system = native_pty_system();

    let pty_size = PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    };

    let shell_cmd = get_shell_command();

    let pair = pty_system.openpty(pty_size).map_err(|e| {
        io::Error::new(io::ErrorKind::Other, e)
    })?;

    // Spawn the command
    let _child = pair.slave.spawn_command(shell_cmd).map_err(|e| {
        io::Error::new(io::ErrorKind::Other, e)
    })?;

    let reader = pair.master.try_clone_reader().map_err(|e| {
        io::Error::new(io::ErrorKind::Other, e)
    })?;
    let writer = pair.master.take_writer().map_err(|e| {
        io::Error::new(io::ErrorKind::Other, e)
    })?;

    Ok((reader, writer))
}

/// WebSocket upgrade handler
async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(params): Query<WsParams>,
) -> Response {
    info!("WebSocket connection requested with cols={:?}, rows={:?}", params.cols, params.rows);

    ws.on_upgrade(|socket| handle_socket(socket, params))
}

/// Handle WebSocket connection
async fn handle_socket(mut socket: WebSocket, params: WsParams) {
    let cols = params.cols.unwrap_or(80);
    let rows = params.rows.unwrap_or(24);

    // Create PTY
    let (mut pty_reader, mut pty_writer) = match create_pty(cols, rows) {
        Ok((r, w)) => (r, w),
        Err(e) => {
            error!("Failed to create PTY: {}", e);
            let _ = socket.send(Message::Text(format!("\r\n\x1b[31mFailed to create PTY: {}\x1b[0m\r\n", e).into())).await;
            return;
        }
    };

    info!("PTY session created");

    // Send welcome message
    let welcome = format!(
        "{}\r\n{}  {}Welcome to webmacs!{}\r\n{}\r\n",
        "\x1b[1;36m╔══════════════════════════════════════════════════════════════╗\x1b[0m",
        "\x1b[1;36m║\x1b[0m",
        "\x1b[1;32m",
        "\x1b[0m",
        "\x1b[1;36m║\x1b[0m                                                              \x1b[1;36m║\x1b[0m",
    );
    let welcome2 = format!(
        "{}  {}You have a real shell session with full PTY support.{}\r\n",
        "\x1b[1;36m║\x1b[0m  ",
        "",
        "\x1b[0m",
    );
    let welcome3 = format!(
        "{}  Try: ls, cd, top, emacs, or any command!                   {}\r\n",
        "\x1b[1;36m║\x1b[0m  ",
        "\x1b[1;36m║\x1b[0m",
    );
    let welcome4 = format!(
        "{}\r\n\r\n",
        "\x1b[1;36m╚══════════════════════════════════════════════════════════════╝\x1b[0m",
    );

    if socket.send(Message::Text(welcome.into())).await.is_err() {
        return;
    }
    if socket.send(Message::Text(welcome2.into())).await.is_err() {
        return;
    }
    if socket.send(Message::Text(welcome3.into())).await.is_err() {
        return;
    }
    if socket.send(Message::Text(welcome4.into())).await.is_err() {
        return;
    }

    // Create channels for bidirectional communication
    let (mut ws_sender, mut ws_receiver) = socket.split();

    // PTY -> WebSocket task
    let pty_to_ws = tokio::spawn(async move {
        let mut buffer = [0u8; 8192];
        loop {
            match pty_reader.read(&mut buffer) {
                Ok(0) => {
                    info!("PTY reader EOF");
                    break;
                }
                Ok(n) => {
                    let data = String::from_utf8_lossy(&buffer[..n]).to_string();
                    if ws_sender.send(Message::Text(data.into())).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    error!("PTY read error: {}", e);
                    break;
                }
            }
        }
    });

    // WebSocket -> PTY task
    let ws_to_pty = tokio::spawn(async move {
        while let Some(result) = ws_receiver.next().await {
            match result {
                Ok(Message::Text(text)) => {
                    // Check for resize message
                    if text.starts_with('{') {
                        if let Ok(msg) = serde_json::from_str::<ResizeMessage>(&text) {
                            if msg.msg_type == "resize" {
                                info!("Received resize request: {}x{}", msg.cols, msg.rows);
                                // Note: portable_pty doesn't support resize after creation
                                continue;
                            }
                        }
                    }

                    // Send to PTY
                    if pty_writer.write_all(text.as_bytes()).is_err() {
                        break;
                    }
                    let _ = pty_writer.flush();
                }
                Ok(Message::Binary(data)) => {
                    if pty_writer.write_all(&data).is_err() {
                        break;
                    }
                    let _ = pty_writer.flush();
                }
                Ok(Message::Close(_)) => {
                    info!("WebSocket close received");
                    break;
                }
                Err(e) => {
                    error!("WebSocket error: {}", e);
                    break;
                }
                _ => {}
            }
        }
    });

    // Wait for both tasks to complete
    let _ = tokio::join!(pty_to_ws, ws_to_pty);

    info!("WebSocket connection closed");
}

/// Serve the main HTML page
async fn index_handler() -> impl IntoResponse {
    let html = include_str!("../static/index.html");
    Html(html)
}

/// HTML response wrapper
struct Html(&'static str);

impl IntoResponse for Html {
    fn into_response(self) -> Response {
        (StatusCode::OK, [("content-type", "text/html")], self.0).into_response()
    }
}

#[tokio::main]
async fn main() -> io::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let port = env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);

    // Ghostty-web dist directory path
    let ghostty_dist = env::var("GHOSTTY_DIST")
        .unwrap_or_else(|_| "../ghostty-web/dist".to_string());

    info!("Using ghostty-web dist from: {}", ghostty_dist);

    // Check if the directory exists
    let dist_path = PathBuf::from(&ghostty_dist);
    if !dist_path.exists() {
        warn!(
            "Ghostty-web dist directory not found at: {}. \
             Make sure ghostty-web is built or set GHOSTTY_DIST environment variable.",
            ghostty_dist
        );
    }

    // Build the router
    let app = Router::new()
        .route("/", get(index_handler))
        .route("/ws", get(ws_handler))
        .nest_service(
            "/dist",
            ServeDir::new(&ghostty_dist).fallback(ServeDir::new("..")),
        );

    let addr: SocketAddr = format!("0.0.0.0:{}", port).parse().unwrap();
    info!("Starting webmacs server on http://0.0.0.0:{}", port);
    info!("WebSocket endpoint: ws://0.0.0.0:{}/ws", port);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
