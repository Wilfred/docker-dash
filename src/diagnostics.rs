use serde_json::{Value, json};
use std::collections::BTreeSet;

// Explicit projections: never return Config, HostConfig, labels, commands,
// health-check output, mount paths, or environment variables wholesale.
pub(super) fn container_details(value: &Value) -> Value {
    let mut port_names = BTreeSet::new();
    for source in [
        &value["Config"]["ExposedPorts"],
        &value["HostConfig"]["PortBindings"],
        &value["NetworkSettings"]["Ports"],
    ] {
        if let Some(ports) = source.as_object() {
            port_names.extend(ports.keys());
        }
    }
    let bindings = |source: &Value| -> Vec<Value> {
        source
            .as_array()
            .into_iter()
            .flatten()
            .map(|binding| {
                json!({
                    "host_ip": binding["HostIp"].as_str().unwrap_or(""),
                    "host_port": binding["HostPort"].as_str().unwrap_or(""),
                })
            })
            .collect()
    };
    let ports: Vec<_> = port_names
        .into_iter()
        .map(|port| {
            json!({
                "container_port": port,
                "published": bindings(&value["NetworkSettings"]["Ports"][port]),
                "configured": bindings(&value["HostConfig"]["PortBindings"][port]),
            })
        })
        .collect();
    let networks: Vec<_> = value["NetworkSettings"]["Networks"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, network)| {
            json!({
                "name": name,
                "ipv4_address": network["IPAddress"].as_str(),
                "ipv6_address": network["GlobalIPv6Address"].as_str(),
            })
        })
        .collect();
    json!({
        "created_at": value["Created"].as_str(),
        "state_error": value["State"]["Error"].as_str().filter(|error| !error.is_empty())
            .map(|error| redact(error, &secret_values(std::slice::from_ref(value)))),
        "health_status": value["State"]["Health"]["Status"].as_str(),
        "healthcheck_configured": value["Config"]["Healthcheck"]["Test"].as_array()
            .is_some_and(|test| !test.is_empty() && test[0].as_str() != Some("NONE")),
        "health_failing_streak": value["State"]["Health"]["FailingStreak"].as_u64(),
        "oom_killed": value["State"]["OOMKilled"].as_bool(),
        "restart_count": value["RestartCount"].as_u64(),
        "restart_policy": value["HostConfig"]["RestartPolicy"]["Name"].as_str(),
        "restart_max_retries": value["HostConfig"]["RestartPolicy"]["MaximumRetryCount"].as_u64(),
        "network_mode": value["HostConfig"]["NetworkMode"].as_str(),
        "ports": ports,
        "networks": networks,
        "memory_limit_bytes": value["HostConfig"]["Memory"].as_u64(),
        "nano_cpus": value["HostConfig"]["NanoCpus"].as_u64(),
        "cpu_quota": value["HostConfig"]["CpuQuota"].as_i64(),
        "cpu_period": value["HostConfig"]["CpuPeriod"].as_u64(),
        "pids_limit": value["HostConfig"]["PidsLimit"].as_i64(),
        "log_driver": value["HostConfig"]["LogConfig"]["Type"].as_str(),
    })
}

pub(super) fn host_details(value: &Value) -> Value {
    json!({
        "name": value["Name"].as_str(),
        "operating_system": value["OperatingSystem"].as_str(),
        "kernel_version": value["KernelVersion"].as_str(),
        "architecture": value["Architecture"].as_str(),
        "cpus": value["NCPU"].as_u64(),
        "memory_bytes": value["MemTotal"].as_u64(),
    })
}

fn human_bytes(bytes: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", units[unit])
}
pub(super) fn usage(container: &Value, stats: &Value) -> Value {
    let cpu = &stats["cpu_stats"];
    let previous = &stats["precpu_stats"];
    let cpu_percent = (|| {
        let delta = cpu["cpu_usage"]["total_usage"]
            .as_u64()?
            .checked_sub(previous["cpu_usage"]["total_usage"].as_u64()?)?;
        let system_delta = cpu["system_cpu_usage"]
            .as_u64()?
            .checked_sub(previous["system_cpu_usage"].as_u64()?)?;
        let cores = cpu["online_cpus"].as_u64().or_else(|| {
            cpu["cpu_usage"]["percpu_usage"]
                .as_array()
                .map(|v| v.len() as u64)
        })?;
        (system_delta > 0 && cores > 0)
            .then(|| delta as f64 / system_delta as f64 * cores as f64 * 100.0)
    })();
    let memory = &stats["memory_stats"];
    let cache = ["total_inactive_file", "inactive_file", "cache"]
        .iter()
        .find_map(|key| memory["stats"][key].as_u64())
        .unwrap_or(0);
    let used = memory["usage"]
        .as_u64()
        .map(|used| used.saturating_sub(cache));
    let limit = memory["limit"].as_u64();
    let memory_percent = used
        .zip(limit)
        .filter(|(_, limit)| *limit > 0)
        .map(|(used, limit)| used as f64 / limit as f64 * 100.0);
    let network_sum = |field: &str| {
        stats["networks"].as_object().map(|networks| {
            networks
                .values()
                .filter_map(|v| v[field].as_u64())
                .fold(0u64, u64::saturating_add)
        })
    };
    let block_sum = |op: &str| {
        stats["blkio_stats"]["io_service_bytes_recursive"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .filter(|v| {
                        v["op"]
                            .as_str()
                            .is_some_and(|value| value.eq_ignore_ascii_case(op))
                    })
                    .filter_map(|v| v["value"].as_u64())
                    .fold(0u64, u64::saturating_add)
            })
    };
    let network_io = network_sum("rx_bytes")
        .zip(network_sum("tx_bytes"))
        .map(|(rx, tx)| format!("{} / {}", human_bytes(rx), human_bytes(tx)));
    let block_io = block_sum("read")
        .zip(block_sum("write"))
        .map(|(read, write)| format!("{} / {}", human_bytes(read), human_bytes(write)));
    json!({
        "id": container["Id"].as_str(),
        "name": container["Names"][0].as_str().unwrap_or("unknown").trim_start_matches('/'),
        "sampled_at": stats["read"].as_str(),
        "cpu_percent": cpu_percent.map(|p|format!("{p:.2}%")), "cpu_percent_value": cpu_percent,
        "memory_usage": used.zip(limit).map(|(used,limit)|format!("{} / {}", human_bytes(used), human_bytes(limit))),
        "memory_usage_bytes": used, "memory_limit_bytes": limit,
        "memory_percent": memory_percent.map(|p|format!("{p:.2}%")),
        "network_io": network_io, "block_io": block_io,
        "network_rx_bytes": network_sum("rx_bytes"), "network_tx_bytes": network_sum("tx_bytes"),
        "block_read_bytes": block_sum("read"), "block_write_bytes": block_sum("write"),
        "memory_percent_value": memory_percent, "pids_current": stats["pids_stats"]["current"].as_u64(),
        "pids": stats["pids_stats"]["current"].as_u64().map(|n|n.to_string()),
    })
}

pub(super) fn secret_values(inspected: &[Value]) -> Vec<String> {
    let mut secrets = BTreeSet::new();
    for entry in inspected
        .iter()
        .flat_map(|c| c["Config"]["Env"].as_array().into_iter().flatten())
    {
        let Some((name, value)) = entry.as_str().and_then(|env| env.split_once('=')) else {
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
        if sensitive && !value.is_empty() {
            secrets.insert(value.to_owned());
            // Connection URLs often log the password separately.
            if let Some((_, rest)) = value.split_once("://")
                && let Some((credentials, _)) = rest.rsplit_once('@')
                && let Some((_, password)) = credentials.split_once(':')
                && !password.is_empty()
            {
                secrets.insert(password.to_owned());
            }
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
