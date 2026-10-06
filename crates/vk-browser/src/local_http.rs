//! A minimal loopback-only HTTP/1.1 server for tests and the bench: serves fixed pages from
//! memory on `127.0.0.1:<ephemeral>`. Not for production use.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;

pub struct LocalServer {
    pub addr: SocketAddr,
    stop: Arc<AtomicBool>,
}

impl LocalServer {
    /// Serve `pages` (path → HTML). Unknown paths get 404.
    pub fn start(pages: HashMap<String, String>) -> Result<LocalServer> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let pages = Arc::new(pages);
        std::thread::Builder::new()
            .name("local-http".into())
            .spawn(move || {
                for conn in listener.incoming() {
                    if stop2.load(Ordering::Relaxed) {
                        break;
                    }
                    let Ok(conn) = conn else { continue };
                    let pages = pages.clone();
                    std::thread::spawn(move || {
                        let _ = handle(conn, &pages);
                    });
                }
            })?;
        Ok(LocalServer { addr, stop })
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Unblock accept().
        let _ = TcpStream::connect(self.addr);
    }
}

fn handle(conn: TcpStream, pages: &HashMap<String, String>) -> Result<()> {
    let mut r = BufReader::new(conn.try_clone()?);
    let mut w = conn;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let path = line.split_whitespace().nth(1).unwrap_or("/").to_owned();
        // Skip headers.
        loop {
            let mut h = String::new();
            if r.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" {
                break;
            }
        }
        let (status, body) = match pages.get(path.split('?').next().unwrap_or("/")) {
            Some(b) => ("200 OK", b.as_str()),
            None => ("404 Not Found", "not found"),
        };
        write!(
            w,
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\r\n{body}",
            body.len()
        )?;
        w.flush()?;
    }
}
