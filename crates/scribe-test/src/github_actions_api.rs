//! Deterministic loopback GitHub Actions REST fixture for Docker E2E tests.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

const MAX_REQUEST_BYTES: usize = 16 * 1024;

#[derive(Deserialize)]
struct Scenario {
    runs: Vec<RunSnapshot>,
    #[serde(default)]
    jobs: HashMap<String, Vec<JobSnapshot>>,
}

#[derive(Clone, Deserialize)]
struct RunSnapshot {
    workflow_runs: Vec<Value>,
    #[serde(flatten)]
    fields: Map<String, Value>,
}

#[derive(Clone, Deserialize)]
struct JobSnapshot {
    jobs: Vec<Value>,
    #[serde(flatten)]
    fields: Map<String, Value>,
}

struct Fixture {
    scenario: Scenario,
    run_index: usize,
    job_indexes: HashMap<String, usize>,
    request_log: File,
}

struct Request {
    method: String,
    target: String,
    if_none_match: Option<String>,
}

struct Response {
    status: u16,
    etag: Option<String>,
    body: Option<Value>,
}

#[derive(Serialize)]
struct RequestLog<'a> {
    method: &'a str,
    target: &'a str,
    if_none_match: Option<&'a str>,
    status: u16,
}

pub fn run(scenario_path: &Path, request_log_path: &Path, port: u16) -> Result<(), String> {
    let raw = std::fs::read(scenario_path)
        .map_err(|error| format!("read {}: {error}", scenario_path.display()))?;
    let scenario: Scenario = serde_json::from_slice(&raw)
        .map_err(|error| format!("decode {}: {error}", scenario_path.display()))?;
    validate_scenario(&scenario)?;
    let request_log = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(request_log_path)
        .map_err(|error| format!("open {}: {error}", request_log_path.display()))?;
    let runtime = tokio::runtime::Runtime::new().map_err(|error| error.to_string())?;
    runtime.block_on(serve(scenario, request_log, port))
}

fn validate_scenario(scenario: &Scenario) -> Result<(), String> {
    if scenario.runs.is_empty() {
        return Err("scenario.runs must contain at least one snapshot".to_owned());
    }
    for (run_id, snapshots) in &scenario.jobs {
        if snapshots.is_empty() {
            return Err(format!("scenario.jobs.{run_id} must contain at least one snapshot"));
        }
    }
    Ok(())
}

async fn serve(scenario: Scenario, request_log: File, port: u16) -> Result<(), String> {
    let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    let listener =
        TcpListener::bind(address).await.map_err(|error| format!("bind {address}: {error}"))?;
    writeln!(std::io::stderr().lock(), "github-actions-api: listening on {address}")
        .map_err(|error| error.to_string())?;

    let mut fixture = Fixture { scenario, run_index: 0, job_indexes: HashMap::new(), request_log };
    loop {
        let (mut stream, _) = listener.accept().await.map_err(|error| error.to_string())?;
        let request = match read_request(&mut stream).await {
            Ok(request) => request,
            Err(error) => {
                write_response(&mut stream, Response::error(400, &error)).await?;
                continue;
            }
        };
        let response = fixture.respond(&request);
        fixture.log(&request, response.status)?;
        write_response(&mut stream, response).await?;
    }
}

impl Fixture {
    fn respond(&mut self, request: &Request) -> Response {
        if request.method != "GET" {
            return Response::error(405, "method not allowed");
        }

        let (path, query) = request.target.split_once('?').unwrap_or((&request.target, ""));
        let parts: Vec<_> = path.trim_start_matches('/').split('/').collect();
        match parts.as_slice() {
            ["repos", _, _, "actions", "runs"] => {
                let Some(head_sha) = query_value(query, "head_sha") else {
                    return Response::error(422, "head_sha is required");
                };
                self.runs_response(head_sha, request.if_none_match.as_deref())
            }
            ["repos", _, _, "actions", "runs", run_id, "jobs"] => {
                self.jobs_response(run_id, request.if_none_match.as_deref())
            }
            _ => Response::error(404, "no such route"),
        }
    }

    fn runs_response(&mut self, head_sha: &str, if_none_match: Option<&str>) -> Response {
        let index = self.run_index;
        let etag = format!("\"runs-{index}\"");
        if if_none_match == Some(etag.as_str()) {
            return Response::not_modified(etag);
        }

        let Some(snapshot) = self.scenario.runs.get(index).cloned() else {
            return Response::error(500, "run progression is exhausted");
        };
        let runs = snapshot
            .workflow_runs
            .iter()
            .filter(|run| run.get("head_sha").and_then(Value::as_str) == Some(head_sha))
            .cloned()
            .collect::<Vec<_>>();
        let mut payload = snapshot.fields;
        payload.insert("total_count".to_owned(), runs.len().into());
        payload.insert("workflow_runs".to_owned(), runs.into());
        self.run_index = advance(index, self.scenario.runs.len());
        Response::json(Value::Object(payload), etag)
    }

    fn jobs_response(&mut self, run_id: &str, if_none_match: Option<&str>) -> Response {
        let Some(snapshots) = self.scenario.jobs.get(run_id) else {
            return Response::error(404, "unknown workflow run");
        };
        let index = *self.job_indexes.get(run_id).unwrap_or(&0);
        let etag = format!("\"jobs-{run_id}-{index}\"");
        if if_none_match == Some(etag.as_str()) {
            return Response::not_modified(etag);
        }

        let Some(snapshot) = snapshots.get(index).cloned() else {
            return Response::error(500, "job progression is exhausted");
        };
        let mut payload = snapshot.fields;
        payload.insert("total_count".to_owned(), snapshot.jobs.len().into());
        payload.insert("jobs".to_owned(), snapshot.jobs.into());
        self.job_indexes.insert(run_id.to_owned(), advance(index, snapshots.len()));
        Response::json(Value::Object(payload), etag)
    }

    fn log(&mut self, request: &Request, status: u16) -> Result<(), String> {
        serde_json::to_writer(
            &mut self.request_log,
            &RequestLog {
                method: &request.method,
                target: &request.target,
                if_none_match: request.if_none_match.as_deref(),
                status,
            },
        )
        .map_err(|error| format!("write request log: {error}"))?;
        self.request_log
            .write_all(b"\n")
            .and_then(|()| self.request_log.flush())
            .map_err(|error| format!("flush request log: {error}"))
    }
}

impl Response {
    fn json(body: Value, etag: String) -> Self {
        Self { status: 200, etag: Some(etag), body: Some(body) }
    }

    fn not_modified(etag: String) -> Self {
        Self { status: 304, etag: Some(etag), body: None }
    }

    fn error(status: u16, message: &str) -> Self {
        Self { status, etag: None, body: Some(json!({ "message": message })) }
    }
}

fn advance(index: usize, len: usize) -> usize {
    (index + 1).min(len - 1)
}

fn query_value<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|part| {
        let (name, value) = part.split_once('=')?;
        (name == key).then_some(value)
    })
}

async fn read_request(stream: &mut TcpStream) -> Result<Request, String> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 1024];
    while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream.read(&mut chunk).await.map_err(|error| error.to_string())?;
        if read == 0 {
            return Err("request ended before headers".to_owned());
        }
        let Some(read_bytes) = chunk.get(..read) else {
            return Err("socket read exceeded its buffer".to_owned());
        };
        bytes.extend_from_slice(read_bytes);
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err("request headers exceed 16 KiB".to_owned());
        }
    }

    let headers = std::str::from_utf8(&bytes).map_err(|_| "request headers are not UTF-8")?;
    let mut lines = headers.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split_whitespace();
    let method = request_line.next().ok_or("request method is missing")?.to_owned();
    let target = request_line.next().ok_or("request target is missing")?.to_owned();
    let version = request_line.next().ok_or("HTTP version is missing")?;
    if request_line.next().is_some() || !matches!(version, "HTTP/1.0" | "HTTP/1.1") {
        return Err("invalid HTTP request line".to_owned());
    }
    let if_none_match = lines.find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("if-none-match").then(|| value.trim().to_owned())
    });
    Ok(Request { method, target, if_none_match })
}

async fn write_response(stream: &mut TcpStream, response: Response) -> Result<(), String> {
    let body = response
        .body
        .map(|body| serde_json::to_vec(&body))
        .transpose()
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    let reason = match response.status {
        200 => "OK",
        304 => "Not Modified",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        422 => "Unprocessable Content",
        500 => "Internal Server Error",
        _ => "Error",
    };
    let etag = response.etag.map(|etag| format!("ETag: {etag}\r\n")).unwrap_or_default();
    let headers = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n",
        response.status,
        reason,
        body.len(),
        etag,
    );
    stream.write_all(headers.as_bytes()).await.map_err(|error| error.to_string())?;
    stream.write_all(&body).await.map_err(|error| error.to_string())
}
