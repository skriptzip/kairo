//! Live updates: a WebSocket to the server that says "your data changed".
//!
//! The socket carries only hints; each one makes the app run its normal delta sync. If the
//! connection drops it is re-established with backoff, and the app's periodic sync still
//! covers the gap, so nothing depends on this channel being up.

use std::{
    io,
    net::TcpStream,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use tungstenite::{Message, WebSocket, client::IntoClientRequest, stream::MaybeTlsStream};

use crate::api::Session;

/// How long the server may stay silent. It pings every 30 s.
const SILENCE_LIMIT: Duration = Duration::from_secs(90);
/// Granularity for noticing a stop request while waiting for data.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Dropping this closes the connection and stops the thread.
pub struct Live {
    stop: Arc<AtomicBool>,
}

impl Drop for Live {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Starts the background connection. Every received hint (and every successful connect,
/// to catch up on what happened while disconnected) becomes one `()` on the receiver.
pub fn start(session: Session) -> (Live, async_channel::Receiver<()>) {
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = async_channel::bounded(1);
    let flag = stop.clone();
    thread::spawn(move || {
        let mut backoff = Duration::from_secs(1);
        while !flag.load(Ordering::Relaxed) {
            let started = Instant::now();
            run_connection(&session, &tx, &flag);
            // A connection that held for a while resets the backoff.
            if started.elapsed() > Duration::from_secs(60) {
                backoff = Duration::from_secs(1);
            }
            sleep_unless_stopped(backoff, &flag);
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    });
    (Live { stop }, rx)
}

fn sleep_unless_stopped(total: Duration, stop: &AtomicBool) {
    let end = Instant::now() + total;
    while Instant::now() < end && !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(250));
    }
}

fn ws_url(server: &str) -> String {
    let rest = server.strip_prefix("https://").map(|r| format!("wss://{r}")).unwrap_or_else(|| {
        format!("ws://{}", server.strip_prefix("http://").unwrap_or(server))
    });
    format!("{rest}/api/v1/ws")
}

fn set_read_timeout(socket: &WebSocket<MaybeTlsStream<TcpStream>>, timeout: Duration) -> io::Result<()> {
    match socket.get_ref() {
        MaybeTlsStream::Plain(stream) => stream.set_read_timeout(Some(timeout)),
        MaybeTlsStream::Rustls(stream) => stream.get_ref().set_read_timeout(Some(timeout)),
        _ => Ok(()),
    }
}

/// One connection from handshake to its end (error, close, silence or stop).
fn run_connection(session: &Session, tx: &async_channel::Sender<()>, stop: &AtomicBool) {
    let Ok(mut request) = ws_url(&session.server).into_client_request() else { return };
    let Ok(auth) = format!("Bearer {}", session.token).parse() else { return };
    request.headers_mut().insert("Authorization", auth);
    let Ok((mut socket, _)) = tungstenite::connect(request) else { return };
    if set_read_timeout(&socket, POLL_INTERVAL).is_err() {
        return;
    }

    let mut last_heard = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        match socket.read() {
            Ok(Message::Text(_)) => {
                last_heard = Instant::now();
                let _ = tx.try_send(()); // a full slot means a sync is already queued
            }
            Ok(Message::Close(_)) => return,
            Ok(_) => last_heard = Instant::now(), // ping/pong; the library answers pings
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) =>
            {
                if last_heard.elapsed() > SILENCE_LIMIT {
                    return;
                }
            }
            Err(_) => return,
        }
    }
    let _ = socket.close(None);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_urls_become_websocket_urls() {
        assert_eq!(ws_url("http://127.0.0.1:8080"), "ws://127.0.0.1:8080/api/v1/ws");
        assert_eq!(ws_url("https://kairo.example.com"), "wss://kairo.example.com/api/v1/ws");
    }

    /// Needs a running server: `KAIRO_TEST_SERVER=http://127.0.0.1:8099 cargo test -- --ignored`
    #[test]
    #[ignore]
    fn receives_hint_when_another_device_writes() {
        let server = std::env::var("KAIRO_TEST_SERVER").expect("KAIRO_TEST_SERVER");
        let name = format!("w{}", std::process::id());
        let a = crate::api::authenticate(&server, &name, "correct horse battery", true, "").unwrap();
        let b = crate::api::authenticate(&server, &name, "correct horse battery", false, "").unwrap();

        let (_live, rx) = start(a);
        // Initial hint on connect.
        rx.recv_blocking().unwrap();
        assert!(rx.try_recv().is_err());

        // Device B writes through the normal API; A must hear about it.
        let res = ureq::put(&format!("{server}/api/v1/calendars/{}", crate::model::new_id()))
            .header("Authorization", &format!("Bearer {}", b.token))
            .send_json(serde_json::json!({ "name": "Team" }))
            .unwrap();
        assert_eq!(res.status().as_u16(), 200);
        let got = std::time::Instant::now();
        rx.recv_blocking().unwrap();
        assert!(got.elapsed() < Duration::from_secs(3));
    }
}
