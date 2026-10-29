//! Docker Engine API responses, limited to the fields the dashboard reads.
//! Fields not declared here are never deserialized.
use serde::{Deserialize, Deserializer};
use std::collections::BTreeMap;

// Docker reports absent objects as either a missing key or null.
fn nullable<'de, D: Deserializer<'de>, T: Default + Deserialize<'de>>(
    deserializer: D,
) -> Result<T, D::Error> {
    Ok(Option::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct Version {
    pub version: Option<String>,
    pub api_version: String,
    #[serde(rename = "MinAPIVersion")]
    pub min_api_version: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
pub(super) struct Info {
    pub name: Option<String>,
    pub operating_system: Option<String>,
    pub kernel_version: Option<String>,
    pub architecture: Option<String>,
    #[serde(rename = "NCPU")]
    pub ncpu: Option<u64>,
    pub mem_total: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct ContainerSummary {
    pub id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub names: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct ImageInspect {
    pub id: String,
    pub created: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct ContainerInspect {
    pub id: String,
    pub name: Option<String>,
    pub image: Option<String>,
    pub created: Option<String>,
    pub restart_count: Option<u64>,
    #[serde(default, deserialize_with = "nullable")]
    pub config: Config,
    #[serde(default, deserialize_with = "nullable")]
    pub state: State,
    #[serde(default, deserialize_with = "nullable")]
    pub host_config: HostConfig,
    #[serde(default, deserialize_with = "nullable")]
    pub network_settings: NetworkSettings,
}

pub(super) type PortMap = BTreeMap<String, Option<Vec<PortBinding>>>;

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
pub(super) struct Config {
    pub image: Option<String>,
    /// Read only to find secret values to mask; never returned.
    #[serde(deserialize_with = "nullable")]
    pub env: Vec<String>,
    pub tty: bool,
    #[serde(deserialize_with = "nullable")]
    pub healthcheck: Healthcheck,
    #[serde(deserialize_with = "nullable")]
    pub exposed_ports: BTreeMap<String, serde::de::IgnoredAny>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
pub(super) struct Healthcheck {
    #[serde(deserialize_with = "nullable")]
    pub test: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
pub(super) struct State {
    pub status: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub exit_code: Option<i64>,
    pub error: Option<String>,
    #[serde(rename = "OOMKilled")]
    pub oom_killed: Option<bool>,
    pub health: Option<Health>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
pub(super) struct Health {
    pub status: Option<String>,
    pub failing_streak: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
pub(super) struct HostConfig {
    pub network_mode: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub port_bindings: PortMap,
    #[serde(deserialize_with = "nullable")]
    pub restart_policy: RestartPolicy,
    pub memory: Option<u64>,
    pub nano_cpus: Option<u64>,
    pub cpu_quota: Option<i64>,
    pub cpu_period: Option<u64>,
    pub pids_limit: Option<i64>,
    #[serde(deserialize_with = "nullable")]
    pub log_config: LogConfig,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
pub(super) struct RestartPolicy {
    pub name: Option<String>,
    pub maximum_retry_count: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub(super) struct LogConfig {
    #[serde(rename = "Type")]
    pub kind: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
pub(super) struct NetworkSettings {
    #[serde(deserialize_with = "nullable")]
    pub ports: PortMap,
    #[serde(deserialize_with = "nullable")]
    pub networks: BTreeMap<String, Network>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
pub(super) struct PortBinding {
    pub host_ip: Option<String>,
    pub host_port: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
pub(super) struct Network {
    #[serde(rename = "IPAddress")]
    pub ip_address: Option<String>,
    #[serde(rename = "GlobalIPv6Address")]
    pub global_ipv6_address: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub(super) struct Stats {
    pub read: Option<String>,
    pub cpu_stats: CpuStats,
    pub precpu_stats: CpuStats,
    pub memory_stats: MemoryStats,
    pub networks: Option<BTreeMap<String, NetworkStats>>,
    pub blkio_stats: BlkioStats,
    pub pids_stats: PidsStats,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub(super) struct CpuStats {
    pub cpu_usage: CpuUsage,
    pub system_cpu_usage: Option<u64>,
    pub online_cpus: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub(super) struct CpuUsage {
    pub total_usage: Option<u64>,
    #[serde(deserialize_with = "nullable")]
    pub percpu_usage: Vec<u64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub(super) struct MemoryStats {
    pub usage: Option<u64>,
    pub limit: Option<u64>,
    #[serde(deserialize_with = "nullable")]
    pub stats: BTreeMap<String, u64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub(super) struct NetworkStats {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub(super) struct BlkioStats {
    pub io_service_bytes_recursive: Option<Vec<BlkioEntry>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub(super) struct BlkioEntry {
    pub op: String,
    pub value: u64,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub(super) struct PidsStats {
    pub current: Option<u64>,
}
