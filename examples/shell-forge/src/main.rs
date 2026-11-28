//! A shell sandbox from a kit package forged through the socket alone: no
//! `sbx` CLI, not even once.
//!
//! The other examples compile their kits once with the CLI
//! ([`common::kit`](../../common/src/kit.rs)); `KitForge` writes the same
//! package itself:
//!
//! - the **base kit** is `docker.io/sbx/shell:latest`, rebuilt from its
//!   published image: the daemon pulls and exports it, and the descriptor of
//!   the kit is read from the image;
//! - a **mixin built in Rust** (`KitDescriptor`, `LifecycleStep`): an install
//!   command given a variable of the sandbox, a startup command run in the
//!   background, a host allowed and one denied while the sandbox runs; it is
//!   also written as a kit directory the CLI reads (`Mixin::save`);
//! - a **mixin read from its directory**, [`json-mixin`](../json-mixin/spec.yaml),
//!   a `spec.yaml` written in the JSON subset of YAML (`Mixin::read`).
//!
//! Before anything is forged, the example shows what a forged mixin may not
//! declare: the library refuses it with a message, rather than sending the
//! daemon a kit it would half apply. Its error tells a refused kit
//! (`Error::InvalidKit`) from a failure of the client, which it keeps as it
//! is (`Error::Client`): a mixin directory that does not exist. It then shows that the package is a
//! creation request and its images, the request assembled again from its
//! parts (`CreateRequest::for_agent`). Then it creates the sandbox and checks
//! that both mixins ran, and that the daemon applied the network rule of the
//! first and removed it with the sandbox. The package is built in memory
//! (`KitForge::build`), its images too (`ImageArchive::Memory`); the same
//! package is assembled by hand from its request and its images
//! (`KitPackage::in_memory`, `PackageImage::in_memory`), as a kit forger of
//! one's own would. Nothing is written until the example saves it
//! (`KitPackage::save`) under `examples/target/forged/`, its images then files
//! of its directory (`ImageArchive::File`), to take its base kit
//! for another composition (`KitForge::with_base_package`), without pulling
//! the image again. The source images of the mixins stay on the daemon, as the
//! kit images of the other examples do.
//!
//! ```sh
//! just example shell-forge
//! ```

use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use docker_sandbox::Client;
use docker_sandbox::exec::ExecSpec;
use docker_sandbox::kit::{ImageArchive, KitPackage, PackageImage};
use docker_sandbox::sandbox::{CreateRequest, Sandbox};
use docker_sandbox_kit::{KitDescriptor, KitForge, LifecycleStep, Mixin};
use example_common::{connect, sandbox};
use serde_json::{Value, json};

/// The published base kit, rebuilt from its image.
const BASE: &str = "docker.io/sbx/shell:latest";

/// Where the package is saved.
const OUTPUT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/forged/shell-forge");

/// The mixin read from its directory.
const JSON_MIXIN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/json-mixin");

/// The file the install command of the Rust mixin writes.
const INSTALLED: &str = "/home/agent/forged-tools.txt";

/// The file the startup command of the Rust mixin writes.
const STARTED: &str = "/tmp/forged-tools-started.txt";

/// The file the install command of the JSON mixin writes.
const JSON_MARKER: &str = "/home/agent/json-mixin.txt";

/// The host the Rust mixin allows; the example never calls it.
const ALLOWED: &str = "docs.example.com:443";

/// The host the Rust mixin denies.
const DENIED: &str = "ads.example.com";

/// Where the mixin built in Rust is written, as the CLI reads a local kit.
const SAVED_MIXIN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/forged/forged-tools");

/// Where the second composition is written.
const REUSED: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../target/forged/shell-forge-reused"
);

/// How long the startup command, run in the background, may take.
const STARTUP_WAIT: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> Result<()> {
    let client = connect().await?;
    refusals()?;

    let tools = tools_mixin()?;
    println!("the mixin built in Rust, as its spec.yaml:");
    println!("{}", tools.descriptor().to_spec());
    // Written as a kit directory, for `sbx create --kit` or `KitCompiler`.
    let saved = tools.save(SAVED_MIXIN)?;
    println!(
        "the mixin written as a kit directory the CLI reads, listed as {}",
        saved.reference()
    );
    // A descriptor can be read and inspected alone, without forging.
    let descriptor = KitDescriptor::read(format!("{JSON_MIXIN}/spec.yaml"))?;
    println!(
        "the descriptor of {}: version {}",
        descriptor.display_name().unwrap_or("no display name"),
        descriptor.to_json()["version"]
    );
    let read = Mixin::read(JSON_MIXIN)?;
    println!(
        "the mixin read from its directory: {}, listed as {}",
        read.name(),
        read.reference()
    );

    eprintln!("forging the package: {BASE} rebuilt from its image, then both mixins");
    let package = KitForge::new(client.clone())
        .with_base(BASE)
        .with_mixin(tools)
        .with_mixin(read)
        .build()
        .await?;
    ensure!(package.dir().is_none(), "the package was written");
    println!(
        "the package holds {} kits, compiled by no sbx ({:?}), for the daemon API {}:",
        package.kit_refs().len(),
        package.compiled_with(),
        package.compiled_api().unwrap_or("unknown")
    );
    for reference in package.kit_refs() {
        println!("  {reference}");
    }
    // A package is a creation request and the images it needs: the request
    // assembled from its parts, as a kit forger assembles it, is the one the
    // package replays.
    let agent = package.agent().context("the package names no agent")?.name;
    let mut assembled = CreateRequest::for_agent("{{name}}", &agent);
    assembled.kits = package.kit_refs();
    assembled.kit_artifacts = package.kit_artifacts();
    ensure!(
        assembled == package.request("{{name}}")?,
        "the request assembled from the package differs from its own"
    );
    println!(
        "its creation request is a request for the {agent} agent with these kits and their \
         artifacts (CreateRequest::for_agent)"
    );
    by_hand(&package, &assembled)?;

    let name = sandbox::unique_name("forge");
    let rule = format!("kit:{name}");
    sandbox::with_sandbox(&client, &package, &name, |sandbox| {
        let rule = rule.clone();
        async move {
            let _running = sandbox.ensure_running().await?;
            check_mixins(&sandbox).await?;
            ensure!(
                has_rule(sandbox.client(), &rule, ALLOWED).await?,
                "the daemon did not apply the allow rule of the mixin"
            );
            ensure!(
                has_rule(sandbox.client(), &format!("{rule}:deny"), DENIED).await?,
                "the daemon did not apply the deny rule of the mixin"
            );
            println!(
                "the daemon applied the network rules of the mixin: {rule} allows {ALLOWED}, \
                 {rule}:deny denies {DENIED}"
            );
            Ok(())
        }
    })
    .await?;
    ensure!(
        !has_rule(&client, &rule, ALLOWED).await?,
        "the network rule outlived its sandbox"
    );
    println!("the daemon removed the rules with the sandbox");

    reuse(&client, &package).await
}

/// Saves `package`, then composes its base kit again with another mixin:
/// kept on disk, it is pulled or exported no more.
async fn reuse(client: &Client, package: &KitPackage) -> Result<()> {
    let saved = package.save(OUTPUT)?;
    println!(
        "the package, built in memory, saved to {}",
        saved
            .dir()
            .map_or_else(String::new, |dir| dir.display().to_string())
    );
    ensure!(
        saved
            .images()
            .iter()
            .all(|image| matches!(image.archive, ImageArchive::File(_))),
        "an image of the saved package is not a file"
    );
    println!(
        "its {} images are now files of that directory",
        saved.images().len()
    );
    let reused = KitForge::new(client.clone())
        .with_base_package(saved)
        .with_mixin(Mixin::read(JSON_MIXIN)?)
        .forge(REUSED)
        .await?;
    println!(
        "the base kit of the package composed again: {}",
        reused.kit_refs().join(", ")
    );
    Ok(())
}

/// Assembles by hand the package `package` from its creation request
/// `request` and its images, as a kit forger of one's own would, and checks
/// that it is the same package.
fn by_hand(package: &KitPackage, request: &CreateRequest) -> Result<()> {
    let mut images = Vec::new();
    for image in package.images() {
        let (Some(reference), ImageArchive::Memory(archive)) = (&image.reference, &image.archive)
        else {
            bail!("an image of the package built in memory is not in memory");
        };
        println!("  image {reference}: {} bytes, in memory", archive.len());
        images.push(PackageImage::in_memory(reference, archive.clone()));
    }
    let assembled = KitPackage::in_memory(serde_json::to_value(request)?, Value::Null, images)?;
    ensure!(
        assembled.request("{{name}}")? == package.request("{{name}}")?
            && assembled.images().len() == package.images().len(),
        "the package assembled by hand differs from the one built"
    );
    println!(
        "the same package assembled by hand from its request and its {} images \
         (KitPackage::in_memory)",
        assembled.images().len()
    );
    Ok(())
}

/// The mixin built in Rust: an install command, a startup command and a
/// network rule.
fn tools_mixin() -> Result<Mixin> {
    let descriptor = KitDescriptor::mixin("Forged tools")
        .with_description("Mixin of the docker-sandbox examples, built in Rust")
        .with_version("0.1.0")
        .with_network_allow([ALLOWED])
        .with_network_deny([DENIED])
        .with_install(
            LifecycleStep::new(format!(
                "echo 'forged tools installed' > {INSTALLED}; \
                 echo \"gateway: ${{MCP_GATEWAY_URL:-none}}\" >> {INSTALLED}"
            ))
            .as_user("agent")
            .with_description("Writes the marker of the forged tools")
            // Install commands run in an emptied environment: the step names
            // the variables it needs.
            .with_gateway_env(),
        )
        .with_startup(
            LifecycleStep::new(format!("date > {STARTED}"))
                .as_user("agent")
                .in_background(),
        );
    Ok(Mixin::new("/kits/forged-tools", descriptor)?)
}

/// Shows what a forged mixin may not declare, each refused with its reason.
fn refusals() -> Result<()> {
    let credential = json!({
        "schemaVersion": "3",
        "kind": "mixin",
        "displayName": "Credential",
        "capabilities": [{ "type": "com.docker.sandbox/credential@1", "config": { "service": "example" } }]
    });
    let error = KitDescriptor::from_json(credential)
        .err()
        .context("a mixin declaring a credential was accepted")?;
    // A refused kit names itself and the reason, and retrying never helps.
    let docker_sandbox_kit::Error::InvalidKit { kit, message } = &error else {
        bail!("a credential was refused as {error}, not as an invalid kit");
    };
    ensure!(
        !error.is_transient(),
        "a refused kit is reported as transient"
    );
    println!("refused, a credential: kit {kit}, {message}");

    let yaml = "schemaVersion: \"3\"\nkind: mixin\ndisplayName: YAML\n";
    let error = KitDescriptor::parse(yaml)
        .err()
        .context("a spec.yaml in YAML was accepted")?;
    println!("refused, a spec.yaml in YAML: {error}");

    let nothing = KitDescriptor::mixin("Nothing").with_install(LifecycleStep::new("  "));
    let error = Mixin::new("/kits/nothing", nothing)
        .err()
        .context("a step without a command was accepted")?;
    println!("refused, a step without a command: {error}");

    // A directory that does not exist is no refusal of the kit: the error of
    // the client is kept as docker-sandbox reports it, a host file here, and
    // classified as the daemon errors are.
    let error = Mixin::read(format!("{JSON_MIXIN}-missing"))
        .err()
        .context("a missing mixin directory was read")?;
    let cause = error
        .client()
        .context("a missing directory is reported as a refusal")?;
    println!(
        "not read, a missing mixin directory: {cause} (not found by the daemon: {}, conflict: {}, \
         invalid input: {}, timeout: {}, worth retrying: {})",
        error.is_not_found(),
        error.is_conflict(),
        error.is_invalid_input(),
        error.is_timeout(),
        error.is_transient()
    );
    Ok(())
}

/// Checks that the commands of both mixins ran in `sandbox`.
async fn check_mixins(sandbox: &Sandbox) -> Result<()> {
    let installed = output(sandbox, &format!("cat {INSTALLED}")).await?;
    let mut lines = installed.lines();
    ensure!(
        lines.next() == Some("forged tools installed"),
        "the install command did not run"
    );
    println!(
        "the install command of the Rust mixin ran, and saw the MCP {}",
        lines.next().unwrap_or("gateway: unknown")
    );

    // The startup command runs in the background: wait for its file.
    let tries = STARTUP_WAIT.as_secs() * 2;
    let started = output(
        sandbox,
        &format!(
            "for i in $(seq {tries}); do [ -s {STARTED} ] && break; sleep 0.5; done; cat {STARTED}"
        ),
    )
    .await?;
    ensure!(!started.is_empty(), "the startup command did not run");
    println!("its startup command ran at {}", started.trim());

    let json = output(sandbox, &format!("cat {JSON_MARKER}")).await?;
    ensure!(
        json == "json-mixin installed\n",
        "the JSON mixin did not run"
    );
    println!("the install command of the JSON mixin ran: {}", json.trim());
    Ok(())
}

/// The standard output of `script`, run with `sh -c` in `sandbox`.
async fn output(sandbox: &Sandbox, script: &str) -> Result<String> {
    Ok(sandbox
        .exec(&ExecSpec::new(["sh", "-c", script]))
        .await?
        .check()?
        .stdout)
}

/// Whether the daemon holds the network rule `rule` for `host`.
async fn has_rule(client: &Client, rule: &str, host: &str) -> Result<bool> {
    let host = host.split(':').next().unwrap_or(host);
    Ok(client
        .policy()
        .rules()
        .await?
        .rules
        .iter()
        .any(|r| r.name == rule && r.resources.iter().any(|resource| resource.contains(host))))
}
