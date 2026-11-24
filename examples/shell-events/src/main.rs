//! The daemon event stream: first the replay of recent state, which ends with
//! a `Sync` event and lets a program build its view of the sandboxes, then
//! live events only, followed while a sandbox is created, used and deleted.
//!
//! ```sh
//! just example shell-events
//! ```

use std::time::Duration;

use anyhow::Result;
use docker_sandbox::event::EventKind;
use docker_sandbox::exec::ExecSpec;
use example_common::{connect, kit, sandbox};

#[tokio::main]
async fn main() -> Result<()> {
    let client = connect().await?;
    let package = kit::package(&client, "shell").await?;
    let name = sandbox::unique_name("events");

    // The full subscription replays recent state first, up to a sync marker.
    let mut replay = client.events().subscribe().await?;
    let mut replayed = 0;
    while let Some(event) = replay.next().await {
        if event?.is_sync() {
            break;
        }
        replayed += 1;
    }
    drop(replay);
    println!("{replayed} events replayed before the sync marker");

    // Subscribed before the sandbox exists, so that its creation is seen.
    let mut events = client.events().subscribe_live().await?;
    let watched = name.clone();
    let follower = tokio::spawn(async move {
        while let Some(event) = events.next().await {
            let Ok(event) = event else { break };
            if !event.is_about(&watched) {
                continue;
            }
            match event.kind() {
                EventKind::SandboxLifecycle => println!("event: sandbox {}", event.action),
                EventKind::SandboxPorts => println!("event: ports {}", event.action),
                EventKind::PolicyNetwork => println!("event: network {}", event.action),
                _ => println!("event: {} {}", event.event_type, event.action),
            }
            if event.action == "deleted" {
                break;
            }
        }
    });

    sandbox::with_sandbox(&client, &package, &name, |sandbox| async move {
        let _running = sandbox.ensure_running().await?;
        sandbox
            .exec(&ExecSpec::new([
                "curl",
                "-sS",
                "-o",
                "/dev/null",
                "https://example.com",
            ]))
            .await?;
        Ok(())
    })
    .await?;

    // The deletion event arrives shortly after the deletion itself.
    let _ = tokio::time::timeout(Duration::from_secs(10), follower).await;
    Ok(())
}
