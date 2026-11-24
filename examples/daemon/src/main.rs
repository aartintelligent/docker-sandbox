//! What a program reads of the daemon to watch it, and the few things it
//! changes on it: a client configured before it connects, the health and
//! version of the daemon, its settings and log levels, the disk it uses, the
//! state of the network policy, its internal state and diagnostics, then the
//! resources and the VM metrics of a sandbox created with limits.
//!
//! One setting and one log level are changed, then restored to what they
//! were, also on an error: the settings are those of the user, shared by
//! every sandbox. The sandbox is deleted when the example ends.
//!
//! ```sh
//! just example daemon
//! ```

use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use docker_sandbox::daemon::Daemon;
use docker_sandbox::{Client, VersionPolicy, default_socket_path};
use example_common::{kit, sandbox};
use serde_json::Value;

/// Virtual CPUs of the sandbox of the example.
const CPUS: u32 = 1;

/// Memory limit of the sandbox of the example, in `sbx` notation.
const MEMORY: &str = "2g";

/// Metric families shown, out of the many the VM exposes: the memory of the
/// guest, its network traffic, the processes the kernel killed for memory.
const WATCHED: [&str; 5] = [
    "sailor_guest_mem_total_pages",
    "sailor_guest_mem_available_pages",
    "sailor_guest_net_rx_bytes_total",
    "sailor_guest_net_tx_bytes_total",
    "sailor_guest_oom_kill_total",
];

/// The setting changed then restored, as the live tests of the library do.
const SETTING: &str = "sandbox.disk.dockerVolume";

/// Its value while it is changed.
const SETTING_VALUE: &str = "12g";

/// The log category changed then restored.
const LOG_TARGET: &str = "proxy";

#[tokio::main]
async fn main() -> Result<()> {
    let client = connect().await?;
    let daemon = client.daemon();
    report(&client).await?;
    changed_and_restored(&daemon).await?;
    internals(&daemon).await?;
    sandbox_with_limits(&client).await
}

/// Connects with explicit options: the socket of the user, the deadlines of
/// requests and of long operations, and a refusal of an API version the
/// library was not checked against.
async fn connect() -> Result<Client> {
    let socket = default_socket_path().context("no state directory to find the socket in")?;
    let client = Client::builder()
        .with_socket(&socket)
        .with_timeout(Duration::from_secs(30))
        .with_long_operation_timeout(Duration::from_secs(20 * 60))
        .with_version_policy(VersionPolicy::Strict)
        .connect()
        .await
        .context("cannot reach sandboxd: is Docker Sandboxes (sbx) installed and running?")?;
    ensure!(client.socket_path() == socket, "another socket");
    println!(
        "connected to {} of the state directory of the user",
        socket.file_name().unwrap_or_default().to_string_lossy()
    );
    Ok(client)
}

/// Reads the health, the settings changed from their default, the log
/// levels, the disk usage and the state of the network policy.
async fn report(client: &Client) -> Result<()> {
    let daemon = client.daemon();
    // The health answered at connection time, and asked again.
    let health = daemon.health().await?;
    println!(
        "daemon {} (API {}), {}",
        health.version,
        health.api_version,
        if health.is_healthy() {
            "healthy"
        } else {
            health.status.as_str()
        }
    );

    // The settings that differ from their default, and where they come from:
    // the user, or the remote configuration of Docker.
    let settings = daemon.settings().await?;
    let changed: Vec<_> = settings
        .iter()
        .filter(|setting| setting.value != setting.default)
        .collect();
    println!(
        "{} settings, {} of them changed from their default:",
        settings.len(),
        changed.len()
    );
    for setting in changed {
        let source = setting.source.as_deref().unwrap_or("unknown source");
        println!("  {} = {} ({source})", setting.key, setting.value);
    }
    for (target, level) in daemon.log_levels().await? {
        println!("log level of {target}: {level}");
    }

    for summary in daemon.disk_usage().await?.summaries {
        println!(
            "disk: {:<14} {:>3} ({} in use), {}, {} reclaimable",
            summary.resource_type,
            summary.total_count,
            summary.active,
            size(summary.size),
            size(summary.reclaimable)
        );
    }

    let policy = client.policy();
    println!(
        "network policy: {}, {} profiles",
        if policy.setup_needed().await? {
            "to be set up with the sbx CLI"
        } else {
            "set up"
        },
        policy.profiles().await?.len()
    );
    Ok(())
}

/// Changes [`SETTING`] and the log level of [`LOG_TARGET`], then restores
/// both, also when a change fails.
async fn changed_and_restored(daemon: &Daemon) -> Result<()> {
    let before = daemon.setting(SETTING).await?;
    let changed = daemon.set_setting(SETTING, SETTING_VALUE).await;
    // A default value is restored by removing the override, any other by
    // setting it again.
    let restored = if before.source.as_deref() == Some("default") {
        daemon.unset_setting(SETTING).await
    } else {
        daemon
            .set_setting(SETTING, before.value.clone())
            .await
            .map(drop)
    };
    let changed = changed?;
    restored?;
    let after = daemon.setting(SETTING).await?;
    ensure!(
        (&after.value, &after.source) == (&before.value, &before.source),
        "{SETTING} was not restored"
    );
    println!(
        "{SETTING}: {} ({}), set to {} ({}), restored to {} ({})",
        before.value,
        before.source.as_deref().unwrap_or("?"),
        changed.value,
        changed.source.as_deref().unwrap_or("?"),
        after.value,
        after.source.as_deref().unwrap_or("?")
    );

    let levels = daemon.log_levels().await?;
    let previous = levels
        .get(LOG_TARGET)
        .context("no log level for the proxy")?
        .clone();
    let changed = daemon.set_log_level(LOG_TARGET, "debug").await;
    let during = daemon.log_levels().await;
    daemon.set_log_level(LOG_TARGET, &previous).await?;
    changed?;
    let during = during?;
    ensure!(
        daemon.log_levels().await? == levels,
        "log levels not restored"
    );
    println!(
        "log level of {LOG_TARGET}: {previous}, set to {}, restored to {previous}",
        during.get(LOG_TARGET).map_or("?", String::as_str)
    );
    Ok(())
}

/// The top-level parts of the internal state and of the diagnostics report,
/// whose shapes are not stable: secrets show there as `***`.
async fn internals(daemon: &Daemon) -> Result<()> {
    let keys = |value: &Value| {
        value
            .as_object()
            .map(|object| object.keys().cloned().collect::<Vec<_>>().join(", "))
            .unwrap_or_default()
    };
    println!("internal state: {}", keys(&daemon.debug_state().await?));
    println!("diagnostics: {}", keys(&daemon.diagnostics().await?));
    Ok(())
}

/// Creates a sandbox with CPU and memory limits, and reads what the daemon
/// records and what its VM reports.
async fn sandbox_with_limits(client: &Client) -> Result<()> {
    let package = kit::package(client, "shell").await?;
    let request = package
        .request(sandbox::unique_name("daemon"))?
        .with_cpus(CPUS)
        .with_memory(MEMORY);
    sandbox::with_sandbox_from(client, &package, &request, |sandbox| async move {
        let _running = sandbox.ensure_running().await?;
        let info = sandbox.inspect().await?;
        println!(
            "{}: {} CPU, {} of memory",
            info.name,
            info.cpus
                .map_or_else(|| "?".to_owned(), |cpus| cpus.to_string()),
            info.memory.as_deref().unwrap_or("?")
        );
        // Prometheus text: one sample per line, named by its family.
        let metrics = sandbox.vm_metrics().await?;
        let samples: Vec<&str> = metrics
            .lines()
            .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
            .collect();
        let families: BTreeSet<&str> = samples
            .iter()
            .filter_map(|line| line.split(['{', ' ']).next())
            .collect();
        println!(
            "VM metrics: {} samples in {} families, among them:",
            samples.len(),
            families.len()
        );
        for line in samples
            .iter()
            .filter(|line| WATCHED.iter().any(|family| line.starts_with(family)))
        {
            println!("  {line}");
        }
        Ok(())
    })
    .await
}

/// `bytes` in the largest binary unit that keeps a whole part.
fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes;
    let mut unit = 0;
    while value >= 1024 && unit < UNITS.len() - 1 {
        value /= 1024;
        unit += 1;
    }
    format!("{value} {}", UNITS[unit])
}
