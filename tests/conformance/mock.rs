//! Starts the suite's mock server and reads its request log.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;

pub struct Mock {
    pub url: String,
    addr: String,
    child: Mutex<Option<Child>>,
}

impl Mock {
    /// Starts `mock/server.py` on a free port and reads the URL from its first
    /// line of output.
    pub fn start(root: &Path) -> Mock {
        let python = std::env::var("DECISIONNODE_PYTHON").unwrap_or_else(|_| "python3".into());
        let mut child = Command::new(python)
            .arg(root.join("mock").join("server.py"))
            .args(["--port", "0", "--cases"])
            .arg(root.join("cases"))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the mock server could not be started (is python3 installed?)");
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let line = line.trim();
        let Some(url) = line.strip_prefix("listening on ") else {
            let _ = child.kill();
            panic!("the mock did not start: {line:?}");
        };
        let mut mock = Mock::connect(url);
        mock.child = Mutex::new(Some(child));
        mock
    }

    /// Uses a mock that is already running.
    pub fn connect(url: &str) -> Mock {
        let url = url.trim_end_matches('/').to_owned();
        let addr = url.trim_start_matches("http://").split('/').next().unwrap().to_owned();
        Mock { url, addr, child: Mutex::new(None) }
    }

    pub fn stop(&self) {
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// The requests the mock recorded for a session.
    pub fn requests(&self, session: &str) -> Value {
        self.get(&format!("/_mock/requests?session={session}"))
    }

    /// The requests any session sent to `path`.
    pub fn requests_at(&self, path: &str) -> Value {
        let mut encoded = String::new();
        for b in path.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => encoded.push(b as char),
                _ => encoded.push_str(&format!("%{b:02X}")),
            }
        }
        self.get(&format!("/_mock/requests?session=*&path={encoded}"))
    }

    fn get(&self, path: &str) -> Value {
        let mut stream = TcpStream::connect(&self.addr).expect("connect to the mock");
        stream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        write!(stream, "GET {path} HTTP/1.0\r\nHost: {}\r\nConnection: close\r\n\r\n", self.addr).unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        let split = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("an HTTP response");
        serde_json::from_slice(&raw[split + 4..]).expect("a JSON body from the mock")
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stop();
    }
}
