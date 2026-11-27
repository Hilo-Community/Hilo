#![allow(dead_code)]
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

/// A loopback TCP server that counts connections and answers each request with `body`
/// after `delay`. Every request it read is kept.
pub struct FakeServer {
    pub address: SocketAddr,
    pub connections: Arc<AtomicUsize>,
    pub requests: Arc<Mutex<Vec<String>>>,
}

impl FakeServer {
    pub async fn start(body: &'static str, delay: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let connections = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (count, seen) = (connections.clone(), requests.clone());
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                count.fetch_add(1, Ordering::SeqCst);
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut buffer = vec![0u8; 64 * 1024];
                    let mut read = 0;
                    // Headers, then the body announced by content-length.
                    loop {
                        let Ok(n) = stream.read(&mut buffer[read..]).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        read += n;
                        let text = String::from_utf8_lossy(&buffer[..read]).to_string();
                        if let Some(end) = text.find("\r\n\r\n") {
                            let length = text[..end]
                                .lines()
                                .find_map(|line| {
                                    line.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                                })
                                .unwrap_or(0);
                            if read >= end + 4 + length {
                                seen.lock().unwrap().push(text);
                                break;
                            }
                        }
                    }
                    tokio::time::sleep(delay).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        Self {
            address,
            connections,
            requests,
        }
    }
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

/// A loopback port nothing listens on: connecting to it fails at once.
pub fn closed_port() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}
