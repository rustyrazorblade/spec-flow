//! End-to-end coverage of the MCP server (§2.5, §6): a real
//! [`spec_flow::mcp_service`] mounted on an ephemeral loopback listener,
//! driven by a real MCP client over HTTP.
//!
//! # What this covers that the unit tests cannot
//!
//! `src/server/logic.rs`'s unit tests already exercise every tool body
//! against `FakeVcs` (including `backlog`'s workflow-file read and
//! `drift`'s off-board dependency fetch). What they cannot reach is the
//! layer this file exists for: the `initialize` handshake and protocol
//! negotiation, the
//! `#[tool_router]`-generated tool table, JSON argument decoding into the
//! wire types, `Json<T>` structured-output encoding, the `ToolError` →
//! JSON-RPC error mapping as the *client* observes it, and — the
//! load-bearing one — that a connection's bound project really does
//! survive from one tool call to the next, which is a property of
//! `rmcp`'s per-session service factory rather than of any code in this
//! crate.
//!
//! # Why the tools are not driven all the way to GitHub here
//!
//! [`spec_flow::mcp_service`] takes a concrete
//! [`spec_flow::ShellVcs`] — the real `git`/`gh` subprocess layer — not
//! a `Vcs` trait object, so there is no seam to inject `FakeVcs`
//! through from outside the crate. Rather than widen the public API for
//! a test (the alternative being a generic parameter that would have to
//! thread through `StreamableHttpService`'s `S` type and the
//! `#[tool_router]` macro), these tests point `ShellVcs` at a `gh`
//! binary that does not exist. That still proves the whole request path
//! end to end — bind check, tool dispatch, argument decode, error
//! mapping — and asserts the *specific* failure is the `gh` call, not a
//! wiring mistake. Asserting real board rows over HTTP would need a live
//! authenticated repo, which is not a unit-test dependency this crate
//! takes anywhere else.
//!
//! That applies to the two **write** tools (§6's `approve`/`set_gate`)
//! as well, and is if anything safer there: a `gh` binary that cannot
//! be executed means no label and no comment can reach a real repo from
//! a test run. What these prove for them is the same request path plus
//! the ordering that matters — a rejected argument (an unknown `phase`,
//! a `decision`/`mode` outside its enum) fails *before* the `gh` layer
//! is reached at all, so a bad call writes nothing.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ErrorCode};
use rmcp::service::{RunningService, ServiceError};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use spec_flow::{
    Binaries, ClaimConfig, CrossProjectMode, GlobalConfig, HarnessesConfig,
    Limits, MergeMode, ProjectConfig, ProjectPointer, ShellVcs,
    project_config_path, save_project_config,
};

/// A global config registering `projects`, bound nowhere in particular
/// (the listener address here is chosen by the OS, not by this value).
fn global_config(projects: Vec<ProjectPointer>) -> GlobalConfig {
    GlobalConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        instance_id: None,
        binaries: Binaries::default(),
        harnesses: HarnessesConfig {
            default: "claude".to_string(),
            harnesses: HashMap::new(),
        },
        limits: Limits { max_concurrent_agents: 3 },
        cross_project_mode: CrossProjectMode::FairShare,
        claim: ClaimConfig {
            heartbeat_ttl: Duration::from_secs(3600),
            heartbeat_interval: Duration::from_secs(300),
        },
        phase_timeout: Duration::from_secs(45 * 60),
        projects,
    }
}

/// Write a real `<project_dir>/config.yaml` and return its registry
/// pointer — `register` reads that file, so it has to exist on disk.
fn register_project(dir: &Path, name: &str, repo: &str) -> ProjectPointer {
    let config = ProjectConfig {
        name: name.to_string(),
        repo: repo.to_string(),
        project_dir: dir.to_path_buf(),
        local_path: dir.join("main"),
        default_branch: "main".to_string(),
        merge_mode: MergeMode::Native,
        gh: None,
        harness: None,
        index_path: dir.join(".index"),
        memory_scope: name.to_string(),
        weight: None,
    };
    save_project_config(&project_config_path(dir), &config).unwrap();
    ProjectPointer { name: name.to_string(), project_dir: dir.to_path_buf() }
}

/// A minimal but schema-valid `.spec-flow/workflow.yaml`, written where
/// `backlog` looks for it: under the project's **primary checkout**
/// (`<project_dir>/main`, matching [`register_project`]'s `local_path`),
/// exactly as `spec-flow init` scaffolds it.
///
/// Deliberately carries a real `ready` seam and one approval-gated phase
/// before it, so the file exercises the shape `backlog` actually reads
/// rather than being an arbitrary parseable blob — the shipped default
/// itself is `pub(crate)`, so an integration test cannot reach for it.
fn scaffold_workflow(dir: &Path) {
    let workflow = "\
labels:
  priority: [P0, P1, P2, P3]
  status: [product-spec, ready, implement]
  gate_prefix: \"gate:\"
  approval_prefix: \"approved:\"
  owner_prefix: \"owner:\"
spec: {tool: openspec, optional: true, skip_label: \"spec:skip\"}
review_panel: []
fix_loop: {max_rounds: 1, on_exhausted: escalate, on_decision_finding: escalate}
phases:
  - {id: product-spec, action: {type: agent, role: product-manager}, gate: human, approval_label: \"approved:product-spec\", status_label: product-spec}
  - {id: implement, action: {type: server, op: start_implement}, requires: [\"approved:product-spec\"], gate: none, status_label: implement}
";
    let spec_flow = dir.join("main").join(".spec-flow");
    std::fs::create_dir_all(&spec_flow).unwrap();
    std::fs::write(spec_flow.join("workflow.yaml"), workflow).unwrap();
}

/// Serve `global` on an ephemeral loopback port and return its address.
///
/// The `gh` binary is deliberately a name that cannot resolve — see this
/// file's module doc.
async fn spawn_server(global: GlobalConfig) -> SocketAddr {
    let vcs = ShellVcs::new(
        "git".to_string(),
        "spec-flow-no-such-gh-binary".to_string(),
    );
    let router = axum::Router::new().nest_service(
        spec_flow::MCP_PATH,
        spec_flow::mcp_service(global, vcs),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    addr
}

async fn connect(addr: SocketAddr) -> RunningService<rmcp::RoleClient, ()> {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!(
            "http://{addr}{}",
            spec_flow::MCP_PATH
        )),
    );
    ().serve(transport).await.expect("MCP handshake failed")
}

/// The JSON-RPC error code behind a failed `call_tool`.
fn error_code(error: &ServiceError) -> ErrorCode {
    match error {
        ServiceError::McpError(data) => data.code,
        other => panic!("expected an MCP error, got {other:?}"),
    }
}

/// The message behind a failed `call_tool`.
fn error_message(error: &ServiceError) -> String {
    match error {
        ServiceError::McpError(data) => data.message.to_string(),
        other => panic!("expected an MCP error, got {other:?}"),
    }
}

fn args(value: serde_json::Value) -> CallToolRequestParams {
    CallToolRequestParams::new("placeholder")
        .with_arguments(value.as_object().cloned().unwrap())
}

fn call(
    name: &'static str,
    value: serde_json::Value,
) -> CallToolRequestParams {
    let mut params = args(value);
    params.name = name.into();
    params
}

#[tokio::test]
async fn the_handshake_announces_this_crate_not_the_sdk() {
    // `ServerInfo::new` defaults to `Implementation::from_build_env()`,
    // whose `env!`s expand inside `rmcp` -- so the naive version of
    // `get_info` announces the server as "rmcp" 3.1.0. §14.1's
    // compatibility contract needs this crate's own name/version.
    let addr = spawn_server(global_config(Vec::new())).await;
    let client = connect(addr).await;

    let info = client.peer_info().expect("peer info after handshake");
    let server = info.server_info.as_ref().expect("server info");
    assert_eq!(server.name, "spec-flow");
    assert_eq!(server.version, env!("CARGO_PKG_VERSION"));

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn the_handshake_negotiates_a_session_bearing_protocol_revision() {
    // The property §15's per-connection project binding rests on: MCP
    // 2026-07-28 removed sessions, and rmcp's transport routes such a
    // client statelessly with a fresh handler per request.
    //
    // This does NOT drive a client that actually offers 2026-07-28 —
    // rmcp 3.1.0's own client `.serve()` unconditionally follows up
    // `initialize` with `notifications/initialized` on the same stream,
    // which the *routing* decision (made from the client's claimed
    // version before the handler negotiates anything down) has already
    // treated as one-shot-stateless for that version, closing the stream
    // out from under it — confirmed by trying exactly that here and
    // getting `TransportChannelClosed`. So there is no rmcp 3.1.0 client
    // this test could drive through a full, working "offers 2026-07-28,
    // gets negotiated down, keeps talking" handshake; the real guards
    // are `SUPPORTED_PROTOCOL_VERSIONS`'s own unit test (below, in
    // `src/server/mod.rs`) and rmcp's handler-side rejection of a
    // per-request version outside it. What this test actually pins is
    // narrower: today's rmcp client defaults below 2026-07-28, so an
    // ordinary handshake never needs the fallback at all.
    let addr = spawn_server(global_config(Vec::new())).await;
    let client = connect(addr).await;

    let info = client.peer_info().expect("peer info after handshake");
    assert_ne!(
        info.protocol_version,
        rmcp::model::ProtocolVersion::V_2026_07_28,
        "a session-less revision cannot carry a bound project"
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn the_server_advertises_exactly_this_slices_tools() {
    let addr = spawn_server(global_config(Vec::new())).await;
    let client = connect(addr).await;

    let mut names: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .iter()
        .map(|tool| tool.name.to_string())
        .collect();
    names.sort();

    // Pinned deliberately: §6 lists ~24 tools, and this server ships
    // seven -- every read-only tool under §6's "Board / status"
    // heading, plus `register`, plus §6's "Approval & gates" write
    // pair. An eighth appearing here without a matching doc update in
    // `src/server/mod.rs`'s "what this server deliberately does not
    // implement" list is exactly the drift worth failing on.
    assert_eq!(
        names,
        vec![
            "approve", "backlog", "board", "drift", "issue", "register",
            "set_gate"
        ]
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn register_binds_the_connection_to_the_project_containing_cwd() {
    let tmp = tempfile::tempdir().unwrap();
    let pointer = register_project(tmp.path(), "proj-a", "owner/repo-a");
    let addr = spawn_server(global_config(vec![pointer])).await;
    let client = connect(addr).await;

    let result = client
        .call_tool(call(
            "register",
            serde_json::json!({
                "worker_ref": "coordinator-1",
                "role": "coordinator",
                "cwd": tmp.path().to_str().unwrap(),
            }),
        ))
        .await
        .unwrap();

    let structured = result.structured_content.expect("structured output");
    assert_eq!(structured["project"], "proj-a");
    assert_eq!(structured["repo"], "owner/repo-a");
    assert_eq!(structured["role"], "coordinator");
    assert_eq!(structured["worker_ref"], "coordinator-1");

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn register_resolves_a_worktree_inside_the_project() {
    // §10.1's realistic case: the coordinator is opened in
    // `<project_dir>/<branch>`, not in `<project_dir>` itself.
    let tmp = tempfile::tempdir().unwrap();
    let pointer = register_project(tmp.path(), "proj-a", "owner/repo-a");
    let worktree = tmp.path().join("issue-42-widget-cache");
    std::fs::create_dir_all(&worktree).unwrap();
    let addr = spawn_server(global_config(vec![pointer])).await;
    let client = connect(addr).await;

    let result = client
        .call_tool(call(
            "register",
            serde_json::json!({
                "worker_ref": "w",
                "role": "coordinator",
                "cwd": worktree.to_str().unwrap(),
            }),
        ))
        .await
        .unwrap();

    assert_eq!(result.structured_content.unwrap()["project"], "proj-a");

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn register_rejects_a_directory_outside_every_registered_project() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("a");
    std::fs::create_dir_all(&project).unwrap();
    let pointer = register_project(&project, "proj-a", "owner/repo-a");
    let elsewhere = tmp.path().join("somewhere-else");
    let addr = spawn_server(global_config(vec![pointer])).await;
    let client = connect(addr).await;

    let error = client
        .call_tool(call(
            "register",
            serde_json::json!({
                "worker_ref": "w",
                "role": "coordinator",
                "cwd": elsewhere.to_str().unwrap(),
            }),
        ))
        .await
        .unwrap_err();

    assert_eq!(error_code(&error), ErrorCode::INVALID_PARAMS);
    assert!(
        error_message(&error).contains("spec-flow init"),
        "the rejection must point at §6's fix: {}",
        error_message(&error)
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn register_rejects_a_coordinator_with_no_cwd() {
    let addr = spawn_server(global_config(Vec::new())).await;
    let client = connect(addr).await;

    let error = client
        .call_tool(call(
            "register",
            serde_json::json!({ "worker_ref": "w", "role": "coordinator" }),
        ))
        .await
        .unwrap_err();

    assert_eq!(error_code(&error), ErrorCode::INVALID_PARAMS);

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn register_rejects_a_spawn_token_rather_than_ignoring_it() {
    let tmp = tempfile::tempdir().unwrap();
    let pointer = register_project(tmp.path(), "proj-a", "owner/repo-a");
    let addr = spawn_server(global_config(vec![pointer])).await;
    let client = connect(addr).await;

    let error = client
        .call_tool(call(
            "register",
            serde_json::json!({
                "worker_ref": "w",
                "role": "developer",
                "cwd": tmp.path().to_str().unwrap(),
                "spawn_token": "abc123",
            }),
        ))
        .await
        .unwrap_err();

    assert_eq!(error_code(&error), ErrorCode::INVALID_PARAMS);
    assert!(
        error_message(&error).contains("spawn_token"),
        "message should name the unimplemented path: {}",
        error_message(&error)
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_connection_cannot_rebind_to_another_project() {
    // §15's hard rule: "a connection is bound to exactly one project for
    // its life ... and never changed."
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let global = global_config(vec![
        register_project(&a, "proj-a", "owner/repo-a"),
        register_project(&b, "proj-b", "owner/repo-b"),
    ]);
    let addr = spawn_server(global).await;
    let client = connect(addr).await;

    client
        .call_tool(call(
            "register",
            serde_json::json!({
                "worker_ref": "w",
                "role": "coordinator",
                "cwd": a.to_str().unwrap(),
            }),
        ))
        .await
        .unwrap();

    let error = client
        .call_tool(call(
            "register",
            serde_json::json!({
                "worker_ref": "w",
                "role": "coordinator",
                "cwd": b.to_str().unwrap(),
            }),
        ))
        .await
        .unwrap_err();

    assert_eq!(error_code(&error), ErrorCode::INVALID_REQUEST);

    // ...and the original binding is untouched, not left half-changed.
    let still = client
        .call_tool(call(
            "register",
            serde_json::json!({
                "worker_ref": "w",
                "role": "coordinator",
                "cwd": a.to_str().unwrap(),
            }),
        ))
        .await
        .unwrap();
    assert_eq!(still.structured_content.unwrap()["project"], "proj-a");

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn two_connections_bind_to_their_own_projects_independently() {
    // The property the per-session service factory exists for: one
    // daemon, many projects, and no shared mutable binding between
    // connections (§2.5).
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let global = global_config(vec![
        register_project(&a, "proj-a", "owner/repo-a"),
        register_project(&b, "proj-b", "owner/repo-b"),
    ]);
    let addr = spawn_server(global).await;

    let first = connect(addr).await;
    let second = connect(addr).await;

    let bound_a = first
        .call_tool(call(
            "register",
            serde_json::json!({
                "worker_ref": "one",
                "role": "coordinator",
                "cwd": a.to_str().unwrap(),
            }),
        ))
        .await
        .unwrap();
    let bound_b = second
        .call_tool(call(
            "register",
            serde_json::json!({
                "worker_ref": "two",
                "role": "coordinator",
                "cwd": b.to_str().unwrap(),
            }),
        ))
        .await
        .unwrap();

    assert_eq!(bound_a.structured_content.unwrap()["project"], "proj-a");
    assert_eq!(bound_b.structured_content.unwrap()["project"], "proj-b");

    first.cancel().await.unwrap();
    second.cancel().await.unwrap();
}

#[tokio::test]
async fn board_and_issue_refuse_an_unregistered_connection() {
    let addr = spawn_server(global_config(Vec::new())).await;
    let client = connect(addr).await;

    let board = client
        .call_tool(call("board", serde_json::json!({})))
        .await
        .unwrap_err();
    let issue = client
        .call_tool(call("issue", serde_json::json!({ "number": 1 })))
        .await
        .unwrap_err();

    assert_eq!(error_code(&board), ErrorCode::INVALID_REQUEST);
    assert_eq!(error_code(&issue), ErrorCode::INVALID_REQUEST);

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn board_reaches_the_git_gh_layer_once_the_connection_is_bound() {
    // The binding survives from `register` to a *later* tool call --
    // the whole point of the per-session handler instance -- and the
    // only thing left failing is the deliberately-missing `gh`.
    let tmp = tempfile::tempdir().unwrap();
    let pointer = register_project(tmp.path(), "proj-a", "owner/repo-a");
    let addr = spawn_server(global_config(vec![pointer])).await;
    let client = connect(addr).await;

    client
        .call_tool(call(
            "register",
            serde_json::json!({
                "worker_ref": "w",
                "role": "coordinator",
                "cwd": tmp.path().to_str().unwrap(),
            }),
        ))
        .await
        .unwrap();

    let error = client
        .call_tool(call("board", serde_json::json!({})))
        .await
        .unwrap_err();

    assert_eq!(error_code(&error), ErrorCode::INTERNAL_ERROR);
    assert!(
        error_message(&error).contains("spec-flow-no-such-gh-binary"),
        "the failure must be the gh call, not a wiring mistake: {}",
        error_message(&error)
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn board_rejects_a_filter_rather_than_silently_ignoring_it() {
    let tmp = tempfile::tempdir().unwrap();
    let pointer = register_project(tmp.path(), "proj-a", "owner/repo-a");
    let addr = spawn_server(global_config(vec![pointer])).await;
    let client = connect(addr).await;

    client
        .call_tool(call(
            "register",
            serde_json::json!({
                "worker_ref": "w",
                "role": "coordinator",
                "cwd": tmp.path().to_str().unwrap(),
            }),
        ))
        .await
        .unwrap();

    let error = client
        .call_tool(call("board", serde_json::json!({ "filter": "P0" })))
        .await
        .unwrap_err();

    assert_eq!(error_code(&error), ErrorCode::INVALID_PARAMS);

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn issue_reaches_the_git_gh_layer_once_the_connection_is_bound() {
    let tmp = tempfile::tempdir().unwrap();
    let pointer = register_project(tmp.path(), "proj-a", "owner/repo-a");
    let addr = spawn_server(global_config(vec![pointer])).await;
    let client = connect(addr).await;

    client
        .call_tool(call(
            "register",
            serde_json::json!({
                "worker_ref": "w",
                "role": "coordinator",
                "cwd": tmp.path().to_str().unwrap(),
            }),
        ))
        .await
        .unwrap();

    let error = client
        .call_tool(call("issue", serde_json::json!({ "number": 42 })))
        .await
        .unwrap_err();

    assert_eq!(error_code(&error), ErrorCode::INTERNAL_ERROR);
    assert!(
        error_message(&error).contains("spec-flow-no-such-gh-binary"),
        "the failure must be the gh call: {}",
        error_message(&error)
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn backlog_and_drift_refuse_an_unregistered_connection() {
    // §15's "project rides the connection": neither tool takes a
    // project argument, so without a binding there is nothing to act on.
    let addr = spawn_server(global_config(Vec::new())).await;
    let client = connect(addr).await;

    let backlog = client
        .call_tool(call("backlog", serde_json::json!({})))
        .await
        .unwrap_err();
    let drift = client
        .call_tool(call("drift", serde_json::json!({})))
        .await
        .unwrap_err();

    assert_eq!(error_code(&backlog), ErrorCode::INVALID_REQUEST);
    assert_eq!(error_code(&drift), ErrorCode::INVALID_REQUEST);

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn backlog_reaches_the_git_gh_layer_once_the_connection_is_bound() {
    let tmp = tempfile::tempdir().unwrap();
    let pointer = register_project(tmp.path(), "proj-a", "owner/repo-a");
    scaffold_workflow(tmp.path());
    let addr = spawn_server(global_config(vec![pointer])).await;
    let client = connect(addr).await;

    client
        .call_tool(call(
            "register",
            serde_json::json!({
                "worker_ref": "w",
                "role": "coordinator",
                "cwd": tmp.path().to_str().unwrap(),
            }),
        ))
        .await
        .unwrap();

    let error = client
        .call_tool(call("backlog", serde_json::json!({})))
        .await
        .unwrap_err();

    // The workflow file parsed (that read happens first); the only thing
    // left failing is the deliberately-missing `gh`.
    assert_eq!(error_code(&error), ErrorCode::INTERNAL_ERROR);
    assert!(
        error_message(&error).contains("spec-flow-no-such-gh-binary"),
        "the failure must be the gh call, not the workflow read: {}",
        error_message(&error)
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn backlog_names_the_workflow_file_it_could_not_read() {
    // The readiness bar is defined by `.spec-flow/workflow.yaml` (§7.0),
    // so a project whose checkout lacks it must fail loudly and say
    // which path -- not fall back to the shipped default and report a
    // readiness bar the team does not use.
    let tmp = tempfile::tempdir().unwrap();
    let pointer = register_project(tmp.path(), "proj-a", "owner/repo-a");
    let addr = spawn_server(global_config(vec![pointer])).await;
    let client = connect(addr).await;

    client
        .call_tool(call(
            "register",
            serde_json::json!({
                "worker_ref": "w",
                "role": "coordinator",
                "cwd": tmp.path().to_str().unwrap(),
            }),
        ))
        .await
        .unwrap();

    let error = client
        .call_tool(call("backlog", serde_json::json!({})))
        .await
        .unwrap_err();

    assert_eq!(error_code(&error), ErrorCode::INTERNAL_ERROR);
    assert!(
        error_message(&error).contains(".spec-flow/workflow.yaml"),
        "the failure must name the missing file: {}",
        error_message(&error)
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn backlog_rejects_a_filter_rather_than_silently_ignoring_it() {
    let tmp = tempfile::tempdir().unwrap();
    let pointer = register_project(tmp.path(), "proj-a", "owner/repo-a");
    scaffold_workflow(tmp.path());
    let addr = spawn_server(global_config(vec![pointer])).await;
    let client = connect(addr).await;

    client
        .call_tool(call(
            "register",
            serde_json::json!({
                "worker_ref": "w",
                "role": "coordinator",
                "cwd": tmp.path().to_str().unwrap(),
            }),
        ))
        .await
        .unwrap();

    let error = client
        .call_tool(call("backlog", serde_json::json!({ "filter": "P0" })))
        .await
        .unwrap_err();

    assert_eq!(error_code(&error), ErrorCode::INVALID_PARAMS);
    // The rejection must name the tool the client actually called --
    // `board(filter)` would send them looking at the wrong call.
    assert!(
        error_message(&error).contains("backlog(filter)"),
        "the rejection must name this tool: {}",
        error_message(&error)
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn drift_reaches_the_git_gh_layer_once_the_connection_is_bound() {
    // `drift()` takes no arguments at all (§6) -- the project rides the
    // connection, and every check runs project-wide by definition.
    let tmp = tempfile::tempdir().unwrap();
    let pointer = register_project(tmp.path(), "proj-a", "owner/repo-a");
    let addr = spawn_server(global_config(vec![pointer])).await;
    let client = connect(addr).await;

    client
        .call_tool(call(
            "register",
            serde_json::json!({
                "worker_ref": "w",
                "role": "coordinator",
                "cwd": tmp.path().to_str().unwrap(),
            }),
        ))
        .await
        .unwrap();

    let error = client
        .call_tool(call("drift", serde_json::json!({})))
        .await
        .unwrap_err();

    assert_eq!(error_code(&error), ErrorCode::INTERNAL_ERROR);
    assert!(
        error_message(&error).contains("spec-flow-no-such-gh-binary"),
        "the failure must be the gh call, not a wiring mistake: {}",
        error_message(&error)
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn issue_rejects_a_missing_number_argument() {
    // Argument decoding is `rmcp`'s job, not this crate's -- pinned so a
    // future `#[serde(default)]` slipping onto a required field shows up
    // as a test failure rather than as issue #0 being read.
    //
    // Note the shape: `rmcp` surfaces a parameter-deserialization
    // failure as a *successful* `tools/call` carrying `isError: true`,
    // not as a JSON-RPC error -- which is what the MCP spec asks for
    // (tool-level failures are results, protocol-level ones are errors).
    // That is why this asserts on the result rather than `unwrap_err`,
    // unlike the tool bodies' own rejections above.
    let tmp = tempfile::tempdir().unwrap();
    let pointer = register_project(tmp.path(), "proj-a", "owner/repo-a");
    let addr = spawn_server(global_config(vec![pointer])).await;
    let client = connect(addr).await;

    client
        .call_tool(call(
            "register",
            serde_json::json!({
                "worker_ref": "w",
                "role": "coordinator",
                "cwd": tmp.path().to_str().unwrap(),
            }),
        ))
        .await
        .unwrap();

    let result =
        client.call_tool(call("issue", serde_json::json!({}))).await.unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(
        format!("{:?}", result.content).contains("number"),
        "the failure should name the missing field: {:?}",
        result.content
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn approve_and_set_gate_refuse_an_unregistered_connection() {
    // §15's "project rides the connection" -- and for a *write* tool
    // the stakes are higher than for a read: without a binding there is
    // no repo to write a label to, and picking one would be the
    // cross-project act §15 forbids outright.
    let addr = spawn_server(global_config(Vec::new())).await;
    let client = connect(addr).await;

    let approve = client
        .call_tool(call(
            "approve",
            serde_json::json!({
                "issue_number": 1,
                "phase": "product-spec",
                "decision": "grant",
            }),
        ))
        .await
        .unwrap_err();
    let set_gate = client
        .call_tool(call(
            "set_gate",
            serde_json::json!({
                "issue_number": 1,
                "phase": "product-spec",
                "mode": "human",
            }),
        ))
        .await
        .unwrap_err();

    assert_eq!(error_code(&approve), ErrorCode::INVALID_REQUEST);
    assert_eq!(error_code(&set_gate), ErrorCode::INVALID_REQUEST);

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn approve_reaches_the_git_gh_layer_once_the_connection_is_bound() {
    // The workflow file parsed and the phase resolved (both happen
    // first); the only thing left failing is the deliberately-missing
    // `gh` -- i.e. the write really is attempted against GitHub before
    // this tool returns (§15).
    let tmp = tempfile::tempdir().unwrap();
    let pointer = register_project(tmp.path(), "proj-a", "owner/repo-a");
    scaffold_workflow(tmp.path());
    let addr = spawn_server(global_config(vec![pointer])).await;
    let client = connect(addr).await;

    client
        .call_tool(call(
            "register",
            serde_json::json!({
                "worker_ref": "w",
                "role": "coordinator",
                "cwd": tmp.path().to_str().unwrap(),
            }),
        ))
        .await
        .unwrap();

    let error = client
        .call_tool(call(
            "approve",
            serde_json::json!({
                "issue_number": 42,
                "phase": "product-spec",
                "decision": "grant",
                "note": "ship it",
            }),
        ))
        .await
        .unwrap_err();

    assert_eq!(error_code(&error), ErrorCode::INTERNAL_ERROR);
    assert!(
        error_message(&error).contains("spec-flow-no-such-gh-binary"),
        "the failure must be the gh call, not the workflow read: {}",
        error_message(&error)
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn approve_rejects_a_phase_the_workflow_does_not_define() {
    // `merge` is an approval label's suffix, not a phase id. The
    // rejection is a client mistake (INVALID_PARAMS) and names the ids
    // that would have worked -- and, crucially, happens before any `gh`
    // call, so a mistyped phase writes nothing.
    let tmp = tempfile::tempdir().unwrap();
    let pointer = register_project(tmp.path(), "proj-a", "owner/repo-a");
    scaffold_workflow(tmp.path());
    let addr = spawn_server(global_config(vec![pointer])).await;
    let client = connect(addr).await;

    client
        .call_tool(call(
            "register",
            serde_json::json!({
                "worker_ref": "w",
                "role": "coordinator",
                "cwd": tmp.path().to_str().unwrap(),
            }),
        ))
        .await
        .unwrap();

    let error = client
        .call_tool(call(
            "approve",
            serde_json::json!({
                "issue_number": 42,
                "phase": "merge",
                "decision": "deny",
            }),
        ))
        .await
        .unwrap_err();

    assert_eq!(error_code(&error), ErrorCode::INVALID_PARAMS);
    let message = error_message(&error);
    assert!(
        message.contains("merge") && message.contains("product-spec"),
        "the rejection must name the phase and list the real ones: \
         {message}"
    );
    assert!(
        !message.contains("spec-flow-no-such-gh-binary"),
        "the argument must be rejected before any gh call: {message}"
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn approve_rejects_a_decision_outside_grant_and_deny() {
    // `decision` is a closed enum on the wire, not a free string --
    // pinned so a "denied"/"approve" typo cannot decode into one of the
    // two real branches. Like every parameter-decoding failure, `rmcp`
    // surfaces this as a successful call carrying `isError: true`.
    let tmp = tempfile::tempdir().unwrap();
    let pointer = register_project(tmp.path(), "proj-a", "owner/repo-a");
    scaffold_workflow(tmp.path());
    let addr = spawn_server(global_config(vec![pointer])).await;
    let client = connect(addr).await;

    client
        .call_tool(call(
            "register",
            serde_json::json!({
                "worker_ref": "w",
                "role": "coordinator",
                "cwd": tmp.path().to_str().unwrap(),
            }),
        ))
        .await
        .unwrap();

    let result = client
        .call_tool(call(
            "approve",
            serde_json::json!({
                "issue_number": 42,
                "phase": "product-spec",
                "decision": "approved",
            }),
        ))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn set_gate_reaches_the_git_gh_layer_once_the_connection_is_bound() {
    let tmp = tempfile::tempdir().unwrap();
    let pointer = register_project(tmp.path(), "proj-a", "owner/repo-a");
    scaffold_workflow(tmp.path());
    let addr = spawn_server(global_config(vec![pointer])).await;
    let client = connect(addr).await;

    client
        .call_tool(call(
            "register",
            serde_json::json!({
                "worker_ref": "w",
                "role": "coordinator",
                "cwd": tmp.path().to_str().unwrap(),
            }),
        ))
        .await
        .unwrap();

    let error = client
        .call_tool(call(
            "set_gate",
            serde_json::json!({
                "issue_number": 42,
                "phase": "implement",
                "mode": "human",
            }),
        ))
        .await
        .unwrap_err();

    assert_eq!(error_code(&error), ErrorCode::INTERNAL_ERROR);
    assert!(
        error_message(&error).contains("spec-flow-no-such-gh-binary"),
        "the failure must be the gh call, not a wiring mistake: {}",
        error_message(&error)
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn set_gate_rejects_a_mode_outside_the_three_gate_modes() {
    // §2.3b's three modes are the whole vocabulary; a fourth value must
    // not decode. Same `isError: true` shape as every other decoding
    // failure.
    let tmp = tempfile::tempdir().unwrap();
    let pointer = register_project(tmp.path(), "proj-a", "owner/repo-a");
    scaffold_workflow(tmp.path());
    let addr = spawn_server(global_config(vec![pointer])).await;
    let client = connect(addr).await;

    client
        .call_tool(call(
            "register",
            serde_json::json!({
                "worker_ref": "w",
                "role": "coordinator",
                "cwd": tmp.path().to_str().unwrap(),
            }),
        ))
        .await
        .unwrap();

    let result = client
        .call_tool(call(
            "set_gate",
            serde_json::json!({
                "issue_number": 42,
                "phase": "product-spec",
                "mode": "manual",
            }),
        ))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));

    client.cancel().await.unwrap();
}
