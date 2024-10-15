use std::io::{self, IoSlice, Read};
use std::net::ToSocketAddrs;
use std::{array::from_fn, collections::HashMap, fmt, io::Write, path::Path};

use flate2::write::GzEncoder;
use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token};

const MAX_FILE_SIZE: usize = 32 * 1024 * 1024; // 32 MiB
const MAX_DEPTH: usize = 8;
const MAX_CONNECTIONS: usize = 1024;
const MAX_PATH_LEN: usize = 256;
const MAX_REQUEST_SIZE: usize = 4096;
const SERVER_TOKEN: Token = Token(MAX_CONNECTIONS);

struct ContentStore {
    plain_responses: HashMap<&'static str, Response>,
    gzip_responses: HashMap<&'static str, Response>,
    not_found: Response,
}

struct Connection {
    token: Token,
    stream: Option<TcpStream>,
    buffer: Box<[u8]>,
    buffer_len: usize,
    pending_write: Option<PendingWrite>,
}

struct PendingWrite {
    header: &'static [u8],
    body: &'static [u8],
    written: usize,
}

struct Request {
    path_buf: [u8; MAX_PATH_LEN],
    path_len: usize,
    accept_gzip: bool,
    if_none_match: Option<u128>,
}

#[derive(Clone, Copy)]
struct Response {
    header: &'static [u8],
    body: &'static [u8],
    etag: Option<u128>,
}

#[derive(Clone, Copy, Debug)]
enum ParseRequestError {
    BadRequest,
    RequestTooLarge,
    Incomplete,
}

impl From<std::str::Utf8Error> for ParseRequestError {
    fn from(_: std::str::Utf8Error) -> Self {
        Self::BadRequest
    }
}

impl Connection {
    fn new(token: Token) -> Self {
        Self {
            token,
            stream: None,
            buffer: Box::new([0; MAX_REQUEST_SIZE]),
            buffer_len: 0,
            pending_write: None,
        }
    }

    fn try_read(&mut self) -> io::Result<usize> {
        let stream = match self.stream.as_mut() {
            Some(stream) => stream,
            None => return Ok(0),
        };
        let remaining = &mut self.buffer[self.buffer_len..];
        let read_bytes = match stream.read(remaining) {
            Ok(n) => n,
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => 0,
            Err(e) => return Err(e),
        };
        println!("Read {} bytes", read_bytes);
        self.buffer_len += read_bytes;
        Ok(read_bytes)
    }

    fn try_write(&mut self) -> io::Result<()> {
        let stream = match self.stream.as_mut() {
            Some(stream) => stream,
            None => return Ok(()),
        };
        let PendingWrite {
            header,
            body,
            written,
        } = match &mut self.pending_write {
            Some(pending_write) => pending_write,
            None => return Ok(()),
        };
        let header_offset = header.len().min(*written);
        let body_offset = written.saturating_sub(header.len());

        let iov = [
            IoSlice::new(&header[header_offset..]),
            IoSlice::new(&body[body_offset..]),
        ];
        let bytes_written = match stream.write_vectored(&iov) {
            Ok(n) => n,
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => 0,
            Err(e) => return Err(e),
        };
        println!("Wrote {} bytes", bytes_written);
        *written += bytes_written;
        if *written >= header.len() + body.len() {
            self.pending_write = None;
        }
        Ok(())
    }

    fn send(&mut self, response: Response) -> io::Result<()> {
        self.pending_write = Some(PendingWrite {
            header: response.header,
            body: response.body,
            written: 0,
        });
        self.try_write()
    }

    fn parse_request(&mut self) -> Result<Request, ParseRequestError> {
        let end = self
            .buffer
            .windows(4)
            .position(|window| window == b"\r\n\r\n");
        if end.is_none() && self.buffer_len == self.buffer.len() {
            self.buffer_len = 0;
            return Err(ParseRequestError::RequestTooLarge);
        }
        let end = end.ok_or(ParseRequestError::Incomplete)? + 4;
        let mut lines = std::str::from_utf8(&self.buffer[..end])?.lines();
        let mut req_parts = lines
            .next()
            .ok_or(ParseRequestError::BadRequest)?
            .splitn(3, ' ');
        let method = req_parts.next().ok_or(ParseRequestError::BadRequest)?;
        if method != "GET" {
            return Err(ParseRequestError::BadRequest);
        }
        let path = req_parts.next().ok_or(ParseRequestError::BadRequest)?;
        let mut accept_gzip = false;
        let mut if_none_match = None;
        for (key, value) in lines.flat_map(|line| line.split_once(": ")) {
            if key.eq_ignore_ascii_case("accept-encoding") {
                accept_gzip = value.split(',').any(|enc| enc.eq_ignore_ascii_case("gzip"));
            } else if key.eq_ignore_ascii_case("if-none-match") {
                let value = value.trim_matches(|c| c == '"');
                if let Ok(etag) = u128::from_str_radix(value, 16) {
                    if_none_match = Some(etag);
                }
            }
        }
        let request = Request::new(path, accept_gzip, if_none_match)?;
        self.buffer.copy_within(end.., 0);
        self.buffer_len -= end;
        Ok(request)
    }

    fn clear(&mut self) {
        self.pending_write = None;
        self.buffer_len = 0;
    }

    fn accept(&mut self, stream: TcpStream, poll: &Poll) -> io::Result<()> {
        self.stream = Some(stream);
        poll.registry().register(
            self.stream.as_mut().unwrap(),
            self.token,
            Interest::READABLE.add(Interest::WRITABLE),
        )?;
        Ok(())
    }

    fn close(&mut self, poll: &Poll) -> io::Result<()> {
        self.clear();
        if let Some(mut stream) = self.stream.take() {
            poll.registry().deregister(&mut stream)?
        }
        println!("Closing connection");
        Ok(())
    }
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("path", &self.path())
            .field("accept_gzip", &self.accept_gzip)
            .field(
                "if_none_match",
                &self.if_none_match.map(|etag| format!("{:x}", etag)),
            )
            .finish()
    }
}

impl Request {
    fn new(
        path_str: &str,
        accept_gzip: bool,
        if_none_match: Option<u128>,
    ) -> Result<Self, ParseRequestError> {
        let path = path_str.as_bytes();
        if path.len() >= MAX_PATH_LEN {
            return Err(ParseRequestError::BadRequest);
        }
        let mut path_buf = [0; MAX_PATH_LEN];
        path_buf[..path.len()].copy_from_slice(path);
        Ok(Self {
            path_buf,
            path_len: path.len(),
            accept_gzip,
            if_none_match,
        })
    }

    fn path(&self) -> &str {
        let path = std::str::from_utf8(&self.path_buf[..self.path_len]).unwrap();
        // remove leading and trailing slashes
        let path = path.strip_prefix('/').unwrap_or(path);
        let path = path.strip_suffix('/').unwrap_or(path);
        path
    }
}

impl Response {
    const NOT_FOUND: Self = Self {
        header: b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
        body: b"",
        etag: None,
    };
    const BAD_REQUEST: Self = Self {
        header: b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n",
        body: b"",
        etag: None,
    };
    const NOT_MODIFIED: Self = Self {
        header: b"HTTP/1.1 304 Not Modified\r\nContent-Length: 0\r\n\r\n",
        body: b"",
        etag: None,
    };

    fn len(&self) -> usize {
        self.header.len() + self.body.len()
    }

    fn not_modified(&self, req: &Request) -> bool {
        match (self.etag, req.if_none_match) {
            (Some(etag), Some(if_none_match)) => etag == if_none_match,
            _ => false,
        }
    }
}

impl fmt::Display for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(std::str::from_utf8(self.header).unwrap())?;
        let body_str = std::string::String::from_utf8_lossy(self.body);
        f.write_str(&body_str)?;
        Ok(())
    }
}

impl Default for ContentStore {
    fn default() -> Self {
        Self {
            plain_responses: HashMap::new(),
            gzip_responses: HashMap::new(),
            not_found: Response::NOT_FOUND,
        }
    }
}

impl ContentStore {
    fn new(root: &str) -> Self {
        std::env::set_current_dir(root).unwrap();
        let mut store = Self::default();

        let mut stack = vec![std::fs::read_dir(".").unwrap()];
        while let Some(dir) = stack.last_mut() {
            // If we've read all entries in the current directory, pop it off the stack
            let Some(entry) = dir.next() else {
                stack.pop();
                continue;
            };
            let entry = entry.unwrap();
            let path = entry.path();
            let meta = match entry.metadata() {
                Ok(meta) => meta,
                Err(e) => {
                    eprintln!("Skipping {}: {}", path.display(), e);
                    continue;
                }
            };
            if meta.is_symlink() {
                eprintln!("Skipping symlink {}", path.display());
            } else if meta.is_dir() {
                if stack.len() > MAX_DEPTH {
                    eprintln!("Skipping {} because it's too deep", path.display());
                    continue;
                }
                stack.push(std::fs::read_dir(path).unwrap());
            } else if meta.is_file() {
                if meta.len() > MAX_FILE_SIZE as u64 {
                    eprintln!("Skipping {} because it's too large", path.display());
                    continue;
                }
                let path = path.strip_prefix(".").unwrap();
                let path_str = path.to_str().unwrap().to_string();
                eprintln!("Adding file {}", path_str);
                store.put(String::leak(path_str));
            } else {
                eprintln!("Skipping special file {}", path.display());
            }
        }

        // If we have a 404.html file, use it as the 404 response
        if let Some(res) = store.get(Request::new("404.html", false, None).unwrap()) {
            store.not_found = custom_not_found(res.body);
        }

        store
    }

    fn serve<A: ToSocketAddrs>(&self, addr: A) -> io::Result<()> {
        let addr = addr.to_socket_addrs()?.next().unwrap();
        let mut listener = TcpListener::bind(addr)?;
        println!("Listening on {}", addr);
        let mut poll = Poll::new()?;
        let mut events = Events::with_capacity(MAX_CONNECTIONS);
        poll.registry()
            .register(&mut listener, SERVER_TOKEN, Interest::READABLE)?;
        let mut connections: [_; MAX_CONNECTIONS] = from_fn(|i| Connection::new(Token(i)));

        loop {
            poll.poll(&mut events, None)?;
            for event in events.iter() {
                match event.token() {
                    SERVER_TOKEN => {
                        let (stream, addr) = match listener.accept() {
                            Ok(conn) => conn,
                            Err(err) => {
                                eprintln!("Error accepting connection: {:?}", err);
                                continue;
                            }
                        };
                        match connections.iter_mut().find(|c| c.stream.is_none()) {
                            Some(conn) => {
                                conn.accept(stream, &poll)?;
                                println!("Accepted connection from: {}", addr);
                            }
                            None => {
                                eprintln!("Connection limit reached, dropping new connection");
                                continue;
                            }
                        }
                    }
                    Token(id) => {
                        let conn = &mut connections[id];
                        if event.is_write_closed() || event.is_read_closed() {
                            conn.close(&poll)?;
                        } else if event.is_writable() {
                            if let Err(err) = conn.try_write() {
                                eprintln!("Error writing to connection: {:?}", err);
                                conn.close(&poll)?;
                            }
                        } else if event.is_readable() && conn.pending_write.is_none() {
                            if let Err(err) = conn.try_read() {
                                eprintln!("Error reading from connection: {:?}", err);
                                conn.close(&poll)?;
                                continue;
                            }
                            let res = match conn.parse_request() {
                                Ok(req) => {
                                    println!("Received request: {:?}", req);
                                    self.get(req).unwrap_or(self.not_found)
                                }
                                Err(ParseRequestError::Incomplete) => continue,
                                Err(_) => {
                                    conn.clear();
                                    Response::BAD_REQUEST
                                }
                            };
                            conn.send(res)?;
                        }
                    }
                }
            }
        }
    }

    fn put(&mut self, path_str: &'static str) {
        let path = Path::new(path_str);
        let file_name = path.file_name().unwrap().to_str().unwrap();
        let parent_dir = path.parent().unwrap().to_str().unwrap();
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default();
        let content_type = content_type(extension);

        let body = match std::fs::read(path) {
            Ok(body) => body,
            Err(err) => {
                eprintln!("Failed to read file {}: {}", path_str, err);
                return;
            }
        };
        let alt_path = (file_name == "index.html").then_some(parent_dir);
        let response = http_response(content_type, body, false);
        self.plain_responses.insert(path_str, response);
        if let Some(alt_path) = alt_path {
            self.plain_responses.insert(alt_path, response);
        }

        if gzippable(content_type) {
            let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(response.body).unwrap();
            let compressed = encoder.finish().unwrap();
            let response = http_response(content_type, compressed, true);
            self.gzip_responses.insert(path_str, response);
            if let Some(alt_path) = alt_path {
                self.gzip_responses.insert(alt_path, response);
            }
        }
    }

    fn get(&self, req: Request) -> Option<Response> {
        let path = req.path();
        let mut res = 'original: {
            if req.accept_gzip {
                if let Some(response) = self.gzip_responses.get(path) {
                    break 'original *response;
                }
            }
            *self.plain_responses.get(path)?
        };
        if res.not_modified(&req) {
            eprintln!("Strong ETag match");
            res = Response::NOT_MODIFIED;
        }
        Some(res)
    }

    fn memory_usage(&self) -> usize {
        self.plain_responses
            .iter()
            .chain(self.gzip_responses.iter())
            .map(|(hash, response)| hash.as_bytes().len() + response.len())
            .sum()
    }
}

fn http_response(content_type: &str, body: Vec<u8>, gzipped: bool) -> Response {
    let body = Box::leak(body.into_boxed_slice());
    let etag: u128 = xxhash_rust::xxh3::xxh3_128(body);
    let cache_control = cache_control(content_type);
    let mut header = Vec::with_capacity(512);
    write!(&mut header, "HTTP/1.1 200 OK\r\n").unwrap();
    if gzipped {
        write!(&mut header, "Content-Encoding: gzip\r\n").unwrap();
    }
    write!(&mut header, "Content-Type: {}\r\n", content_type).unwrap();
    write!(&mut header, "Content-Length: {}\r\n", body.len()).unwrap();
    write!(&mut header, "ETag: \"{:x}\"\r\n", etag).unwrap();
    write!(&mut header, "Cache-Control: {}\r\n", cache_control).unwrap();
    write!(&mut header, "Vary: Accept-Encoding\r\n\r\n").unwrap();
    let header = Box::leak(header.into_boxed_slice());
    Response {
        header,
        body,
        etag: Some(etag),
    }
}

fn custom_not_found(body: &'static [u8]) -> Response {
    let mut header = Vec::with_capacity(512);
    write!(&mut header, "HTTP/1.1 404 Not Found\r\n").unwrap();
    write!(&mut header, "Content-Type: text/html\r\n").unwrap();
    write!(&mut header, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
    let header = Box::leak(header.into_boxed_slice());
    Response {
        header,
        body,
        etag: None,
    }
}

fn content_type(extension: &str) -> &'static str {
    match extension {
        "html" => "text/html",
        "txt" => "text/plain",
        "css" => "text/css",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        "ogg" => "audio/ogg",
        "webm" => "video/webm",
        "pdf" => "application/pdf",
        "xml" => "application/xml",
        "js" => "application/javascript",
        "json" => "application/json",
        "woff2" => "font/woff2",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

fn cache_control(content_type: &str) -> &'static str {
    match content_type {
        "application/javascript" => "max-age=3600, public", // 1 hour
        "font/woff2" => "max-age=86400, public",            // 1 day
        _ => "max-age=0, public",                           // 1 minute
    }
}

fn gzippable(content_type: &str) -> bool {
    let mut content_type = content_type.split('/');
    let category = content_type.next().unwrap();
    let subtype = content_type.next().unwrap();
    match category {
        "text" => true,
        "application" => {
            matches!(subtype, "xml" | "json" | "pdf" | "javascript")
        }
        _ => false,
    }
}

fn main() {
    let cs = ContentStore::new("../elo/paracosm/docs/public/public");
    println!("Memory usage: {} bytes", cs.memory_usage());
    cs.serve("127.0.0.1:8080").unwrap();
}
