//! Router leases are observations, not promises of Internet reachability.
//! Snapshot readers use a separate cache; discovery never runs in an IPC request.
use dawg_core::{
    ForwardingStatus, NetworkConfig, NetworkStatus, ServerConfig, ServerStatus, format_address,
    is_public_ip,
};
use igd_next::{
    AddPortError, GatewayIpVersion, GetGenericPortMappingEntryError, PortMappingProtocol,
    SearchOptions,
    aio::{
        Gateway,
        tokio::{Tokio, search_gateway},
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    fs,
    io::AsyncWriteExt,
    sync::{Mutex, RwLock},
    time::timeout,
};

const LEASE_SECONDS: u32 = 3600;
const RENEW_MS: u64 = 1_200_000;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
const ENUMERATION_TIMEOUT: Duration = Duration::from_secs(4);
const REDISCOVER: Duration = Duration::from_secs(60);
const RETRY: Duration = Duration::from_secs(30);
const MAX_ENTRIES: u32 = 256;
type LiveGateway = Gateway<Tokio>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct OwnedLease {
    server_id: String,
    gateway_ip: IpAddr,
    external_port: u16,
    internal_ip: Ipv4Addr,
    internal_port: u16,
    renew_at_ms: u64,
    expires_at_ms: Option<u64>,
    #[serde(default)]
    checked_at_ms: u64,
}
impl OwnedLease {
    fn key(&self) -> String {
        format!(
            "{}|{}|{}",
            self.server_id, self.gateway_ip, self.external_port
        )
    }
    fn description(&self) -> String {
        format!("Dawg {}", self.server_id)
    }
    fn matches(&self, entry: &Entry) -> bool {
        entry.external_port == self.external_port
            && entry.internal_ip == self.internal_ip
            && entry.internal_port == self.internal_port
            && entry.description == self.description()
            && entry.remote_host.is_empty()
    }
    fn same_target(&self, other: &Self) -> bool {
        self.server_id == other.server_id
            && self.gateway_ip == other.gateway_ip
            && self.external_port == other.external_port
            && self.internal_ip == other.internal_ip
            && self.internal_port == other.internal_port
    }
}

#[derive(Clone)]
struct Entry {
    external_port: u16,
    internal_ip: Ipv4Addr,
    internal_port: u16,
    description: String,
    remote_host: String,
    enabled: bool,
    lease_seconds: u32,
}

/// The policy is tested with an in-memory router; tests never touch a real gateway.
trait Router {
    fn entries(&self) -> impl Future<Output = Result<Vec<Entry>, String>> + Send;
    fn add(
        &self,
        lease: &OwnedLease,
        seconds: u32,
    ) -> impl Future<Output = Result<(), AddFailure>> + Send;
    fn remove(&self, port: u16) -> impl Future<Output = Result<(), String>> + Send;
}
enum AddFailure {
    PermanentOnly,
    Other(String),
}

impl Router for LiveGateway {
    async fn entries(&self) -> Result<Vec<Entry>, String> {
        timeout(ENUMERATION_TIMEOUT, async {
            let mut entries = Vec::new();
            for index in 0..MAX_ENTRIES {
                match self.get_generic_port_mapping_entry(index).await {
                    Ok(entry) if entry.protocol == PortMappingProtocol::TCP => {
                        // A non-IP internal client cannot be proved to be our local endpoint.
                        let internal_ip = entry.internal_client.parse().map_err(|_| "The router returned a non-IP mapping target; ownership cannot be checked.".to_string())?;
                        entries.push(Entry { external_port: entry.external_port, internal_ip, internal_port: entry.internal_port, description: entry.port_mapping_description, remote_host: entry.remote_host, enabled: entry.enabled, lease_seconds: entry.lease_duration });
                    }
                    Ok(_) => {}
                    Err(GetGenericPortMappingEntryError::SpecifiedArrayIndexInvalid) => return Ok(entries),
                    Err(error) => return Err(format!("Cannot inspect existing router mappings: {error}. Configure port forwarding manually on this router.")),
                }
            }
            Err("The router has more than 256 mappings; ownership could not be checked safely.".into())
        }).await.map_err(|_| "Reading router mappings timed out; no existing mapping was overwritten.".to_string())?
    }
    async fn add(&self, lease: &OwnedLease, seconds: u32) -> Result<(), AddFailure> {
        let address = SocketAddr::new(lease.internal_ip.into(), lease.internal_port);
        let description = lease.description();
        match timeout(
            CONTROL_TIMEOUT,
            self.add_port(
                PortMappingProtocol::TCP,
                lease.external_port,
                address,
                seconds,
                &description,
            ),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(AddPortError::OnlyPermanentLeasesSupported)) => Err(AddFailure::PermanentOnly),
            Ok(Err(AddPortError::PortInUse)) => Err(AddFailure::Other(format!(
                "Public TCP port {} is already in use. Choose another public port or remove the conflicting rule yourself; Dawg did not allocate a different port.",
                lease.external_port
            ))),
            Ok(Err(error)) => Err(AddFailure::Other(format!(
                "The router rejected port forwarding: {error}"
            ))),
            Err(_) => Err(AddFailure::Other(
                "The router did not answer the port-forwarding request in time.".into(),
            )),
        }
    }
    async fn remove(&self, port: u16) -> Result<(), String> {
        match timeout(
            CONTROL_TIMEOUT,
            self.remove_port(PortMappingProtocol::TCP, port),
        )
        .await
        {
            Ok(Ok(())) | Ok(Err(igd_next::RemovePortError::NoSuchPortMapping)) => Ok(()),
            Ok(Err(error)) => Err(format!(
                "Cannot remove Dawg's previous port mapping: {error}"
            )),
            Err(_) => Err("Removing Dawg's previous port mapping timed out.".into()),
        }
    }
}

async fn ensure_mapping(
    router: &impl Router,
    wanted: &OwnedLease,
    previous: Option<&OwnedLease>,
    now: u64,
) -> Result<OwnedLease, String> {
    let entries = router.entries().await?;
    let matches: Vec<_> = entries
        .iter()
        .filter(|entry| entry.external_port == wanted.external_port)
        .collect();
    if matches.iter().any(|entry| !wanted.matches(entry)) {
        return Err(format!(
            "Public TCP port {} belongs to another endpoint or rule. Dawg left it unchanged.",
            wanted.external_port
        ));
    }
    if matches.iter().any(|entry| entry.enabled)
        && previous.is_some_and(|old| old.same_target(wanted) && old.renew_at_ms > now)
    {
        let mut verified = previous.unwrap().clone();
        verified.checked_at_ms = now;
        return Ok(verified);
    }
    match router.add(wanted, LEASE_SECONDS).await {
        Ok(()) => {}
        Err(AddFailure::PermanentOnly) => {
            // The caller records ownership atomically BEFORE this operation. The
            // enumerated endpoint and UUID must also match before any cleanup.
            router.add(wanted, 0).await.map_err(|error| match error {
                AddFailure::PermanentOnly => "The router rejected a permanent lease too.".into(),
                AddFailure::Other(error) => error,
            })?;
        }
        Err(AddFailure::Other(error)) => return Err(error),
    }
    let checked = router.entries().await?;
    let entry = checked
        .iter()
        .find(|entry| wanted.matches(entry) && entry.enabled)
        .ok_or("The router acknowledged the request but the mapping could not be verified.")?;
    let mut lease = wanted.clone();
    lease.checked_at_ms = now;
    lease.renew_at_ms = now.saturating_add(if entry.lease_seconds == 0 {
        RENEW_MS
    } else {
        RENEW_MS
            .min(u64::from(entry.lease_seconds).saturating_mul(1000) / 3)
            .max(1000)
    });
    lease.expires_at_ms = (entry.lease_seconds != 0)
        .then(|| now.saturating_add(u64::from(entry.lease_seconds).saturating_mul(1000)));
    Ok(lease)
}

async fn remove_owned(router: &impl Router, owned: &OwnedLease) -> Result<(), String> {
    let entries = router.entries().await?;
    if entries.iter().any(|entry| owned.matches(entry)) {
        if entries
            .iter()
            .any(|entry| entry.external_port == owned.external_port && !owned.matches(entry))
        {
            return Err("The router has another rule on the same public TCP port; automatic deletion was skipped.".into());
        }
        router.remove(owned.external_port).await?;
    }
    // Missing or now foreign entries are deliberately left alone.
    Ok(())
}

struct CachedStatus {
    network: NetworkConfig,
    port: u16,
    value: NetworkStatus,
}
struct Cache {
    lan_ip: Option<Ipv4Addr>,
    entries: BTreeMap<String, CachedStatus>,
}
struct GatewayState {
    gateway: LiveGateway,
    wan: Result<IpAddr, String>,
    local_ip: Ipv4Addr,
}
struct Worker {
    gateway: Option<GatewayState>,
    next_discovery: Instant,
    last_error: Option<String>,
    checked_at_ms: Option<u64>,
    leases: Vec<OwnedLease>,
    verified: BTreeSet<String>,
    attempts: BTreeMap<String, Instant>,
}

pub struct NetworkService {
    data_dir: PathBuf,
    worker: Mutex<Worker>,
    cache: RwLock<Cache>,
    stopping: AtomicBool,
}
impl Default for NetworkService {
    fn default() -> Self {
        Self::new()
    }
}
impl NetworkService {
    pub fn new() -> Self {
        Self::with_data_dir(dawg_core::data_dir())
    }
    fn with_data_dir(data_dir: PathBuf) -> Self {
        let now = now_ms();
        let leases = std::fs::read(data_dir.join("network-leases.json"))
            .ok()
            .filter(|bytes| bytes.len() <= 128 * 1024)
            .and_then(|bytes| serde_json::from_slice::<Vec<OwnedLease>>(&bytes).ok())
            .unwrap_or_default()
            .into_iter()
            .filter(|lease| {
                uuid::Uuid::parse_str(&lease.server_id).is_ok()
                    && lease.external_port != 0
                    && lease.internal_port != 0
                    && usable_local(lease.internal_ip)
                    && lease.expires_at_ms.is_none_or(|expiry| expiry > now)
            })
            .take(256)
            .collect();
        Self {
            data_dir,
            worker: Mutex::new(Worker {
                gateway: None,
                next_discovery: Instant::now(),
                last_error: None,
                checked_at_ms: None,
                leases,
                verified: BTreeSet::new(),
                attempts: BTreeMap::new(),
            }),
            cache: RwLock::new(Cache {
                lan_ip: route_ip("192.0.2.1:9".parse().unwrap()),
                entries: BTreeMap::new(),
            }),
            stopping: AtomicBool::new(false),
        }
    }

    pub async fn status(&self, config: &ServerConfig) -> NetworkStatus {
        let cache = self.cache.read().await;
        cache
            .entries
            .get(&config.id)
            .filter(|entry| entry.network == config.network && entry.port == config.port)
            .map_or_else(
                || base_status(config, cache.lan_ip),
                |entry| entry.value.clone(),
            )
    }

    pub async fn update(&self, servers: Vec<(ServerConfig, ServerStatus)>) {
        if self.stopping.load(Ordering::SeqCst) {
            return;
        }
        let lan = route_ip("192.0.2.1:9".parse().unwrap());
        let mut output = BTreeMap::new();
        {
            let mut cache = self.cache.write().await;
            cache.lan_ip = lan;
            for (config, state) in &servers {
                let mut value = base_status(config, lan);
                if wants_mapping(config, *state) {
                    if let Some(old) = cache
                        .entries
                        .get(&config.id)
                        .filter(|old| old.network == config.network && old.port == config.port)
                    {
                        value = old.value.clone();
                    }
                } else if config.network.upnp_enabled {
                    value.detail = Some("The forwarding setting is saved. Its fixed port will be restored when the server starts.".into());
                }
                output.insert(
                    config.id.clone(),
                    CachedStatus {
                        network: config.network.clone(),
                        port: config.port,
                        value,
                    },
                );
            }
            cache.entries = output
                .iter()
                .map(|(id, value)| {
                    (
                        id.clone(),
                        CachedStatus {
                            network: value.network.clone(),
                            port: value.port,
                            value: value.value.clone(),
                        },
                    )
                })
                .collect();
        }
        let active: Vec<_> = servers
            .iter()
            .filter(|(config, state)| wants_mapping(config, *state))
            .map(|(config, _)| config)
            .collect();
        let mut worker = self.worker.lock().await;
        if self.stopping.load(Ordering::SeqCst) {
            return;
        }
        let time = now_ms();
        worker
            .leases
            .retain(|lease| lease.expires_at_ms.is_none_or(|expiry| expiry > time));
        let mut fresh_gateway = false;
        if worker
            .gateway
            .as_ref()
            .is_some_and(|gateway| route_ip(gateway.gateway.addr) != Some(gateway.local_ip))
        {
            worker.next_discovery = Instant::now();
        }
        if (!active.is_empty() || !worker.leases.is_empty())
            && Instant::now() >= worker.next_discovery
        {
            fresh_gateway = true;
            worker.verified.clear();
            worker.checked_at_ms = Some(time);
            match discover().await {
                Ok(gateway) => {
                    worker.gateway = Some(gateway);
                    worker.last_error = None;
                    worker.next_discovery = Instant::now() + REDISCOVER;
                }
                Err(error) => {
                    worker.gateway = None;
                    worker.last_error = Some(error);
                    worker.next_discovery = Instant::now() + RETRY;
                }
            }
        }
        if self.stopping.load(Ordering::SeqCst) {
            return;
        }
        if let Some(gateway) = worker.gateway.as_ref() {
            let handle = gateway.gateway.clone();
            let gateway_ip = handle.addr.ip();
            let local_ip = gateway.local_ip;
            let wan = gateway.wan.clone();
            let previous = worker.leases.clone();
            for lease in previous {
                if self.stopping.load(Ordering::SeqCst) {
                    break;
                }
                let desired = active.iter().find(|config| config.id == lease.server_id);
                let keep = desired.is_some_and(|config| {
                    lease.gateway_ip == gateway_ip
                        && lease.internal_ip == local_ip
                        && lease.internal_port == config.port
                        && lease.external_port == config.network.external_port(config.port)
                });
                if !keep
                    && lease.gateway_ip == gateway_ip
                    && due(&worker.attempts, &format!("cleanup:{}", lease.key()))
                {
                    match remove_owned(&handle, &lease).await {
                        Ok(()) => {
                            worker.leases.retain(|entry| entry.key() != lease.key());
                            worker.verified.remove(&lease.key());
                        }
                        Err(error) => {
                            if let Some(status) = output.get_mut(&lease.server_id) {
                                status.value.forwarding = ForwardingStatus::Error;
                                status.value.checked_at_ms = Some(time);
                                status.value.detail = Some(format!(
                                    "Dawg could not remove its previous router rule: {error}"
                                ));
                            }
                            worker.last_error = Some(error);
                            worker
                                .attempts
                                .insert(format!("cleanup:{}", lease.key()), Instant::now() + RETRY);
                        }
                    }
                }
            }
            for config in &active {
                if self.stopping.load(Ordering::SeqCst) {
                    break;
                }
                let status = &mut output.get_mut(&config.id).unwrap().value;
                status.lan_address = Some(format_address(&local_ip.to_string(), config.port));
                status.checked_at_ms = worker.checked_at_ms;
                if !observe_wan(status, config, &wan) {
                    continue;
                }
                let wanted = OwnedLease {
                    server_id: config.id.clone(),
                    gateway_ip,
                    external_port: config.network.external_port(config.port),
                    internal_ip: local_ip,
                    internal_port: config.port,
                    renew_at_ms: 0,
                    expires_at_ms: None,
                    checked_at_ms: 0,
                };
                let previous = worker
                    .leases
                    .iter()
                    .find(|lease| lease.key() == wanted.key())
                    .cloned();
                if previous
                    .as_ref()
                    .is_some_and(|old| !old.same_target(&wanted))
                {
                    status.forwarding = ForwardingStatus::Error;
                    status.detail = Some("Dawg could not remove its previous LAN mapping safely. Check the previous router rule before retrying.".into());
                    continue;
                }
                let needs_check = fresh_gateway
                    || previous
                        .as_ref()
                        .is_none_or(|lease| lease.renew_at_ms <= time)
                    || !worker.verified.contains(&wanted.key());
                if needs_check && due(&worker.attempts, &config.id) {
                    let saved = worker.leases.clone();
                    worker.leases.retain(|lease| lease.key() != wanted.key());
                    worker.leases.push(wanted.clone());
                    // Save ownership before any router mutation, including permanent-only fallback.
                    if let Err(error) = persist(&self.data_dir, &worker.leases).await {
                        worker.leases = saved;
                        worker.verified.remove(&wanted.key());
                        status.forwarding = ForwardingStatus::Error;
                        status.detail = Some(format!(
                            "Port forwarding was not changed because its ownership record could not be saved: {error}"
                        ));
                        worker
                            .attempts
                            .insert(config.id.clone(), Instant::now() + RETRY);
                        continue;
                    }
                    status.checked_at_ms = Some(time);
                    match ensure_mapping(&handle, &wanted, previous.as_ref(), time).await {
                        Ok(lease) => {
                            worker.leases.retain(|entry| entry.key() != lease.key());
                            worker.verified.insert(lease.key());
                            worker.leases.push(lease);
                            worker.attempts.remove(&config.id);
                        }
                        Err(error) => {
                            worker.verified.remove(&wanted.key());
                            worker
                                .attempts
                                .insert(config.id.clone(), Instant::now() + RETRY);
                            status.forwarding = ForwardingStatus::Error;
                            status.detail = Some(error);
                        }
                    }
                }
                if worker.verified.contains(&wanted.key()) {
                    status.forwarding = ForwardingStatus::Active;
                    status.checked_at_ms = worker
                        .leases
                        .iter()
                        .find(|lease| lease.key() == wanted.key())
                        .map(|lease| lease.checked_at_ms);
                    status.detail = Some(if config.network.public_host.is_some() {
                        "The router's fixed TCP mapping is verified and will be restored and renewed automatically. The saved hostname/IP and its DNS/DDNS must be configured separately; access from the Internet has not been tested."
                    } else {
                        "The router's fixed TCP mapping is verified and will be restored and renewed automatically. The displayed WAN IP may change; configure a DNS/DDNS hostname for a stable join address. Access from the Internet has not been tested."
                    }.into());
                }
            }
            if let Err(error) = persist(&self.data_dir, &worker.leases).await {
                worker.last_error =
                    Some(format!("Could not save the router lease record: {error}"));
            }
        } else {
            for config in &active {
                let status = &mut output.get_mut(&config.id).unwrap().value;
                let baseline = base_status(config, lan);
                status.join_address = baseline.join_address;
                status.external_ip = baseline.external_ip;
                status.forwarding = ForwardingStatus::Unavailable;
                status.checked_at_ms = worker.checked_at_ms;
                status.detail = Some(
                    worker
                        .last_error
                        .clone()
                        .unwrap_or_else(|| "Waiting to discover the local router.".into()),
                );
            }
        }
        self.cache.write().await.entries = output;
    }

    pub async fn shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        let mut worker = self.worker.lock().await;
        if worker.leases.is_empty() {
            return;
        }
        let gateway = if let Some(gateway) = &worker.gateway {
            Some(gateway.gateway.clone())
        } else {
            discover().await.ok().map(|gateway| gateway.gateway)
        };
        if let Some(gateway) = gateway {
            let previous = worker.leases.clone();
            let _ = timeout(Duration::from_secs(12), async {
                for lease in previous {
                    if lease.gateway_ip == gateway.addr.ip()
                        && remove_owned(&gateway, &lease).await.is_ok()
                    {
                        worker.leases.retain(|entry| entry.key() != lease.key());
                    }
                }
            })
            .await;
        }
        if let Err(error) = persist(&self.data_dir, &worker.leases).await {
            eprintln!("Network lease cleanup record: {error}");
        }
    }
}

fn due(attempts: &BTreeMap<String, Instant>, id: &str) -> bool {
    attempts.get(id).is_none_or(|next| Instant::now() >= *next)
}
fn observe_wan(
    status: &mut NetworkStatus,
    config: &ServerConfig,
    wan: &Result<IpAddr, String>,
) -> bool {
    // Never retain an old public WAN address after a failed query or CGNAT.
    status.join_address = config
        .network
        .public_host
        .as_ref()
        .map(|host| format_address(host, config.network.external_port(config.port)));
    status.external_ip = None;
    match wan {
        Ok(ip) => {
            status.external_ip = Some(ip.to_string());
            if !is_public_ip(*ip) {
                status.forwarding = ForwardingStatus::Unavailable;
                status.detail = Some(format!(
                    "The router reports WAN address {ip}, which is private or shared (CGNAT/double NAT). A rule on this router cannot provide public access by itself. Use the upstream router, a public VPS, or a tunnel."
                ));
                return false;
            }
            if status.join_address.is_none() {
                status.join_address = Some(format_address(
                    &ip.to_string(),
                    config.network.external_port(config.port),
                ));
            }
            true
        }
        Err(error) => {
            status.forwarding = ForwardingStatus::Unavailable;
            status.detail = Some(error.clone());
            false
        }
    }
}
fn wants_mapping(config: &ServerConfig, state: ServerStatus) -> bool {
    config.network.upnp_enabled && matches!(state, ServerStatus::Starting | ServerStatus::Running)
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn usable_local(ip: Ipv4Addr) -> bool {
    !ip.is_unspecified() && !ip.is_loopback() && !ip.is_multicast() && !ip.is_broadcast()
}
fn route_ip(destination: SocketAddr) -> Option<Ipv4Addr> {
    // UDP connect chooses the kernel route without sending a packet or using DNS.
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect(destination).ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if usable_local(ip) => Some(ip),
        _ => None,
    }
}
fn base_status(config: &ServerConfig, local: Option<Ipv4Addr>) -> NetworkStatus {
    let external_port = config.network.external_port(config.port);
    let direct_ip = local.map(IpAddr::V4).filter(|ip| is_public_ip(*ip));
    NetworkStatus {
        lan_address: local.map(|ip| format_address(&ip.to_string(), config.port)),
        external_ip: direct_ip.map(|ip| ip.to_string()),
        join_address: config.network.public_host.as_ref().map(|host| format_address(host, external_port))
            .or_else(|| direct_ip.map(|ip| format_address(&ip.to_string(), external_port))),
        forwarding: if config.network.upnp_enabled { ForwardingStatus::Pending } else { ForwardingStatus::Disabled },
        detail: Some(if config.network.upnp_enabled {
            "Waiting for the server to start and its router mapping to be checked."
        } else if config.network.public_host.is_some() {
            "The saved public hostname/IP is displayed. Configure any required router/firewall rules and DNS/DDNS separately; Internet reachability is unverified."
        } else if direct_ip.is_some() {
            "A public interface address was detected. The host firewall and public access have not been checked; this does not guarantee a permanent IP."
        } else {
            "Only a LAN address is known. Enable router forwarding, use a tunnel, or configure an assigned public IP or DNS/DDNS hostname."
        }.into()),
        ..Default::default()
    }
}
async fn discover() -> Result<GatewayState, String> {
    let mut options = SearchOptions::default();
    options.timeout = Some(Duration::from_secs(3));
    options.single_search_timeout = Some(Duration::from_secs(1));
    options.gateway_ip_version = GatewayIpVersion::V4;
    let gateway = timeout(Duration::from_secs(4), search_gateway(options)).await
        .map_err(|_| "Router discovery timed out. Enable UPnP on the home router or set up a manual forwarding rule.".to_string())?
        .map_err(|error| format!("No usable UPnP router was found ({error}). Home: enable UPnP or add a manual TCP rule. VPS: configure its firewall or use a tunnel."))?;
    let local_ip = route_ip(gateway.addr)
        .ok_or("Cannot determine the LAN address used to reach the router.")?;
    let wan = timeout(CONTROL_TIMEOUT, gateway.get_external_ip())
        .await
        .map_err(|_| "The router's WAN-address request timed out.".to_string())
        .and_then(|result| {
            result.map_err(|error| format!("Cannot read the router's WAN address: {error}"))
        });
    Ok(GatewayState {
        gateway,
        wan,
        local_ip,
    })
}
async fn persist(data_dir: &Path, leases: &[OwnedLease]) -> Result<(), String> {
    if leases.is_empty() && !data_dir.join("network-leases.json").exists() {
        return Ok(());
    }
    let bytes = serde_json::to_vec_pretty(leases).map_err(|error| error.to_string())?;
    let destination = data_dir.join("network-leases.json");
    if fs::symlink_metadata(&destination)
        .await
        .is_ok_and(|metadata| {
            metadata.file_type().is_file() && metadata.permissions().mode() & 0o777 == 0o600
        })
        && fs::read(&destination)
            .await
            .is_ok_and(|current| current == bytes)
    {
        return Ok(());
    }
    let temporary = data_dir.join(format!(".network-leases-{}.tmp", uuid::Uuid::new_v4()));
    let result = async {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)
            .await
            .map_err(|error| error.to_string())?;
        file.write_all(&bytes)
            .await
            .map_err(|error| error.to_string())?;
        file.sync_all().await.map_err(|error| error.to_string())?;
        drop(file);
        fs::rename(&temporary, &destination)
            .await
            .map_err(|error| error.to_string())?;
        // The ownership filename must survive a crash before requesting a
        // permanent router rule, not only the temporary file's contents.
        fs::File::open(data_dir)
            .await
            .map_err(|error| error.to_string())?
            .sync_all()
            .await
            .map_err(|error| error.to_string())
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(temporary).await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    #[derive(Default)]
    struct MockState {
        entries: Vec<Entry>,
        adds: Vec<(u16, Ipv4Addr, u16, u32)>,
        removes: Vec<u16>,
        permanent_only: bool,
        fail_enumeration: bool,
    }
    #[derive(Default)]
    struct MockRouter(StdMutex<MockState>);
    impl Router for MockRouter {
        async fn entries(&self) -> Result<Vec<Entry>, String> {
            let state = self.0.lock().unwrap();
            if state.fail_enumeration {
                Err("Router does not support inspection".into())
            } else {
                Ok(state.entries.clone())
            }
        }
        async fn add(&self, lease: &OwnedLease, seconds: u32) -> Result<(), AddFailure> {
            let mut state = self.0.lock().unwrap();
            state.adds.push((
                lease.external_port,
                lease.internal_ip,
                lease.internal_port,
                seconds,
            ));
            if state.permanent_only && seconds != 0 {
                return Err(AddFailure::PermanentOnly);
            }
            state
                .entries
                .retain(|entry| entry.external_port != lease.external_port);
            state.entries.push(Entry {
                external_port: lease.external_port,
                internal_ip: lease.internal_ip,
                internal_port: lease.internal_port,
                description: lease.description(),
                remote_host: String::new(),
                enabled: true,
                lease_seconds: seconds,
            });
            Ok(())
        }
        async fn remove(&self, port: u16) -> Result<(), String> {
            let mut state = self.0.lock().unwrap();
            state.removes.push(port);
            state.entries.retain(|entry| entry.external_port != port);
            Ok(())
        }
    }

    fn lease() -> OwnedLease {
        OwnedLease {
            server_id: "00000000-0000-4000-8000-000000000001".into(),
            gateway_ip: "192.168.1.1".parse().unwrap(),
            external_port: 25566,
            internal_ip: "192.168.1.50".parse().unwrap(),
            internal_port: 25565,
            renew_at_ms: 0,
            expires_at_ms: None,
            checked_at_ms: 0,
        }
    }
    fn config() -> ServerConfig {
        ServerConfig {
            id: lease().server_id,
            name: "Test server".into(),
            directory: "/unused".into(),
            kind: dawg_core::ServerKind::Vanilla,
            minecraft_version: "1.21.11".into(),
            memory_mb: 2048,
            port: 25565,
            java_path: "java".into(),
            eula_accepted: true,
            network: NetworkConfig {
                external_port: Some(25566),
                ..Default::default()
            },
        }
    }
    fn temp_dir() -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("dawg-network-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        directory
    }

    #[tokio::test]
    async fn restores_and_renews_the_same_requested_port_after_router_restart() {
        let router = MockRouter::default();
        let wanted = lease();
        let initial = ensure_mapping(&router, &wanted, None, 10_000)
            .await
            .unwrap();
        assert_eq!(initial.external_port, 25566);
        assert!(initial.expires_at_ms.unwrap() > initial.renew_at_ms);
        let checked = ensure_mapping(&router, &wanted, Some(&initial), 20_000)
            .await
            .unwrap();
        assert_eq!(checked.checked_at_ms, 20_000);
        assert_eq!(router.0.lock().unwrap().adds.len(), 1);
        let renewed = ensure_mapping(&router, &wanted, Some(&initial), initial.renew_at_ms)
            .await
            .unwrap();
        assert_eq!(router.0.lock().unwrap().adds.len(), 2);
        router.0.lock().unwrap().entries.clear(); // A reboot loses the rule before its lease expires.
        ensure_mapping(
            &router,
            &wanted,
            Some(&renewed),
            renewed.checked_at_ms + 100,
        )
        .await
        .unwrap();
        let state = router.0.lock().unwrap();
        assert_eq!(state.adds.len(), 3);
        assert!(
            state
                .adds
                .iter()
                .all(|(public, local, internal, seconds)| *public == 25566
                    && *local == wanted.internal_ip
                    && *internal == 25565
                    && *seconds == LEASE_SECONDS)
        );
    }

    #[tokio::test]
    async fn conflicts_and_uninspectable_routers_never_change_an_existing_rule() {
        let router = MockRouter::default();
        let wanted = lease();
        ensure_mapping(&router, &wanted, None, 10_000)
            .await
            .unwrap();
        {
            let mut state = router.0.lock().unwrap();
            state.entries[0].description = "Another application".into();
            state.adds.clear();
        }
        let error = ensure_mapping(&router, &wanted, None, 20_000)
            .await
            .unwrap_err();
        assert!(error.contains("left it unchanged"));
        remove_owned(&router, &wanted).await.unwrap();
        assert!(router.0.lock().unwrap().adds.is_empty());
        assert!(router.0.lock().unwrap().removes.is_empty());
        router.0.lock().unwrap().fail_enumeration = true;
        assert!(
            ensure_mapping(&router, &wanted, None, 30_000)
                .await
                .is_err()
        );
        assert!(remove_owned(&router, &wanted).await.is_err());
        assert!(router.0.lock().unwrap().adds.is_empty());
        assert!(router.0.lock().unwrap().removes.is_empty());
    }

    #[tokio::test]
    async fn permanent_only_routers_keep_the_fixed_port_and_ownership_record() {
        let router = MockRouter::default();
        router.0.lock().unwrap().permanent_only = true;
        let wanted = lease();
        let actual = ensure_mapping(&router, &wanted, None, 10_000)
            .await
            .unwrap();
        assert_eq!(actual.expires_at_ms, None);
        let state = router.0.lock().unwrap();
        assert_eq!(
            state
                .adds
                .iter()
                .map(|entry| (entry.0, entry.3))
                .collect::<Vec<_>>(),
            vec![(25566, 3600), (25566, 0)]
        );
    }

    #[tokio::test]
    async fn lan_changes_remove_only_the_exact_old_rule_before_recreating() {
        let router = MockRouter::default();
        let wanted = lease();
        let old = ensure_mapping(&router, &wanted, None, 10_000)
            .await
            .unwrap();
        let mut changed = wanted.clone();
        changed.internal_ip = "192.168.1.99".parse().unwrap();
        assert!(
            ensure_mapping(&router, &changed, Some(&old), 20_000)
                .await
                .is_err()
        );
        remove_owned(&router, &old).await.unwrap();
        ensure_mapping(&router, &changed, None, 20_000)
            .await
            .unwrap();
        let state = router.0.lock().unwrap();
        assert_eq!(state.removes, [25566]);
        assert_eq!(state.entries[0].internal_ip, changed.internal_ip);
        assert_eq!(state.entries[0].external_port, old.external_port);
    }

    #[tokio::test]
    async fn ambiguous_remote_host_rules_are_not_deleted_by_port() {
        let router = MockRouter::default();
        let wanted = lease();
        ensure_mapping(&router, &wanted, None, 10_000)
            .await
            .unwrap();
        {
            let mut state = router.0.lock().unwrap();
            let mut other = state.entries[0].clone();
            other.remote_host = "1.1.1.1".into();
            state.entries.push(other);
        }
        assert!(remove_owned(&router, &wanted).await.is_err());
        assert!(router.0.lock().unwrap().removes.is_empty());
    }

    #[test]
    fn private_or_missing_wan_discards_a_stale_public_join_address() {
        let config = config();
        for ip in ["100.64.0.1", "10.0.0.1", "192.168.1.1", "127.0.0.1"] {
            let mut status = NetworkStatus {
                join_address: Some("1.1.1.1:25566".into()),
                forwarding: ForwardingStatus::Active,
                ..Default::default()
            };
            assert!(!observe_wan(&mut status, &config, &Ok(ip.parse().unwrap())));
            assert_eq!(status.forwarding, ForwardingStatus::Unavailable);
            assert_eq!(status.join_address, None);
            assert!(status.detail.unwrap().contains("CGNAT"));
        }
        let mut status = NetworkStatus {
            external_ip: Some("1.1.1.1".into()),
            join_address: Some("1.1.1.1:25566".into()),
            ..Default::default()
        };
        assert!(!observe_wan(
            &mut status,
            &config,
            &Err("WAN query failed".into())
        ));
        assert_eq!(status.external_ip, None);
        assert_eq!(status.join_address, None);
    }

    #[test]
    fn configured_hostname_is_preserved_without_claiming_dns_or_reachability() {
        let mut config = config();
        config.network.public_host = Some("dawg.example.net".into());
        let status = base_status(&config, Some("192.168.1.50".parse().unwrap()));
        assert_eq!(
            status.join_address.as_deref(),
            Some("dawg.example.net:25566")
        );
        assert_eq!(status.lan_address.as_deref(), Some("192.168.1.50:25565"));
        assert_eq!(status.external_ip, None);
        assert_eq!(status.forwarding, ForwardingStatus::Disabled);
        assert!(status.detail.unwrap().contains("unverified"));
        config.network.public_host = None;
        assert_eq!(
            base_status(&config, Some("192.168.1.50".parse().unwrap())).join_address,
            None
        );
        assert_eq!(
            base_status(&config, Some("1.1.1.1".parse().unwrap()))
                .join_address
                .as_deref(),
            Some("1.1.1.1:25566")
        );
    }

    #[tokio::test]
    async fn status_reads_do_not_wait_for_the_router_worker() {
        let directory = temp_dir();
        let service = NetworkService::with_data_dir(directory.clone());
        let _worker = service.worker.lock().await;
        let snapshot = timeout(Duration::from_millis(100), service.status(&config()))
            .await
            .unwrap();
        assert_eq!(snapshot.forwarding, ForwardingStatus::Disabled);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn owned_lease_metadata_is_private_durable_and_restored_after_restart() {
        use std::os::unix::fs::PermissionsExt;
        let directory = temp_dir();
        let permanent = lease();
        persist(&directory, std::slice::from_ref(&permanent))
            .await
            .unwrap();
        let path = directory.join("network-leases.json");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let service = NetworkService::with_data_dir(directory.clone());
        assert_eq!(service.worker.lock().await.leases, [permanent]);
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn forwarding_is_only_requested_for_starting_or_running_servers() {
        let mut config = config();
        assert!(!wants_mapping(&config, ServerStatus::Running));
        config.network.upnp_enabled = true;
        assert!(wants_mapping(&config, ServerStatus::Starting));
        assert!(wants_mapping(&config, ServerStatus::Running));
        for status in [
            ServerStatus::Stopped,
            ServerStatus::Installing,
            ServerStatus::Stopping,
            ServerStatus::Crashed,
        ] {
            assert!(!wants_mapping(&config, status));
        }
    }
}
