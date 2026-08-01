/*! The MCP daemon itself (§2.5, §5, §6, §14) — `spec-flow serve`.

This is the first slice of §6's tool surface: the async foundation
(`tokio` + `rmcp` over streamable HTTP/SSE, loopback), plus the three
read-only tools that make a coordinator session possible at all —
`register`, `board`, and `issue`.

# The connection *is* the project (§2.5, §6, §15)

§15's hard rule — "a connection is bound to exactly one project for its
life ... and never changed" — is implemented by giving every MCP session
its **own** [`SpecFlowServer`] instance, whose `bound` field starts empty
and is filled exactly once by `register`. That is not an accident of
this design; it is [`StreamableHttpService`]'s documented shape:
`StreamableHttpService::new` takes a *service factory* (`impl Fn() ->
Result<S, io::Error>`) and calls it once per session, handing the
resulting handler to a per-session worker task that serves every request
carrying that session's `Mcp-Session-Id`. Connection-scoped state is
therefore just a field, with no session-id bookkeeping of our own.

## CONFIRMED constraint: MCP `2026-07-28` has no sessions, so it cannot
## carry a bound project

Read from `rmcp` 3.1.0's own source (`transport/streamable_http_server/
tower.rs`), not inferred: the transport decides session-vs-stateless
routing from the protocol version in the client's `initialize` body (or
its `MCP-Protocol-Version` header) *before* any handler is consulted —
`is_legacy_request` → `uses_legacy_lifecycle(version)`, which is `version
< 2026-07-28`. Per SEP-2567 the `2026-07-28` revision **removes
sessions**, and the config field's own doc says so: "requests negotiating
that version are always served statelessly regardless of this setting."
On that stateless path the factory runs **per request**, so a `register`
would bind a handler that is dropped before the next call arrives.

There is no way to honour "project rides the connection" for a client
that negotiates `2026-07-28` — the protocol has deliberately removed the
identity the binding needs. So [`SpecFlowServer`] overrides
[`ServerHandler::supported_protocol_versions`] to advertise only the
session-bearing revisions; a client offering `2026-07-28` negotiates down
to `2025-11-25` instead. This is a real, pinned limitation of the design,
not a workaround chosen for convenience: when a spawned-phase/coordinator
client one day *requires* `2026-07-28`, the binding has to move to
something the stateless protocol does carry (§2.6's correlation token is
the obvious candidate — it is already a per-request-presentable secret,
unlike a coordinator's `cwd`), and that is a design decision for the
step that needs it.

# What this slice deliberately does not implement

Not stubs — absent, and listed here so nobody has to grep for them. Every
other §6 tool: `next_assignment`, `backlog`, `drift`, `start_implement`,
`submit_artifact`, `submit_review`, `create_issue`, `advance`,
`cancel_work`, `sync_ci`, `address`, `approve`, `set_gate`, `link`,
`unlink`, `acquire_lease`/`heartbeat_lease`/`release_lease`/
`lease_status`, `report`, and `instructions`. `init` stays a CLI
subcommand here rather than an MCP tool (§14.1 ships it as one); wiring
it as a tool too is a later decision, not an omission with a workaround.

Also absent, and each for a stated reason:

- **`register`'s spawned-phase path** (§2.6's `spawn_token`). Presenting
  a token is *rejected*, not ignored — see
  [`logic::ToolError::SpawnTokenUnsupported`]. There is nothing to bind a
  token to: no spawner is running alongside this server to have minted
  one, and no `submit_artifact` exists for it to gate.
- **`board`'s `filter` argument** (§6). Likewise rejected rather than
  ignored: §6 gives `filter` no shape, so silently returning an
  unfiltered board to a client that asked for a filtered one would be a
  wrong answer dressed as a right one.
- **`board`'s WIP-vs-`max_concurrent_agents` count and its single
  recommended `next_action`** (§6, §2.7). WIP needs this instance's live
  [`crate::ProcessSpawner`] `LocalProcess` map, which nothing runs
  alongside the GitHub-state layer yet — the same boundary
  [`crate::board`]'s own doc draws for the worktree/per-agent columns it
  omits. `next_action` needs [`crate::schedule::next_action`], which
  needs each issue's *actionability* from the phase engine; wiring the
  scheduler into the server is a step of its own, and a `next_action`
  computed from a partial view would be worse than none.
- **`instance_id` auto-generation on first `serve`** (§11.1, and
  [`crate::GlobalConfig::instance_id`]'s doc, which names `serve` as its
  home). Nothing in this slice writes a claim, so nothing needs an
  instance id yet; generating and persisting one as a side effect of a
  read-only server would be an unrequested config mutation.
- **The CI/PR poller** (§12) and any background task at all. This server
  is purely request-driven; `serve` starts a listener and nothing else.
- **Any caching or request batching.** `board` re-reads every open issue
  from GitHub on every call, one `gh` subprocess at a time. This is
  measurably slow — see [`logic::read_board`]'s "Cost" section for the
  number measured against a live repo and for why neither fix (§2.3/§5's
  disposable local cache, or a batched GraphQL read) is tuning that
  belongs in this slice.
*/

mod logic;
mod wire;

use std::borrow::Cow;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{
    ErrorData, ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};

use crate::config::{GlobalConfig, ProjectConfig};
use crate::vcs::ShellVcs;

pub use self::logic::ToolError;
pub use self::wire::{
    BoardArgs, BoardResult, BoardRowWire, CiConclusionWire, ClaimWire,
    IssueArgs, IssueResult, IssueStateWire, PriorityWire,
    PullRequestStateWire, PullRequestStatusWire, RegisterArgs, RegisterResult,
    RelationshipsWire,
};

/// The HTTP path the MCP endpoint is mounted at.
///
/// Fixed rather than configurable: §11.1's global config carries a bind
/// address and nothing about a path, and every client is configured with
/// a full URL anyway.
pub const MCP_PATH: &str = "/mcp";

/// How many open issues one `board` call will page in.
///
/// A constant, not a config knob, because §6's `board(filter?)` is the
/// place paging/filtering belongs and that argument is unimplemented
/// (see the module doc) — inventing a `board_limit:` config field now
/// would pin a shape the eventual `filter` may well subsume. 200 is
/// generous next to the `max_concurrent_agents` handful a fleet actually
/// works at once, and a repo that exceeds it gets
/// [`BoardResult::truncated`] rather than a quietly short board.
pub const BOARD_ISSUE_LIMIT: u32 = 200;

/// The MCP protocol revisions this server advertises.
///
/// Every revision `rmcp` 3.1.0 knows about (`ProtocolVersion::KNOWN_VERSIONS`)
/// *except* `2026-07-28` — see the module doc's "CONFIRMED constraint"
/// section for why a session-less revision cannot carry §15's
/// per-connection project binding. `2024-11-05` is included: it predates
/// streamable HTTP as a named transport, but `rmcp`'s streamable-HTTP
/// server still routes it through `legacy_session_mode` like every other
/// pre-`2026-07-28` revision, so there is no reason to reject it here.
const SUPPORTED_PROTOCOL_VERSIONS: &[ProtocolVersion] = &[
    ProtocolVersion::V_2024_11_05,
    ProtocolVersion::V_2025_03_26,
    ProtocolVersion::V_2025_06_18,
    ProtocolVersion::V_2025_11_25,
];

/// Errors starting the daemon (§14.1's `spec-flow serve`).
///
/// No `Config` variant: [`serve`] takes an already-loaded
/// [`GlobalConfig`], so locating and reading `~/.config/spec-flow/
/// config.yaml` is the binary's failure to report (with its own
/// `anyhow` context about which file and what to do about it), not this
/// function's — the same injection split [`crate::init`] uses for its
/// [`crate::Vcs`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ServeError {
    /// The configured `listen` address is not a loopback address. A hard
    /// refusal, not a warning: §2.5 says the daemon "never binds a
    /// public interface" and describes a single-operator local service
    /// with "no auth, no RBAC", so a non-loopback bind would expose an
    /// unauthenticated control plane over every project on the machine.
    /// Remote access is §2.5's own answer — the operator's SSH tunnel.
    #[error(
        "listen address {addr} is not a loopback address; the daemon is \
         an unauthenticated single-operator service and never binds a \
         public interface (§2.5) — use 127.0.0.1 or ::1, and reach it \
         remotely over your own SSH tunnel"
    )]
    NonLoopbackListen {
        /// The rejected address from the global config.
        addr: SocketAddr,
    },

    /// The listen socket could not be bound.
    #[error("failed to bind {addr}")]
    Bind {
        /// The address that could not be bound.
        addr: SocketAddr,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },

    /// The HTTP server stopped with an error.
    #[error("the MCP server stopped unexpectedly")]
    Serve {
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
}

/// What one connection is bound to (§2.5, §6, §15).
///
/// Both halves are captured at `register` and never re-read: the pointer
/// for its `name` (the project handle §2.5 identifies a coordinator
/// session by) and the project's own config for the repo slug every `gh`
/// call is scoped with (§8.5).
#[derive(Clone, Debug)]
struct BoundProject {
    name: String,
    config: ProjectConfig,
}

/// State shared by every connection: read once at `serve`, never mutated.
#[derive(Debug)]
struct SharedState {
    global: GlobalConfig,
    vcs: Arc<ShellVcs>,
}

/// One MCP connection's handler — the `spec-flow` daemon as an
/// [`rmcp`] server (§5, §6).
///
/// One instance per session (see the module doc): `shared` is cloned
/// from the factory's captured [`Arc`], `bound` starts empty and is
/// filled exactly once by [`SpecFlowServer::register`].
#[derive(Clone)]
pub struct SpecFlowServer {
    shared: Arc<SharedState>,
    /// This connection's project, once registered.
    ///
    /// A `std::sync::Mutex`, not `tokio`'s: every critical section here
    /// is a clone or a compare of a small struct with no `.await` inside
    /// it, which is exactly the case the async-mutex guidance says to
    /// keep synchronous.
    bound: Arc<Mutex<Option<BoundProject>>>,
    /// The generated tool table. Read by the `#[tool_handler]`-generated
    /// `ServerHandler::call_tool`/`list_tools`.
    tool_router: ToolRouter<Self>,
}

#[tool_router(router = tool_router)]
impl SpecFlowServer {
    /// A fresh, unbound handler for one connection.
    fn new(shared: Arc<SharedState>) -> SpecFlowServer {
        SpecFlowServer {
            shared,
            bound: Arc::new(Mutex::new(None)),
            tool_router: SpecFlowServer::tool_router(),
        }
    }

    /// `register(worker_ref, role, cwd?, spawn_token?)` (§6) — announce
    /// this agent and bind the connection to a project for its life.
    ///
    /// Coordinator path only in this slice: no `spawn_token`, and a
    /// `cwd` that the daemon resolves against its project registry
    /// (§2.5). A directory outside every registered project is rejected
    /// with the "run `spec-flow init` there" answer §6 prescribes.
    #[tool(
        description = "Announce this agent and bind the connection to the \
                       registered project containing `cwd`. Every later \
                       tool call on this connection acts on that project \
                       and no other."
    )]
    pub async fn register(
        &self,
        Parameters(args): Parameters<RegisterArgs>,
    ) -> Result<Json<RegisterResult>, ErrorData> {
        let result = self.register_inner(args).await;
        result.map(Json).map_err(tool_error)
    }

    /// `board(filter?)` (§6, §13) — the orchestration board for the
    /// connection's bound project.
    #[tool(description = "The orchestration board for this connection's \
                       project: every open issue with its phase, owner, \
                       gates, approvals, dependencies, and PR/CI signals.")]
    pub async fn board(
        &self,
        Parameters(args): Parameters<BoardArgs>,
    ) -> Result<Json<BoardResult>, ErrorData> {
        self.board_inner(args).await.map(Json).map_err(tool_error)
    }

    /// `issue(number)` (§6) — one issue's derived state.
    #[tool(description = "One issue's derived state (phase, gates, \
                       approvals, owner, dependencies, linked pull \
                       requests) within this connection's project.")]
    pub async fn issue(
        &self,
        Parameters(args): Parameters<IssueArgs>,
    ) -> Result<Json<IssueResult>, ErrorData> {
        self.issue_inner(args).await.map(Json).map_err(tool_error)
    }

    // -- tool bodies, split out so the `?` operator is usable and the
    //    error mapping happens in exactly one place per tool --

    async fn register_inner(
        &self,
        args: RegisterArgs,
    ) -> Result<RegisterResult, ToolError> {
        if args.spawn_token.is_some() {
            return Err(ToolError::SpawnTokenUnsupported);
        }
        let cwd = args.cwd.ok_or(ToolError::CwdRequired)?;

        // Off the async worker thread like every other `Vcs`/config-file
        // read this handler reaches — `resolve_project` reads
        // `<project_dir>/config.yaml` from disk, so it belongs in
        // `blocking()` for the same reason `board`/`issue`'s reads do,
        // even though this one file is small.
        let global = self.shared.global.clone();
        let cwd_owned = cwd.clone();
        let (pointer, config) =
            blocking(move || logic::resolve_project(&global, &cwd_owned))
                .await?;

        let effective = {
            let mut bound = self.lock_bound();
            match bound.as_ref() {
                // §15: bound "for its life ... and never changed". A
                // re-`register` naming the *same* project changes
                // nothing, so it is accepted as the idempotent retry it
                // almost certainly is; naming a different one is the
                // cross-project act the rule forbids outright.
                Some(existing) if existing.name != pointer.name => {
                    return Err(ToolError::AlreadyBound {
                        bound: existing.name.clone(),
                        requested: pointer.name.clone(),
                    });
                }
                // Already bound to this same project: the stored
                // binding is left untouched rather than overwritten from
                // the fresh read above, so an operator editing
                // `config.yaml` between two `register` calls on one
                // connection cannot change that connection's effective
                // repo mid-life. The *reported* result is taken from
                // this same untouched binding, not from `config`, so a
                // client is never told about a repo this connection
                // will not actually act against.
                Some(existing) => existing.clone(),
                None => {
                    let fresh = BoundProject {
                        name: pointer.name.clone(),
                        config: config.clone(),
                    };
                    *bound = Some(fresh.clone());
                    fresh
                }
            }
        };

        tracing::info!(
            project = %effective.name,
            repo = %effective.config.repo,
            role = %args.role,
            worker_ref = %args.worker_ref,
            "connection bound to project"
        );

        Ok(RegisterResult {
            project: effective.name,
            repo: effective.config.repo.clone(),
            project_dir: effective.config.project_dir.display().to_string(),
            role: args.role,
            worker_ref: args.worker_ref,
        })
    }

    async fn board_inner(
        &self,
        args: BoardArgs,
    ) -> Result<BoardResult, ToolError> {
        if args.filter.is_some() {
            return Err(ToolError::BoardFilterUnsupported);
        }
        let bound = self.bound_project()?;
        let vcs = Arc::clone(&self.shared.vcs);

        // Read the binding once, before the `await`, and report *that*
        // one back: re-reading it afterwards would let a concurrent
        // `register` on the same connection relabel rows that were
        // fetched against a different project.
        let for_task = bound.clone();
        let (rows, truncated) = blocking(move || {
            logic::read_board(
                vcs.as_ref(),
                &for_task.config,
                BOARD_ISSUE_LIMIT,
            )
        })
        .await?;

        Ok(BoardResult {
            project: bound.name,
            repo: bound.config.repo,
            rows: rows.iter().map(Into::into).collect(),
            issue_limit: BOARD_ISSUE_LIMIT,
            truncated,
        })
    }

    async fn issue_inner(
        &self,
        args: IssueArgs,
    ) -> Result<IssueResult, ToolError> {
        let bound = self.bound_project()?;
        let vcs = Arc::clone(&self.shared.vcs);
        let number = args.number;

        let for_task = bound.clone();
        let (snapshot, url) = blocking(move || {
            logic::read_issue(vcs.as_ref(), &for_task.config, number)
        })
        .await?;

        Ok(IssueResult::from_snapshot(
            &snapshot,
            &bound.name,
            &bound.config.repo,
            url,
        ))
    }

    /// This connection's bound project, or [`ToolError::NotRegistered`].
    fn bound_project(&self) -> Result<BoundProject, ToolError> {
        self.lock_bound().clone().ok_or(ToolError::NotRegistered)
    }

    /// Lock `bound`, recovering from a poisoned mutex.
    ///
    /// A poisoned lock here means some earlier tool call panicked while
    /// holding it. The data it guards is a plain `Option<BoundProject>`
    /// that is only ever wholesale-replaced, so it cannot have been left
    /// half-updated — treating the poison as fatal would kill an
    /// otherwise healthy session over a bit that carries no information
    /// about this data.
    fn lock_bound(&self) -> std::sync::MutexGuard<'_, Option<BoundProject>> {
        self.bound.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for SpecFlowServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            // `ServerInfo::new` defaults `server_info` to
            // `Implementation::from_build_env()`, whose `env!` macros are
            // expanded *inside the `rmcp` crate* — so without this the
            // handshake announces `{"name":"rmcp","version":"3.1.0"}`,
            // confirmed live against the built binary. §14.1's
            // compatibility contract ("which binary speaks which MCP
            // tool-surface revision") needs this crate's own identity.
            .with_server_info(rmcp::model::Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "spec-flow: the work queue and git/GitHub mechanics for \
                 AI-agent software delivery. Call `register` first with \
                 the directory this session was opened in — it binds \
                 this connection to exactly one project, and every other \
                 tool acts on that project only.",
            )
    }

    /// See the module doc: `2026-07-28` removes MCP sessions, and this
    /// server's per-connection project binding cannot exist without one.
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(SUPPORTED_PROTOCOL_VERSIONS)
    }
}

/// Run a blocking `git`/`gh` call off the async runtime's worker threads.
///
/// [`crate::vcs`] is synchronous by design (§2.1's "shell out to the
/// operator's local `git`/`gh`"), and a `board` call fans out to dozens
/// of subprocesses; running those inline would park a `tokio` worker for
/// the whole fan-out. A join error means the closure itself panicked —
/// reported as [`ToolError::Internal`], not a [`VcsError`](crate::vcs::VcsError),
/// since a panic in this crate's own code is not an environment failure a
/// client could do anything about by fixing their `git`/`gh` install.
async fn blocking<T, F>(f: F) -> Result<T, ToolError>
where
    F: FnOnce() -> Result<T, ToolError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(join_error) => {
            tracing::error!(
                error = %join_error,
                "a git/gh worker task failed"
            );
            Err(ToolError::Internal(join_error.to_string()))
        }
    }
}

/// Map a [`ToolError`] onto the JSON-RPC error the client sees.
///
/// The split is by whose problem it is: a malformed or unsupported
/// argument is `invalid_params`, a connection used out of order is
/// `invalid_request`, and a config/`gh` failure is `internal_error` —
/// so a client can tell "fix your call" from "fix your machine" without
/// parsing the message text.
fn tool_error(error: ToolError) -> ErrorData {
    let message = format!("{error}");
    match error {
        ToolError::CwdRequired
        | ToolError::CwdNotAbsolute { .. }
        | ToolError::UnregisteredDirectory { .. }
        | ToolError::SpawnTokenUnsupported
        | ToolError::BoardFilterUnsupported => {
            ErrorData::invalid_params(message, None)
        }
        ToolError::NotRegistered | ToolError::AlreadyBound { .. } => {
            ErrorData::invalid_request(message, None)
        }
        ToolError::Config(_) | ToolError::Vcs(_) => {
            // The source chain carries the actual `gh` stderr / config
            // path; `{error}` alone would print only the outermost
            // `#[error(transparent)]` line.
            ErrorData::internal_error(format_chain(&error), None)
        }
        ToolError::Internal(_) => ErrorData::internal_error(message, None),
    }
}

/// Render an error and every source under it as one line.
///
/// The library's error types follow the style guide's `#[source]`
/// chaining, but MCP carries a single `message` string — so the chain is
/// flattened here rather than lost.
fn format_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

/// Build the `tower` service that serves MCP over streamable HTTP/SSE.
///
/// Split out from [`serve`] so tests can mount it on their own
/// ephemeral listener (see `tests/mcp_server.rs`) without a global
/// config file or a fixed port.
///
/// `rmcp`'s default [`StreamableHttpServerConfig`] is mostly what §2.5
/// wants and is left alone for two fields: `legacy_session_mode: true`
/// (the per-session handler instance this server's project binding
/// depends on) and `allowed_hosts: ["localhost", "127.0.0.1", "::1"]`
/// (inbound `Host` validation, which is what actually stops a
/// DNS-rebinding browser from reaching a loopback-bound daemon).
///
/// One field is overridden: `stateless_protocol_metadata_required: true`.
/// The routing decision between the legacy (session-bearing) path and the
/// stateless `2026-07-28` path is made from the client's `initialize`
/// body *or*, failing that, its raw `MCP-Protocol-Version` header
/// (`rmcp`'s own `tower.rs`) — a non-compliant client could set that
/// header to `2026-07-28` without ever completing an `initialize` that
/// negotiates it, land on the stateless path this server never advertises
/// support for, and get a `register` that silently binds a
/// throwaway-per-request handler instead of the rejection the module
/// doc's "CONFIRMED constraint" section promises. This option makes that
/// path require the per-request protocol metadata `2026-07-28` itself
/// specifies, so a client without it is rejected before dispatch instead
/// of silently losing its binding — the same "reject, don't ignore"
/// stance the rest of this slice takes on `spawn_token`/`filter`. Every
/// client this server actually advertises support for negotiates below
/// `2026-07-28` and is routed through the unaffected legacy path (per this
/// field's own doc), so this cannot reject a compliant caller.
pub fn mcp_service(
    global: GlobalConfig,
    vcs: ShellVcs,
) -> StreamableHttpService<SpecFlowServer, LocalSessionManager> {
    let shared = Arc::new(SharedState { global, vcs: Arc::new(vcs) });

    let mut config = StreamableHttpServerConfig::default();
    config.stateless_protocol_metadata_required = true;

    StreamableHttpService::new(
        move || Ok(SpecFlowServer::new(Arc::clone(&shared))),
        Arc::new(LocalSessionManager::default()),
        config,
    )
}

/// Run the daemon (§14.1's `spec-flow serve`) until Ctrl-C.
///
/// Binds `global.listen` (§11.1) and serves MCP at [`MCP_PATH`]. `vcs`
/// is passed in rather than constructed here so the binary keeps
/// ownership of resolving §8.5's configured `git`/`gh` paths — the same
/// split [`crate::init`] uses.
pub async fn serve(
    global: GlobalConfig,
    vcs: ShellVcs,
) -> Result<(), ServeError> {
    let addr = global.listen;
    if !addr.ip().is_loopback() {
        return Err(ServeError::NonLoopbackListen { addr });
    }

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|source| ServeError::Bind { addr, source })?;

    let projects = global.projects.len();
    let router =
        axum::Router::new().nest_service(MCP_PATH, mcp_service(global, vcs));

    tracing::info!(
        %addr,
        path = MCP_PATH,
        projects,
        "spec-flow serving MCP over HTTP/SSE"
    );

    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            match tokio::signal::ctrl_c().await {
                Ok(()) => tracing::info!("shutting down"),
                Err(error) => tracing::error!(
                    %error,
                    "could not listen for Ctrl-C; the server will keep \
                     running until killed"
                ),
            }
        })
        .await
        .map_err(|source| ServeError::Serve { source })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `ToolError` for each variant's mapping test, built without
    /// reaching for a real config or `Vcs`.
    fn errors() -> Vec<(ToolError, &'static str)> {
        vec![
            (ToolError::CwdRequired, "invalid_params"),
            (
                ToolError::CwdNotAbsolute { cwd: "rel".to_string() },
                "invalid_params",
            ),
            (
                ToolError::UnregisteredDirectory { cwd: "/x".to_string() },
                "invalid_params",
            ),
            (ToolError::SpawnTokenUnsupported, "invalid_params"),
            (ToolError::BoardFilterUnsupported, "invalid_params"),
            (ToolError::NotRegistered, "invalid_request"),
            (
                ToolError::AlreadyBound {
                    bound: "a".to_string(),
                    requested: "b".to_string(),
                },
                "invalid_request",
            ),
        ]
    }

    #[test]
    fn tool_error_maps_client_mistakes_to_the_right_json_rpc_codes() {
        for (error, expected) in errors() {
            let code = match expected {
                "invalid_params" => rmcp::model::ErrorCode::INVALID_PARAMS,
                "invalid_request" => rmcp::model::ErrorCode::INVALID_REQUEST,
                other => panic!("unexpected expectation {other}"),
            };

            assert_eq!(tool_error(error).code, code);
        }
    }

    #[test]
    fn tool_error_maps_environment_failures_to_internal_error() {
        let error = ToolError::Vcs(crate::vcs::VcsError::BinaryNotFound {
            binary: "gh".to_string(),
        });

        assert_eq!(
            tool_error(error).code,
            rmcp::model::ErrorCode::INTERNAL_ERROR
        );
    }

    #[test]
    fn tool_error_maps_a_panicked_worker_task_to_internal_error_not_vcs() {
        // A `spawn_blocking` join failure is a bug in this crate, not an
        // environment problem -- it must not be reported as though `gh`
        // itself failed (see `blocking`'s doc).
        let error = ToolError::Internal("panicked".to_string());

        assert_eq!(
            tool_error(error).code,
            rmcp::model::ErrorCode::INTERNAL_ERROR
        );
    }

    #[test]
    fn tool_error_keeps_the_source_chain_in_the_message() {
        // `#[error(transparent)]` wrappers print only their own line;
        // without the flattening this asserts, a `gh` failure would
        // reach the client as an empty-ish message.
        let error = ToolError::Vcs(crate::vcs::VcsError::CommandFailed {
            command: "gh issue list".to_string(),
            status: 1,
            stderr: "could not resolve to a Repository".to_string(),
        });

        let message = tool_error(error).message.to_string();

        assert!(
            message.contains("could not resolve to a Repository"),
            "message lost its cause: {message}"
        );
    }

    #[test]
    fn supported_protocol_versions_excludes_the_session_less_revision() {
        // The load-bearing assertion behind this module's "the
        // connection is the project" design -- see the module doc.
        assert!(
            !SUPPORTED_PROTOCOL_VERSIONS
                .contains(&ProtocolVersion::V_2026_07_28),
            "advertising a session-less revision would break §15's \
             per-connection project binding"
        );
        assert!(
            SUPPORTED_PROTOCOL_VERSIONS
                .contains(&ProtocolVersion::V_2025_11_25)
        );
        // Pinned so the list stays "every known revision except
        // 2026-07-28" -- see this constant's doc for why 2024-11-05
        // belongs here despite predating the "streamable HTTP" name.
        assert!(
            SUPPORTED_PROTOCOL_VERSIONS
                .contains(&ProtocolVersion::V_2024_11_05)
        );
    }

    #[test]
    fn the_server_handler_actually_advertises_supported_protocol_versions() {
        // The constant above is only load-bearing if `SpecFlowServer`'s
        // `ServerHandler` override actually returns it -- this pins the
        // wiring, not just the list, so deleting the override in
        // `impl ServerHandler for SpecFlowServer` (which would silently
        // fall back to rmcp's default, every known revision including
        // 2026-07-28) fails a test instead of only regressing silently.
        let global = GlobalConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            instance_id: None,
            binaries: crate::config::Binaries::default(),
            harnesses: crate::config::HarnessesConfig {
                default: "claude".to_string(),
                harnesses: std::collections::HashMap::new(),
            },
            limits: crate::config::Limits { max_concurrent_agents: 3 },
            cross_project_mode: crate::config::CrossProjectMode::FairShare,
            claim: crate::config::ClaimConfig {
                heartbeat_ttl: std::time::Duration::from_secs(3600),
                heartbeat_interval: std::time::Duration::from_secs(300),
            },
            phase_timeout: std::time::Duration::from_secs(45 * 60),
            projects: Vec::new(),
        };
        let vcs = ShellVcs::new("git".to_string(), "gh".to_string());
        let shared = Arc::new(SharedState { global, vcs: Arc::new(vcs) });
        let server = SpecFlowServer::new(shared);

        assert_eq!(
            ServerHandler::supported_protocol_versions(&server).as_ref(),
            SUPPORTED_PROTOCOL_VERSIONS
        );
    }
}
