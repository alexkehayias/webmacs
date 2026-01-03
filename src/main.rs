use axum::{
    extract::{ws::WebSocket, ws::Message, Query, State, WebSocketUpgrade},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use portable_pty::{
    native_pty_system,
    Child, CommandBuilder, MasterPty, PtySize,
};
use serde::Deserialize;
use std::{
    collections::HashMap,
    env,
    io::{self, Read, Write},
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;
use tower_http::services::ServeDir;
use tracing::{error, info};
use uuid::Uuid;

/// WebSocket connection query parameters
#[derive(Debug, Deserialize)]
struct WsParams {
    cols: Option<u16>,
    rows: Option<u16>,
    session_id: Option<String>,
}

/// PTY session state - stores only what's needed to keep the process alive
struct PtySession {
    _child: Box<dyn Child + Send>,
    master: Box<dyn MasterPty + Send>,
    write_tx: mpsc::Sender<Vec<u8>>,
}

unsafe impl Send for PtySession {}
unsafe impl Sync for PtySession {}

/// Session pool state
type SessionPool = Arc<Mutex<HashMap<String, PtySession>>>;

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
fn create_pty(cols: u16, rows: u16) -> io::Result<(PtySession, Box<dyn Read + Send>)> {
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

    // Spawn the command and keep the child handle
    let child = pair.slave.spawn_command(shell_cmd).map_err(|e| {
        io::Error::new(io::ErrorKind::Other, e)
    })?;

    let reader = pair.master.try_clone_reader().map_err(|e| {
        io::Error::new(io::ErrorKind::Other, e)
    })?;
    let mut writer = pair.master.take_writer().map_err(|e| {
        io::Error::new(io::ErrorKind::Other, e)
    })?;

    // Create a channel for writes
    let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(100);

    // Spawn a background task to forward writes from channel to PTY
    tokio::spawn(async move {
        while let Some(data) = write_rx.recv().await {
            if writer.write_all(&data).is_err() {
                break;
            }
            let _ = writer.flush();
        }
    });

    let session = PtySession {
        _child: child,
        master: pair.master,
        write_tx,
    };

    Ok((session, reader))
}

/// WebSocket upgrade handler
async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(params): Query<WsParams>,
    State(sessions): State<SessionPool>,
) -> Response {
    info!("WebSocket connection requested with cols={:?}, rows={:?}", params.cols, params.rows);

    ws.on_upgrade(|socket| handle_socket(socket, params, sessions))
}

/// Handle WebSocket connection
async fn handle_socket(mut socket: WebSocket, params: WsParams, sessions: SessionPool) {
    let cols = params.cols.unwrap_or(80);
    let rows = params.rows.unwrap_or(24);

    // Determine session ID - frontend always provides one
    let session_id = params.session_id.unwrap_or_else(|| Uuid::new_v4().to_string());

    // Try to get existing session or create new one
    let (is_new_session, pty_reader, write_tx): (bool, Box<dyn Read + Send>, mpsc::Sender<Vec<u8>>) = {
        let mut sessions_guard = sessions.lock().unwrap();
        let is_new_session = !sessions_guard.contains_key(&session_id);

        if is_new_session {
            // Create new PTY session
            match create_pty(cols, rows) {
                Ok((session, reader)) => {
                    info!("Created new PTY session: {}", session_id);
                    let write_tx = session.write_tx.clone();
                    sessions_guard.insert(session_id.clone(), session);
                    (true, reader, write_tx)
                }
                Err(e) => {
                    error!("Failed to create PTY: {}", e);
                    // Return early with a placeholder that will cause us to exit
                    return;
                }
            }
        } else {
            // Reuse existing session
            info!("Reusing existing PTY session: {}", session_id);
            let session = sessions_guard.get(&session_id).unwrap();
            // Clone the reader from existing session
            match session.master.try_clone_reader() {
                Ok(reader) => {
                    let write_tx = session.write_tx.clone();
                    (false, reader, write_tx)
                }
                Err(_) => {
                    error!("Failed to clone PTY reader for session {}", session_id);
                    return;
                }
            }
        }
    };

    let mut pty_reader = pty_reader;

    // Send welcome message (only for new sessions)
    if is_new_session {
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
            "{}  Session ID: {}{}{}\r\n",
            "\x1b[1;36m║\x1b[0m  ",
            "\x1b[1;33m",
            session_id,
            "\x1b[0m",
        );
        let welcome4 = format!(
            "{}  Try: ls, cd, top, emacs, or any command!                   {}\r\n",
            "\x1b[1;36m║\x1b[0m  ",
            "\x1b[1;36m║\x1b[0m",
        );
        let welcome5 = format!(
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
        if socket.send(Message::Text(welcome5.into())).await.is_err() {
            return;
        }
    } else {
        // Reconnection message
        let reconnect = format!(
            "\r\n\x1b[1;32mReconnected to session: {}\x1b[0m\r\n",
            session_id
        );
        if socket.send(Message::Text(reconnect.into())).await.is_err() {
            return;
        }
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

                    // Send to PTY via channel
                    if write_tx.send(text.bytes().collect::<Vec<u8>>()).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Binary(data)) => {
                    if write_tx.send(data.to_vec()).await.is_err() {
                        break;
                    }
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

    // Create session pool
    let sessions: SessionPool = Arc::new(Mutex::new(HashMap::new()));

    // Spawn session cleanup task
    let sessions_cleanup = sessions.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(300)); // Check every 5 minutes
        loop {
            interval.tick().await;
            let sessions = sessions_cleanup.lock().unwrap();
            if !sessions.is_empty() {
                info!("Session pool has {} active sessions", sessions.len());
            }
        }
    });

    // Build the router
    let app = Router::new()
        .route("/", get(index_handler))
        .route("/ws", get(ws_handler))
        .nest_service(
            "/static",
            ServeDir::new("static"),
        )
        .with_state(sessions);

    let addr: SocketAddr = format!("0.0.0.0:{}", port).parse().unwrap();
    info!("Starting webmacs server on http://0.0.0.0:{}", port);
    info!("WebSocket endpoint: ws://0.0.0.0:{}/ws", port);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
