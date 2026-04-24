use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::config::RuntimeConfig;

const PROTOCOL_VERSION: u64 = 1;
const DEFAULT_TERMINAL_OUTPUT_LIMIT: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub enum AcpEvent {
    AgentMessageChunk {
        session_id: String,
        text: String,
    },
    ToolCall {
        session_id: String,
        tool_call_id: Option<String>,
        title: String,
        status: Option<String>,
    },
    ToolCallUpdate {
        session_id: String,
        tool_call_id: Option<String>,
        title: Option<String>,
        status: Option<String>,
    },
    PermissionRequest {
        session_id: String,
        permission_id: String,
        title: String,
    },
    TerminalStarted {
        session_id: String,
        id: String,
        title: String,
        command: String,
        cwd: Option<String>,
    },
    TerminalOutput {
        session_id: String,
        id: String,
        text: String,
    },
    TerminalDone {
        session_id: String,
        id: String,
        exit_code: Option<i64>,
        signal: Option<String>,
    },
}

impl AcpEvent {
    fn session_id(&self) -> &str {
        match self {
            AcpEvent::AgentMessageChunk { session_id, .. }
            | AcpEvent::ToolCall { session_id, .. }
            | AcpEvent::ToolCallUpdate { session_id, .. }
            | AcpEvent::PermissionRequest { session_id, .. }
            | AcpEvent::TerminalStarted { session_id, .. }
            | AcpEvent::TerminalOutput { session_id, .. }
            | AcpEvent::TerminalDone { session_id, .. } => session_id,
        }
    }
}

#[derive(Debug)]
pub struct PromptResponse {
    pub stop_reason: String,
}

pub struct AcpClient {
    request_tx: mpsc::Sender<Value>,
    events_tx: broadcast::Sender<AcpEvent>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>,
    next_request_id: AtomicU64,
    permission_coordinator: PermissionCoordinator,
    session_cwds: Arc<Mutex<HashMap<String, PathBuf>>>,
    default_cwd: Arc<Mutex<Option<PathBuf>>>,
}

impl AcpClient {
    pub async fn connect(cfg: &RuntimeConfig) -> Result<Self> {
        Self::connect_internal(cfg, false).await
    }

    pub async fn connect_with_external_permissions(cfg: &RuntimeConfig) -> Result<Self> {
        Self::connect_internal(cfg, true).await
    }

    async fn connect_internal(cfg: &RuntimeConfig, interactive_permissions: bool) -> Result<Self> {
        cfg.preflight_acp()?;

        let mut command = Command::new(&cfg.acp.command);
        let path = preferred_child_path(&cfg.cwd);
        command
            .args(&cfg.acp.args)
            .current_dir(&cfg.cwd)
            .env("PATH", path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if let Some(api_key) = &cfg.acp.anthropic_api_key {
            command.env("ANTHROPIC_API_KEY", api_key);
        }

        let mut child = command.spawn().with_context(|| {
            format!(
                "failed to spawn ACP adapter command: {} {}",
                cfg.acp.command,
                cfg.acp.args.join(" ")
            )
        })?;

        let stdin = child
            .stdin
            .take()
            .context("failed to capture ACP subprocess stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("failed to capture ACP subprocess stdout")?;
        let stderr = child
            .stderr
            .take()
            .context("failed to capture ACP subprocess stderr")?;

        let (request_tx, request_rx) = mpsc::channel::<Value>(256);
        let (events_tx, _) = broadcast::channel::<AcpEvent>(1024);
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let permission_coordinator = PermissionCoordinator::default();
        let session_cwds = Arc::new(Mutex::new(HashMap::new()));
        let default_cwd = Arc::new(Mutex::new(None));

        let client_host = Arc::new(ClientHost::new(
            cfg.cwd.clone(),
            interactive_permissions,
            permission_coordinator.clone(),
            session_cwds.clone(),
            default_cwd.clone(),
        ));

        tokio::spawn(write_loop(stdin, request_rx));
        tokio::spawn(stderr_loop(stderr));
        tokio::spawn(read_loop(
            stdout,
            request_tx.clone(),
            pending.clone(),
            events_tx.clone(),
            client_host,
        ));
        tokio::spawn(wait_loop(child));

        Ok(Self {
            request_tx,
            events_tx,
            pending,
            next_request_id: AtomicU64::new(1),
            permission_coordinator,
            session_cwds,
            default_cwd,
        })
    }

    pub async fn initialize(&self) -> Result<()> {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "clientCapabilities": {
                "fs": {
                    "readTextFile": true,
                    "writeTextFile": true
                },
                "terminal": true
            },
            "clientInfo": {
                "name": "boss-engine",
                "version": env!("CARGO_PKG_VERSION")
            }
        });

        let result = self.request("initialize", params).await?;
        let protocol_version = result
            .get("protocolVersion")
            .and_then(Value::as_u64)
            .context("initialize response missing protocolVersion")?;

        if protocol_version != PROTOCOL_VERSION {
            bail!("protocol version mismatch: expected {PROTOCOL_VERSION}, got {protocol_version}");
        }

        Ok(())
    }

    pub async fn new_session(&self, cwd: &Path) -> Result<String> {
        let params = json!({
            "cwd": cwd.display().to_string(),
            "mcpServers": []
        });

        let result = self.request("session/new", params).await?;
        let session_id = result
            .get("sessionId")
            .and_then(Value::as_str)
            .context("session/new response missing sessionId")?;
        self.session_cwds
            .lock()
            .await
            .insert(session_id.to_owned(), cwd.to_path_buf());
        *self.default_cwd.lock().await = Some(cwd.to_path_buf());

        Ok(session_id.to_owned())
    }

    pub async fn prompt_streaming<F>(
        &self,
        session_id: &str,
        text: &str,
        mut on_event: F,
    ) -> Result<PromptResponse>
    where
        F: FnMut(AcpEvent),
    {
        let params = json!({
            "sessionId": session_id,
            "prompt": [
                {
                    "type": "text",
                    "text": text
                }
            ]
        });

        let mut events = self.events_tx.subscribe();
        let request = self.request("session/prompt", params);
        tokio::pin!(request);

        loop {
            tokio::select! {
                response = &mut request => {
                    let value = response?;
                    let stop_reason = value
                        .get("stopReason")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_owned();
                    return Ok(PromptResponse { stop_reason });
                }
                maybe_event = events.recv() => {
                    if let Ok(event) = maybe_event {
                        // Some adapter-initiated client requests may omit sessionId.
                        // Route those updates to the active prompt in this PoC runtime.
                        if event.session_id() == session_id || event.session_id().is_empty() {
                            on_event(event);
                        }
                    }
                }
            }
        }
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();

        self.pending.lock().await.insert(request_id, tx);

        let payload = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params,
        });

        self.request_tx
            .send(payload)
            .await
            .context("failed to send JSON-RPC request to writer loop")?;

        rx.await
            .context("response channel closed before JSON-RPC response")?
    }

    pub async fn respond_permission(&self, permission_id: &str, granted: bool) -> Result<()> {
        let applied = self
            .permission_coordinator
            .resolve(permission_id.to_owned(), granted)
            .await;

        if !applied {
            bail!("unknown permission request id: {permission_id}");
        }

        Ok(())
    }
}

fn preferred_child_path(cwd: &Path) -> OsString {
    let mut paths = Vec::new();
    if let Some(boss_cli_dir) = boss_cli_shim_dir(cwd) {
        paths.push(boss_cli_dir.into_os_string());
    }
    if let Some(nvm_bin) = std::env::var_os("NVM_BIN") {
        paths.push(nvm_bin);
    }
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path).map(|path| path.into_os_string()));
    }

    std::env::join_paths(paths).unwrap_or_else(|_| std::env::var_os("PATH").unwrap_or_default())
}

fn boss_cli_shim_dir(cwd: &Path) -> Option<PathBuf> {
    let candidate = cwd.join("tools/boss/bin");
    candidate.is_dir().then_some(candidate)
}

fn merged_child_path(preferred_path: &OsString, requested_path: Option<&str>) -> OsString {
    let Some(requested_path) = requested_path else {
        return preferred_path.clone();
    };

    let mut paths = std::env::split_paths(preferred_path)
        .map(|path| path.into_os_string())
        .collect::<Vec<_>>();
    paths.extend(std::env::split_paths(requested_path).map(|path| path.into_os_string()));

    std::env::join_paths(paths).unwrap_or_else(|_| preferred_path.clone())
}

async fn write_loop(mut stdin: ChildStdin, mut rx: mpsc::Receiver<Value>) {
    while let Some(message) = rx.recv().await {
        let encoded = match serde_json::to_string(&message) {
            Ok(line) => line,
            Err(err) => {
                error!(?err, "failed to encode JSON-RPC message");
                continue;
            }
        };

        if let Err(err) = stdin.write_all(encoded.as_bytes()).await {
            error!(?err, "failed to write to ACP subprocess stdin");
            break;
        }
        if let Err(err) = stdin.write_all(b"\n").await {
            error!(?err, "failed to terminate JSON-RPC line");
            break;
        }
        if let Err(err) = stdin.flush().await {
            error!(?err, "failed to flush ACP subprocess stdin");
            break;
        }
    }
}

async fn read_loop<R: AsyncRead + Unpin + Send + 'static>(
    stdout: R,
    request_tx: mpsc::Sender<Value>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>,
    events_tx: broadcast::Sender<AcpEvent>,
    client_host: Arc<ClientHost>,
) {
    let mut reader = BufReader::new(stdout).lines();

    while let Ok(Some(line)) = reader.next_line().await {
        if line.trim().is_empty() {
            continue;
        }

        let message: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(err) => {
                warn!(?err, raw = %line, "failed to parse JSON-RPC line");
                continue;
            }
        };

        let has_method = message.get("method").and_then(Value::as_str).is_some();
        let has_id = message.get("id").is_some();

        if has_method && has_id {
            if let Some(response) =
                handle_incoming_request(&message, &events_tx, client_host.as_ref()).await
            {
                if request_tx.send(response).await.is_err() {
                    error!("failed to send JSON-RPC response to writer loop");
                    break;
                }
            }
            continue;
        }

        if has_method && !has_id {
            handle_incoming_notification(&message, &events_tx);
            continue;
        }

        if !has_method && has_id {
            handle_incoming_response(&message, &pending).await;
            continue;
        }

        debug!(raw = %line, "ignoring JSON-RPC message without method/id");
    }
}

fn handle_incoming_notification(message: &Value, events_tx: &broadcast::Sender<AcpEvent>) {
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return;
    };

    if method != "session/update" {
        return;
    }

    let Some(params) = message.get("params") else {
        return;
    };

    let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
        return;
    };

    let Some(update) = params.get("update") else {
        return;
    };

    let Some(update_type) = update.get("sessionUpdate").and_then(Value::as_str) else {
        return;
    };

    match update_type {
        "agent_message_chunk" => {
            let text = update
                .get("content")
                .and_then(|content| content.get("text"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            if let Some(text) = text {
                let _ = events_tx.send(AcpEvent::AgentMessageChunk {
                    session_id: session_id.to_owned(),
                    text,
                });
            }
        }
        "tool_call" => {
            let title = update
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("tool call")
                .to_owned();
            let status = update
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let tool_call_id = update
                .get("toolCallId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let _ = events_tx.send(AcpEvent::ToolCall {
                session_id: session_id.to_owned(),
                tool_call_id,
                title,
                status,
            });
        }
        "tool_call_update" => {
            let title = update
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let status = update
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let tool_call_id = update
                .get("toolCallId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let _ = events_tx.send(AcpEvent::ToolCallUpdate {
                session_id: session_id.to_owned(),
                tool_call_id,
                title,
                status,
            });
        }
        _ => {}
    }
}

async fn handle_incoming_response(
    message: &Value,
    pending: &Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>,
) {
    let Some(id) = message.get("id").and_then(Value::as_u64) else {
        return;
    };

    let pending_request = pending.lock().await.remove(&id);
    let Some(tx) = pending_request else {
        return;
    };

    if let Some(error_value) = message.get("error") {
        let _ = tx.send(Err(anyhow!("ACP request failed: {error_value}")));
        return;
    }

    let result = message.get("result").cloned().unwrap_or_else(|| json!({}));
    let _ = tx.send(Ok(result));
}

async fn handle_incoming_request(
    message: &Value,
    events_tx: &broadcast::Sender<AcpEvent>,
    client_host: &ClientHost,
) -> Option<Value> {
    let method = message.get("method")?.as_str()?.to_owned();
    let request_id = message.get("id")?.clone();
    let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
    let session_id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    match client_host.handle_request(&method, params, events_tx).await {
        Ok(result) => Some(json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": result,
        })),
        Err(err) => {
            let status = err
                .chain()
                .map(|cause| cause.to_string())
                .collect::<Vec<_>>()
                .join(": ");
            let _ = events_tx.send(AcpEvent::ToolCall {
                session_id,
                tool_call_id: None,
                title: method.clone(),
                status: Some(format!("failed: {status}")),
            });
            warn!(?err, method, "failed to handle incoming ACP request");
            Some(json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {
                    "code": -32000,
                    "message": err.to_string(),
                }
            }))
        }
    }
}

async fn wait_loop(mut child: Child) {
    match child.wait().await {
        Ok(status) => {
            info!(?status, "ACP subprocess exited");
        }
        Err(err) => {
            error!(?err, "ACP subprocess wait failed");
        }
    }
}

async fn stderr_loop<R: AsyncRead + Unpin>(stderr: R) {
    let mut reader = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = reader.next_line().await {
        if !line.trim().is_empty() {
            debug!(target: "acp_stderr", "{line}");
        }
    }
}

#[derive(Clone, Default)]
struct PermissionCoordinator {
    inner: Arc<PermissionCoordinatorInner>,
}

#[derive(Default)]
struct PermissionCoordinatorInner {
    next_id: AtomicU64,
    pending: Mutex<HashMap<String, oneshot::Sender<bool>>>,
}

impl PermissionCoordinator {
    async fn register(&self) -> (String, oneshot::Receiver<bool>) {
        let request_id = format!(
            "perm-{}",
            self.inner.next_id.fetch_add(1, Ordering::Relaxed) + 1
        );
        let (tx, rx) = oneshot::channel();
        self.inner
            .pending
            .lock()
            .await
            .insert(request_id.clone(), tx);
        (request_id, rx)
    }

    async fn resolve(&self, request_id: String, granted: bool) -> bool {
        if let Some(tx) = self.inner.pending.lock().await.remove(&request_id) {
            let _ = tx.send(granted);
            return true;
        }
        false
    }
}

struct ClientHost {
    terminals: TerminalManager,
    interactive_permissions: bool,
    permission_coordinator: PermissionCoordinator,
    session_cwds: Arc<Mutex<HashMap<String, PathBuf>>>,
    default_cwd: Arc<Mutex<Option<PathBuf>>>,
}

impl ClientHost {
    fn new(
        cwd: PathBuf,
        interactive_permissions: bool,
        permission_coordinator: PermissionCoordinator,
        session_cwds: Arc<Mutex<HashMap<String, PathBuf>>>,
        default_cwd: Arc<Mutex<Option<PathBuf>>>,
    ) -> Self {
        Self {
            terminals: TerminalManager::new(
                preferred_child_path(&cwd),
                session_cwds.clone(),
                default_cwd.clone(),
            ),
            interactive_permissions,
            permission_coordinator,
            session_cwds,
            default_cwd,
        }
    }

    async fn handle_request(
        &self,
        method: &str,
        params: Value,
        events_tx: &broadcast::Sender<AcpEvent>,
    ) -> Result<Value> {
        match method {
            "fs/read_text_file" => self.read_text_file(params).await,
            "fs/write_text_file" => self.write_text_file(params).await,
            "terminal/create" => self.terminals.create(params, events_tx).await,
            "terminal/output" => self.terminals.output(params).await,
            "terminal/wait_for_exit" => self.terminals.wait_for_exit(params).await,
            "terminal/kill" => self.terminals.kill(params).await,
            "terminal/release" => self.terminals.release(params).await,
            "session/request_permission" => self.request_permission(params, events_tx).await,
            other => bail!("unsupported ACP client method: {other}"),
        }
    }

    async fn read_text_file(&self, params: Value) -> Result<Value> {
        #[derive(Deserialize)]
        struct ReadRequest {
            #[serde(rename = "sessionId")]
            session_id: Option<String>,
            path: String,
            line: Option<usize>,
            limit: Option<usize>,
        }

        let request: ReadRequest =
            serde_json::from_value(params).context("invalid read request")?;
        let session_cwd = self.session_cwd(request.session_id.as_deref()).await;
        let resolved_path =
            resolve_relative_to_session(Path::new(&request.path), session_cwd.as_deref());
        let content = tokio::fs::read_to_string(&resolved_path)
            .await
            .with_context(|| format!("failed to read file {}", resolved_path.display()))?;

        if request.line.is_none() && request.limit.is_none() {
            return Ok(json!({ "content": content }));
        }

        let start = request.line.unwrap_or(1).saturating_sub(1);
        let limit = request.limit.unwrap_or(usize::MAX);

        let sliced = content
            .lines()
            .skip(start)
            .take(limit)
            .collect::<Vec<_>>()
            .join("\n");

        Ok(json!({ "content": sliced }))
    }

    async fn write_text_file(&self, params: Value) -> Result<Value> {
        #[derive(Deserialize)]
        struct WriteRequest {
            #[serde(rename = "sessionId")]
            session_id: Option<String>,
            path: String,
            content: String,
        }

        let request: WriteRequest =
            serde_json::from_value(params).context("invalid write request")?;
        let session_cwd = self.session_cwd(request.session_id.as_deref()).await;
        let path = resolve_relative_to_session(Path::new(&request.path), session_cwd.as_deref());
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await.with_context(|| {
                    format!("failed to create parent directories for {}", path.display())
                })?;
            }
        }
        tokio::fs::write(&path, request.content)
            .await
            .with_context(|| format!("failed to write file {}", path.display()))?;

        Ok(json!({}))
    }

    async fn session_cwd(&self, session_id: Option<&str>) -> Option<PathBuf> {
        if let Some(session_id) = session_id {
            if let Some(cwd) = self.session_cwds.lock().await.get(session_id).cloned() {
                return Some(cwd);
            }
        }
        self.default_cwd.lock().await.clone()
    }

    async fn request_permission(
        &self,
        params: Value,
        events_tx: &broadcast::Sender<AcpEvent>,
    ) -> Result<Value> {
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();

        let title = params
            .get("toolCall")
            .and_then(|tool_call| tool_call.get("title"))
            .and_then(Value::as_str)
            .unwrap_or("Tool permission")
            .to_owned();

        let Some(options) = params.get("options").and_then(Value::as_array) else {
            return Ok(json!({ "outcome": { "outcome": "cancelled" } }));
        };

        let allow_option = options.iter().find_map(|option| {
            option
                .get("kind")
                .and_then(Value::as_str)
                .and_then(|kind| {
                    if kind == "allow_once" || kind == "allow_always" {
                        option.get("optionId").and_then(Value::as_str)
                    } else {
                        None
                    }
                })
                .map(str::to_owned)
        });

        let reject_option = options.iter().find_map(|option| {
            option
                .get("kind")
                .and_then(Value::as_str)
                .and_then(|kind| {
                    if kind == "reject_once" {
                        option.get("optionId").and_then(Value::as_str)
                    } else {
                        None
                    }
                })
                .map(str::to_owned)
        });

        if !self.interactive_permissions {
            return match allow_option.or_else(|| {
                options.first().and_then(|option| {
                    option
                        .get("optionId")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
            }) {
                Some(option_id) => Ok(json!({
                    "outcome": {
                        "outcome": "selected",
                        "optionId": option_id,
                    }
                })),
                None => Ok(json!({ "outcome": { "outcome": "cancelled" } })),
            };
        }

        let (permission_id, rx) = self.permission_coordinator.register().await;
        let _ = events_tx.send(AcpEvent::PermissionRequest {
            session_id,
            permission_id: permission_id.clone(),
            title: title.clone(),
        });

        let granted = match tokio::time::timeout(Duration::from_secs(600), rx).await {
            Ok(Ok(value)) => value,
            Ok(Err(_)) => false,
            Err(_) => false,
        };

        let selected = if granted { allow_option } else { reject_option };

        match selected.or_else(|| {
            if granted {
                options.first().and_then(|option| {
                    option
                        .get("optionId")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
            } else {
                None
            }
        }) {
            Some(option_id) => Ok(json!({
                "outcome": {
                    "outcome": "selected",
                    "optionId": option_id,
                }
            })),
            None => Ok(json!({ "outcome": { "outcome": "cancelled" } })),
        }
    }
}

struct TerminalManager {
    terminals: Mutex<HashMap<String, Arc<TerminalProcess>>>,
    next_id: AtomicU64,
    child_path: OsString,
    session_cwds: Arc<Mutex<HashMap<String, PathBuf>>>,
    default_cwd: Arc<Mutex<Option<PathBuf>>>,
}

impl TerminalManager {
    fn new(
        child_path: OsString,
        session_cwds: Arc<Mutex<HashMap<String, PathBuf>>>,
        default_cwd: Arc<Mutex<Option<PathBuf>>>,
    ) -> Self {
        Self {
            terminals: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            child_path,
            session_cwds,
            default_cwd,
        }
    }

    async fn create(
        &self,
        params: Value,
        events_tx: &broadcast::Sender<AcpEvent>,
    ) -> Result<Value> {
        #[derive(Deserialize)]
        struct EnvVariable {
            name: String,
            value: String,
        }

        #[derive(Deserialize)]
        struct ToolCallContext {
            title: Option<String>,
        }

        #[derive(Deserialize)]
        struct CreateRequest {
            #[serde(rename = "sessionId")]
            session_id: Option<String>,
            #[serde(rename = "toolCallId")]
            tool_call_id: Option<String>,
            #[serde(rename = "toolCall")]
            tool_call: Option<ToolCallContext>,
            command: String,
            args: Option<Vec<String>>,
            cwd: Option<String>,
            env: Option<Vec<EnvVariable>>,
            #[serde(rename = "outputByteLimit")]
            output_byte_limit: Option<usize>,
        }

        let request: CreateRequest =
            serde_json::from_value(params).context("invalid terminal/create request")?;
        let CreateRequest {
            session_id,
            tool_call_id,
            tool_call,
            command: raw_command,
            args: request_args,
            cwd,
            env,
            output_byte_limit,
        } = request;

        let (executable, args, launch_mode) =
            normalize_terminal_command(raw_command.clone(), request_args);
        let session_cwd = self.session_cwd(session_id.as_deref()).await;
        let resolved_cwd =
            resolve_terminal_cwd(cwd.as_deref().map(Path::new), session_cwd.as_deref());
        let cwd_label = resolved_cwd
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<none>".to_owned());
        info!(
            raw_command = %raw_command,
            executable = %executable,
            args = ?args,
            cwd = %cwd_label,
            launch_mode,
            "handling terminal/create request",
        );

        let mut command = Command::new(&executable);
        command
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env("PATH", &self.child_path);

        if let Some(cwd) = &resolved_cwd {
            if !cwd.is_dir() {
                bail!(
                    "terminal/create cwd does not exist or is not a directory: {}",
                    cwd.display()
                );
            }
            command.current_dir(cwd);
        }

        if let Some(env_vars) = env {
            for env_var in env_vars {
                if env_var.name == "PATH" {
                    command.env(
                        "PATH",
                        merged_child_path(&self.child_path, Some(&env_var.value)),
                    );
                } else {
                    command.env(env_var.name, env_var.value);
                }
            }
        }

        let child = command.spawn().with_context(|| {
            format!(
                "failed to spawn terminal command executable={} args={args:?}",
                executable
            )
        })?;

        let terminal_id = format!(
            "terminal-{}",
            self.next_id.fetch_add(1, Ordering::Relaxed) + 1
        );
        let stream_id = tool_call_id.unwrap_or_else(|| terminal_id.clone());
        let session_id = session_id.unwrap_or_default();
        let title = tool_call
            .and_then(|call| call.title)
            .unwrap_or_else(|| "Terminal command".to_owned());

        let output_limit = output_byte_limit.unwrap_or(DEFAULT_TERMINAL_OUTPUT_LIMIT);

        let terminal = Arc::new(TerminalProcess::new(
            child,
            output_limit,
            session_id.clone(),
            stream_id.clone(),
            events_tx.clone(),
        ));
        terminal.start_output_pumps().await?;

        self.terminals
            .lock()
            .await
            .insert(terminal_id.clone(), terminal);

        let _ = events_tx.send(AcpEvent::TerminalStarted {
            session_id,
            id: stream_id,
            title,
            command: raw_command,
            cwd,
        });

        Ok(json!({ "terminalId": terminal_id }))
    }

    async fn output(&self, params: Value) -> Result<Value> {
        let terminal = self.get_terminal(&params).await?;
        let output = terminal.output.lock().await.clone();
        let truncated = terminal.truncated.load(Ordering::Relaxed);
        let exit_status = terminal.capture_exit_status().await;

        Ok(json!({
            "output": output,
            "truncated": truncated,
            "exitStatus": exit_status,
        }))
    }

    async fn wait_for_exit(&self, params: Value) -> Result<Value> {
        let terminal = self.get_terminal(&params).await?;
        let exit_status = terminal.wait_for_exit().await?;
        Ok(json!(exit_status))
    }

    async fn kill(&self, params: Value) -> Result<Value> {
        let terminal = self.get_terminal(&params).await?;
        terminal.kill().await?;
        Ok(json!({}))
    }

    async fn release(&self, params: Value) -> Result<Value> {
        let terminal_id = params
            .get("terminalId")
            .and_then(Value::as_str)
            .context("terminalId missing from terminal request")?
            .to_owned();

        let terminal = self.terminals.lock().await.remove(&terminal_id);
        if let Some(terminal) = terminal {
            terminal.kill().await?;
        }

        Ok(json!({}))
    }

    async fn get_terminal(&self, params: &Value) -> Result<Arc<TerminalProcess>> {
        let terminal_id = params
            .get("terminalId")
            .and_then(Value::as_str)
            .context("terminalId missing from terminal request")?;

        let map = self.terminals.lock().await;
        map.get(terminal_id)
            .cloned()
            .with_context(|| format!("terminal not found: {terminal_id}"))
    }

    async fn session_cwd(&self, session_id: Option<&str>) -> Option<PathBuf> {
        if let Some(session_id) = session_id {
            if let Some(cwd) = self.session_cwds.lock().await.get(session_id).cloned() {
                return Some(cwd);
            }
        }
        self.default_cwd.lock().await.clone()
    }
}

fn resolve_relative_to_session(path: &Path, session_cwd: Option<&Path>) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else if let Some(session_cwd) = session_cwd {
        session_cwd.join(path)
    } else {
        path.to_path_buf()
    }
}

fn resolve_terminal_cwd(request_cwd: Option<&Path>, session_cwd: Option<&Path>) -> Option<PathBuf> {
    request_cwd
        .map(|cwd| resolve_relative_to_session(cwd, session_cwd))
        .or_else(|| session_cwd.map(Path::to_path_buf))
}

fn normalize_terminal_command(
    raw_command: String,
    request_args: Option<Vec<String>>,
) -> (String, Vec<String>, &'static str) {
    if let Some(args) = request_args {
        return (raw_command, args, "structured");
    }

    if command_uses_shell_operators(&raw_command) {
        return (
            "/bin/bash".to_owned(),
            vec!["-lc".to_owned(), raw_command],
            "shell",
        );
    }

    match shlex::split(&raw_command) {
        Some(parts) => {
            if let Some((program, rest)) = parts.split_first() {
                (program.to_owned(), rest.to_vec(), "shlex")
            } else {
                (raw_command, Vec::new(), "raw")
            }
        }
        None => (raw_command, Vec::new(), "raw"),
    }
}

fn command_uses_shell_operators(command: &str) -> bool {
    const SHELL_TOKENS: [&str; 8] = ["&&", "||", "|", ";", "$(", "`", ">", "<"];
    SHELL_TOKENS.iter().any(|token| command.contains(token))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{
        boss_cli_shim_dir, merged_child_path, normalize_terminal_command, preferred_child_path,
        resolve_relative_to_session, resolve_terminal_cwd,
    };

    #[test]
    fn normalize_terminal_command_uses_structured_args() {
        let (program, args, mode) = normalize_terminal_command(
            "javac".to_owned(),
            Some(vec!["/tmp/hello.java".to_owned()]),
        );
        assert_eq!(program, "javac");
        assert_eq!(args, vec!["/tmp/hello.java"]);
        assert_eq!(mode, "structured");
    }

    #[test]
    fn normalize_terminal_command_splits_shell_words() {
        let (program, args, mode) =
            normalize_terminal_command("javac /tmp/hello.java".to_owned(), None);
        assert_eq!(program, "javac");
        assert_eq!(args, vec!["/tmp/hello.java"]);
        assert_eq!(mode, "shlex");
    }

    #[test]
    fn normalize_terminal_command_uses_shell_for_operators() {
        let (program, args, mode) = normalize_terminal_command("cd /tmp && ls".to_owned(), None);
        assert_eq!(program, "/bin/bash");
        assert_eq!(args, vec!["-lc", "cd /tmp && ls"]);
        assert_eq!(mode, "shell");
    }

    #[test]
    fn preferred_child_path_includes_boss_cli_shim_dir() {
        let tempdir = tempfile::tempdir().unwrap();
        let shim_dir = tempdir.path().join("tools/boss/bin");
        std::fs::create_dir_all(&shim_dir).unwrap();

        let path = preferred_child_path(tempdir.path());
        let paths = std::env::split_paths(&path).collect::<Vec<_>>();

        assert_eq!(paths.first(), Some(&shim_dir));
    }

    #[test]
    fn boss_cli_shim_dir_returns_none_when_missing() {
        let tempdir = tempfile::tempdir().unwrap();
        assert_eq!(boss_cli_shim_dir(tempdir.path()), None);
    }

    #[test]
    fn merged_child_path_prepends_preferred_entries() {
        let preferred =
            std::env::join_paths([PathBuf::from("/preferred/bin"), PathBuf::from("/usr/bin")])
                .unwrap();

        let merged = merged_child_path(&preferred, Some("/custom/bin:/bin"));
        let paths = std::env::split_paths(&merged).collect::<Vec<_>>();

        assert_eq!(paths[0], PathBuf::from("/preferred/bin"));
        assert_eq!(paths[1], PathBuf::from("/usr/bin"));
        assert_eq!(paths[2], PathBuf::from("/custom/bin"));
        assert_eq!(paths[3], PathBuf::from("/bin"));
    }

    #[test]
    fn resolve_relative_to_session_joins_relative_paths() {
        let resolved =
            resolve_relative_to_session(Path::new("src/main.rs"), Some(Path::new("/tmp/session")));
        assert_eq!(resolved, PathBuf::from("/tmp/session/src/main.rs"));
    }

    #[test]
    fn resolve_terminal_cwd_defaults_to_session_cwd() {
        let resolved = resolve_terminal_cwd(None, Some(Path::new("/tmp/session")));
        assert_eq!(resolved, Some(PathBuf::from("/tmp/session")));
    }
}

struct TerminalProcess {
    child: Mutex<Child>,
    output: Arc<Mutex<String>>,
    truncated: Arc<AtomicBool>,
    output_limit: usize,
    exit_status: Arc<Mutex<Option<Value>>>,
    events_tx: broadcast::Sender<AcpEvent>,
    session_id: String,
    stream_id: String,
    completion_emitted: AtomicBool,
}

impl TerminalProcess {
    fn new(
        child: Child,
        output_limit: usize,
        session_id: String,
        stream_id: String,
        events_tx: broadcast::Sender<AcpEvent>,
    ) -> Self {
        Self {
            child: Mutex::new(child),
            output: Arc::new(Mutex::new(String::new())),
            truncated: Arc::new(AtomicBool::new(false)),
            output_limit,
            exit_status: Arc::new(Mutex::new(None)),
            events_tx,
            session_id,
            stream_id,
            completion_emitted: AtomicBool::new(false),
        }
    }

    async fn start_output_pumps(&self) -> Result<()> {
        let mut child = self.child.lock().await;

        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(Self::pump_output(
                stdout,
                self.output.clone(),
                self.truncated.clone(),
                self.output_limit,
                self.events_tx.clone(),
                self.session_id.clone(),
                self.stream_id.clone(),
            ));
        }

        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(Self::pump_output(
                stderr,
                self.output.clone(),
                self.truncated.clone(),
                self.output_limit,
                self.events_tx.clone(),
                self.session_id.clone(),
                self.stream_id.clone(),
            ));
        }

        Ok(())
    }

    async fn pump_output<R: AsyncRead + Unpin>(
        mut reader: R,
        output: Arc<Mutex<String>>,
        truncated: Arc<AtomicBool>,
        output_limit: usize,
        events_tx: broadcast::Sender<AcpEvent>,
        session_id: String,
        stream_id: String,
    ) {
        let mut buf = vec![0_u8; 4096];

        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&buf[..n]);
                    let chunk_string = chunk.to_string();
                    let mut locked = output.lock().await;
                    locked.push_str(&chunk_string);

                    if locked.len() > output_limit {
                        let excess = locked.len() - output_limit;
                        let mut drain_to = excess;
                        while drain_to < locked.len() && !locked.is_char_boundary(drain_to) {
                            drain_to += 1;
                        }
                        locked.drain(..drain_to);
                        truncated.store(true, Ordering::Relaxed);
                    }

                    let _ = events_tx.send(AcpEvent::TerminalOutput {
                        session_id: session_id.clone(),
                        id: stream_id.clone(),
                        text: chunk_string,
                    });
                }
                Err(err) => {
                    warn!(?err, "failed to read terminal output");
                    break;
                }
            }
        }
    }

    async fn capture_exit_status(&self) -> Option<Value> {
        if let Some(cached) = self.exit_status.lock().await.clone() {
            return Some(cached);
        }

        let mut child = self.child.lock().await;
        match child.try_wait() {
            Ok(Some(status)) => {
                let mapped = map_exit_status(status);
                *self.exit_status.lock().await = Some(mapped.clone());
                self.emit_completion_if_needed(&mapped);
                Some(mapped)
            }
            Ok(None) => None,
            Err(err) => {
                warn!(?err, "failed to query terminal status");
                None
            }
        }
    }

    async fn wait_for_exit(&self) -> Result<Value> {
        if let Some(cached) = self.exit_status.lock().await.clone() {
            return Ok(cached);
        }

        let mut child = self.child.lock().await;
        let status = child.wait().await.context("terminal wait failed")?;
        let mapped = map_exit_status(status);
        *self.exit_status.lock().await = Some(mapped.clone());
        self.emit_completion_if_needed(&mapped);
        Ok(mapped)
    }

    async fn kill(&self) -> Result<()> {
        let mut child = self.child.lock().await;
        match child.try_wait() {
            Ok(Some(_)) => Ok(()),
            Ok(None) => child.kill().await.context("terminal kill failed"),
            Err(err) => Err(anyhow!(err).context("failed to inspect terminal process state")),
        }
    }

    fn emit_completion_if_needed(&self, status: &Value) {
        if self
            .completion_emitted
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }

        let exit_code = status.get("exitCode").and_then(Value::as_i64);
        let signal = status
            .get("signal")
            .and_then(Value::as_str)
            .map(str::to_owned);

        let _ = self.events_tx.send(AcpEvent::TerminalDone {
            session_id: self.session_id.clone(),
            id: self.stream_id.clone(),
            exit_code,
            signal,
        });
    }
}

fn map_exit_status(status: std::process::ExitStatus) -> Value {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        return json!({
            "exitCode": status.code(),
            "signal": status.signal().map(|sig| sig.to_string()),
        });
    }

    #[cfg(not(unix))]
    {
        json!({
            "exitCode": status.code(),
            "signal": Value::Null,
        })
    }
}
