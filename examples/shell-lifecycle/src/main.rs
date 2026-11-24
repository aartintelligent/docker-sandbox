//! The life of a shell sandbox step by step, with the errors a program meets
//! on the way and how it tells them apart.
//!
//! - A creation with an unknown governance profile is refused before anything
//!   is created (`Error::is_invalid_input`).
//! - The creation is followed as it happens (`Sandboxes::create_stream`), with
//!   a port published from the start, the shared skills store turned off and
//!   the credential bindings file of the host recorded, as the CLI does; a
//!   failure would come as a line of that stream.
//! - A sandbox that does not exist is `Error::is_not_found`; a command in a
//!   stopped sandbox is `Error::is_conflict`.
//! - A session (`Sandbox::hold_session`) keeps the sandbox running once it is
//!   started (`Sandbox::start_with`); the daemon stops it about 30 seconds
//!   after the last session closes.
//! - The port published from the start is part of the *desired*
//!   configuration: replacing it there changes nothing until the next start.
//!   A UDP port is published and unpublished on a running sandbox.
//! - The working directory is recorded.
//! - The deletion names the identifier returned at creation: a deletion that
//!   names another one is refused, so that a program never deletes a sandbox
//!   recreated under the same name by someone else.
//!
//! The sandbox is deleted at the end, also on an error or Ctrl-C.
//!
//! ```sh
//! just example shell-lifecycle
//! ```

use anyhow::{Context, Result, anyhow, bail, ensure};
use docker_sandbox::exec::ExecSpec;
use docker_sandbox::kit::KitPackage;
use docker_sandbox::sandbox::{
    CreateEvent, Created, DeleteOptions, PortBinding, Sandbox, SkillsMode, StartOptions,
};
use docker_sandbox::{Client, Error};
use example_common::{connect, kit, sandbox};

/// Port of the sandbox published from the start.
const SANDBOX_PORT: u16 = 8080;

/// UDP port of the sandbox published while it runs.
const UDP_PORT: u16 = 5353;

/// Working directory recorded for the sandbox.
const WORKING_DIR: &str = "/tmp";

#[tokio::main]
async fn main() -> Result<()> {
    let client = connect().await?;
    let package = kit::package(&client, "shell").await?;
    let name = sandbox::unique_name("lifecycle");
    let outcome = tokio::select! {
        outcome = run(&client, &package, &name) => outcome,
        _ = tokio::signal::ctrl_c() => Err(anyhow!("interrupted")),
    };
    // The example deletes its sandbox itself; this is for an early exit.
    match client.sandbox(&name).force_delete().await {
        Err(error) if !error.is_not_found() => eprintln!("cannot delete {name}: {error}"),
        _ => {}
    }
    outcome
}

/// Runs the life of the sandbox `name`.
async fn run(client: &Client, package: &KitPackage, name: &str) -> Result<()> {
    let base_port = 40_000 + u16::try_from(std::process::id() % 20_000)?;
    let sandbox = client.sandbox(name);
    refused_creation(client, package, name).await?;
    let created = streamed_creation(client, package, name, base_port).await?;

    let missing = client.sandbox("example-no-such-sandbox").inspect().await;
    if let Err(error) = &missing {
        ensure!(error.is_not_found(), "unexpected error: {error}");
        println!(
            "a sandbox that does not exist: not found (code {})",
            error.api_code().unwrap_or("none")
        );
    }
    // A sandbox runs right after its creation, until about 30 seconds after
    // its last session; stopped at once here.
    let info = sandbox.stop().await?;
    ensure!(!info.is_running(), "still running");
    println!("stopped: {}", info.status);
    stopped_command(&sandbox).await?;

    // A session keeps the sandbox running once started.
    let session = sandbox.hold_session().await?;
    let info = sandbox.start_with(&StartOptions::default()).await?;
    ensure!(info.is_running(), "not running");
    println!(
        "started: {}, session open: {}",
        info.status,
        session.is_connected()
    );
    port_changes(&sandbox, base_port).await?;
    sandbox.set_working_dir(WORKING_DIR).await?;
    let info = sandbox.inspect().await?;
    println!(
        "working directory recorded: {}",
        info.working_dir.as_deref().unwrap_or("(none)")
    );
    session.release();
    guarded_deletion(&sandbox, &created.id).await
}

/// Creates `name` with an unknown governance profile, which the daemon
/// refuses before creating anything.
async fn refused_creation(client: &Client, package: &KitPackage, name: &str) -> Result<()> {
    let refused = package
        .request(name)?
        .with_profile("example-no-such-profile");
    let error = package
        .create_with(client, &refused)
        .await
        .err()
        .context("a creation with an unknown profile succeeded")?;
    ensure!(error.is_invalid_input(), "unexpected error: {error}");
    ensure!(
        client
            .sandbox(name)
            .inspect()
            .await
            .is_err_and(|error| error.is_not_found()),
        "the refused sandbox exists"
    );
    println!("unknown profile: refused as invalid input, nothing created ({error})");
    Ok(())
}

/// Creates `name` with a port published from the start and no skills store,
/// following the creation as it happens.
async fn streamed_creation(
    client: &Client,
    package: &KitPackage,
    name: &str,
    host_port: u16,
) -> Result<Created> {
    let mut request = package
        .request(name)?
        .with_port(PortBinding::tcp(SANDBOX_PORT, host_port))
        .with_skills(SkillsMode::Off);
    // The CLI records the file of the credentials the user approved for
    // third-party kits; built-in kits need none.
    if let Some(home) = std::env::var_os("HOME") {
        let bindings = std::path::Path::new(&home).join(".config/sbx/credentials.yaml");
        request = request.with_bindings_path(bindings.to_string_lossy());
    }
    let loaded = package.load_images(client).await?;
    println!("{loaded} kit archives loaded, the others already on the daemon");
    let mut events = client.sandboxes().create_stream(&request).await?;
    let mut progress = 0;
    let created = loop {
        match events
            .next()
            .await
            .context("the creation stream ended early")??
        {
            CreateEvent::Progress(message) => {
                progress += 1;
                println!("  creating: {message}");
            }
            CreateEvent::Result(created) => break *created,
            CreateEvent::Error(message) => bail!("creation failed: {message}"),
            CreateEvent::Unknown(line) => println!("  unknown line: {line}"),
            other => println!("  other event: {other:?}"),
        }
    };
    println!(
        "created {} (id {}), {}, after {progress} progress lines",
        created.name, created.id, created.status
    );

    // A failure comes as a line of the stream, the answer being HTTP 200: a
    // program that relays the stream reads its lines with `parse_line`. This
    // one is the shape the daemon sends for a missing kit image.
    let failure = r#"{"error": "image is not cached locally: image \"sbx-kit-src:spec-0\""}"#;
    if let Some(CreateEvent::Error(message)) = CreateEvent::parse_line(failure)? {
        println!("a failed creation, read from its line: {message}");
    }
    Ok(created)
}

/// Replaces the desired port, which applies at the next start, then
/// publishes and unpublishes a UDP port on the running sandbox.
async fn port_changes(sandbox: &Sandbox, base_port: u16) -> Result<()> {
    println!(
        "published from the start: {}",
        ports(&sandbox.ports().await?)
    );
    let old = PortBinding::tcp(SANDBOX_PORT, base_port);
    let new = PortBinding::tcp(SANDBOX_PORT, base_port + 1);
    sandbox.replace_desired_port(&old, &new).await?;
    println!(
        "desired port replaced, published now: {}",
        ports(&sandbox.ports().await?)
    );
    sandbox.stop().await?;
    sandbox.start_with(&StartOptions::default()).await?;
    println!(
        "after a stop and a start: {}",
        ports(&sandbox.ports().await?)
    );

    let udp = PortBinding::udp(UDP_PORT, base_port + 2).with_host_ip("127.0.0.1");
    let published = sandbox.publish(&[udp]).await?;
    println!("UDP published: {}", ports(&published));
    sandbox.unpublish(&published).await?;
    println!("UDP unpublished: {}", ports(&sandbox.ports().await?));
    Ok(())
}

/// Deletes `sandbox` naming another identifier, which the daemon refuses,
/// then naming its own `id`.
async fn guarded_deletion(sandbox: &Sandbox, id: &str) -> Result<()> {
    let wrong = DeleteOptions::default()
        .forced()
        .with_expected_id("not-the-id-of-the-sandbox");
    let error = sandbox
        .delete_with(&wrong)
        .await
        .err()
        .context("a deletion with another identifier succeeded")?;
    ensure!(error.is_conflict(), "unexpected error: {error}");
    println!(
        "deletion with another identifier: refused as a conflict (code {})",
        error.api_code().unwrap_or("none")
    );
    let right = DeleteOptions::default().forced().with_expected_id(id);
    sandbox.delete_with(&right).await?;
    ensure!(
        sandbox
            .inspect()
            .await
            .is_err_and(|error| error.is_not_found()),
        "the sandbox is still there"
    );
    println!("deleted with its own identifier");
    Ok(())
}

/// Runs a command in the stopped sandbox, which the daemon refuses.
async fn stopped_command(sandbox: &Sandbox) -> Result<()> {
    let refused = sandbox.exec(&ExecSpec::new(["true"])).await;
    match refused {
        Err(error @ Error::Api(_)) if error.is_conflict() => {
            println!(
                "a command in a stopped sandbox: conflict, worth retrying once it runs: {}",
                error.is_transient()
            );
            Ok(())
        }
        Err(error) => Err(error.into()),
        Ok(_) => bail!("a command ran in a stopped sandbox"),
    }
}

/// `ports` as `host:port->sandbox_port/protocol` items.
fn ports(ports: &[PortBinding]) -> String {
    if ports.is_empty() {
        return "none".to_owned();
    }
    ports
        .iter()
        .map(|port| {
            format!(
                "{}:{}->{}/{}",
                port.host_ip.as_deref().unwrap_or("*"),
                port.host_port,
                port.sandbox_port,
                port.protocol.as_deref().unwrap_or("tcp")
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}
