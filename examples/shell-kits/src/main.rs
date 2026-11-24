//! A mixin kit added to running shell sandboxes, the two ways the socket
//! offers: installed in place, or added by a swap of the container.
//!
//! The mixin of the example ([`example-mixin`](../example-mixin/spec.yaml))
//! writes a marker file when it is installed. It is compiled once with the
//! `shell` base kit, as a package of both, by the `sbx` CLI; each sandbox is
//! created from the `shell` kit alone, given some work in progress, then gets
//! the mixin from that package:
//!
//! - **in place** (`KitPackage::install_into`): the install command runs in
//!   the running container. Fast, but the kit list of the sandbox stays as it
//!   was, so a recreation of the container would lose the kit;
//! - **by a swap** (`KitPackage::add_to`): the daemon commits the container to
//!   an image, then recreates it with the longer kit list. The work in
//!   progress is kept, the sandbox keeps its identity, and the kit list now
//!   holds the mixin.
//!
//! A third sandbox gets both changes by hand, from the parts of the package
//! (its kit artifacts and references, the agent its base kit describes, its
//! images) and the two routes of the daemon (`Sandbox::add_kit_artifact`,
//! `Sandbox::swap_container`): what a program does to compose kits itself.
//!
//! ```sh
//! just example shell-kits
//! ```

use anyhow::{Context, Result, ensure};
use docker_sandbox::Client;
use docker_sandbox::exec::ExecSpec;
use docker_sandbox::kit::{KitAgent, KitPackage};
use docker_sandbox::sandbox::{Sandbox, SwapRequest};
use example_common::{connect, kit, sandbox};

/// The mixin kit of the example.
const MIXIN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/example-mixin");

/// The file the mixin writes when it is installed.
const MARKER: &str = "/home/agent/example-mixin.txt";

/// Work in progress, written before the kit changes.
const WORK: &str = "/home/agent/work-in-progress.txt";

/// Label of a sandbox that lists its kits, as a JSON array of references.
const KITS_LABEL: &str = "com.docker.sandbox.kits";

#[tokio::main]
async fn main() -> Result<()> {
    let client = connect().await?;
    let base = kit::package(&client, "shell").await?;
    let mixed = kit::package_with_mixin(&client, "shell", MIXIN.as_ref()).await?;

    let in_place = mixed.clone();
    let name = sandbox::unique_name("kits-in-place");
    sandbox::with_sandbox(&client, &base, &name, |sandbox| async move {
        let _running = sandbox.ensure_running().await?;
        let before = prepare(&sandbox).await?;
        let installed = in_place.install_into(&sandbox).await?;
        let after = state(&sandbox).await?;
        println!(
            "in place: {installed} mixin installed, marker {}, same sandbox: {}, kits listed: {} then {}",
            presence(after.marker),
            after.id == before.id,
            before.kits,
            after.kits
        );
        ensure!(after.marker, "the mixin did not run");
        ensure!(after.kits == before.kits, "the kit list changed");
        Ok(())
    })
    .await?;

    let swapped = mixed.clone();
    let name = sandbox::unique_name("kits-swap");
    sandbox::with_sandbox(&client, &base, &name, |sandbox| async move {
        let running = sandbox.ensure_running().await?;
        let before = prepare(&sandbox).await?;
        swap(&sandbox, &swapped).await?;
        // The new container needs a session of its own.
        drop(running);
        let _running = sandbox.ensure_running().await?;
        let after = state(&sandbox).await?;
        let work = sandbox.exec(&ExecSpec::new(["cat", WORK])).await?.success();
        println!(
            "by a swap: marker {}, work in progress {}, same sandbox: {}, kits listed: {} then {}",
            presence(after.marker),
            if work { "kept" } else { "lost" },
            after.id == before.id,
            before.kits,
            after.kits
        );
        ensure!(after.marker, "the mixin did not run");
        ensure!(work, "the work in progress was lost");
        ensure!(after.kits == before.kits + 1, "the kit list did not grow");
        Ok(())
    })
    .await?;

    by_hand(&client, &base, &mixed).await
}

/// The same two changes made by hand from the parts of the package: its kit
/// artifacts and references, its images, then the two routes of the daemon.
async fn by_hand(client: &Client, base: &KitPackage, mixed: &KitPackage) -> Result<()> {
    let artifacts = mixed.kit_artifacts();
    let refs = mixed.kit_refs();
    let agent = KitAgent::from_artifact(&artifacts[0]).context("the base kit has no manifest")?;
    println!(
        "the package holds {} kits, base first: agent {} ({}), then {}",
        refs.len(),
        agent.name,
        agent.binary.as_deref().unwrap_or("no program"),
        refs.iter().skip(1).cloned().collect::<Vec<_>>().join(", ")
    );
    let name = sandbox::unique_name("kits-by-hand");
    let mixed = mixed.clone();
    sandbox::with_sandbox(client, base, &name, |sandbox| async move {
        let running = sandbox.ensure_running().await?;
        let before = prepare(&sandbox).await?;
        // The images of the mixin first: the daemon resolves nothing itself.
        mixed.load_images(sandbox.client()).await?;
        sandbox.add_kit_artifact(&artifacts[1]).await?;
        ensure!(state(&sandbox).await?.marker, "the mixin did not run");
        println!("by hand, in place: the artifact of the mixin installed");

        let request = SwapRequest {
            docker_name_override: format!("{}-swap-by-hand", sandbox.name()),
            kit_artifacts: artifacts,
            kit_refs: refs,
        };
        sandbox.swap_container(&request).await?;
        drop(running);
        let _running = sandbox.ensure_running().await?;
        let after = state(&sandbox).await?;
        ensure!(after.kits == before.kits + 1, "the kit list did not grow");
        println!(
            "by hand, by a swap: the complete kit list, {} kits listed",
            after.kits
        );
        Ok(())
    })
    .await
}

/// What the example compares before and after a kit change.
struct State {
    /// Identifier of the sandbox.
    id: String,
    /// Number of kits the sandbox lists.
    kits: usize,
    /// Whether the marker of the mixin is there.
    marker: bool,
}

/// Writes the work in progress, checks that the mixin is not there yet, and
/// returns the state of the sandbox.
async fn prepare(sandbox: &Sandbox) -> Result<State> {
    sandbox.write_file(WORK, "half done\n").await?;
    let state = state(sandbox).await?;
    ensure!(!state.marker, "the mixin is there too early");
    Ok(state)
}

/// Adds the mixin of `package` to `sandbox` by a swap of its container.
async fn swap(sandbox: &Sandbox, package: &KitPackage) -> Result<()> {
    let outcome = package.add_to(sandbox).await?;
    println!(
        "container swapped: the previous one committed to {}",
        outcome.committed_image_id.as_deref().unwrap_or("an image")
    );
    Ok(())
}

/// The identifier, kit count and marker of `sandbox`.
async fn state(sandbox: &Sandbox) -> Result<State> {
    let info = sandbox.inspect().await?;
    let kits: Vec<String> = serde_json::from_str(
        info.labels
            .get(KITS_LABEL)
            .context("the sandbox lists no kits")?,
    )
    .context("the kit list is a JSON array")?;
    let marker = sandbox
        .exec(&ExecSpec::new(["test", "-e", MARKER]))
        .await?
        .success();
    Ok(State {
        id: info.id,
        kits: kits.len(),
        marker,
    })
}

/// `there` or `absent`.
fn presence(present: bool) -> &'static str {
    if present { "there" } else { "absent" }
}
