//! Ephemeral HTTP/1.1 keep-alive loopback WDA server driven by a router closure
//! (Python `tests/test_wda_client.py::_LoopbackWDAServer`): records every request's
//! client port and `(method, path, payload)`, and `drop_once` closes the connection
//! on the next request without answering.
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// `(method, path, payload)` -> `(status, content type, body)`.
pub type Router = Arc<dyn Fn(&str, &str, &Value) -> (u16, String, Vec<u8>) + Send + Sync>;

#[derive(Default)]
pub struct LoopbackState {
    pub ports: Vec<u16>,
    pub requests: Vec<(String, String, Value)>,
    pub drop_once: bool,
}

pub struct LoopbackWda {
    pub url: String,
    pub port: u16,
    pub state: Arc<Mutex<LoopbackState>>,
    stop: Arc<AtomicBool>,
}

impl LoopbackWda {
    pub fn start(router: impl Fn(&str, &str, &Value) -> (u16, String, Vec<u8>) + Send + Sync + 'static) -> LoopbackWda {
        let router: Router = Arc::new(router);
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(LoopbackState::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (st, sp) = (state.clone(), stop.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if sp.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(conn) = conn else { continue };
                let (st, r, sp) = (st.clone(), router.clone(), sp.clone());
                std::thread::spawn(move || serve_conn(conn, st, r, sp));
            }
        });
        LoopbackWda { url: format!("http://127.0.0.1:{port}"), port, state, stop }
    }

    pub fn ports(&self) -> Vec<u16> {
        self.state.lock().unwrap().ports.clone()
    }

    pub fn paths(&self) -> Vec<String> {
        self.state.lock().unwrap().requests.iter().map(|(_, p, _)| p.clone()).collect()
    }
}

impl Drop for LoopbackWda {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

fn serve_conn(conn: TcpStream, state: Arc<Mutex<LoopbackState>>, router: Router, stop: Arc<AtomicBool>) {
    let peer = conn.peer_addr().map(|a| a.port()).unwrap_or(0);
    let _ = conn.set_read_timeout(Some(Duration::from_secs(30)));
    let mut w = match conn.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let mut r = BufReader::new(conn);
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).unwrap_or(0) == 0 || stop.load(Ordering::SeqCst) {
            return;
        }
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("").to_string();
        let mut len = 0usize;
        loop {
            let mut h = String::new();
            if r.read_line(&mut h).unwrap_or(0) == 0 {
                return;
            }
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                if k.trim().eq_ignore_ascii_case("content-length") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
        }
        let mut body = vec![0u8; len];
        if r.read_exact(&mut body).is_err() {
            return;
        }
        let payload: Value = if body.is_empty() { Value::Null } else { serde_json::from_slice(&body).unwrap_or(Value::Null) };
        {
            let mut st = state.lock().unwrap();
            st.ports.push(peer);
            st.requests.push((method.clone(), path.clone(), payload.clone()));
            if st.drop_once {
                st.drop_once = false;
                return; // close without answering
            }
        }
        let (status, ct, out) = router(&method, &path, &payload);
        let head = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
            out.len()
        );
        if w.write_all(head.as_bytes()).and_then(|_| w.write_all(&out)).is_err() {
            return;
        }
    }
}
