//! Optional, per-server Playit agent. No provider credentials enter snapshots or logs.
//!
//! Contracts are from playit-cloud/playit-agent v1.0.12 (api_client/api.rs and
//! playit-ipc/ipc.rs). Release assets execute only after their GitHub-published
//! SHA-256 digest has been checked. Keeping each agent's private state preserves
//! its allocation across daemon, Minecraft, machine and ISP-address changes.

use dawg_core::{
    ForwardingStatus, NetworkConfig, NetworkStatus, ServerConfig, ServerStatus, data_dir,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{Mutex, RwLock},
    task::JoinHandle,
};

const VERSION: &str = "1.0.12";
const API: &str = "https://api.playit.gg";
const ACCOUNT: &str = "https://playit.gg/account/tunnels";
const MAX_JSON: usize = 1024 * 1024;
const MAX_BINARY: usize = 64 * 1024 * 1024;

pub struct TunnelService {
    slots: Mutex<HashMap<String, Slot>>,
    installer: Arc<Mutex<()>>,
    client: reqwest::Client,
}

struct Slot {
    port: u16,
    status: Arc<RwLock<NetworkStatus>>,
    task: JoinHandle<()>,
}

impl TunnelService {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .user_agent("Dawg-Minecraft-Manager/0.1")
            // Never send an agent key to a redirect target.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("TLS HTTP client");
        Self {
            slots: Mutex::new(HashMap::new()),
            installer: Arc::new(Mutex::new(())),
            client,
        }
    }

    /// Only manages workers; HTTP and binary installation never block snapshots.
    pub async fn update(&self, servers: Vec<(ServerConfig, ServerStatus)>) {
        let wanted: HashMap<_, _> = servers
            .into_iter()
            .filter(|(config, _)| config.network.tunnel_enabled)
            .map(|(config, _)| (config.id.clone(), config))
            .collect();
        let mut slots = self.slots.lock().await;
        slots.retain(|id, slot| {
            let keep =
                wanted.get(id).is_some_and(|c| c.port == slot.port) && !slot.task.is_finished();
            if !keep {
                slot.task.abort();
            }
            keep
        });
        for (id, config) in wanted {
            if slots.contains_key(&id) {
                continue;
            }
            let state = Arc::new(RwLock::new(observation(
                ForwardingStatus::Pending,
                "Preparing the free Minecraft tunnel.",
                None,
                None,
            )));
            let status = state.clone();
            let client = self.client.clone();
            let installer = self.installer.clone();
            let port = config.port;
            let task = tokio::spawn(async move {
                worker(config, status, client, installer).await;
            });
            slots.insert(
                id,
                Slot {
                    port,
                    status: state,
                    task,
                },
            );
        }
    }

    pub async fn status(&self, config: &ServerConfig) -> Option<NetworkStatus> {
        if !config.network.tunnel_enabled {
            return None;
        }
        let status = self
            .slots
            .lock()
            .await
            .get(&config.id)
            .filter(|slot| slot.port == config.port)
            .map(|slot| slot.status.clone());
        Some(match status {
            Some(status) => status.read().await.clone(),
            None => observation(
                ForwardingStatus::Pending,
                "Preparing the free Minecraft tunnel.",
                None,
                None,
            ),
        })
    }

    pub async fn shutdown(&self) {
        let mut slots = self.slots.lock().await;
        let tasks: Vec<_> = slots
            .drain()
            .map(|(_, slot)| {
                slot.task.abort();
                slot.task
            })
            .collect();
        drop(slots);
        // Dropping a worker's kill_on_drop child kills it; Tokio reaps the process.
        for task in tasks {
            let _ = task.await;
        }
    }
}

fn observation(
    forwarding: ForwardingStatus,
    detail: &str,
    join_address: Option<String>,
    setup_url: Option<String>,
) -> NetworkStatus {
    NetworkStatus {
        forwarding,
        detail: Some(detail.into()),
        join_address,
        setup_url,
        checked_at_ms: Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        ),
        ..Default::default()
    }
}

#[derive(Default, Serialize, Deserialize)]
struct PrivateState {
    claim_code: Option<String>,
    secret: Option<String>,
    tunnel_id: Option<String>,
    configured_port: Option<u16>,
    // Set before a create request: an ambiguous network failure must not allocate
    // another public tunnel on the next poll or reboot.
    create_requested: bool,
}

async fn worker(
    config: ServerConfig,
    status: Arc<RwLock<NetworkStatus>>,
    client: reqwest::Client,
    installer: Arc<Mutex<()>>,
) {
    // Server identifiers normally are UUIDs. Hashing also makes imported IDs safe
    // filesystem components and keeps Unix socket paths short.
    let identity = hex(&Sha256::digest(config.id.as_bytes()))[..24].to_string();
    let directory = data_dir().join("tunnel").join(identity);
    let state_path = directory.join("state.json");
    let mut state = match load_state(&directory, &state_path).await {
        Ok(state) => state,
        Err(message) => {
            *status.write().await = observation(ForwardingStatus::Error, message, None, None);
            return;
        }
    };
    let mut child: Option<Child> = None;
    let mut binary: Option<PathBuf> = None;
    let mut restart_after = tokio::time::Instant::now();
    let mut next_provider_poll = tokio::time::Instant::now();
    let mut control_failures = 0_u8;
    loop {
        let result = async {
            if state.secret.is_none() {
                if state.claim_code.as_deref().is_none_or(|code| claim_url(code).is_none()) {
                    state.claim_code = Some(uuid::Uuid::new_v4().simple().to_string()[..10].into());
                    save_state(&state_path, &state).await?;
                }
                let code = state.claim_code.as_deref().expect("created claim");
                let url = claim_url(code);
                *status.write().await = observation(ForwardingStatus::NeedsSetup,
                    "Open Playit setup and approve this Minecraft agent. A free provider account is required.", None, url.clone());
                let setup = match api_call(&client, "/claim/setup", None, json!({"code":code,"agent_type":"self-managed","version":format!("playit {VERSION}")})).await {
                    Ok(value) => value,
                    Err(error) if error.expired_claim => {
                        state.claim_code = None;
                        save_state(&state_path, &state).await?;
                        return Ok(());
                    }
                    Err(error) => return Err(error.message),
                };
                match setup.as_str() {
                    Some("UserAccepted") => {
                        let exchanged = api(&client, "/claim/exchange", None, json!({"code":code})).await?;
                        let secret = exchanged.get("secret_key").and_then(Value::as_str).filter(|s| valid_secret(s))
                            .ok_or("Playit returned an invalid agent key; setup was not saved.")?;
                        state.secret = Some(secret.to_owned());
                        state.claim_code = None;
                        save_state(&state_path, &state).await?;
                    }
                    Some("UserRejected") => {
                        *status.write().await = observation(ForwardingStatus::NeedsSetup,
                            "Playit setup was rejected. Open setup to approve the agent, or disable the tunnel.", None, url);
                        return Ok(());
                    }
                    Some("WaitingForUserVisit" | "WaitingForUser") => return Ok(()),
                    _ => return Err("Playit returned an unsupported setup response."),
                }
            }
            if binary.is_none() {
                *status.write().await = observation(ForwardingStatus::Pending, "Installing the verified official Playit agent.", None, None);
                let _guard = installer.lock().await;
                binary = Some(install_agent(&client).await?);
            }
            if let Some(process) = child.as_mut()
                && process.try_wait().map_err(|_| "Could not inspect the managed Playit process.")?.is_some() {
                    child = None;
                    restart_after = tokio::time::Instant::now() + Duration::from_secs(15);
                    *status.write().await = observation(ForwardingStatus::Pending, "The tunnel agent exited; restarting it shortly.", None, None);
            }
            if child.is_none() {
                if tokio::time::Instant::now() < restart_after { return Ok(()); }
                let key_path = directory.join("agent.key");
                private_write(&key_path, state.secret.as_deref().expect("claimed").as_bytes()).await?;
                let socket = directory.join("agent.sock");
                // This private socket belongs to the previous managed process.
                let _ = tokio::fs::remove_file(&socket).await;
                let mut command = Command::new(binary.as_ref().expect("installed"));
                command.arg("--secret-path").arg(&key_path).arg("--socket-path").arg(&socket)
                    .arg("--log-path").arg("/dev/null")
                    .current_dir(&directory).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true);
                // A daemon crash must not leave an unmanaged tunnel behind.
                #[cfg(target_os = "linux")]
                {
                    let parent = unsafe { libc::getpid() };
                    unsafe { command.pre_exec(move || {
                        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 { return Err(std::io::Error::last_os_error()); }
                        if libc::getppid() != parent { return Err(std::io::Error::other("Daemon exited while launching tunnel")); }
                        Ok(())
                    }); }
                }
                child = Some(command.spawn().map_err(|_| "Could not start the verified Playit agent.")?);
                *status.write().await = observation(ForwardingStatus::Pending, "Connecting the Minecraft tunnel.", None, None);
                next_provider_poll = tokio::time::Instant::now() + Duration::from_secs(6);
                return Ok(());
            }
            // IPC status proves the dedicated agent is running and the selected
            // route targets this exact Minecraft port, rather than a Web UI.
            let lifecycle = match ipc_state(&directory.join("agent.sock")).await {
                Ok(lifecycle) => { control_failures = 0; lifecycle }
                Err(message) => {
                    control_failures = control_failures.saturating_add(1);
                    if control_failures >= 3 {
                        if let Some(mut process) = child.take() {
                            let _ = process.start_kill();
                            let _ = tokio::time::timeout(Duration::from_secs(3), process.wait()).await;
                        }
                        control_failures = 0;
                        restart_after = tokio::time::Instant::now() + Duration::from_secs(15);
                    }
                    return Err(message);
                }
            };
            if let Some(join) = matching_route(&lifecycle, config.port) {
                *status.write().await = observation(ForwardingStatus::Active,
                    "Free Minecraft tunnel connected. Keep this agent and allocation to retain the join address.", Some(join), None);
                return Ok(());
            }
            if tokio::time::Instant::now() >= next_provider_poll {
                next_provider_poll = tokio::time::Instant::now() + Duration::from_secs(20);
                let secret = state.secret.clone().expect("claimed");
                let run = api(&client, "/v1/agents/rundata", Some(&secret), json!({})).await?;
                reconcile_route(&client, &secret, &run, &mut state, &state_path, config.port).await?;
            }
            let bad_secret = lifecycle.get("state").and_then(Value::as_str) == Some("has_invalid_secret");
            *status.write().await = observation(if bad_secret { ForwardingStatus::NeedsSetup } else { ForwardingStatus::Pending },
                if bad_secret { "Playit rejected the saved agent. Review the agent in your provider account." }
                else { "Waiting for Playit to assign the Minecraft join address. Account verification or tunnel configuration may be required." },
                None, Some(ACCOUNT.into()));
            Ok::<(), &'static str>(())
        }.await;
        if let Err(message) = result {
            // Deliberately use fixed messages: provider bodies, HTTP URLs, IPC
            // errors and subprocess output may contain secrets or login links.
            *status.write().await = observation(
                ForwardingStatus::Error,
                message,
                None,
                state
                    .claim_code
                    .as_deref()
                    .and_then(claim_url)
                    .or_else(|| Some(ACCOUNT.into())),
            );
            tokio::time::sleep(Duration::from_secs(20)).await;
        } else {
            tokio::time::sleep(Duration::from_secs(4)).await;
        }
    }
}

async fn load_state(directory: &Path, path: &Path) -> Result<PrivateState, &'static str> {
    private_directory(directory).await?;
    match bounded_file(path, MAX_JSON).await {
        Ok(bytes) if bytes.len() <= MAX_JSON => {
            let state: PrivateState = serde_json::from_slice(&bytes).map_err(|_| "Saved tunnel configuration is invalid. Restore its private state before reconnecting.")?;
            if state
                .secret
                .as_deref()
                .is_some_and(|secret| !valid_secret(secret))
            {
                return Err("Saved tunnel credentials are invalid.");
            }
            Ok(state)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(PrivateState::default()),
        _ => Err("Could not read private tunnel configuration."),
    }
}

async fn save_state(path: &Path, state: &PrivateState) -> Result<(), &'static str> {
    let bytes =
        serde_json::to_vec(state).map_err(|_| "Could not save private tunnel configuration.")?;
    private_write(path, &bytes).await
}

fn valid_secret(secret: &str) -> bool {
    (32..=512).contains(&secret.len())
        && secret.len().is_multiple_of(2)
        && secret.bytes().all(|c| c.is_ascii_hexdigit())
}

fn claim_url(code: &str) -> Option<String> {
    (code.len() == 10 && code.bytes().all(|c| c.is_ascii_hexdigit()))
        .then(|| format!("https://playit.gg/claim/{code}"))
}

async fn api(
    client: &reqwest::Client,
    endpoint: &str,
    secret: Option<&str>,
    body: Value,
) -> Result<Value, &'static str> {
    api_call(client, endpoint, secret, body)
        .await
        .map_err(|error| error.message)
}

struct ApiFailure {
    message: &'static str,
    definitive_rejection: bool,
    expired_claim: bool,
}

impl From<&'static str> for ApiFailure {
    fn from(message: &'static str) -> Self {
        Self {
            message,
            definitive_rejection: false,
            expired_claim: false,
        }
    }
}

async fn api_call(
    client: &reqwest::Client,
    endpoint: &str,
    secret: Option<&str>,
    body: Value,
) -> Result<Value, ApiFailure> {
    let mut request = client.post(format!("{API}{endpoint}")).json(&body);
    if let Some(secret) = secret {
        request = request.header(
            reqwest::header::AUTHORIZATION,
            format!("Agent-Key {secret}"),
        );
    }
    let response = request
        .send()
        .await
        .map_err(|_| "Playit could not be reached. The tunnel will retry automatically.")?;
    if !response.status().is_success() {
        return Err(ApiFailure {
            message: "Playit rejected the request. Check provider availability and account setup.",
            definitive_rejection: response.status().is_client_error()
                && response.status() != reqwest::StatusCode::REQUEST_TIMEOUT,
            expired_claim: false,
        });
    }
    let value: Value = serde_json::from_slice(&bounded_body(response, MAX_JSON).await?)
        .map_err(|_| "Playit returned an invalid response.")?;
    if value.get("status").and_then(Value::as_str) == Some("success") {
        Ok(value.get("data").cloned().unwrap_or(Value::Null))
    } else {
        Err(ApiFailure {
            message: "Playit requires account setup, email verification or an available free tunnel allocation. Open the provider dashboard.",
            definitive_rejection: matches!(
                value.get("status").and_then(Value::as_str),
                Some("fail" | "error")
            ),
            expired_claim: value.get("status").and_then(Value::as_str) == Some("fail")
                && value.get("data").and_then(Value::as_str) == Some("CodeExpired"),
        })
    }
}

fn route_config(port: u16) -> Value {
    json!({"fields":[{"name":"local_ip","value":"127.0.0.1"},{"name":"local_port","value":port.to_string()}]})
}

async fn reconcile_route(
    client: &reqwest::Client,
    secret: &str,
    run: &Value,
    state: &mut PrivateState,
    path: &Path,
    port: u16,
) -> Result<(), &'static str> {
    let agent_id = run
        .get("agent_id")
        .and_then(Value::as_str)
        .filter(|id| uuid::Uuid::parse_str(id).is_ok())
        .ok_or("Playit did not return this agent's identity.")?;
    let tunnels = run
        .get("tunnels")
        .and_then(Value::as_array)
        .ok_or("Playit did not return its tunnel configuration.")?;
    if let Some(existing) = tunnels.iter().find(|t| {
        t.get("tunnel_type").and_then(Value::as_str) == Some("minecraft-java")
            && t.get("id").and_then(Value::as_str).is_some_and(|id| {
                Some(id) == state.tunnel_id.as_deref()
                    || (state.tunnel_id.is_none() && tunnel_targets_port(t, port))
            })
    }) {
        let id = existing
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| uuid::Uuid::parse_str(id).is_ok())
            .ok_or("Playit returned an invalid tunnel identity.")?;
        if !tunnel_targets_port(existing, port) {
            api(
                client,
                "/v1/tunnels/config",
                Some(secret),
                json!({"tunnel_id":id,"new_agent_id":null,"new_config":route_config(port)}),
            )
            .await?;
        }
        state.tunnel_id = Some(id.into());
        state.configured_port = Some(port);
        state.create_requested = true;
        save_state(path, state).await?;
        return Ok(());
    }
    if state.tunnel_id.is_some()
        || state.create_requested
        || run
            .get("pending")
            .and_then(Value::as_array)
            .is_some_and(|pending| !pending.is_empty())
    {
        // A user may remove an allocation intentionally. Never silently replace
        // the persistent address or repeatedly consume their free allocations.
        return Ok(());
    }
    state.create_requested = true;
    save_state(path, state).await?;
    let created = match api_call(
        client,
        "/v1/tunnels/create",
        Some(secret),
        json!({
            "ports":{"type":"tunnel-type","details":"minecraft-java"},
            "origin":{"type":"agent","data":{"agent_id":agent_id,"config":route_config(port)}},
            "enabled":true,"alloc":null,"name":"Dawg Minecraft","firewall_id":null
        }),
    )
    .await
    {
        Ok(created) => created,
        Err(error) => {
            // Explicit rejection is safe to retry after the account is fixed;
            // connection loss remains ambiguous and retains its create marker.
            if error.definitive_rejection {
                state.create_requested = false;
                save_state(path, state).await?;
            }
            return Err(error.message);
        }
    };
    let id = created
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| uuid::Uuid::parse_str(id).is_ok())
        .ok_or(
            "Playit did not confirm tunnel creation. Check the provider dashboard before retrying.",
        )?;
    state.tunnel_id = Some(id.into());
    state.configured_port = Some(port);
    save_state(path, state).await
}

fn tunnel_targets_port(tunnel: &Value, port: u16) -> bool {
    let Some(fields) = tunnel
        .pointer("/agent_config/fields")
        .and_then(Value::as_array)
    else {
        return false;
    };
    let field = |name: &str| {
        fields
            .iter()
            .find(|field| field.get("name").and_then(Value::as_str) == Some(name))
            .and_then(|field| field.get("value"))
            .and_then(Value::as_str)
    };
    matches!(field("local_ip"), Some("127.0.0.1" | "localhost"))
        && field("local_port").and_then(|value| value.parse::<u16>().ok()) == Some(port)
}

fn matching_route(lifecycle: &Value, port: u16) -> Option<String> {
    if lifecycle.get("state").and_then(Value::as_str) != Some("running") {
        return None;
    }
    lifecycle
        .pointer("/data/tunnels")?
        .as_array()?
        .iter()
        .find_map(|tunnel| {
            let destination = tunnel.get("destination")?.as_str()?;
            if destination != format!("127.0.0.1:{port}")
                && destination != format!("localhost:{port}")
            {
                return None;
            }
            if tunnel.get("is_disabled")?.as_bool()? {
                return None;
            }
            validated_join_address(tunnel.get("display_address")?.as_str()?)
        })
}

fn validated_join_address(address: &str) -> Option<String> {
    let (host, port) = match address.rsplit_once(':') {
        Some((host, port)) => (host, Some(port.parse::<u16>().ok()?)),
        None => (address, None),
    };
    if port == Some(0) {
        return None;
    }
    let candidate = NetworkConfig {
        public_host: Some(host.into()),
        ..Default::default()
    };
    candidate.validate().ok()?;
    // Provider hostnames, not numeric IPs, deliver a stable no-domain join address.
    if host.parse::<std::net::IpAddr>().is_ok() {
        return None;
    }
    Some(address.into())
}

#[cfg(unix)]
async fn ipc_state(socket: &Path) -> Result<Value, &'static str> {
    tokio::time::timeout(Duration::from_secs(3), async {
        let stream = tokio::net::UnixStream::connect(socket)
            .await
            .map_err(|_| "Waiting for the tunnel agent's control socket.")?;
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read).take(MAX_JSON as u64);
        let mut frame = String::new();
        reader
            .read_line(&mut frame)
            .await
            .map_err(|_| "Could not read tunnel agent status.")?;
        let hello: Value = serde_json::from_str(&frame)
            .map_err(|_| "Unsupported tunnel agent control protocol.")?;
        if hello.get("message_kind").and_then(Value::as_str) != Some("hello")
            || hello
                .pointer("/data/protocol/ipc_version")
                .and_then(Value::as_u64)
                != Some(2)
        {
            return Err("Unsupported tunnel agent control protocol.");
        }
        write
            .write_all(
                b"{\"ipc_version\":2,\"request_id\":1,\"request\":{\"type\":\"get_state\"}}\n",
            )
            .await
            .map_err(|_| "Could not request tunnel agent status.")?;
        frame.clear();
        reader
            .read_line(&mut frame)
            .await
            .map_err(|_| "Could not read tunnel agent status.")?;
        let response: Value = serde_json::from_str(&frame)
            .map_err(|_| "Unsupported tunnel agent status response.")?;
        if response.get("message_kind").and_then(Value::as_str) != Some("response")
            || response.pointer("/data/request_id").and_then(Value::as_u64) != Some(1)
            || response
                .pointer("/data/ipc_version")
                .and_then(Value::as_u64)
                != Some(2)
            || response
                .pointer("/data/response/type")
                .and_then(Value::as_str)
                != Some("state")
        {
            return Err("Unsupported tunnel agent status response.");
        }
        response
            .pointer("/data/response/data")
            .cloned()
            .ok_or("Missing tunnel agent state.")
    })
    .await
    .map_err(|_| "The tunnel agent did not answer in time.")?
}

#[cfg(not(unix))]
async fn ipc_state(_: &Path) -> Result<Value, &'static str> {
    Err("Managed tunnels currently require Linux.")
}

#[derive(Serialize, Deserialize)]
struct VerifiedAsset {
    tag: String,
    name: String,
    sha256: String,
}

async fn install_agent(client: &reqwest::Client) -> Result<PathBuf, &'static str> {
    let name = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "playit-linux-amd64",
        ("linux", "aarch64") => "playit-linux-aarch64",
        ("linux", "arm") => "playit-linux-armv7",
        ("linux", "x86") => "playit-linux-i686",
        _ => return Err("Managed free tunnels currently support Linux x86 and ARM hosts."),
    };
    let directory = data_dir().join("tunnel/bin").join(format!("v{VERSION}"));
    private_directory(&directory).await?;
    let path = directory.join(name);
    let stamp_path = directory.join(format!("{name}.verified.json"));
    if let (Ok(bytes), Ok(stamp)) = (
        bounded_file(&path, MAX_BINARY).await,
        bounded_file(&stamp_path, 4096).await,
    ) && bytes.len() <= MAX_BINARY
        && stamp.len() <= 4096
        && let Ok(stamp) = serde_json::from_slice::<VerifiedAsset>(&stamp)
        && stamp.tag == format!("v{VERSION}")
        && stamp.name == name
        && stamp.sha256 == hex(&Sha256::digest(&bytes))
    {
        return Ok(path);
    }
    let response = client
        .get(format!(
            "https://api.github.com/repos/playit-cloud/playit-agent/releases/tags/v{VERSION}"
        ))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(
            |_| "Could not fetch the official Playit release metadata. Installation will retry.",
        )?;
    if !response.status().is_success() {
        return Err(
            "GitHub blocked official release metadata. The agent cannot be verified; installation will retry.",
        );
    }
    let release: Value = serde_json::from_slice(&bounded_body(response, MAX_JSON).await?)
        .map_err(|_| "Invalid official Playit release metadata.")?;
    let (url, digest) = release_asset(&release, name)?;
    let download = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() > 5 || attempt.url().scheme() != "https" {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        .build()
        .map_err(|_| "Could not prepare the verified agent download.")?;
    let response =
        download.get(url).send().await.map_err(
            |_| "Could not download the official Playit agent. Installation will retry.",
        )?;
    if !response.status().is_success() {
        return Err("The official Playit download is unavailable. Installation will retry.");
    }
    let bytes = bounded_body(response, MAX_BINARY).await?;
    if hex(&Sha256::digest(&bytes)) != digest {
        return Err("Playit binary checksum mismatch. The unverified download was discarded.");
    }
    private_write(&path, &bytes).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .await
            .map_err(|_| "Could not secure the verified agent executable.")?;
    }
    let stamp = VerifiedAsset {
        tag: format!("v{VERSION}"),
        name: name.into(),
        sha256: digest,
    };
    private_write(
        &stamp_path,
        &serde_json::to_vec(&stamp).map_err(|_| "Could not save agent verification metadata.")?,
    )
    .await?;
    Ok(path)
}

fn release_asset(release: &Value, name: &str) -> Result<(String, String), &'static str> {
    if release.get("tag_name").and_then(Value::as_str) != Some(&format!("v{VERSION}"))
        || release.get("draft").and_then(Value::as_bool) != Some(false)
    {
        return Err("The official Playit release did not match the pinned version.");
    }
    let asset = release
        .get("assets")
        .and_then(Value::as_array)
        .and_then(|assets| {
            assets
                .iter()
                .find(|a| a.get("name").and_then(Value::as_str) == Some(name))
        })
        .ok_or("The official release has no agent for this platform.")?;
    let digest = asset.get("digest").and_then(Value::as_str).and_then(|d| d.strip_prefix("sha256:"))
        .filter(|d| d.len() == 64 && d.bytes().all(|c| c.is_ascii_hexdigit())).ok_or("The official release has no SHA-256 digest. Unverified agent installation was refused.")?;
    let expected_url =
        format!("https://github.com/playit-cloud/playit-agent/releases/download/v{VERSION}/{name}");
    if asset.get("browser_download_url").and_then(Value::as_str) != Some(expected_url.as_str()) {
        return Err("The official release contained an unexpected agent download URL.");
    }
    Ok((expected_url, digest.to_ascii_lowercase()))
}

async fn bounded_body(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, &'static str> {
    if response
        .content_length()
        .is_some_and(|size| size > limit as u64)
    {
        return Err("The provider response exceeded the expected size.");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Could not read the provider response.")?
    {
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err("The provider response exceeded the expected size.");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn bounded_file(path: &Path, limit: usize) -> std::io::Result<Vec<u8>> {
    let metadata = tokio::fs::symlink_metadata(path).await?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > limit as u64 {
        return Err(std::io::Error::other(
            "Private tunnel file is not a bounded regular file",
        ));
    }
    let file = tokio::fs::File::open(path).await?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes).await?;
    if bytes.len() > limit {
        return Err(std::io::Error::other("Private tunnel file exceeds limit"));
    }
    Ok(bytes)
}

async fn private_directory(path: &Path) -> Result<(), &'static str> {
    tokio::fs::create_dir_all(path)
        .await
        .map_err(|_| "Could not create private tunnel storage.")?;
    // Secure all tunnel parents too: the data root can contain game files which
    // are intentionally readable to other services.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let root = data_dir().join("tunnel");
        let mut current = Some(path);
        while let Some(directory) = current {
            if !directory.starts_with(&root) {
                break;
            }
            let meta = tokio::fs::symlink_metadata(directory)
                .await
                .map_err(|_| "Could not inspect private tunnel storage.")?;
            if !meta.is_dir() || meta.file_type().is_symlink() {
                return Err("Private tunnel storage must not be a symbolic link.");
            }
            tokio::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
                .await
                .map_err(|_| "Could not secure private tunnel storage.")?;
            if directory == root {
                break;
            }
            current = directory.parent();
        }
    }
    Ok(())
}

async fn private_write(path: &Path, bytes: &[u8]) -> Result<(), &'static str> {
    let temporary = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let result = async {
        let mut file = options
            .open(&temporary)
            .await
            .map_err(|_| "Could not create a private tunnel file.")?;
        file.write_all(bytes)
            .await
            .map_err(|_| "Could not write a private tunnel file.")?;
        file.sync_all()
            .await
            .map_err(|_| "Could not persist private tunnel state.")?;
        drop(file);
        tokio::fs::rename(&temporary, path)
            .await
            .map_err(|_| "Could not replace private tunnel state.")
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(temporary).await;
    }
    result
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_requires_pinned_official_url_and_published_sha256() {
        let mut release = json!({"tag_name":"v1.0.12","draft":false,"assets":[{"name":"playit-linux-amd64","digest":format!("sha256:{}", "a".repeat(64)),"browser_download_url":"https://github.com/playit-cloud/playit-agent/releases/download/v1.0.12/playit-linux-amd64"}]});
        assert!(release_asset(&release, "playit-linux-amd64").is_ok());
        release["assets"][0]["digest"] = Value::Null;
        assert!(release_asset(&release, "playit-linux-amd64").is_err());
        release["assets"][0]["digest"] = json!(format!("sha256:{}", "a".repeat(64)));
        release["assets"][0]["browser_download_url"] = json!("https://untrusted.example/agent");
        assert!(release_asset(&release, "playit-linux-amd64").is_err());
    }

    #[test]
    fn only_running_enabled_routes_to_the_exact_minecraft_port_are_reported() {
        let mut lifecycle = json!({"state":"running","data":{"tunnels":[{"display_address":"example.joinmc.link","destination":"127.0.0.1:25565","is_disabled":false}]}});
        assert_eq!(
            matching_route(&lifecycle, 25565).as_deref(),
            Some("example.joinmc.link")
        );
        assert!(matching_route(&lifecycle, 8080).is_none());
        lifecycle["data"]["tunnels"][0]["is_disabled"] = json!(true);
        assert!(matching_route(&lifecycle, 25565).is_none());
        lifecycle["data"]["tunnels"][0]["is_disabled"] = json!(false);
        lifecycle["state"] = json!("starting");
        assert!(matching_route(&lifecycle, 25565).is_none());
    }

    #[test]
    fn endpoint_and_setup_links_cannot_contain_secrets_or_local_addresses() {
        for address in [
            "localhost:25565",
            "127.0.0.1:25565",
            "192.168.1.2",
            "https://evil.example/x",
            "evil.example:0",
            "evil.example:65536",
            "evil.example/path",
            "secret@evil.example",
            "1.1.1.1",
        ] {
            assert!(validated_join_address(address).is_none(), "{address}");
        }
        assert_eq!(
            validated_join_address("a.playit.gg:12345").as_deref(),
            Some("a.playit.gg:12345")
        );
        assert!(claim_url("deadbeef01").is_some());
        for code in ["short", "secret?x=1", "../account", "deadbeef0100"] {
            assert!(claim_url(code).is_none());
        }
    }

    #[test]
    fn persisted_allocation_and_ambiguous_create_survive_restart() {
        let state = PrivateState {
            tunnel_id: Some(uuid::Uuid::new_v4().to_string()),
            secret: Some("a".repeat(64)),
            create_requested: true,
            configured_port: Some(25565),
            ..Default::default()
        };
        let restored: PrivateState =
            serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        assert_eq!(restored.tunnel_id, state.tunnel_id);
        assert!(restored.create_requested);
        assert_eq!(restored.configured_port, Some(25565));
        let snapshot = serde_json::to_string(&observation(
            ForwardingStatus::Pending,
            "Waiting for provider setup.",
            None,
            None,
        ))
        .unwrap();
        assert!(!snapshot.contains(state.secret.as_deref().unwrap()));
    }

    #[tokio::test]
    async fn fake_official_ipc_state_uses_versioned_envelopes() {
        let directory =
            std::env::temp_dir().join(format!("dawg-playit-ipc-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir(&directory).await.unwrap();
        let socket = directory.join("agent.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream.write_all(b"{\"message_kind\":\"hello\",\"data\":{\"protocol\":{\"ipc_version\":2,\"capabilities\":[]}}}\n").await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut line = String::new();
            BufReader::new(read).read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["request"]["type"], "get_state");
            write.write_all(b"{\"message_kind\":\"response\",\"data\":{\"ipc_version\":2,\"request_id\":1,\"response\":{\"type\":\"state\",\"data\":{\"state\":\"starting\"}}}}\n").await.unwrap();
        });
        assert_eq!(ipc_state(&socket).await.unwrap()["state"], "starting");
        server.await.unwrap();
        tokio::fs::remove_dir_all(directory).await.unwrap();
    }
}
