//! HTTP JSON daemon for the independent file-search Rust workspace.
//!
//! The daemon is intentionally framework-free and reuses the same core and
//! SQLite crates as the CLI. It is a service adapter, not the search API
//! boundary; future desktop, REST, or native hosts can reuse the same request
//! handlers or replace the transport.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use kfs_core::{BackendError, MetadataIndex, SearchConfig, SearchQuery, SearchRoot};
use kfs_index_sqlite::{match_names, SqliteIndex};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone)]
pub struct DaemonConfig {
  pub db_path: PathBuf,
  pub addr: String,
  pub duration: Option<Duration>,
  pub max_requests: Option<usize>,
  pub allowed_roots: Vec<SearchRoot>,
  pub allow_non_loopback: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonRunStats {
  pub addr: String,
  pub requests: usize,
  pub elapsed_ms: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HttpRequest {
  method: String,
  path: String,
  body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HttpResponse {
  status: u16,
  body: String,
}

#[derive(Debug, Deserialize)]
struct RootsRequest {
  roots: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SearchRequest {
  query: String,
  roots: Vec<String>,
  limit: Option<usize>,
}

#[derive(Debug, Serialize)]
struct SearchResultDto {
  path: String,
  score: i32,
  provider: String,
  matches: Vec<String>,
}

pub fn serve(config: DaemonConfig) -> Result<DaemonRunStats, BackendError> {
  validate_bind_addr(&config)?;
  let listener = TcpListener::bind(&config.addr).map_err(BackendError::from)?;
  listener.set_nonblocking(true).map_err(BackendError::from)?;
  let addr = listener
    .local_addr()
    .map_err(BackendError::from)?
    .to_string();
  let started = Instant::now();
  let mut index =
    SqliteIndex::open(&config.db_path).map_err(|err| BackendError::new(err.to_string()))?;
  let mut requests = 0;

  loop {
    if config
      .duration
      .is_some_and(|duration| started.elapsed() >= duration)
    {
      break;
    }
    if config
      .max_requests
      .is_some_and(|max_requests| requests >= max_requests)
    {
      break;
    }

    match listener.accept() {
      Ok((stream, _addr)) => {
        handle_connection(&mut index, stream, &config.allowed_roots)?;
        requests += 1;
      }
      Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
        thread::sleep(Duration::from_millis(10));
      }
      Err(err) => return Err(BackendError::from(err)),
    }
  }

  Ok(DaemonRunStats {
    addr,
    requests,
    elapsed_ms: started.elapsed().as_millis(),
  })
}

fn handle_connection(
  index: &mut SqliteIndex,
  mut stream: TcpStream,
  allowed_roots: &[SearchRoot],
) -> Result<(), BackendError> {
  let request = read_http_request(&mut stream)?;
  let response = match request {
    Some(request) => handle_request(index, request, allowed_roots),
    None => json_response(400, json!({ "error": "empty request" })),
  };
  write_http_response(&mut stream, &response)
}

fn handle_request(
  index: &mut SqliteIndex,
  request: HttpRequest,
  allowed_roots: &[SearchRoot],
) -> HttpResponse {
  match (request.method.as_str(), request.path.as_str()) {
    ("GET", "/health") => json_response(200, json!({ "ok": true })),
    ("GET", "/status") => match index.status() {
      Ok(status) => json_response(
        200,
        json!({
            "roots": status.into_iter().map(|root| {
                json!({
                    "path": root.path.to_string_lossy(),
                    "entry_count": root.entry_count,
                    "generation": root.generation,
                    "dirty": root.dirty,
                    "last_full_scan_at": root.last_full_scan_at,
                    "last_incremental_at": root.last_incremental_at
                })
            }).collect::<Vec<_>>()
        }),
      ),
      Err(err) => json_response(500, json!({ "error": err.to_string() })),
    },
    ("POST", "/search") => match parse_json::<SearchRequest>(&request.body) {
      Ok(search) => {
        let config = match config_from_requested_roots(&search.roots, allowed_roots) {
          Ok(config) => config,
          Err(err) => return json_response(400, json!({ "error": err.message })),
        };
        let mut query = SearchQuery::new(search.query);
        if let Some(limit) = search.limit {
          query.limit = limit;
        }
        match index.search_with_metrics(&config, &query) {
          Ok(outcome) => json_response(
            200,
            json!({
                "candidate_count": outcome.candidate_count,
                "results": outcome.results.into_iter().map(SearchResultDto::from).collect::<Vec<_>>()
            }),
          ),
          Err(err) => json_response(500, json!({ "error": err.to_string() })),
        }
      }
      Err(err) => json_response(400, json!({ "error": err.message })),
    },
    ("POST", "/index/rebuild") => match parse_roots_request(&request.body, allowed_roots) {
      Ok(config) => match index.rebuild_index(&config) {
        Ok(stats) => json_response(
          200,
          json!({
              "stats": {
                  "roots": stats.roots,
                  "entries": stats.entries,
                  "skipped": stats.skipped,
                  "errors": stats.errors
              }
          }),
        ),
        Err(err) => json_response(500, json!({ "error": err.to_string() })),
      },
      Err(err) => json_response(400, json!({ "error": err.message })),
    },
    ("POST", "/index/refresh") => match parse_roots_request(&request.body, allowed_roots) {
      Ok(config) => match index.refresh_index(&config) {
        Ok(stats) => json_response(
          200,
          json!({
              "stats": {
                  "roots": stats.roots,
                  "inserted": stats.inserted,
                  "updated": stats.updated,
                  "deleted": stats.deleted,
                  "unchanged": stats.unchanged,
                  "skipped": stats.skipped,
                  "errors": stats.errors
              }
          }),
        ),
        Err(err) => json_response(500, json!({ "error": err.to_string() })),
      },
      Err(err) => json_response(400, json!({ "error": err.message })),
    },
    _ => json_response(404, json!({ "error": "not found" })),
  }
}

fn read_http_request(stream: &mut TcpStream) -> Result<Option<HttpRequest>, BackendError> {
  stream
    .set_read_timeout(Some(Duration::from_secs(2)))
    .map_err(BackendError::from)?;
  let mut buffer = Vec::new();
  let mut chunk = [0_u8; 4096];
  loop {
    match stream.read(&mut chunk) {
      Ok(0) => break,
      Ok(count) => {
        buffer.extend_from_slice(&chunk[..count]);
        if request_is_complete(&buffer) {
          break;
        }
      }
      Err(err)
        if matches!(
          err.kind(),
          std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ) =>
      {
        break;
      }
      Err(err) => return Err(BackendError::from(err)),
    }
  }
  if buffer.is_empty() {
    return Ok(None);
  }
  parse_http_request(&buffer).map(Some)
}

fn parse_http_request(buffer: &[u8]) -> Result<HttpRequest, BackendError> {
  let header_end =
    find_header_end(buffer).ok_or_else(|| BackendError::new("request headers were incomplete"))?;
  let headers = String::from_utf8_lossy(&buffer[..header_end]);
  let mut lines = headers.lines();
  let request_line = lines
    .next()
    .ok_or_else(|| BackendError::new("missing request line"))?;
  let mut parts = request_line.split_whitespace();
  let method = parts
    .next()
    .ok_or_else(|| BackendError::new("missing method"))?
    .to_string();
  let path = parts
    .next()
    .ok_or_else(|| BackendError::new("missing path"))?
    .to_string();
  Ok(HttpRequest {
    method,
    path,
    body: buffer[header_end + 4..].to_vec(),
  })
}

fn request_is_complete(buffer: &[u8]) -> bool {
  let Some(header_end) = find_header_end(buffer) else {
    return false;
  };
  let content_length = content_length(&buffer[..header_end]).unwrap_or(0);
  buffer.len() >= header_end + 4 + content_length
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
  buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn content_length(headers: &[u8]) -> Option<usize> {
  let headers = String::from_utf8_lossy(headers);
  headers.lines().find_map(|line| {
    let (name, value) = line.split_once(':')?;
    if name.eq_ignore_ascii_case("content-length") {
      value.trim().parse::<usize>().ok()
    } else {
      None
    }
  })
}

fn write_http_response(
  stream: &mut TcpStream,
  response: &HttpResponse,
) -> Result<(), BackendError> {
  let reason = match response.status {
    200 => "OK",
    400 => "Bad Request",
    404 => "Not Found",
    500 => "Internal Server Error",
    _ => "OK",
  };
  let text = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response.status,
        reason,
        response.body.len(),
        response.body
    );
  stream
    .write_all(text.as_bytes())
    .map_err(BackendError::from)
}

fn parse_roots_request(
  body: &[u8],
  allowed_roots: &[SearchRoot],
) -> Result<SearchConfig, BackendError> {
  let request = parse_json::<RootsRequest>(body)?;
  config_from_requested_roots(&request.roots, allowed_roots)
}

fn parse_json<T>(body: &[u8]) -> Result<T, BackendError>
where
  T: for<'de> Deserialize<'de>,
{
  serde_json::from_slice(body).map_err(|err| BackendError::new(err.to_string()))
}

fn config_from_requested_roots(
  roots: &[String],
  allowed_roots: &[SearchRoot],
) -> Result<SearchConfig, BackendError> {
  if roots.is_empty() {
    return Err(BackendError::new("at least one root is required"));
  }
  let allowed = allowed_roots
    .iter()
    .filter(|root| root.enabled)
    .collect::<Vec<_>>();
  if allowed.is_empty() {
    return Err(BackendError::new("daemon has no allowed roots"));
  }

  let mut requested_roots = Vec::with_capacity(roots.len());
  for root in roots {
    let requested = SearchRoot::new(root);
    let Some(allowed_root) = allowed
      .iter()
      .copied()
      .find(|allowed_root| requested.path.starts_with(&allowed_root.path))
    else {
      return Err(BackendError::new(format!(
        "root is outside daemon allowed roots: {}",
        requested.path.to_string_lossy()
      )));
    };
    requested_roots.push(
      requested
        .include_hidden(allowed_root.include_hidden)
        .include_ignored(allowed_root.include_ignored)
        .with_priority(allowed_root.priority),
    );
  }

  Ok(SearchConfig {
    roots: requested_roots,
  })
}

pub fn validate_bind_addr(config: &DaemonConfig) -> Result<(), BackendError> {
  if config.allow_non_loopback || addr_is_loopback(&config.addr) {
    return Ok(());
  }
  Err(BackendError::new(
    "non-loopback daemon bind requires --allow-non-loopback",
  ))
}

fn addr_is_loopback(addr: &str) -> bool {
  if let Ok(socket_addr) = addr.parse::<SocketAddr>() {
    return socket_addr.ip().is_loopback();
  }
  addr
    .split_once(':')
    .is_some_and(|(host, _port)| host == "localhost")
}

fn json_response(status: u16, value: serde_json::Value) -> HttpResponse {
  HttpResponse {
    status,
    body: value.to_string(),
  }
}

impl From<kfs_core::SearchResult> for SearchResultDto {
  fn from(result: kfs_core::SearchResult) -> Self {
    Self {
      path: result.path.to_string_lossy().into_owned(),
      score: result.score,
      provider: result.provider,
      matches: match_names(&result.matches)
        .into_iter()
        .map(ToOwned::to_owned)
        .collect(),
    }
  }
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::time::{SystemTime, UNIX_EPOCH};

  use super::*;

  #[test]
  fn parse_http_request_reads_method_path_and_body() {
    let request =
      parse_http_request(b"POST /search HTTP/1.1\r\nContent-Length: 13\r\n\r\n{\"query\":\"x\"}")
        .unwrap();

    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/search");
    assert_eq!(request.body, br#"{"query":"x"}"#);
  }

  #[test]
  fn health_endpoint_returns_ok_json() {
    let mut index = SqliteIndex::open_memory().unwrap();

    let response = handle_request(
      &mut index,
      HttpRequest {
        method: "GET".to_string(),
        path: "/health".to_string(),
        body: Vec::new(),
      },
      &[],
    );

    assert_eq!(response.status, 200);
    assert!(response.body.contains("\"ok\":true"));
  }

  #[test]
  fn search_endpoint_returns_indexed_results() {
    let root = temp_dir("search");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("daemon-target.md"), "daemon\n").unwrap();
    let mut index = SqliteIndex::open_memory().unwrap();
    let allowed_roots = vec![SearchRoot::new(&root)];
    let rebuild = handle_request(
      &mut index,
      HttpRequest {
        method: "POST".to_string(),
        path: "/index/rebuild".to_string(),
        body: format!(r#"{{"roots":["{}"]}}"#, root.to_string_lossy()).into_bytes(),
      },
      &allowed_roots,
    );
    let response = handle_request(
      &mut index,
      HttpRequest {
        method: "POST".to_string(),
        path: "/search".to_string(),
        body: format!(
          r#"{{"query":"daemon target","roots":["{}"],"limit":5}}"#,
          root.to_string_lossy()
        )
        .into_bytes(),
      },
      &allowed_roots,
    );
    let _ = fs::remove_dir_all(&root);

    assert_eq!(rebuild.status, 200);
    assert_eq!(response.status, 200);
    assert!(response.body.contains("daemon-target.md"));
    assert!(response.body.contains("candidate_count"));
  }

  #[test]
  fn rejects_roots_outside_daemon_allowlist() {
    let allowed = temp_dir("allowed");
    let denied = temp_dir("denied");
    fs::create_dir_all(&allowed).unwrap();
    fs::create_dir_all(&denied).unwrap();
    let allowed_roots = vec![SearchRoot::new(&allowed)];

    let err = config_from_requested_roots(&[denied.to_string_lossy().into_owned()], &allowed_roots)
      .unwrap_err();

    let _ = fs::remove_dir_all(&allowed);
    let _ = fs::remove_dir_all(&denied);

    assert!(err.message.contains("outside daemon allowed roots"));
  }

  #[test]
  fn daemon_rejects_non_loopback_bind_without_opt_in() {
    let config = DaemonConfig {
      db_path: PathBuf::from("/tmp/kfs-review-daemon.sqlite"),
      addr: "0.0.0.0:0".to_string(),
      duration: Some(Duration::from_millis(1)),
      max_requests: Some(0),
      allowed_roots: Vec::new(),
      allow_non_loopback: false,
    };

    let err = validate_bind_addr(&config).unwrap_err();

    assert!(err.message.contains("non-loopback"));
  }

  fn temp_dir(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .unwrap()
      .as_nanos();
    std::env::temp_dir().join(format!("kfs-daemon-{name}-{nonce}"))
  }
}
