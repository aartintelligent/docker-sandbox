//! Decoding of real daemon payloads.
//!
//! Fixtures are exported from an audit snapshot with `audit fixtures`. Each
//! successful exchange of an operation covered by this crate must decode into
//! its type, and every fixture must be accounted for: a new fixture after an
//! `sbx` upgrade fails [`every_fixture_is_accounted_for`] until it is decoded
//! here or listed as not exposed.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers fail the test that calls them by panicking"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use docker_sandbox::daemon::{DaemonInfo, DiskUsage, Health, Setting};
use docker_sandbox::exec::{ExecInspect, ExecOutput, ExecSpec};
use docker_sandbox::image::{ImageInspect, ImageSummary, LocalImageMetadata, PullProgress};
use docker_sandbox::kit::KitAgent;
use docker_sandbox::mcp::{
    GatewayMode, LoadedServer, McpGateway, McpServerCheck, RegistrationCheck,
};
use docker_sandbox::policy::{
    CheckResult, NetworkCheck, NetworkRules, RuleChange, RuleChangeResult,
};
use docker_sandbox::sandbox::{
    CreateEvent, CreateRequest, Created, CredentialsOutcome, CredentialsUpdate, PortBinding,
    SandboxInfo, SavedImage, SwapOutcome, SwapRequest,
};
use serde::de::DeserializeOwned;
use serde_json::Value;

/// Successful exchanges of routes the crate deliberately does not expose.
/// None at the moment: the audit only records errors for those routes.
const NOT_EXPOSED: &[&str] = &[];

/// Successful answers made of a message only, which the crate does not read.
const MESSAGE_ONLY: &[&str] = &[
    "delete_docker_images_remove_200",
    "delete_sandbox_name_200",
    "delete_sandbox_name_mounts_200",
    "post_docker_images_load_200",
    "post_sandbox_name_exec_exec_id_resize_200",
    "post_sandbox_name_kits_200",
    "post_sandbox_name_mounts_200",
    "put_sandbox_name_files_200",
    "put_sandbox_name_working_directory_200",
];

/// A captured exchange.
struct Fixture {
    /// File name without extension.
    name: String,
    /// Request body, `null` when there is none.
    request: Value,
    /// Response body.
    response: Value,
    /// Response status.
    status: u16,
}

/// Directory of the fixtures.
fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Reads the fixture `name`.
fn fixture(name: &str) -> Fixture {
    let path = fixtures_dir().join(format!("{name}.json"));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let value: Value = serde_json::from_str(&text).unwrap();
    Fixture {
        name: name.to_owned(),
        request: value["request"].clone(),
        response: value["response"].clone(),
        status: u16::try_from(value["status"].as_u64().unwrap()).unwrap(),
    }
}

/// Decodes `value`, a part of `fixture`, into `T`.
fn decode<T: DeserializeOwned>(fixture: &Fixture, value: &Value) -> T {
    serde_json::from_value(value.clone())
        .unwrap_or_else(|e| panic!("{} does not decode: {e}\n{value:#}", fixture.name))
}

/// Decodes the response of the fixture `name`.
fn response<T: DeserializeOwned>(name: &str) -> T {
    let f = fixture(name);
    decode(&f, &f.response)
}

/// Decodes the request of the fixture `name`.
fn request<T: DeserializeOwned>(name: &str) -> T {
    let f = fixture(name);
    decode(&f, &f.request)
}

/// Lines of a streamed response: the spy stores multi-line streams as
/// `{"ndjson": [...]}` and a single-line stream as the line itself.
fn lines(name: &str) -> Vec<String> {
    let f = fixture(name);
    match f.response.get("ndjson").and_then(Value::as_array) {
        Some(lines) => lines.iter().map(Value::to_string).collect(),
        None => vec![f.response.to_string()],
    }
}

/// Fixtures decoded by the tests below.
const DECODED: &[&str] = &[
    "get_daemon_health_200",
    "get_daemon_info_200",
    "get_daemon_settings_200",
    "get_daemon_settings_key_200",
    "post_daemon_settings_key_200",
    "get_daemon_loglevel_200",
    "get_daemon_disk_usage_200",
    "get_sandbox_200",
    "get_sandbox_name_200",
    "post_sandbox_name_start_200",
    "post_sandbox_name_stop_200",
    "post_sandbox_200",
    "post_sandbox_201",
    "post_sandbox_name_save_201",
    "post_sandbox_name_swap_container_201",
    "post_sandbox_name_credentials_200",
    "post_sandbox_name_exec_200",
    "get_sandbox_name_exec_exec_id_200",
    "get_sandbox_name_ports_200",
    "post_sandbox_name_ports_200",
    "post_sandbox_name_ports_unpublish_200",
    "post_sandbox_name_ports_replace_desired_200",
    "get_sandbox_name_mcp_gateway_200",
    "post_sandbox_name_mcp_gateway_servers_200",
    "get_mcp_gateway_mode_200",
    "post_mcp_registration_check_200",
    "get_docker_images_200",
    "get_docker_images_inspect_200",
    "get_docker_images_local_metadata_200",
    "post_docker_images_create_200",
    "get_policy_network_rules_200",
    "post_policy_network_rules_200",
    "post_policy_network_check_200",
    "get_policy_network_setup_200",
    "get_policy_network_profiles_200",
];

#[test]
fn daemon_payloads_decode() {
    assert!(response::<Health>("get_daemon_health_200").is_healthy());
    assert!(
        response::<DaemonInfo>("get_daemon_info_200")
            .api_socket
            .is_some()
    );
    let f = fixture("get_daemon_settings_200");
    assert!(decode::<Vec<Setting>>(&f, &f.response["settings"]).len() > 10);
    response::<Setting>("get_daemon_settings_key_200");
    response::<Setting>("post_daemon_settings_key_200");
    assert!(
        response::<BTreeMap<String, String>>("get_daemon_loglevel_200").contains_key("general")
    );
    assert!(
        !response::<DiskUsage>("get_daemon_disk_usage_200")
            .summaries
            .is_empty()
    );
}

#[test]
fn sandbox_payloads_decode() {
    // Captured while no sandbox existed: an empty list.
    response::<Vec<SandboxInfo>>("get_sandbox_200");
    for name in [
        "get_sandbox_name_200",
        "post_sandbox_name_start_200",
        "post_sandbox_name_stop_200",
    ] {
        let sandbox: SandboxInfo = response(name);
        assert!(!sandbox.id.is_empty() && !sandbox.name.is_empty(), "{name}");
    }
    assert!(
        response::<SavedImage>("post_sandbox_name_save_201")
            .image_id
            .starts_with("sha256:")
    );
    request::<SwapRequest>("post_sandbox_name_swap_container_201");
    response::<SwapOutcome>("post_sandbox_name_swap_container_201");
    request::<CredentialsUpdate>("post_sandbox_name_credentials_200");
    assert!(
        response::<CredentialsOutcome>("post_sandbox_name_credentials_200")
            .withheld
            .is_empty()
    );
}

#[test]
fn create_requests_round_trip_losslessly() {
    for name in ["post_sandbox_200", "post_sandbox_201"] {
        let f = fixture(name);
        let request: CreateRequest = decode(&f, &f.request);
        assert!(!request.kit_artifacts.is_empty(), "{name}");
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            f.request,
            "{name}: CreateRequest must forward every field unchanged"
        );
    }
}

#[test]
fn kit_agents_are_read_from_creation_requests() {
    for name in ["post_sandbox_200", "post_sandbox_201"] {
        let f = fixture(name);
        let agent = KitAgent::from_artifact(&f.request["kit_artifacts"][0])
            .unwrap_or_else(|| panic!("{name}: the base kit has no manifest"));
        assert!(agent.binary.is_some(), "{name}: {agent:?}");
        assert!(!agent.command(["--version"]).is_empty());
    }
}

#[test]
fn creation_answers_decode() {
    let events: Vec<CreateEvent> = lines("post_sandbox_200")
        .iter()
        .filter_map(|line| CreateEvent::parse_line(line).unwrap())
        .collect();
    assert!(matches!(events.last(), Some(CreateEvent::Result(created)) if !created.id.is_empty()));
    assert!(events.iter().all(|e| !matches!(e, CreateEvent::Unknown(_))));

    // Without `Accept: application/x-ndjson`, the daemon answers 201 with the
    // outcome alone.
    assert_eq!(fixture("post_sandbox_201").status, 201);
    assert!(!response::<Created>("post_sandbox_201").id.is_empty());
}

#[test]
fn exec_payloads_decode() {
    assert!(
        !request::<ExecSpec>("post_sandbox_name_exec_200")
            .cmd
            .is_empty()
    );
    response::<ExecOutput>("post_sandbox_name_exec_200");
    assert!(!response::<ExecInspect>("get_sandbox_name_exec_exec_id_200").running);
}

#[test]
fn port_payloads_decode() {
    for name in ["get_sandbox_name_ports_200", "post_sandbox_name_ports_200"] {
        response::<Vec<PortBinding>>(name);
    }
    request::<Vec<PortBinding>>("post_sandbox_name_ports_200");
    request::<Vec<PortBinding>>("post_sandbox_name_ports_unpublish_200");
    let f = fixture("post_sandbox_name_ports_replace_desired_200");
    decode::<PortBinding>(&f, &f.request["old"]);
    decode::<PortBinding>(&f, &f.request["new"]);
}

#[test]
fn mcp_payloads_decode() {
    response::<McpGateway>("get_sandbox_name_mcp_gateway_200");
    response::<LoadedServer>("post_sandbox_name_mcp_gateway_servers_200");
    assert_eq!(
        response::<GatewayMode>("get_mcp_gateway_mode_200").decision,
        "local"
    );
    let f = fixture("post_mcp_registration_check_200");
    assert!(
        !decode::<McpServerCheck>(&f, &f.request["spec"])
            .name
            .is_empty()
    );
    assert!(response::<RegistrationCheck>("post_mcp_registration_check_200").allowed);
}

#[test]
fn kit_installation_requests_carry_one_artifact() {
    let f = fixture("post_sandbox_name_kits_200");
    let artifact = &f.request["kit_artifact"];
    assert!(
        artifact["manifest"].is_object() && artifact["commands"].is_object(),
        "{artifact:#}"
    );
}

#[test]
fn image_payloads_decode() {
    let images: Vec<ImageSummary> = response("get_docker_images_200");
    assert!(images.iter().all(|i| !i.reference().is_empty()));
    response::<ImageInspect>("get_docker_images_inspect_200");
    response::<LocalImageMetadata>("get_docker_images_local_metadata_200");
    let progress: Vec<PullProgress> = lines("post_docker_images_create_200")
        .iter()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        progress
            .iter()
            .all(|p| p.error.is_none() && p.status.is_some())
    );
}

#[test]
fn policy_payloads_decode() {
    let rules: NetworkRules = response("get_policy_network_rules_200");
    assert!(!rules.rules.is_empty());
    // The first exchange may be a change the daemon refused inside its HTTP
    // 200 answer, as the `errors` suite sends one: the outcome says so.
    let f = fixture("post_policy_network_rules_200");
    let results: Vec<RuleChangeResult> = decode(&f, &f.response["results"]);
    for result in &results {
        assert_eq!(result.is_applied(), result.error.is_none());
    }
    if serde_json::from_value::<Vec<RuleChange>>(f.request["actions"].clone()).is_err() {
        assert!(
            results.iter().any(|r| !r.is_applied()),
            "a request the crate cannot express must be a refused one"
        );
    }
    request::<NetworkCheck>("post_policy_network_check_200");
    response::<CheckResult>("post_policy_network_check_200");
    assert!(fixture("get_policy_network_setup_200").response["needed"].is_boolean());
    assert!(fixture("get_policy_network_profiles_200").response["profiles"].is_array());
}

#[test]
fn error_and_message_answers_carry_a_message() {
    for name in fixture_names() {
        let f = fixture(&name);
        if f.status >= 400 || MESSAGE_ONLY.contains(&name.as_str()) {
            assert!(
                f.response["message"]
                    .as_str()
                    .is_some_and(|m| !m.is_empty()),
                "{name}: the daemon is expected to answer {{\"message\": …}}"
            );
        }
    }
}

#[test]
fn every_fixture_is_accounted_for() {
    let known: BTreeSet<&str> = DECODED
        .iter()
        .chain(MESSAGE_ONLY)
        .chain(NOT_EXPOSED)
        .copied()
        .collect();
    for name in fixture_names() {
        let error = fixture(&name).status >= 400;
        assert!(
            error || known.contains(name.as_str()),
            "{name} is neither decoded by a test nor listed in MESSAGE_ONLY or NOT_EXPOSED"
        );
    }
}

/// Names of every fixture file.
fn fixture_names() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(fixtures_dir())
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            let name = path.file_stem()?.to_str()?.to_owned();
            (path.extension()? == "json").then_some(name)
        })
        .collect();
    names.sort();
    names
}
