use std::fmt::Write as _;
use std::future;
use std::hint::black_box;
use std::io::{self, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use alacritty_terminal::Term;
use alacritty_terminal::grid::Dimensions;
use rmp_serde::to_vec_named;
use scribe_common::agent::{
    AgentError, AgentPayload, AgentPolicyMode, AgentRequest, AgentResponse, AgentSession,
    AgentWindow, AgentWorkspace, AgentWorldSnapshot,
};
use scribe_common::ai_state::{AiProvider, AiState};
use scribe_common::config::{AGENT_MAX_RESPONSE_BYTES_CEILING, AgentApiConfig, SharingMode};
use scribe_common::ids::{SessionId, WindowId, WorkspaceId};
use scribe_common::protocol::ServerMessage;
use scribe_pty::{async_fd::AsyncPtyFd, event_listener::ScribeEventListener};
use scribe_server::agent_api::{AgentApiState, AgentSessionTarget, DispatchSources, dispatch};
use scribe_server::session_manager::build_term_config;
use tokio::io::split;
use tokio::runtime::Runtime;
use tokio::sync::{Mutex, mpsc};
use vte::ansi::Processor as AnsiProcessor;

const WARMUP_ITERATIONS: usize = 20;
const MEASURED_ITERATIONS: usize = 200;
const WORLD_BUDGET: Duration = Duration::from_millis(50);
const VIEWPORT_BUDGET: Duration = Duration::from_millis(100);
const SCROLLBACK_BUDGET: Duration = Duration::from_millis(250);
const VIEWPORT_ROWS: usize = 50;
const SCROLLBACK_LINES: u32 = 1_000;

struct TestDims {
    cols: usize,
    rows: usize,
}

impl Dimensions for TestDims {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

struct Measurement {
    name: &'static str,
    p95: Duration,
    budget: Option<Duration>,
    max_serialized_bytes: usize,
}

struct BenchmarkContext {
    runtime: Runtime,
    world: AgentWorldSnapshot,
    viewport_term: Arc<Mutex<Term<ScribeEventListener>>>,
    scrollback_term: Arc<Mutex<Term<ScribeEventListener>>>,
    ceiling_term: Arc<Mutex<Term<ScribeEventListener>>>,
    allowed_state: AgentApiState,
    screen_request: AgentRequest,
    denied_state: AgentApiState,
    denied_request: AgentRequest,
    deny_touches: Arc<AtomicUsize>,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let mut stderr = io::stderr().lock();
            drop(writeln!(stderr, "{error}"));
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let context = benchmark_context()?;
    let measurements = run_measurements(&context)?;
    report(&measurements)?;
    validate(&context, &measurements)
}

fn benchmark_context() -> Result<BenchmarkContext, String> {
    let scrollback = usize::try_from(SCROLLBACK_LINES).map_err(|error| error.to_string())?;
    Ok(BenchmarkContext {
        runtime: Runtime::new().map_err(|error| format!("failed to create runtime: {error}"))?,
        world: world_fixture()?,
        viewport_term: term_fixture(0, VIEWPORT_ROWS, 200, 96)?,
        scrollback_term: term_fixture(scrollback, VIEWPORT_ROWS, 200, 96)?,
        ceiling_term: term_fixture(scrollback, VIEWPORT_ROWS, 300, 299)?,
        allowed_state: AgentApiState::new(AgentApiConfig {
            read_content: AgentPolicyMode::Allow,
            ..AgentApiConfig::default()
        }),
        screen_request: AgentRequest::ReadScreen {
            request_id: 1,
            agent_label: "agent-api-bench".into(),
            origin_session_id: None,
            session_id: fixed_id::<SessionId>(1)?,
            scrollback_lines: None,
        },
        denied_state: AgentApiState::default(),
        denied_request: AgentRequest::ReadScreen {
            request_id: 2,
            agent_label: "agent-api-bench".into(),
            origin_session_id: None,
            session_id: fixed_id::<SessionId>(2)?,
            scrollback_lines: Some(SCROLLBACK_LINES),
        },
        deny_touches: Arc::new(AtomicUsize::new(0)),
    })
}

fn run_measurements(context: &BenchmarkContext) -> Result<[Measurement; 5], String> {
    // Live world aggregation lands in scribe-8uuf.9. Until then this measures
    // the stable response DTO at a representative server-wide cardinality.
    let world = measure("world", Some(WORLD_BUDGET), || {
        serialize_response(&AgentResponse {
            request_id: 1,
            result: Ok(AgentPayload::World { snapshot: context.world.clone() }),
        })
    })?;
    let viewport = measure("viewport", Some(VIEWPORT_BUDGET), || {
        read_screen_once(
            &context.runtime,
            &context.allowed_state,
            &context.screen_request,
            Arc::clone(&context.viewport_term),
            0,
        )
    })?;
    let scrollback = measure("scrollback_1000", Some(SCROLLBACK_BUDGET), || {
        read_screen_once(
            &context.runtime,
            &context.allowed_state,
            &context.screen_request,
            Arc::clone(&context.scrollback_term),
            SCROLLBACK_LINES,
        )
    })?;
    let ceiling = measure("response_ceiling", None, || {
        read_screen_once(
            &context.runtime,
            &context.allowed_state,
            &context.screen_request,
            Arc::clone(&context.ceiling_term),
            SCROLLBACK_LINES,
        )
    })?;
    let deny = measure("default_deny_no_touch", None, || {
        denied_read_once(
            &context.runtime,
            &context.denied_state,
            &context.denied_request,
            Arc::clone(&context.deny_touches),
        )
    })?;
    Ok([world, viewport, scrollback, ceiling, deny])
}

fn validate(context: &BenchmarkContext, measurements: &[Measurement]) -> Result<(), String> {
    if context.deny_touches.load(Ordering::Relaxed) != 0 {
        return Err("default-Deny request touched Term lookup".into());
    }
    let ceiling =
        usize::try_from(AGENT_MAX_RESPONSE_BYTES_CEILING).map_err(|error| error.to_string())?;
    for measurement in measurements {
        if let Some(budget) = measurement.budget
            && measurement.p95 > budget
        {
            return Err(format!(
                "{} p95 {:.3} ms exceeds {} ms budget",
                measurement.name,
                duration_ms(measurement.p95),
                budget.as_millis()
            ));
        }
        if measurement.max_serialized_bytes > ceiling {
            return Err(format!(
                "cargo bench -p scribe-server --bench agent_api: serialized response exceeds {} bytes ({}: {} bytes)",
                AGENT_MAX_RESPONSE_BYTES_CEILING,
                measurement.name,
                measurement.max_serialized_bytes
            ));
        }
    }
    Ok(())
}

fn measure(
    name: &'static str,
    budget: Option<Duration>,
    mut operation: impl FnMut() -> Result<usize, String>,
) -> Result<Measurement, String> {
    for _ in 0..WARMUP_ITERATIONS {
        black_box(operation()?);
    }

    let mut durations = Vec::with_capacity(MEASURED_ITERATIONS);
    let mut max_serialized_bytes = 0;
    for _ in 0..MEASURED_ITERATIONS {
        let started = Instant::now();
        let serialized_bytes = black_box(operation()?);
        durations.push(started.elapsed());
        max_serialized_bytes = max_serialized_bytes.max(serialized_bytes);
    }
    durations.sort_unstable();
    let rank = (MEASURED_ITERATIONS * 95).div_ceil(100).saturating_sub(1);
    let p95 =
        durations.get(rank).copied().ok_or_else(|| format!("{name} produced no measurements"))?;
    Ok(Measurement { name, p95, budget, max_serialized_bytes })
}

fn read_screen_once(
    runtime: &Runtime,
    state: &AgentApiState,
    request: &AgentRequest,
    term: Arc<Mutex<Term<ScribeEventListener>>>,
    scrollback_lines: u32,
) -> Result<usize, String> {
    let request = match request {
        AgentRequest::ReadScreen {
            request_id, agent_label, origin_session_id, session_id, ..
        } => AgentRequest::ReadScreen {
            request_id: *request_id,
            agent_label: agent_label.clone(),
            origin_session_id: *origin_session_id,
            session_id: *session_id,
            scrollback_lines: Some(scrollback_lines),
        },
        _ => return Err("screen benchmark received a non-screen request".into()),
    };
    let dispatch_result = runtime.block_on(async {
        let target = benchmark_target(term)?;
        Ok::<_, String>(
            dispatch(
                state,
                1,
                &request,
                DispatchSources {
                    capture_world: || async { future::pending().await },
                    lookup_session: move |_| async move { Some(target) },
                    run_action: |_, _| async { future::pending().await },
                },
                None::<fn(ServerMessage) -> std::future::Ready<()>>,
            )
            .await,
        )
    })?;
    let serialized = serialize_response(dispatch_result.response())?;
    drop(dispatch_result);
    Ok(serialized)
}

fn denied_read_once(
    runtime: &Runtime,
    state: &AgentApiState,
    request: &AgentRequest,
    touches: Arc<AtomicUsize>,
) -> Result<usize, String> {
    let dispatch_result = runtime.block_on(dispatch(
        state,
        1,
        request,
        DispatchSources {
            capture_world: || async { future::pending().await },
            lookup_session: move |_| {
                touches.fetch_add(1, Ordering::Relaxed);
                future::ready(None)
            },
            run_action: |_, _| async { future::pending().await },
        },
        None::<fn(ServerMessage) -> std::future::Ready<()>>,
    ));
    if !matches!(dispatch_result.response().result, Err(AgentError::Denied { .. })) {
        return Err("default-Deny request did not return AgentError::Denied".into());
    }
    serialize_response(dispatch_result.response())
}

fn serialize_response(response: &AgentResponse) -> Result<usize, String> {
    to_vec_named(&ServerMessage::AgentResponse(response.clone()))
        .map(|bytes| black_box(bytes).len())
        .map_err(|error| format!("failed to serialize agent response: {error}"))
}

fn benchmark_target(
    term: Arc<Mutex<Term<ScribeEventListener>>>,
) -> Result<AgentSessionTarget, String> {
    let (writer, _reader) = UnixStream::pair().map_err(|error| error.to_string())?;
    writer.set_nonblocking(true).map_err(|error| error.to_string())?;
    let writer: OwnedFd = writer.into();
    let writer = AsyncPtyFd::new(writer).map_err(|error| error.to_string())?;
    let (_read, write) = split(writer);
    Ok(AgentSessionTarget {
        term,
        pty_write: Arc::new(Mutex::new(write)),
        title: Some("benchmark".into()),
        cwd: Some(PathBuf::from("/work/scribe")),
    })
}

fn term_fixture(
    scrollback_lines: usize,
    viewport_rows: usize,
    cols: usize,
    line_width: usize,
) -> Result<Arc<Mutex<Term<ScribeEventListener>>>, String> {
    if line_width >= cols {
        return Err("fixture line width must leave one terminal column free".into());
    }
    let (sender, _receiver) = mpsc::unbounded_channel();
    let listener = ScribeEventListener::new(fixed_id::<SessionId>(3)?, sender);
    let mut term = Term::new(
        build_term_config(scrollback_lines),
        &TestDims { cols, rows: viewport_rows },
        listener,
    );
    let line_count = scrollback_lines.saturating_add(viewport_rows);
    let mut input = Vec::with_capacity(line_count.saturating_mul(line_width.saturating_add(2)));
    for line in 0..line_count {
        let prefix = format!("line-{line:04}-");
        input.extend_from_slice(prefix.as_bytes());
        input.extend(std::iter::repeat_n(b'x', line_width.saturating_sub(prefix.len())));
        if line + 1 < line_count {
            input.extend_from_slice(b"\r\n");
        }
    }
    let mut processor: AnsiProcessor = AnsiProcessor::new();
    processor.advance(&mut term, &input);
    Ok(Arc::new(Mutex::new(term)))
}

fn world_fixture() -> Result<AgentWorldSnapshot, String> {
    const WINDOW_COUNT: usize = 8;
    const WORKSPACES_PER_WINDOW: usize = 2;
    const SESSIONS_PER_WORKSPACE: usize = 4;

    let mut windows = Vec::with_capacity(WINDOW_COUNT);
    let mut workspaces = Vec::with_capacity(WINDOW_COUNT * WORKSPACES_PER_WINDOW);
    let mut sessions =
        Vec::with_capacity(WINDOW_COUNT * WORKSPACES_PER_WINDOW * SESSIONS_PER_WORKSPACE);
    for window_index in 0..WINDOW_COUNT {
        let window_id = fixed_id::<WindowId>(1_000 + window_index)?;
        let mut workspace_names = Vec::with_capacity(WORKSPACES_PER_WINDOW);
        for workspace_offset in 0..WORKSPACES_PER_WINDOW {
            let workspace_index = window_index * WORKSPACES_PER_WINDOW + workspace_offset;
            let workspace_id = fixed_id::<WorkspaceId>(2_000 + workspace_index)?;
            let workspace_name = format!("workspace-{workspace_index}");
            workspace_names.push(workspace_name.clone());
            let mut session_ids = Vec::with_capacity(SESSIONS_PER_WORKSPACE);
            for session_offset in 0..SESSIONS_PER_WORKSPACE {
                let session_index = workspace_index * SESSIONS_PER_WORKSPACE + session_offset;
                let session_id = fixed_id::<SessionId>(3_000 + session_index)?;
                session_ids.push(session_id);
                sessions.push(AgentSession {
                    session_id,
                    window_id,
                    workspace_id,
                    title: Some(format!("session-{session_index}")),
                    cwd: Some(PathBuf::from(format!("/work/project-{workspace_index}"))),
                    provider: Some(AiProvider::Pi),
                    ai_state: Some(AiState::Processing),
                    task_label: Some("benchmark task".into()),
                    context_fill_percent: Some(42),
                    is_caller: session_index == 0,
                });
            }
            workspaces.push(AgentWorkspace {
                workspace_id,
                name: Some(workspace_name),
                window_id,
                session_ids,
            });
        }
        windows.push(AgentWindow {
            window_id,
            workspace_names,
            session_count: WORKSPACES_PER_WINDOW * SESSIONS_PER_WORKSPACE,
            connected: true,
            sharing_mode: SharingMode::SingleController,
            participant_count: 1,
        });
    }
    Ok(AgentWorldSnapshot {
        windows,
        workspaces,
        sessions,
        snapshot_id: 1,
        captured_at: 1_777_777_777,
    })
}

fn fixed_id<T>(value: usize) -> Result<T, String>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    let text = format!("00000000-0000-4000-8000-{value:012x}");
    text.parse::<T>().map_err(|error| format!("invalid fixture id {text}: {error}"))
}

fn report(measurements: &[Measurement]) -> Result<(), String> {
    let mut output = format!(
        "agent_api benchmark: {WARMUP_ITERATIONS} warmup + {MEASURED_ITERATIONS} measured iterations\n"
    );
    for measurement in measurements {
        let budget = measurement
            .budget
            .map_or_else(|| "n/a".into(), |duration| format!("{} ms", duration.as_millis()));
        writeln!(
            output,
            "{}: p95={:.3} ms budget={} max_serialized={} bytes",
            measurement.name,
            duration_ms(measurement.p95),
            budget,
            measurement.max_serialized_bytes
        )
        .map_err(|error| format!("failed to format benchmark report: {error}"))?;
    }
    io::stdout()
        .lock()
        .write_all(output.as_bytes())
        .map_err(|error| format!("failed to write benchmark report: {error}"))
}

fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}
