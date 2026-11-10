//! The ClickHouse program: connections over ClickHouse's HTTP interface, which every ClickHouse
//! server and ClickHouse Cloud speak, and which `@clickhouse/client` speaks too. A URL is
//! `clickhouse://user:password@host:8123/database` for HTTP and `clickhouses://…` for HTTPS (port
//! 8443 where it names none); each parameter of its query string is a setting sent with every
//! request, so the analysis reads a statement under the settings the app runs it with
//! (`?join_use_nulls=1`).
//!
//! The engine's describe is answered by `DESCRIBE (<statement>)`, which names each result column
//! and its type without running the statement, and its catalog queries by their rows. A describe
//! the server refuses is an answer the engine reads; a server that cannot be reached, or refuses the
//! login, is told apart as the other programs tell it. The connection is kept between requests and
//! opened again where the server closed it.

use crate::{Failure, Program};
use reylo_engine::clickhouse::api::ClickhouseDescribe;
use reylo_engine::clickhouse::Clickhouse as Engine;
use reylo_engine::{AnswerCache, AsyncAnswerer, DbError, HostError, ReplayError};
use reylo_engine_core::api::Analysis;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufStream};
use tokio::net::TcpStream;

/// How long a connection attempt may take, as the other programs give one.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the server may take to answer one request: a describe and a catalog query read no data.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// ClickHouse's error codes for a login it refuses: a wrong user or password, and a user it requires one of.
const REFUSED_LOGIN: [&str; 3] = ["516", "192", "193"];

/// What a describe's statement is wrapped in, whose length a syntax error's position counts.
const DESCRIBE: &str = "DESCRIBE (\n";

trait Io: AsyncRead + AsyncWrite + Unpin + Send + Sync {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + Sync> Io for T {}

/// Where a URL's server is, and how each request reaches it.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub user: Option<String>,
    pub password: Option<String>,
    pub database: Option<String>,
    /// The URL's own parameters, each a setting of every request.
    pub settings: Vec<(String, String)>,
}

/// The settings every request carries, which a URL's parameter of the same name does not replace:
/// the answer whole before its status, so that an error is the status and never half a result.
const OWN_SETTINGS: [(&str, &str); 2] = [("default_format", "JSONCompact"), ("wait_end_of_query", "1")];

/// The server a URL names. `clickhouse://` is HTTP on port 8123, `clickhouses://` HTTPS on 8443;
/// `clickhouse+http://` and `clickhouse+https://` say the same as SQLAlchemy's URLs do.
pub fn parse(url: &str) -> Result<Target, String> {
    let (scheme, rest) = url.split_once("://").ok_or_else(|| "cannot read the URL: it has no scheme, as in clickhouse://localhost:8123/default".to_string())?;
    let tls = match scheme.to_ascii_lowercase().as_str() {
        "clickhouse" | "clickhouse+http" => false,
        "clickhouses" | "clickhouse+https" => true,
        other => return Err(format!("cannot read the URL: {other}:// names no ClickHouse server; write clickhouse:// for HTTP or clickhouses:// for HTTPS")),
    };
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (userinfo, address) = match authority.rsplit_once('@') {
        Some((userinfo, address)) => (Some(userinfo), address),
        None => (None, authority),
    };
    let (user, password) = match userinfo {
        Some(info) => match info.split_once(':') {
            Some((user, password)) => (Some(crate::decode(user)), Some(crate::decode(password))),
            None => (Some(crate::decode(info)), None),
        },
        None => (None, None),
    };
    // An IPv6 address stands in brackets, its port after them.
    let (host, port) = match address.strip_prefix('[').and_then(|a| a.split_once(']')) {
        Some((host, after)) => (host, after.strip_prefix(':')),
        None => match address.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (address, None),
        },
    };
    if host.is_empty() {
        return Err("cannot read the URL: it names no host, as in clickhouse://localhost:8123/default".into());
    }
    let port = match port {
        Some(port) => port.parse::<u16>().map_err(|_| format!("cannot read the URL: {port} is no port"))?,
        None => {
            if tls {
                8443
            } else {
                8123
            }
        }
    };
    // The ports of ClickHouse's native protocol, which this program does not speak.
    if port == 9000 || port == 9440 {
        return Err(format!("port {port} speaks ClickHouse's native protocol, and Reylo reads a ClickHouse server over its HTTP interface: name its HTTP port, 8123, or 8443 with clickhouses://"));
    }
    let database = Some(crate::decode(path.trim_end_matches('/'))).filter(|d| !d.is_empty());
    let mut settings = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = crate::decode(key);
        if key == "database" || OWN_SETTINGS.iter().any(|(own, _)| *own == key) {
            continue;
        }
        settings.push((key, crate::decode(value)));
    }
    Ok(Target { host: host.to_string(), port, tls, user: user.filter(|u| !u.is_empty()), password, database, settings })
}

/// A value of a URL's query string, percent-encoded.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Why a request got no answer of the server's to read.
#[derive(Debug)]
enum Lost {
    /// The server could not be reached, or went away.
    Unreachable(String),
    /// A setting to fix: TLS, or an answer that is no HTTP.
    Error(String),
}

impl Lost {
    fn answer(&self) -> Value {
        match self {
            Lost::Unreachable(message) => json!({ "status": "unreachable", "message": message }),
            Lost::Error(message) => json!({ "status": "error", "message": message }),
        }
    }

    fn message(&self) -> &str {
        match self {
            Lost::Unreachable(message) | Lost::Error(message) => message,
        }
    }
}

/// A server's answer: its status, its exception's code where it sent one, and its body.
struct Response {
    status: u16,
    exception: Option<String>,
    body: Vec<u8>,
}

/// An error the server answered with, as the engine reads one: its message without the code before
/// it and the version after it, its code, and the position a syntax error names.
fn exception(response: &Response) -> DbError {
    let text = String::from_utf8_lossy(&response.body);
    let text = text.trim();
    let code = response.exception.clone().or_else(|| text.strip_prefix("Code: ").map(|rest| rest.chars().take_while(char::is_ascii_digit).collect()).filter(|c: &String| !c.is_empty()));
    let mut message = text;
    if let Some(rest) = message.strip_prefix("Code: ") {
        message = rest.trim_start_matches(|c: char| c.is_ascii_digit()).trim_start_matches('.').trim_start();
    }
    message = message.strip_prefix("DB::Exception: ").unwrap_or(message);
    if let Some(at) = message.rfind(" (version ") {
        message = &message[..at];
    }
    let position = message.find("failed at position ").and_then(|at| message[at + "failed at position ".len()..].chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok());
    let message = if message.is_empty() { format!("the server answered {}", response.status) } else { syntax_error(message).unwrap_or_else(|| message.to_string()) };
    DbError { message, code, position }
}

/// A syntax error without what the server quotes of the text it was given, which holds the
/// engine's own words around the statement and a position counted in them: the token it stopped
/// at, and what it expected there.
fn syntax_error(message: &str) -> Option<String> {
    let rest = message.strip_prefix("Syntax error: failed at position ")?;
    let open = rest.find('(')?;
    let token = &rest[open + 1..open + 1 + rest[open + 1..].find(')')?];
    let expected = rest.find("Expected one of: ").map(|at| rest[at..].trim_end());
    Some(match expected {
        Some(expected) => format!("Syntax error at {token}. {expected}"),
        None => format!("Syntax error at {token}"),
    })
}

/// One server, and the connection kept to it between requests.
pub struct Http {
    target: Target,
    tls: Option<Arc<rustls::ClientConfig>>,
    stream: Option<BufStream<Box<dyn Io>>>,
}

impl Http {
    fn new(target: Target) -> Result<Http, String> {
        let tls = if target.tls { Some(Arc::new(crate::tls::config(crate::tls::Check::Full, None, None)?)) } else { None };
        Ok(Http { target, tls, stream: None })
    }

    async fn connect(&self) -> Result<BufStream<Box<dyn Io>>, Lost> {
        let address = (self.target.host.as_str(), self.target.port);
        let tcp = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(address)).await {
            Err(_) => return Err(Lost::Unreachable("no answer within 10 seconds".into())),
            Ok(Err(e)) => return Err(Lost::Unreachable(e.to_string())),
            Ok(Ok(tcp)) => tcp,
        };
        let _ = tcp.set_nodelay(true);
        let Some(config) = &self.tls else { return Ok(BufStream::new(Box::new(tcp))) };
        let name = rustls::pki_types::ServerName::try_from(self.target.host.clone()).map_err(|e| Lost::Error(format!("{} is no host name a certificate can be checked for: {e}", self.target.host)))?;
        match tokio::time::timeout(CONNECT_TIMEOUT, tokio_rustls::TlsConnector::from(config.clone()).connect(name, tcp)).await {
            Err(_) => Err(Lost::Unreachable("no TLS handshake within 10 seconds".into())),
            // A certificate the check refused, or a server that answered TLS with something else.
            Ok(Err(e)) => Err(Lost::Error(format!("the TLS handshake failed: {e}"))),
            Ok(Ok(stream)) => Ok(BufStream::new(Box::new(stream))),
        }
    }

    /// The request line's path: the database, the settings every request carries, and the URL's own.
    fn path(&self) -> String {
        let mut pairs: Vec<String> = Vec::new();
        if let Some(database) = &self.target.database {
            pairs.push(format!("database={}", encode(database)));
        }
        for (key, value) in OWN_SETTINGS {
            pairs.push(format!("{key}={value}"));
        }
        for (key, value) in &self.target.settings {
            pairs.push(format!("{}={}", encode(key), encode(value)));
        }
        format!("/?{}", pairs.join("&"))
    }

    async fn exchange(stream: &mut BufStream<Box<dyn Io>>, head: &str, body: &[u8]) -> std::io::Result<(Response, bool)> {
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(body).await?;
        stream.flush().await?;
        let bad = |why: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, why.to_string());
        let mut line = String::new();
        if stream.read_line(&mut line).await? == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the server closed the connection"));
        }
        let status = line.split_whitespace().nth(1).and_then(|s| s.parse::<u16>().ok()).filter(|_| line.starts_with("HTTP/")).ok_or_else(|| bad("the server's answer is no HTTP response: the URL names ClickHouse's HTTP port, 8123 by default"))?;
        let (mut length, mut chunked, mut close, mut exception) = (None, false, false, None);
        loop {
            line.clear();
            if stream.read_line(&mut line).await? == 0 {
                return Err(bad("the server's answer ended in its headers"));
            }
            let header = line.trim_end();
            if header.is_empty() {
                break;
            }
            let Some((name, value)) = header.split_once(':') else { continue };
            let value = value.trim();
            match name.to_ascii_lowercase().as_str() {
                "content-length" => length = value.parse::<usize>().ok(),
                "transfer-encoding" => chunked = value.to_ascii_lowercase().contains("chunked"),
                "connection" => close = value.eq_ignore_ascii_case("close"),
                "x-clickhouse-exception-code" => exception = Some(value.to_string()),
                _ => {}
            }
        }
        let mut body = Vec::new();
        if chunked {
            loop {
                line.clear();
                stream.read_line(&mut line).await?;
                let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or_default().trim(), 16).map_err(|_| bad("the server's answer has a chunk of no size"))?;
                if size == 0 {
                    // The trailers, to the blank line that ends them.
                    loop {
                        line.clear();
                        if stream.read_line(&mut line).await? == 0 || line.trim_end().is_empty() {
                            break;
                        }
                    }
                    break;
                }
                let start = body.len();
                body.resize(start + size, 0);
                stream.read_exact(&mut body[start..]).await?;
                line.clear();
                stream.read_line(&mut line).await?;
            }
        } else if let Some(length) = length {
            body.resize(length, 0);
            stream.read_exact(&mut body).await?;
        } else {
            stream.read_to_end(&mut body).await?;
            close = true;
        }
        Ok((Response { status, exception, body }, close))
    }

    /// One statement sent, and the server's answer to it: over the connection kept, opened again
    /// once where the server had closed it.
    async fn send(&mut self, sql: &str) -> Result<Response, Lost> {
        let mut head = format!("POST {} HTTP/1.1\r\nHost: {}:{}\r\nUser-Agent: reylo/{}\r\nContent-Type: text/plain; charset=UTF-8\r\nContent-Length: {}\r\nConnection: keep-alive\r\n", self.path(), self.target.host, self.target.port, env!("CARGO_PKG_VERSION"), sql.len());
        for (name, value) in [("X-ClickHouse-User", &self.target.user), ("X-ClickHouse-Key", &self.target.password)] {
            let Some(value) = value else { continue };
            if value.contains(['\r', '\n']) {
                return Err(Lost::Error("the URL's user or password holds a line break, which no HTTP header carries".into()));
            }
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        for attempt in 0..2 {
            let kept = self.stream.is_some();
            let mut stream = match self.stream.take() {
                Some(stream) => stream,
                None => self.connect().await?,
            };
            match tokio::time::timeout(REQUEST_TIMEOUT, Self::exchange(&mut stream, &head, sql.as_bytes())).await {
                Err(_) => return Err(Lost::Unreachable("no answer within 60 seconds".into())),
                Ok(Ok((response, close))) => {
                    if !close {
                        self.stream = Some(stream);
                    }
                    return Ok(response);
                }
                // A connection the server closed while it sat idle is opened again, once.
                Ok(Err(_)) if kept && attempt == 0 => continue,
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::InvalidData => return Err(Lost::Error(e.to_string())),
                Ok(Err(e)) => return Err(Lost::Unreachable(e.to_string())),
            }
        }
        Err(Lost::Unreachable("the server closed the connection".into()))
    }

    /// A query's rows, each an array of its values, or the error the server answered with.
    async fn rows(&mut self, sql: &str) -> Result<Result<Vec<Vec<Value>>, DbError>, Lost> {
        let response = self.send(sql).await?;
        if response.status != 200 {
            return Ok(Err(exception(&response)));
        }
        // A statement that returns no rows answers with no body.
        if response.body.iter().all(u8::is_ascii_whitespace) {
            return Ok(Ok(Vec::new()));
        }
        let parsed: Value = serde_json::from_slice(&response.body).map_err(|e| Lost::Error(format!("the server's answer is not the JSON it was asked for: {e}")))?;
        let rows = parsed["data"].as_array().map(|rows| rows.iter().filter_map(|row| row.as_array().cloned()).collect()).unwrap_or_default();
        Ok(Ok(rows))
    }

    /// The columns `DESCRIBE` reports for a statement, or the error the server answered with, a
    /// syntax error's position counted in the statement itself.
    async fn describe(&mut self, sql: &str) -> Result<Result<ClickhouseDescribe, DbError>, Lost> {
        match self.rows(&format!("{DESCRIBE}{sql}\n)")).await? {
            Ok(rows) => {
                let columns: Vec<Value> = rows.iter().map(|row| json!({ "name": row.first(), "type": row.get(1) })).collect();
                serde_json::from_value(json!({ "columns": columns })).map(Ok).map_err(|e| Lost::Error(format!("the server's describe does not read: {e}")))
            }
            Err(mut error) => {
                error.position = error.position.and_then(|p| p.checked_sub(DESCRIBE.len()));
                Ok(Err(error))
            }
        }
    }
}

/// The engine's needs, answered over a connection.
struct Answers<'a>(&'a mut Http);

impl AsyncAnswerer<ClickhouseDescribe> for Answers<'_> {
    type Error = HostError;

    fn describe(&mut self, sql: &str) -> impl Future<Output = Result<Result<ClickhouseDescribe, DbError>, HostError>> + Send {
        let http = &mut *self.0;
        async move { http.describe(sql).await.map_err(|lost| HostError(lost.message().to_string())) }
    }

    fn query(&mut self, sql: &str, params: &[Value]) -> impl Future<Output = Result<Result<Vec<Vec<Value>>, DbError>, HostError>> + Send {
        let http = &mut *self.0;
        async move {
            if !params.is_empty() {
                return Err(HostError(format!("a ClickHouse catalog query takes its values in its text, and this one was given {params:?}")));
            }
            http.rows(sql).await.map_err(|lost| HostError(lost.message().to_string()))
        }
    }
}

struct Connection {
    http: Http,
    /// The engine's answers, kept between statements as the loader's session keeps them.
    answers: AnswerCache<ClickhouseDescribe>,
}

impl Connection {
    /// The analysis of one statement over the connection; a failure is the answer the program gives for it.
    async fn analyze(&mut self, sql: &str) -> Result<Analysis, Value> {
        match reylo_engine::replay::run_async::<Engine, _>(sql, &mut Answers(&mut self.http), &mut self.answers).await {
            Ok(analysis) => Ok(analysis),
            // The server went while the engine asked.
            Err(ReplayError::Host(e)) => Err(json!({ "status": "unreachable", "message": e.0.lines().next().unwrap_or_default() })),
            Err(e) => Err(json!({ "status": "error", "message": e.to_string() })),
        }
    }
}

/// A connection to the server a URL names, and the server's `version()`, or the answer the program
/// gives for why there is none.
async fn connect(url: &str) -> Result<(Http, String), Value> {
    let target = parse(url).map_err(|message| json!({ "status": "error", "message": message }))?;
    let mut http = Http::new(target).map_err(|message| json!({ "status": "error", "message": message }))?;
    match http.rows("select version()").await {
        Err(lost) => Err(lost.answer()),
        Ok(Err(e)) if e.code.as_deref().is_some_and(|code| REFUSED_LOGIN.contains(&code)) => Err(json!({ "status": "refused", "message": e.message, "code": e.code })),
        Ok(Err(e)) => Err(json!({ "status": "error", "message": e.message, "code": e.code })),
        Ok(Ok(rows)) => {
            let version = rows.first().and_then(|r| r.first()).and_then(Value::as_str).unwrap_or_default().to_string();
            Ok((http, version))
        }
    }
}

/// The statements of one command line's run, analyzed over one connection, which keeps the engine's
/// answers between them.
pub struct Session {
    connection: Connection,
    /// The server's `version()`, as `26.8.1.1`.
    pub version: String,
}

impl Session {
    pub async fn open(url: &str) -> Result<Session, Failure> {
        let (http, version) = connect(url).await.map_err(|answer| Failure::of(&answer))?;
        Ok(Session { connection: Connection { http, answers: AnswerCache::until_cleared() }, version })
    }

    pub async fn analyze(&mut self, sql: &str) -> Result<Analysis, Failure> {
        self.connection.analyze(sql).await.map_err(|answer| Failure::of(&answer))
    }

    /// A query's rows, each an array of its values, for a host that reads the database itself.
    pub async fn query(&mut self, sql: &str) -> Result<Vec<Vec<Value>>, Failure> {
        match self.connection.http.rows(sql).await {
            Err(lost) => Err(Failure::of(&lost.answer())),
            Ok(Err(e)) => Err(Failure::Error(e.message)),
            Ok(Ok(rows)) => Ok(rows),
        }
    }
}

#[derive(Default)]
pub struct Clickhouse {
    connections: HashMap<u64, Connection>,
    next: u64,
}

fn closed(id: u64) -> Value {
    json!({ "status": "unreachable", "message": format!("connection {id} is closed") })
}

impl Program for Clickhouse {
    fn call(&self, request: &str) -> String {
        reylo_engine_core::api::call::<Engine>(request)
    }

    async fn serve(&mut self, request: &Value) -> Option<Value> {
        let op = request["op"].as_str()?;
        let id = request["conn"].as_u64().unwrap_or(0);
        let sql = request["sql"].as_str().unwrap_or_default();
        Some(match op {
            "open" => match connect(request["url"].as_str().unwrap_or_default()).await {
                Ok((http, version)) => {
                    self.next += 1;
                    self.connections.insert(self.next, Connection { http, answers: AnswerCache::until_cleared() });
                    json!({ "status": "opened", "conn": self.next, "version": version })
                }
                Err(failure) => failure,
            },
            "run" => match self.connections.get_mut(&id) {
                None => closed(id),
                Some(c) => match c.analyze(sql).await {
                    Ok(analysis) => json!({ "status": "analysis", "analysis": analysis }),
                    Err(failure) => failure,
                },
            },
            // A query the host runs itself (the schema's fingerprint), which takes its values in its text.
            "query" => match self.connections.get_mut(&id) {
                None => closed(id),
                Some(c) => {
                    if request["params"].as_array().is_some_and(|params| !params.is_empty()) {
                        return Some(json!({ "status": "error", "message": "a ClickHouse query of the host's takes its values in its text" }));
                    }
                    match c.http.rows(sql).await {
                        Ok(Ok(rows)) => json!({ "status": "rows", "rows": rows }),
                        Ok(Err(e)) if e.code.as_deref().is_some_and(|code| REFUSED_LOGIN.contains(&code)) => json!({ "status": "refused", "message": e.message, "code": e.code }),
                        Ok(Err(e)) => json!({ "status": "error", "message": e.message, "code": e.code }),
                        Err(lost) => lost.answer(),
                    }
                }
            },
            // The schema changed: what the engine was told about the old one is stale.
            "forget" => match self.connections.get_mut(&id) {
                None => closed(id),
                Some(c) => {
                    c.answers.clear();
                    json!({ "status": "ok" })
                }
            },
            "close" => {
                self.connections.remove(&id);
                json!({ "status": "ok" })
            }
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_names_the_server_its_database_and_its_settings() {
        let target = parse("clickhouse://app:s%40cret@db.example:18123/analytics?join_use_nulls=1&default_format=CSV").unwrap();
        assert_eq!(target, Target { host: "db.example".into(), port: 18123, tls: false, user: Some("app".into()), password: Some("s@cret".into()), database: Some("analytics".into()), settings: vec![("join_use_nulls".into(), "1".into())] });
        let cloud = parse("clickhouses://default:pw@abc.eu-west-1.aws.clickhouse.cloud").unwrap();
        assert_eq!((cloud.host.as_str(), cloud.port, cloud.tls, cloud.database), ("abc.eu-west-1.aws.clickhouse.cloud", 8443, true, None));
        assert_eq!(parse("clickhouse://localhost").map(|t| (t.port, t.user)), Ok((8123, None)));
        assert_eq!(parse("ClickHouse+HTTPS://[::1]:8443/app").map(|t| (t.host, t.port, t.tls)), Ok(("::1".to_string(), 8443, true)));
        assert!(parse("clickhouse://localhost:9000/app").unwrap_err().contains("native protocol"));
        assert!(parse("http://localhost:8123").unwrap_err().contains("clickhouse://"));
        assert!(parse("clickhouse:///app").unwrap_err().contains("names no host"));
    }

    #[test]
    fn a_request_carries_the_database_and_the_settings() {
        let http = Http::new(parse("clickhouse://localhost/my%20db?join_use_nulls=1").unwrap()).unwrap();
        assert_eq!(http.path(), "/?database=my%20db&default_format=JSONCompact&wait_end_of_query=1&join_use_nulls=1");
    }

    #[test]
    fn the_server_s_exception_is_read_for_its_message_code_and_position() {
        let of = |body: &str, code: Option<&str>| exception(&Response { status: 400, exception: code.map(String::from), body: body.as_bytes().to_vec() });
        let e = of("Code: 47. DB::Exception: Unknown expression identifier `nope` in scope SELECT nope FROM events. Maybe you meant: ['name']. (UNKNOWN_IDENTIFIER) (version 26.8.1.1 (official build))\n", Some("47"));
        assert_eq!((e.message.as_str(), e.code.as_deref(), e.position), ("Unknown expression identifier `nope` in scope SELECT nope FROM events. Maybe you meant: ['name']. (UNKNOWN_IDENTIFIER)", Some("47"), None));
        let e = of("Code: 62. DB::Exception: Syntax error: failed at position 18 (into): into t. Expected one of: FROM. (SYNTAX_ERROR) (version 26.8.1.1 (official build))", None);
        assert_eq!((e.message.as_str(), e.code.as_deref(), e.position), ("Syntax error at into. Expected one of: FROM. (SYNTAX_ERROR)", Some("62"), Some(18)));
        assert_eq!(of("", None).message, "the server answered 400");
    }
}
