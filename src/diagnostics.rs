//! JSON returned by the dashboard. Each type is an explicit projection: never
//! return Config, HostConfig, labels, commands, health-check output, mount
//! paths, or environment variables wholesale.
use super::docker;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Serialize)]
pub(super) struct Overview {
    pub docker_version: Option<String>,
    pub host: Host,
    pub host_uptime_seconds: Option<f64>,
    pub containers: Vec<Container>,
    pub checked_at: String,
}

#[derive(Serialize)]
pub(super) struct Host {
    pub name: Option<String>,
    pub operating_system: Option<String>,
    pub kernel_version: Option<String>,
    pub architecture: Option<String>,
    pub cpus: Option<u64>,
    pub memory_bytes: Option<u64>,
}
impl From<docker::Info> for Host {
    fn from(info: docker::Info) -> Self {
        Self {
            name: info.name,
            operating_system: info.operating_system,
            kernel_version: info.kernel_version,
            architecture: info.architecture,
            cpus: info.ncpu,
            memory_bytes: info.mem_total,
        }
    }
}

#[derive(Serialize)]
pub(super) struct Container {
    pub id: String,
    pub name: String,
    pub image: String,
    pub version: String,
    pub image_id: Option<String>,
    pub image_created_at: Option<String>,
    pub status: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub exit_code: Option<i64>,
    pub uptime_seconds: Option<i64>,
    pub created_at: Option<String>,
    pub state_error: Option<String>,
    pub health_status: Option<String>,
    pub healthcheck_configured: bool,
    pub health_failing_streak: Option<u64>,
    pub oom_killed: Option<bool>,
    pub restart_count: Option<u64>,
    pub restart_policy: Option<String>,
    pub restart_max_retries: Option<u64>,
    pub network_mode: Option<String>,
    pub ports: Vec<Port>,
    pub networks: Vec<NetworkAttachment>,
    pub memory_limit_bytes: Option<u64>,
    pub nano_cpus: Option<u64>,
    pub cpu_quota: Option<i64>,
    pub cpu_period: Option<u64>,
    pub pids_limit: Option<i64>,
    pub log_driver: Option<String>,
}

#[derive(Serialize)]
pub(super) struct Port {
    pub container_port: String,
    pub published: Vec<Binding>,
    pub configured: Vec<Binding>,
}

#[derive(Serialize)]
pub(super) struct Binding {
    pub host_ip: String,
    pub host_port: String,
}

#[derive(Serialize)]
pub(super) struct NetworkAttachment {
    pub name: String,
    pub ipv4_address: Option<String>,
    pub ipv6_address: Option<String>,
}

fn rfc3339(date: DateTime<Utc>) -> String {
    date.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}
fn timestamp(value: Option<&str>) -> Option<DateTime<Utc>> {
    value
        .filter(|date| !date.starts_with("0001-"))
        .and_then(|date| DateTime::parse_from_rfc3339(date).ok())
        .map(|date| date.with_timezone(&Utc))
}
fn image_version(image: &str) -> String {
    if image.starts_with("sha256:") || super::valid_id(image) {
        return "untagged".into();
    }
    if let Some((_, digest)) = image.split_once('@') {
        return digest.into();
    }
    image
        .rsplit('/')
        .next()
        .and_then(|name| name.split_once(':'))
        .map(|(_, tag)| tag.to_owned())
        .unwrap_or_else(|| "latest (implicit)".into())
}
fn bindings(ports: &docker::PortMap, port: &str) -> Vec<Binding> {
    ports
        .get(port)
        .and_then(Option::as_ref)
        .into_iter()
        .flatten()
        .map(|binding| Binding {
            host_ip: binding.host_ip.clone().unwrap_or_default(),
            host_port: binding.host_port.clone().unwrap_or_default(),
        })
        .collect()
}

pub(super) fn container(
    c: &docker::ContainerInspect,
    images: &[docker::ImageInspect],
) -> Container {
    let image_created_at = c
        .image
        .as_deref()
        .and_then(|id| images.iter().find(|image| image.id == id))
        .and_then(|image| timestamp(image.created.as_deref()))
        .map(rfc3339);
    let image = c.config.image.clone().unwrap_or_else(|| "unknown".into());
    let state = &c.state;
    let status = state.status.clone().unwrap_or_else(|| "unknown".into());
    let started = timestamp(state.started_at.as_deref());
    let uptime_seconds = started
        .filter(|_| matches!(status.as_str(), "running" | "paused"))
        .map(|start| (Utc::now() - start).num_seconds().max(0));
    let exit_code = state
        .exit_code
        .filter(|_| matches!(status.as_str(), "exited" | "dead"));
    let mut port_names = BTreeSet::new();
    port_names.extend(c.config.exposed_ports.keys());
    port_names.extend(c.host_config.port_bindings.keys());
    port_names.extend(c.network_settings.ports.keys());
    let ports = port_names
        .into_iter()
        .map(|port| Port {
            container_port: port.clone(),
            published: bindings(&c.network_settings.ports, port),
            configured: bindings(&c.host_config.port_bindings, port),
        })
        .collect();
    let networks = c
        .network_settings
        .networks
        .iter()
        .map(|(name, network)| NetworkAttachment {
            name: name.clone(),
            ipv4_address: network.ip_address.clone(),
            ipv6_address: network.global_ipv6_address.clone(),
        })
        .collect();
    let host = &c.host_config;
    let health = state.health.as_ref();
    Container {
        id: c.id.clone(),
        name: c
            .name
            .as_deref()
            .unwrap_or("unnamed")
            .trim_start_matches('/')
            .into(),
        version: image_version(&image),
        image_id: c.image.clone(),
        image_created_at,
        image,
        status,
        started_at: started.map(rfc3339),
        finished_at: timestamp(state.finished_at.as_deref()).map(rfc3339),
        exit_code,
        uptime_seconds,
        created_at: c.created.clone(),
        state_error: state
            .error
            .as_deref()
            .filter(|error| !error.is_empty())
            .map(|error| redact(error, &secret_values(c))),
        health_status: health.and_then(|h| h.status.clone()),
        healthcheck_configured: c
            .config
            .healthcheck
            .test
            .first()
            .is_some_and(|test| test != "NONE"),
        health_failing_streak: health.and_then(|h| h.failing_streak),
        oom_killed: state.oom_killed,
        restart_count: c.restart_count,
        restart_policy: host.restart_policy.name.clone(),
        restart_max_retries: host.restart_policy.maximum_retry_count,
        network_mode: host.network_mode.clone(),
        ports,
        networks,
        memory_limit_bytes: host.memory,
        nano_cpus: host.nano_cpus,
        cpu_quota: host.cpu_quota,
        cpu_period: host.cpu_period,
        pids_limit: host.pids_limit,
        log_driver: host.log_config.kind.clone(),
    }
}

#[derive(Serialize)]
pub(super) struct Usage {
    pub checked_at: String,
    pub containers: Vec<ContainerUsage>,
}

#[derive(Serialize)]
pub(super) struct ContainerUsage {
    pub id: String,
    pub name: String,
    pub sampled_at: Option<String>,
    pub cpu_percent: Option<f64>,
    pub memory_usage_bytes: Option<u64>,
    pub memory_limit_bytes: Option<u64>,
    pub memory_percent: Option<f64>,
    pub network_rx_bytes: Option<u64>,
    pub network_tx_bytes: Option<u64>,
    pub block_read_bytes: Option<u64>,
    pub block_write_bytes: Option<u64>,
    pub pids: Option<u64>,
}

pub(super) fn usage(container: &docker::ContainerSummary, stats: &docker::Stats) -> ContainerUsage {
    let cpu = &stats.cpu_stats;
    let previous = &stats.precpu_stats;
    let cpu_percent = (|| {
        let delta = cpu
            .cpu_usage
            .total_usage?
            .checked_sub(previous.cpu_usage.total_usage?)?;
        let system_delta = cpu
            .system_cpu_usage?
            .checked_sub(previous.system_cpu_usage?)?;
        let cores = cpu
            .online_cpus
            .unwrap_or(cpu.cpu_usage.percpu_usage.len() as u64);
        (system_delta > 0 && cores > 0)
            .then(|| delta as f64 / system_delta as f64 * cores as f64 * 100.0)
    })();
    let memory = &stats.memory_stats;
    let cache = ["total_inactive_file", "inactive_file", "cache"]
        .iter()
        .find_map(|key| memory.stats.get(*key).copied())
        .unwrap_or(0);
    let used = memory.usage.map(|used| used.saturating_sub(cache));
    let memory_percent = used
        .zip(memory.limit)
        .filter(|(_, limit)| *limit > 0)
        .map(|(used, limit)| used as f64 / limit as f64 * 100.0);
    let network_sum = |field: fn(&docker::NetworkStats) -> u64| {
        stats
            .networks
            .as_ref()
            .map(|networks| networks.values().map(field).fold(0u64, u64::saturating_add))
    };
    let block_sum = |op: &str| {
        stats
            .blkio_stats
            .io_service_bytes_recursive
            .as_ref()
            .map(|entries| {
                entries
                    .iter()
                    .filter(|entry| entry.op.eq_ignore_ascii_case(op))
                    .map(|entry| entry.value)
                    .fold(0u64, u64::saturating_add)
            })
    };
    ContainerUsage {
        id: container.id.clone(),
        name: container
            .names
            .first()
            .map_or("unknown", String::as_str)
            .trim_start_matches('/')
            .into(),
        sampled_at: stats.read.clone(),
        cpu_percent,
        memory_usage_bytes: used,
        memory_limit_bytes: memory.limit,
        memory_percent,
        network_rx_bytes: network_sum(|n| n.rx_bytes),
        network_tx_bytes: network_sum(|n| n.tx_bytes),
        block_read_bytes: block_sum("read"),
        block_write_bytes: block_sum("write"),
        pids: stats.pids_stats.current,
    }
}

/// Shorter values are too likely to be ordinary words or flags; masking them
/// would hide unrelated log text.
const MIN_SECRET_LEN: usize = 8;

fn maskable(value: &str) -> bool {
    value.chars().count() >= MIN_SECRET_LEN && value.parse::<f64>().is_err()
}

pub(super) fn secret_values(container: &docker::ContainerInspect) -> Vec<String> {
    let mut secrets = BTreeSet::new();
    for entry in &container.config.env {
        let Some((name, value)) = entry.split_once('=') else {
            continue;
        };
        let name = name.to_ascii_uppercase();
        let sensitive = [
            "SECRET",
            "TOKEN",
            "PASSWORD",
            "PASSWD",
            "CREDENTIAL",
            "API_KEY",
            "PRIVATE_KEY",
            "ACCESS_KEY",
            "AUTH",
            "COOKIE",
            "DATABASE_URL",
            "DSN",
        ]
        .iter()
        .any(|part| name.contains(part))
            || name.ends_with("_KEY")
            || name.ends_with("_PASS")
            || name == "KEY"
            || name == "PASS"
            || value
                .split_once("://")
                .is_some_and(|(_, rest)| rest.contains('@'));
        if sensitive && maskable(value) {
            secrets.insert(value.to_owned());
        }
        // Connection URLs often log the password separately.
        if sensitive
            && let Some((_, rest)) = value.split_once("://")
            && let Some((credentials, _)) = rest.rsplit_once('@')
            && let Some((_, password)) = credentials.split_once(':')
            && maskable(password)
        {
            secrets.insert(password.to_owned());
        }
    }
    let mut secrets: Vec<_> = secrets.into_iter().collect();
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    secrets
}

pub(super) fn redact(text: &str, secrets: &[String]) -> String {
    // Scan the original text once; never reprocess replacement markers.
    let mut output = String::new();
    let mut remaining = text;
    while !remaining.is_empty() {
        let next = secrets
            .iter()
            .filter(|secret| !secret.is_empty())
            .filter_map(|secret| remaining.find(secret).map(|pos| (pos, secret.len())))
            .min_by_key(|(pos, len)| (*pos, std::cmp::Reverse(*len)));
        let Some((pos, len)) = next else {
            output.push_str(remaining);
            break;
        };
        output.push_str(&remaining[..pos]);
        output.push_str("[REDACTED]");
        remaining = &remaining[pos + len..];
    }
    output
}

/// Redacts a log line, leaving its Docker timestamp intact for ordering.
pub(super) fn redact_log_line(line: &str, secrets: &[String]) -> String {
    match line.split_once(' ') {
        Some((time, message)) if DateTime::parse_from_rfc3339(time).is_ok() => {
            format!("{time} {}", redact(message, secrets))
        }
        _ => redact(line, secrets),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn versions_handle_registry_ports_and_digests() {
        assert_eq!(image_version("localhost:5000/team/app:1.2"), "1.2");
        assert_eq!(
            image_version("localhost:5000/team/app"),
            "latest (implicit)"
        );
        assert_eq!(image_version("app@sha256:abc"), "sha256:abc");
    }
    #[test]
    fn short_and_numeric_values_are_not_masked() {
        let container: docker::ContainerInspect = serde_json::from_value(serde_json::json!({
            "Id": "aaaaaaaaaaaa",
            "Config": {"Env": [
                "AUTH_ENABLED=true",
                "PASSWORD_MIN_LENGTH=0",
                "TOKEN_TTL=86400000",
                "API_KEY=ENV_SECRET",
                "DATABASE_URL=postgres://user:pw@db/app",
            ]}
        }))
        .unwrap();
        let secrets = secret_values(&container);
        assert_eq!(secrets, ["postgres://user:pw@db/app", "ENV_SECRET"]);
        assert_eq!(
            redact_log_line(
                "2026-01-01T00:00:00.000000000Z auth true, retries 10, key ENV_SECRET",
                &secrets
            ),
            "2026-01-01T00:00:00.000000000Z auth true, retries 10, key [REDACTED]"
        );
    }
}
