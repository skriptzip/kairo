//! WebSocket endpoint: tells a client when its data changed on the server.
//!
//! The socket only carries hints (`{"rev":N}`). The client then runs its normal delta sync,
//! so a missed hint (dropped connection, lagging receiver) never loses data: the next
//! connection starts with the current revision and the periodic poll still exists.

use std::time::Duration;

use axum::{
    Extension,
    body::Bytes,
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::Response,
};

use crate::{
    AppState, User, auth,
    auth::SessionId,
    current_rev,
};

const PING_EVERY: Duration = Duration::from_secs(30);

pub async fn ws(
    State(state): State<AppState>,
    Extension(User(user)): Extension<User>,
    Extension(SessionId(session)): Extension<SessionId>,
    upgrade: WebSocketUpgrade,
) -> Response {
    upgrade.on_upgrade(move |socket| serve(socket, state, user, session))
}

fn hint(rev: i64) -> Message {
    Message::Text(format!("{{\"rev\":{rev}}}").into())
}

async fn serve(mut socket: WebSocket, state: AppState, user: String, session: String) {
    // Subscribe before reading the revision so no write can slip in between.
    let mut changes = state.changes.subscribe();
    let head = current_rev(&state.db.lock().unwrap()).unwrap_or(0);
    if socket.send(hint(head)).await.is_err() {
        return;
    }
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;

    loop {
        tokio::select! {
            change = changes.recv() => {
                let rev = match change {
                    Ok((who, rev)) if who == user => rev,
                    Ok(_) => continue,
                    // We missed some notifications: just point at the newest state.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        current_rev(&state.db.lock().unwrap()).unwrap_or(0)
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                if socket.send(hint(rev)).await.is_err() {
                    break;
                }
            }
            incoming = socket.recv() => match incoming {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                Some(Ok(_)) => {} // pings are answered by the library; clients send nothing else
            },
            _ = ping.tick() => {
                // Also the moment to drop sockets whose session was revoked or expired.
                if !auth::session_valid(&state.db.lock().unwrap(), &session) {
                    break;
                }
                if socket.send(Message::Ping(Bytes::new())).await.is_err() {
                    break;
                }
            }
        }
    }
}
