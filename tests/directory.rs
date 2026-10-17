use clap::Parser;
use namedrop::{
    ActorKind, AgentRuntime, AgentSessionId, Clock, HerdrCommandExecutor, HerdrCommandOutput,
    IdGenerator, LifecycleRuntime, MutationAttribution, PaneAddressDraft, PaneAddressId,
    PaneBindingId, PaneRoleId, StateNamespace, StateStore, complete_capability_profile,
    execute_control_operation, execute_directory_operation, execute_group_operation,
    execute_integration_operation, execute_lifecycle_operation, execute_message_operation,
    issue_registration_challenge, operation_catalog,
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct TestDirectory {
    root: PathBuf,
    state: PathBuf,
    home: PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "namedrop-directory-{}-{sequence}",
            std::process::id()
        ));
        let state = root.join("state");
        let home = root.join("home");
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(&home).unwrap();
        Self { root, state, home }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct FakeHerdr;

impl HerdrCommandExecutor for FakeHerdr {
    fn execute(&self, args: &[&str]) -> Result<HerdrCommandOutput, String> {
        let panes = || {
            json!({"result":{"panes":[
                pane("lead", Some("session-lead"), "idle", true),
                pane("reviewer", Some("session-reviewer"), "idle", true),
                pane("generic", Some("session-generic"), "idle", true),
                pane("busy", Some("session-busy"), "working", true),
                pane("quiet", None, "unknown", false),
                pane("build", None, "unknown", false),
            ]}})
            .to_string()
        };
        let stdout = match args {
            ["workspace", "list"] => {
                r#"{"result":{"workspaces":[{"workspace_id":"workspace-1"}]}}"#.to_owned()
            }
            ["worktree", "list", "--cwd", "/repo", "--json"]
            | ["worktree", "list", "--json"]
            | ["worktree", "list", "--workspace", "workspace-1", "--json"] => {
                r#"{"result":{"worktrees":[{"path":"/repo","open_workspace_id":"workspace-1"}]}}"#
                    .to_owned()
            }
            ["pane", "list", "--workspace", "workspace-1"] | ["pane", "list"] => panes(),
            ["agent", "prompt", ..] => {
                return Err("unexpected dispatch in directory test".to_owned());
            }
            _ => return Err(format!("unexpected Herdr command: {args:?}")),
        };
        Ok(HerdrCommandOutput { stdout })
    }
}

fn pane(alias: &str, session: Option<&str>, status: &str, agent: bool) -> Value {
    json!({
        "pane_id": format!("pane-{alias}"),
        "terminal_id": format!("terminal-{alias}"),
        "tab_id": format!("tab-{alias}"),
        "workspace_id": "workspace-1",
        "agent": agent.then_some("codex"),
        "display_agent": agent.then_some("Codex"),
        "agent_session": session.map(|session| json!({"value":session})),
        "agent_status": status,
    })
}

struct FixedClock;
impl Clock for FixedClock {
    fn now_unix_ms(&self) -> u64 {
        100
    }

    fn wait_ms(&self, _duration_ms: u64) {}
}

struct SequenceIds(AtomicU64);
impl IdGenerator for SequenceIds {
    fn next_id(&self, kind: &str) -> String {
        format!("{kind}-{}", self.0.fetch_add(1, Ordering::Relaxed))
    }
}

fn namespace() -> StateNamespace {
    StateNamespace::for_space("socket-a", "workspace-1")
}

fn attribution() -> MutationAttribution {
    MutationAttribution::new(ActorKind::Human, "directory-test", 1)
}

fn draft(alias: &str) -> PaneAddressDraft {
    PaneAddressDraft {
        id: PaneAddressId::new(format!("address-{alias}")),
        role_id: PaneRoleId::new(format!("role-{alias}")),
        role_name: format!("Role {alias}"),
        role_description: format!("Role description for {alias}"),
        alias: alias.to_owned(),
    }
}

fn runtime<'a>(
    directory: &'a TestDirectory,
    ids: &'a SequenceIds,
    pane_id: &'a str,
) -> LifecycleRuntime<'a, FakeHerdr, FixedClock, SequenceIds> {
    LifecycleRuntime {
        state_directory: &directory.state,
        home_directory: Some(&directory.home),
        agent_config: namedrop::AgentConfigDirectories::default(),
        cwd: Path::new("/repo"),
        workspace_id: Some("workspace-1"),
        current_pane_id: Some(pane_id),
        session_socket_id: "socket-a",
        herdr: &FakeHerdr,
        clock: &FixedClock,
        ids,
    }
}

fn register(directory: &TestDirectory, ids: &SequenceIds, alias: &str, address_id: &str) {
    let pane_id = format!("pane-{alias}");
    let runtime = runtime(directory, ids, &pane_id);
    let challenge = issue_registration_challenge(
        &PaneAddressId::new(address_id),
        AgentRuntime::Codex,
        &runtime,
    )
    .unwrap();
    execute_integration_operation(
        "register",
        json!({"runtime":"codex","protocol_version":"collab/v1","nonce":challenge.nonce,
            "capabilities":complete_capability_profile()}),
        &runtime,
    )
    .unwrap();
}

fn setup(directory: &TestDirectory) -> SequenceIds {
    let scope = namespace();
    StateStore::open(&directory.state)
        .unwrap()
        .transaction(|transaction| {
            transaction.activate_worktree(&scope, &attribution())?;
            for (alias, binding) in [
                ("lead", "pane-lead"),
                ("old-reviewer", "pane-reviewer"),
                ("generic", "pane-generic"),
                ("busy", "pane-busy"),
                ("quiet", "pane-quiet"),
                ("build", "pane-build"),
            ] {
                let mut address = draft(alias);
                if alias == "old-reviewer" {
                    address.id = PaneAddressId::new("address-reviewer");
                }
                transaction.create_pane_address(&scope, &address, &attribution())?;
                transaction.bind_pane(
                    &scope,
                    &PaneBindingId::new(format!("binding-{alias}")),
                    &address.id,
                    binding,
                    &attribution(),
                )?;
            }
            for (alias, session, status) in [
                ("lead", "session-lead", "idle"),
                ("reviewer", "session-reviewer", "idle"),
                ("generic", "session-generic", "idle"),
                ("busy", "session-busy", "working"),
            ] {
                transaction.observe_agent_session(
                    &scope,
                    &format!("observation-{alias}"),
                    &PaneAddressId::new(format!("address-{alias}")),
                    &AgentSessionId::new(session),
                    status,
                    &attribution(),
                )?;
            }
            transaction.replace_alias(
                &scope,
                &PaneAddressId::new("address-reviewer"),
                "reviewer",
                &attribution(),
            )?;
            transaction.set_address_persona(
                &scope,
                &PaneAddressId::new("address-reviewer"),
                Some("Reviews Rust changes for risk."),
            )?;
            let retired = draft("retired");
            transaction.create_pane_address(&scope, &retired, &attribution())?;
            transaction.retire_pane_address(&scope, &retired.id, &attribution())?;
            transaction.complete_reconciliation(&scope)?;
            let other_scope = StateNamespace::for_space("socket-b", "workspace-1");
            transaction.activate_worktree(&other_scope, &attribution())?;
            transaction.create_pane_address(&other_scope, &draft("other"), &attribution())?;
            transaction.complete_reconciliation(&other_scope)
        })
        .unwrap();
    rusqlite::Connection::open(directory.state.join("namedrop.sqlite3"))
        .unwrap()
        .execute(
            "UPDATE pane_addresses SET pane_type='terminal' WHERE address_id='address-build'",
            [],
        )
        .unwrap();
    let ids = SequenceIds(AtomicU64::new(1));
    for (alias, address_id) in [
        ("lead", "address-lead"),
        ("reviewer", "address-reviewer"),
        ("generic", "address-generic"),
        ("busy", "address-busy"),
    ] {
        register(directory, &ids, alias, address_id);
    }
    execute_group_operation(
        "group.set",
        json!({"alias":"team","members":["lead","reviewer","generic","reviewer"]}),
        &runtime(directory, &ids, "pane-lead"),
    )
    .unwrap();
    ids
}

fn resolve(directory: &TestDirectory, ids: &SequenceIds, input: Value) -> Result<Value, String> {
    execute_lifecycle_operation(
        "address.resolve",
        input,
        &runtime(directory, ids, "pane-lead"),
    )
}

fn candidates(directory: &TestDirectory, ids: &SequenceIds, input: Value) -> Result<Value, String> {
    execute_lifecycle_operation(
        "address.candidates",
        input,
        &runtime(directory, ids, "pane-lead"),
    )
}

fn result<'a>(resolved: &'a Value, query: &str) -> &'a Value {
    resolved["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|result| result["query"] == query)
        .unwrap_or_else(|| panic!("missing {query} in {resolved}"))
}

fn candidate<'a>(page: &'a Value, alias: &str) -> &'a Value {
    page["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|candidate| candidate["alias"] == alias)
        .unwrap_or_else(|| panic!("missing candidate {alias} in {page}"))
}

#[test]
fn resolve_is_exact_and_reports_self_groups_terminals_and_alias_history() {
    let directory = TestDirectory::new();
    let ids = setup(&directory);
    let resolved = resolve(
        &directory,
        &ids,
        json!({"aliases":["reviewer","team","old-reviewer","retired","missing","Reviewer","build"],
            "include_persona":true}),
    )
    .unwrap();

    let reviewer = result(&resolved, "reviewer");
    assert_eq!(reviewer["status"], "found");
    assert_eq!(reviewer["type"], "address");
    assert_eq!(reviewer["pane_type"], "agent");
    assert_eq!(reviewer["pane_binding_id"], "binding-old-reviewer");
    assert_eq!(reviewer["persona"], "Reviews Rust changes for risk.");
    assert_eq!(reviewer["readiness"]["automatic_selection_suitable"], true);

    let group = result(&resolved, "team");
    assert_eq!(group["status"], "found");
    assert_eq!(group["type"], "group");
    assert_eq!(
        group["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|member| member["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["address-lead", "address-reviewer", "address-generic"]
    );
    assert_eq!(
        group["group_fingerprint"],
        namedrop::group_membership_fingerprint(&[
            PaneAddressId::new("address-reviewer"),
            PaneAddressId::new("address-generic"),
        ])
    );

    let renamed = result(&resolved, "old-reviewer");
    assert_eq!(renamed["status"], "retired");
    assert_eq!(renamed["replacement"]["alias"], "reviewer");
    assert!(renamed.get("alias").is_none(), "replacement was followed");
    assert_eq!(result(&resolved, "retired")["status"], "retired");
    assert_eq!(result(&resolved, "missing")["status"], "unknown");
    assert_eq!(result(&resolved, "Reviewer")["status"], "unknown");
    assert_eq!(result(&resolved, "build")["pane_type"], "terminal");
    assert_eq!(
        result(&resolved, "build")["readiness"]["automatic_selection_suitable"],
        false
    );

    let own = resolve(&directory, &ids, json!({"self":true})).unwrap();
    assert_eq!(own["results"][0]["status"], "found");
    assert_eq!(own["results"][0]["self"], true);
    assert_eq!(own["results"][0]["id"], "address-lead");
    assert!(own["results"][0].get("persona").is_none());
}

#[test]
fn candidates_filter_before_deterministic_pagination_and_invalidate_cursors() {
    let directory = TestDirectory::new();
    let ids = setup(&directory);
    let first = candidates(&directory, &ids, json!({"limit":2})).unwrap();
    let aliases = |page: &Value| {
        page["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|candidate| candidate["alias"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(aliases(&first), ["build", "busy"]);
    assert_eq!(first["total"], 5);
    assert_eq!(first["has_more"], true);
    let cursor = first["next_cursor"].as_str().unwrap().to_owned();
    assert!(cursor.starts_with("directory-v1:"));
    let second = candidates(&directory, &ids, json!({"limit":2,"cursor":cursor})).unwrap();
    assert_eq!(aliases(&second), ["generic", "quiet"]);
    assert_eq!(second["offset"], 2);
    let third = candidates(
        &directory,
        &ids,
        json!({"limit":2,"cursor":second["next_cursor"]}),
    )
    .unwrap();
    assert_eq!(aliases(&third), ["reviewer"]);
    assert_eq!(third["has_more"], false);
    assert_eq!(third["next_cursor"], Value::Null);
    assert!(aliases(&third).into_iter().all(|alias| alias != "lead"));
    assert_eq!(candidate(&second, "generic")["persona"], Value::Null);

    let with_self = candidates(&directory, &ids, json!({"limit":64,"include_self":true})).unwrap();
    assert_eq!(with_self["total"], 6);
    assert_eq!(candidate(&with_self, "lead")["id"], "address-lead");
    let terminals = candidates(&directory, &ids, json!({"pane_type":"terminal"})).unwrap();
    assert_eq!(aliases(&terminals), ["build"]);
    let selectable = candidates(
        &directory,
        &ids,
        json!({"availability":"selectable","limit":1}),
    )
    .unwrap();
    assert_eq!(selectable["total"], 2);
    assert_eq!(aliases(&selectable), ["generic"]);
    assert!(selectable["has_more"].as_bool().unwrap());

    let stale = first["next_cursor"].as_str().unwrap().to_owned();
    StateStore::open(&directory.state)
        .unwrap()
        .transaction(|transaction| {
            transaction.observe_agent_session(
                &namespace(),
                "observation-reviewer-working",
                &PaneAddressId::new("address-reviewer"),
                &AgentSessionId::new("session-reviewer"),
                "working",
                &attribution(),
            )
        })
        .unwrap();
    let continued = candidates(&directory, &ids, json!({"cursor":stale.clone()})).unwrap();
    assert_eq!(continued["offset"], 2);
    assert!(
        candidates(
            &directory,
            &ids,
            json!({"cursor":stale,"availability":"selectable"})
        )
        .unwrap_err()
        .contains("different directory filters")
    );
    StateStore::open(&directory.state)
        .unwrap()
        .transaction(|transaction| {
            transaction.replace_alias(
                &namespace(),
                &PaneAddressId::new("address-reviewer"),
                "reviewer-new",
                &attribution(),
            )
        })
        .unwrap();
    assert!(
        candidates(&directory, &ids, json!({"cursor":stale}))
            .unwrap_err()
            .contains("stale")
    );
}

#[test]
fn readiness_distinguishes_queueing_selection_dispatch_and_held_work() {
    let directory = TestDirectory::new();
    let ids = setup(&directory);
    let initial = candidates(&directory, &ids, json!({"limit":64})).unwrap();
    let busy = candidate(&initial, "busy")["readiness"].clone();
    assert!(
        busy["constraints"]
            .as_array()
            .unwrap()
            .contains(&json!("recipient_busy_or_unknown"))
    );
    assert_eq!(busy["explicit_send_queueable"], true);
    assert_eq!(busy["automatic_selection_suitable"], false);
    assert_eq!(busy["dispatch_eligible"], false);

    execute_message_operation(
        "send",
        json!({"request_id":"directory-held","source":"@quiet Wait for registration."}),
        &runtime(&directory, &ids, "pane-lead"),
    )
    .unwrap();
    rusqlite::Connection::open(directory.state.join("namedrop.sqlite3"))
        .unwrap()
        .execute(
            "UPDATE mailbox_deliveries SET state='held' WHERE address_id='address-quiet'",
            [],
        )
        .unwrap();
    let held = candidates(&directory, &ids, json!({"limit":64})).unwrap();
    let quiet = candidate(&held, "quiet")["readiness"].clone();
    assert_eq!(quiet["earlier_deliveries"], 1);
    assert_eq!(quiet["held_deliveries"], 1);
    assert!(
        quiet["constraints"]
            .as_array()
            .unwrap()
            .contains(&json!("earlier_delivery_pending"))
    );
    assert_eq!(quiet["explicit_send_queueable"], true);

    execute_control_operation(
        "worktree.pause",
        json!({}),
        &runtime(&directory, &ids, "pane-lead"),
    )
    .unwrap();
    let paused = candidates(&directory, &ids, json!({"limit":64})).unwrap();
    let reviewer = candidate(&paused, "reviewer")["readiness"].clone();
    assert!(
        reviewer["constraints"]
            .as_array()
            .unwrap()
            .contains(&json!("space_paused"))
    );
    assert_eq!(reviewer["explicit_send_queueable"], true);
    assert_eq!(reviewer["automatic_selection_suitable"], false);
    assert_eq!(reviewer["dispatch_eligible"], false);
    assert_eq!(
        candidates(&directory, &ids, json!({"availability":"selectable"})).unwrap()["total"],
        0
    );

    let listed = execute_lifecycle_operation(
        "address.list",
        json!({}),
        &runtime(&directory, &ids, "pane-lead"),
    )
    .unwrap();
    assert!(listed.is_array(), "address.list top-level shape changed");
    let listed_reviewer = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|address| address["alias"] == "reviewer")
        .unwrap();
    assert_eq!(
        listed_reviewer["selection"],
        "collaboration paused; not selectable"
    );
    for field in [
        "id",
        "alias",
        "pane_type",
        "persona",
        "registered",
        "selection",
    ] {
        assert!(listed_reviewer.get(field).is_some(), "missing {field}");
    }
    assert!(listed_reviewer.get("readiness").is_none());
}

#[test]
fn directory_personas_are_sanitized_bounded_and_optional_on_resolve() {
    let directory = TestDirectory::new();
    let ids = setup(&directory);
    let hostile = format!("prefix\u{1b}[2J\u{202e}{}ROLE-SENTINEL", "x".repeat(400));
    rusqlite::Connection::open(directory.state.join("namedrop.sqlite3"))
        .unwrap()
        .execute(
            "UPDATE pane_addresses SET persona=?1 WHERE address_id='address-reviewer'",
            [hostile],
        )
        .unwrap();

    let without = resolve(&directory, &ids, json!({"aliases":["reviewer"]})).unwrap();
    assert!(without["results"][0].get("persona").is_none());
    let with = resolve(
        &directory,
        &ids,
        json!({"aliases":["reviewer"],"include_persona":true}),
    )
    .unwrap();
    let persona = with["results"][0]["persona"].as_str().unwrap();
    assert!(!persona.contains(['\u{1b}', '\u{202e}']));
    assert!(persona.contains("\\u{1b}") && persona.contains("\\u{202e}"));
    assert!(persona.contains("ROLE-SENTINEL"));
    assert!(persona.chars().count() <= namedrop::directory::DIRECTORY_PERSONA_FULL_CHARS);
    assert_eq!(with["results"][0]["persona_truncated"], false);

    let page = candidates(&directory, &ids, json!({"limit":64})).unwrap();
    let excerpt = candidate(&page, "reviewer")["persona"].as_str().unwrap();
    assert!(!excerpt.contains("ROLE-SENTINEL"));
    assert!(excerpt.ends_with('…'));
    assert_eq!(candidate(&page, "reviewer")["persona_truncated"], true);

    rusqlite::Connection::open(directory.state.join("namedrop.sqlite3"))
        .unwrap()
        .execute(
            "UPDATE pane_addresses SET persona=?1 WHERE address_id='address-reviewer'",
            ["y".repeat(1_200)],
        )
        .unwrap();
    let overlong = resolve(
        &directory,
        &ids,
        json!({"aliases":["reviewer"],"include_persona":true}),
    )
    .unwrap();
    assert_eq!(overlong["results"][0]["persona_truncated"], true);
    assert!(
        overlong["results"][0]["persona"]
            .as_str()
            .unwrap()
            .ends_with('…')
    );
    assert_eq!(candidate(&page, "generic")["persona"], Value::Null);
    let rendered = namedrop::render_command_result_to_text(&namedrop::CommandResult::success(
        "address.candidates",
        page,
    ));
    assert!(!rendered.contains(['\u{1b}', '\u{202e}']));
}

#[test]
fn reads_never_create_migrate_or_change_state() {
    let directory = TestDirectory::new();
    let ids = setup(&directory);
    let database = directory.state.join("namedrop.sqlite3");
    let before = fs::read(&database).unwrap();
    let files_before = fs::read_dir(&directory.state)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    resolve(&directory, &ids, json!({"aliases":["team","old-reviewer"]})).unwrap();
    candidates(&directory, &ids, json!({"limit":2})).unwrap();
    execute_lifecycle_operation(
        "address.list",
        json!({}),
        &runtime(&directory, &ids, "pane-lead"),
    )
    .unwrap();
    assert_eq!(fs::read(&database).unwrap(), before);
    let files_after = fs::read_dir(&directory.state)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert_eq!(files_after, files_before);

    let connection = rusqlite::Connection::open(&database).unwrap();
    connection.pragma_update(None, "user_version", 4).unwrap();
    drop(connection);
    let incompatible = fs::read(&database).unwrap();
    let error = resolve(&directory, &ids, json!({"aliases":["reviewer"]})).unwrap_err();
    assert!(error.contains("schema 4"), "{error}");
    let list_error = execute_lifecycle_operation(
        "address.list",
        json!({}),
        &runtime(&directory, &ids, "pane-lead"),
    )
    .unwrap_err();
    assert!(list_error.contains("schema 4"), "{list_error}");
    assert_eq!(fs::read(&database).unwrap(), incompatible);
    assert_eq!(
        rusqlite::Connection::open(&database)
            .unwrap()
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        4
    );

    let missing = TestDirectory::new();
    let missing_ids = SequenceIds(AtomicU64::new(1));
    let error = execute_directory_operation(
        "address.resolve",
        json!({"aliases":["reviewer"]}),
        &runtime(&missing, &missing_ids, "pane-lead"),
    )
    .unwrap_err();
    assert!(error.contains("no saved state"), "{error}");
    assert!(!missing.state.join("namedrop.sqlite3").exists());
}

#[test]
fn automatic_selection_capacity_rejects_a_second_assignment() {
    let directory = TestDirectory::new();
    let ids = setup(&directory);
    let runtime = runtime(&directory, &ids, "pane-lead");
    let input = |request_id: &str| {
        json!({
            "request_id": request_id,
            "source": "@reviewer Work on this.",
            "recipient_expectations": {
                "recipients": [{"address_id": "address-reviewer"}],
                "require_automatic_selection_suitable": true
            }
        })
    };
    execute_message_operation("send", input("capacity-first"), &runtime).unwrap();

    assert!(
        execute_message_operation("send", input("capacity-second"), &runtime)
            .unwrap_err()
            .starts_with("recipient_unavailable:")
    );
}

#[test]
fn namespaces_inputs_cli_and_registered_profile_keep_their_boundaries() {
    let directory = TestDirectory::new();
    let ids = setup(&directory);
    let other_runtime = LifecycleRuntime {
        session_socket_id: "socket-b",
        ..runtime(&directory, &ids, "pane-lead")
    };
    let other = execute_lifecycle_operation(
        "address.resolve",
        json!({"aliases":["reviewer","other"]}),
        &other_runtime,
    )
    .unwrap();
    assert_eq!(result(&other, "reviewer")["status"], "unknown");
    assert_eq!(result(&other, "other")["status"], "found");

    for input in [
        json!({}),
        json!({"aliases":[]}),
        json!({"aliases":["reviewer"],"self":true}),
        json!({"self":false}),
        json!({"aliases":[""]}),
        json!({"aliases":["@"]}),
        json!({"aliases":["x".repeat(129)]}),
        json!({"aliases":vec!["reviewer"; 33]}),
        json!({"aliases":["reviewer"],"surprise":true}),
    ] {
        assert!(resolve(&directory, &ids, input).is_err());
    }
    for input in [
        json!({"limit":0}),
        json!({"limit":65}),
        json!({"cursor":""}),
    ] {
        assert!(candidates(&directory, &ids, input).is_err());
    }

    for (token, argv) in [
        ("address.resolve", ["address", "resolve"]),
        ("address.candidates", ["address", "candidates"]),
    ] {
        let operation = operation_catalog().operation(token).unwrap();
        assert!(operation.automatic_permission);
        assert_eq!(operation.argv, argv);
        assert!(namedrop::is_lifecycle_operation(token));
        let cli = namedrop::cli::Cli::try_parse_from(
            std::iter::once("namedrop").chain(argv).collect::<Vec<_>>(),
        )
        .unwrap();
        assert_eq!(cli.operation(), token);
        assert!(
            operation_catalog()
                .management_operations
                .iter()
                .any(|operation| operation.token == token)
        );
        assert!(!complete_capability_profile().contains(&token.to_owned()));
    }
    assert_eq!(complete_capability_profile().len(), 17);
}
