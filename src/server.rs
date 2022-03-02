use crate::response::{Body, Chunked};
use crate::{Mock, Request};
use std::cell::RefCell;
use std::fmt::Display;
use std::io;
use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

impl Mock {
    fn method_matches(&self, request: &Request) -> bool {
        self.method == request.method
    }

    fn path_matches(&self, request: &Request) -> bool {
        self.path.matches_value(&request.path)
    }

    fn headers_match(&self, request: &Request) -> bool {
        self.headers.iter().all(|&(ref field, ref expected)| {
            expected.matches_values(&request.find_header_values(field))
        })
    }

    fn body_matches(&self, request: &Request) -> bool {
        let raw_body = &request.body;
        let safe_body = &String::from_utf8_lossy(raw_body);

        self.body.matches_value(safe_body) || self.body.matches_binary_value(raw_body)
    }

    #[allow(clippy::missing_const_for_fn)]
    fn is_missing_hits(&self) -> bool {
        match (self.expected_hits_at_least, self.expected_hits_at_most) {
            (Some(_at_least), Some(at_most)) => self.hits < at_most,
            (Some(at_least), None) => self.hits < at_least,
            (None, Some(at_most)) => self.hits < at_most,
            (None, None) => self.hits < 1,
        }
    }
}

impl<'a> PartialEq<Request> for &'a mut Mock {
    fn eq(&self, other: &Request) -> bool {
        self.method_matches(other)
            && self.path_matches(other)
            && self.headers_match(other)
            && self.body_matches(other)
    }
}

pub struct Server {
    pub listening_addr: Option<SocketAddr>,
    pub mocks: Vec<Mock>,
    pub unmatched_requests: Vec<Request>,
}

impl Server {
    #[allow(clippy::missing_const_for_fn)]
    fn new() -> Self {
        Self {
            listening_addr: None,
            mocks: Vec::new(),
            unmatched_requests: Vec::new(),
        }
    }

    pub fn try_start(&mut self) {
        if self.listening_addr.is_some() {
            return;
        }

        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let res = TcpListener::bind("127.0.0.1:0");
            let (listener, addr) = match res {
                Ok(listener) => {
                    let addr = listener.local_addr().unwrap();
                    tx.send(Some(addr)).unwrap();
                    (listener, addr)
                }
                Err(err) => {
                    error!("{}", err);
                    tx.send(None).unwrap();
                    return;
                }
            };

            debug!("[{:?}] Server is listening", addr);
            for stream in listener.incoming() {
                if let Ok(stream) = stream {
                    let request = Request::from(&stream);
                    debug!("[{:?}] Request received: {}", addr, request);
                    if request.is_ok() {
                        handle_request(request, stream);
                    } else {
                        let message = request
                            .error()
                            .map_or("Could not parse the request.", |err| err.as_str());
                        debug!("Could not parse request because: {}", message);
                        respond_with_error(stream, request.version, message);
                    }
                } else {
                    debug!("Could not read from stream");
                }
            }
        });

        self.listening_addr = rx.recv().ok().and_then(|addr| addr);
    }
}

pub struct ServerPool {
    servers: Vec<Mutex<Server>>,
}

impl ServerPool {
    fn new(size: usize) -> Self {
        let mut servers = vec![];

        for _ in 0..size {
            let server = Server::new();
            servers.push(Mutex::new(server));
        }

        ServerPool { servers }
    }

    fn find_server(&self) -> MutexGuard<Server> {
        self.servers
            .iter()
            .find_map(|server| server.try_lock().ok())
            .or_else(|| self.servers[0].lock().ok())
            .unwrap()
    }
}

lazy_static! {
    pub static ref SERVER: Mutex<Server> = Mutex::new(Server::new());
    pub static ref POOL_RC: Arc<ServerPool> = Arc::new(ServerPool::new(4));
}

thread_local!(
    pub static LOCAL_SERVER: RefCell<MutexGuard<'static, Server>> =
        RefCell::new(POOL_RC.find_server());
);

/// Address and port of the local server.
/// Can be used with `std::net::TcpStream`.
///
/// The server will be started if necessary.
pub fn address() -> SocketAddr {
    LOCAL_SERVER.with(|server| {
        let mut server = server.borrow_mut();
        server.try_start();
        server.listening_addr.expect("server should be listening")
    })
}

/// A local `http://…` URL of the server.
///
/// The server will be started if necessary.
pub fn url() -> String {
    format!("http://{}", address())
}

fn handle_request(request: Request, stream: TcpStream) {
    LOCAL_SERVER.with(|server| {
        let mut server = server.borrow_mut();
        debug!("[{:?}] Matching request", server.listening_addr);

        let mut matchings_mocks = server
            .mocks
            .iter_mut()
            .filter(|mock| mock == &request)
            .collect::<Vec<_>>();

        let maybe_missing_hits = matchings_mocks.iter_mut().find(|m| m.is_missing_hits());

        let mock = match maybe_missing_hits {
            Some(m) => Some(m),
            None => matchings_mocks.last_mut(),
        };

        if let Some(mock) = mock {
            debug!("Mock found");
            mock.hits += 1;
            respond_with_mock(stream, request.version, mock, request.is_head());
        } else {
            debug!("Mock not found");
            respond_with_mock_not_found(stream, request.version);
            server.unmatched_requests.push(request);
        }
    });
}

fn respond(
    stream: TcpStream,
    version: (u8, u8),
    status: impl Display,
    headers: Option<&Vec<(String, String)>>,
    body: Option<&str>,
) {
    let body = body.map(|s| Body::Bytes(s.as_bytes().to_owned()));
    if let Err(e) = respond_bytes(stream, version, status, headers, body.as_ref()) {
        eprintln!("warning: Mock response write error: {}", e);
    }
}

fn respond_bytes(
    mut stream: TcpStream,
    version: (u8, u8),
    status: impl Display,
    headers: Option<&Vec<(String, String)>>,
    body: Option<&Body>,
) -> io::Result<()> {
    let mut response = Vec::from(format!("HTTP/{}.{} {}\r\n", version.0, version.1, status));
    let mut has_content_length_header = false;

    if let Some(headers) = headers {
        for &(ref key, ref value) in headers {
            response.extend(key.as_bytes());
            response.extend(b": ");
            response.extend(value.as_bytes());
            response.extend(b"\r\n");
        }

        has_content_length_header = headers.iter().any(|(key, _)| key == "content-length");
    }

    match body {
        Some(Body::Bytes(bytes)) => {
            if !has_content_length_header {
                response.extend(format!("content-length: {}\r\n", bytes.len()).as_bytes());
            }
        }
        Some(Body::Fn(_)) => {
            response.extend(b"transfer-encoding: chunked\r\n");
        }
        None => {}
    };
    response.extend(b"\r\n");
    stream.write_all(&response)?;
    match body {
        Some(Body::Bytes(bytes)) => {
            stream.write_all(bytes)?;
        }
        Some(Body::Fn(cb)) => {
            let mut chunked = Chunked::new(&mut stream);
            cb(&mut chunked)?;
            chunked.finish()?;
        }
        None => {}
    };
    stream.flush()
}

fn respond_with_mock(stream: TcpStream, version: (u8, u8), mock: &Mock, skip_body: bool) {
    let body = if skip_body {
        None
    } else {
        Some(&mock.response.body)
    };

    if let Err(e) = respond_bytes(
        stream,
        version,
        &mock.response.status,
        Some(&mock.response.headers),
        body,
    ) {
        eprintln!("warning: Mock response write error: {}", e);
    }
}

fn respond_with_mock_not_found(stream: TcpStream, version: (u8, u8)) {
    respond(
        stream,
        version,
        "501 Mock Not Found",
        Some(&vec![("content-length".into(), "0".into())]),
        None,
    );
}

fn respond_with_error(stream: TcpStream, version: (u8, u8), message: &str) {
    respond(stream, version, "422 Mock Error", None, Some(message));
}
