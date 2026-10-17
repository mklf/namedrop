use namedrop::{
    ActorKind, AgentRuntime, Clock, HerdrCommandExecutor, HerdrCommandOutput, HookOutputFormat,
    IdGenerator, LifecycleRuntime, MutationAttribution, StateNamespace, StateStore,
    execute_lifecycle_operation, format_user_prompt_submit_hook_output,
    install_integration_package,
};
use serde_json::json;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    home: PathBuf,
    state: PathBuf,
    binary: PathBuf,
    ids: Ids,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "namedrop-kimi-install-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let home = root.join("home");
        let state = root.join("plugin state");
        fs::create_dir_all(home.join(".kimi-code")).unwrap();
        fs::create_dir_all(&state).unwrap();
        let binary = root.join("checkout with spaces/namedrop");
        Self {
            root,
            home,
            state,
            binary,
            ids: Ids(AtomicU64::new(0)),
        }
    }

    fn config_path(&self) -> PathBuf {
        self.home.join(".kimi-code/config.toml")
    }

    fn config(&self) -> String {
        fs::read_to_string(self.config_path()).unwrap()
    }

    fn install(&self) -> PathBuf {
        install_integration_package(
            &self.home,
            AgentRuntime::KimiCode,
            &self.binary,
            &self.state,
        )
        .unwrap()
    }

    fn runtime(&self) -> LifecycleRuntime<'_, Host, Time, Ids> {
        LifecycleRuntime {
            state_directory: &self.state,
            home_directory: Some(&self.home),
            agent_config: namedrop::AgentConfigDirectories::default(),
            cwd: Path::new("/repo"),
            workspace_id: None,
            current_pane_id: Some("pane-agent"),
            session_socket_id: "socket-a",
            herdr: &Host,
            clock: &Time,
            ids: &self.ids,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct Host;
impl HerdrCommandExecutor for Host {
    fn execute(&self, args: &[&str]) -> Result<HerdrCommandOutput, String> {
        let stdout = match args {
            ["workspace", "list"] => {
                r#"{"result":{"workspaces":[{"workspace_id":"workspace-1"}]}}"#
            }
            ["worktree", "list", "--cwd", "/repo", "--json"] | ["worktree", "list", "--json"] => {
                r#"{"result":{"worktrees":[{"path":"/repo","open_workspace_id":"workspace-1"}]}}"#
            }
            ["pane", "list", "--workspace", "workspace-1"] => {
                r#"{"result":{"panes":[{"pane_id":"pane-agent","terminal_id":"terminal-agent","tab_id":"tab-1","workspace_id":"workspace-1","agent":"kimi","agent_session":{"value":"session-1"},"agent_status":"idle"}]}}"#
            }
            _ => return Err(format!("unexpected Herdr command: {args:?}")),
        };
        Ok(HerdrCommandOutput {
            stdout: stdout.to_owned(),
        })
    }
}

struct Time;
impl Clock for Time {
    fn now_unix_ms(&self) -> u64 {
        42
    }
    fn wait_ms(&self, _duration_ms: u64) {}
}

struct Ids(AtomicU64);
impl IdGenerator for Ids {
    fn next_id(&self, kind: &str) -> String {
        format!("{kind}-{}", self.0.fetch_add(1, Ordering::Relaxed))
    }
}

#[test]
fn fresh_install_preserves_user_config_and_adds_one_hook_and_scoped_permissions() {
    let fixture = Fixture::new();
    fs::write(
        fixture.config_path(),
        "# user comment\ndefault_model = \"user-model\"\n\n[[permission.rules]]\ndecision = \"deny\"\npattern = \"Bash(rm -rf*)\"\nreason = \"user safety\"\n\n[[hooks]]\nevent = \"SessionStart\"\ncommand = \"user-hook\"\n",
    )
    .unwrap();
    let package = fixture.install();
    let config = fixture.config();
    assert!(config.starts_with("# user comment\ndefault_model = \"user-model\""));
    assert!(config.contains("pattern = \"Bash(rm -rf*)\""));
    assert!(config.contains("command = \"user-hook\""));
    assert_eq!(config.matches("[[permission.rules]]").count(), 21);
    assert_eq!(config.matches("[[hooks]]").count(), 2);
    assert_eq!(config.matches("event = \"UserPromptSubmit\"").count(), 1);
    assert_eq!(config.matches("Namedrop local collaboration").count(), 20);
    assert_eq!(
        config.matches(" *{,/**/*})").count(),
        20,
        "Kimi permission globs must match absolute and relative paths after the operation"
    );
    assert!(!config.contains("cancel-held"));
    assert!(
        config.contains(
            &package
                .join("scripts/user-prompt-submit")
                .to_string_lossy()
                .into_owned()
        )
    );
    let instructions = fs::read_to_string(package.join("SKILL.md")).unwrap();
    assert!(instructions.contains("Kimi Code: Activation installs the UserPromptSubmit hook"));
    assert!(!instructions.contains("Codex: request elevated"));
    let wrapper = fs::read_to_string(package.join("scripts/user-prompt-submit")).unwrap();
    assert!(wrapper.contains("--format kimi"));
}

#[test]
fn repeated_install_is_stable_and_runtime_update_replaces_only_managed_entries() {
    let fixture = Fixture::new();
    fixture.install();
    let before = fs::metadata(fixture.config_path())
        .unwrap()
        .modified()
        .unwrap();
    fixture.install();
    assert_eq!(
        fs::metadata(fixture.config_path())
            .unwrap()
            .modified()
            .unwrap(),
        before
    );
    let next = fixture.root.join("next runtime/namedrop");
    install_integration_package(&fixture.home, AgentRuntime::KimiCode, &next, &fixture.state)
        .unwrap();
    let config = fixture.config();
    assert!(!config.contains(&fixture.binary.to_string_lossy().into_owned()));
    assert!(config.contains(&next.to_string_lossy().into_owned()));
    assert_eq!(config.matches("[[permission.rules]]").count(), 20);
    assert_eq!(config.matches("[[hooks]]").count(), 1);
}

#[test]
fn glob_special_paths_are_escaped_in_kimi_permission_patterns() {
    let fixture = Fixture::new();
    let binary = fixture.root.join("glob*?[x]/namedrop");
    install_integration_package(
        &fixture.home,
        AgentRuntime::KimiCode,
        &binary,
        &fixture.state,
    )
    .unwrap();
    let config = fixture.config();
    assert!(config.contains(r"glob\*\?\[x\]/namedrop"), "{config}");
    assert!(config.contains("plugin state"), "{config}");
}

#[test]
fn invalid_or_symlinked_config_is_refused_without_touching_the_target() {
    let fixture = Fixture::new();
    fs::write(fixture.config_path(), "not valid = [toml\n").unwrap();
    let before = fixture.config();
    let error = install_integration_package(
        &fixture.home,
        AgentRuntime::KimiCode,
        &fixture.binary,
        &fixture.state,
    )
    .unwrap_err();
    assert!(error.contains("not valid TOML"), "{error}");
    assert_eq!(fixture.config(), before);

    fs::remove_file(fixture.config_path()).unwrap();
    let target = fixture.root.join("user target");
    fs::write(&target, "default_model = \"keep\"\n").unwrap();
    std::os::unix::fs::symlink(&target, fixture.config_path()).unwrap();
    let error = install_integration_package(
        &fixture.home,
        AgentRuntime::KimiCode,
        &fixture.binary,
        &fixture.state,
    )
    .unwrap_err();
    assert!(error.contains("not a regular file or directory"), "{error}");
    assert_eq!(
        fs::read_to_string(target).unwrap(),
        "default_model = \"keep\"\n"
    );
}

#[test]
fn hook_output_uses_kimi_message_and_permission_decision_shapes() {
    let context = namedrop::format_user_prompt_submit_hook_output(
        json!({"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"Namedrop workspace roster. custom"}}),
        HookOutputFormat::Kimi,
    )
    .unwrap();
    assert_eq!(
        context,
        json!({"message":"Namedrop workspace roster. custom"})
    );
    let block = format_user_prompt_submit_hook_output(
        json!({"decision":"block","reason":"Collaboration message sent."}),
        HookOutputFormat::Kimi,
    )
    .unwrap();
    assert_eq!(
        block,
        json!({
            "message":"Collaboration message sent.",
            "hookSpecificOutput": {
                "permissionDecision":"deny",
                "permissionDecisionReason":"Collaboration message sent."
            }
        })
    );
    assert!(
        format_user_prompt_submit_hook_output(json!({"unexpected":true}), HookOutputFormat::Kimi)
            .is_none()
    );
    let legacy = json!({"decision":"block","reason":"Collaboration message sent."});
    assert_eq!(
        format_user_prompt_submit_hook_output(legacy.clone(), HookOutputFormat::Legacy),
        Some(legacy)
    );
}

#[test]
fn generated_wrapper_forwards_kimi_block_and_drops_legacy_or_unknown_output() {
    let fixture = Fixture::new();
    let block = format_user_prompt_submit_hook_output(
        json!({"decision":"block","reason":"Collaboration message sent."}),
        HookOutputFormat::Kimi,
    )
    .unwrap()
    .to_string();
    fs::create_dir_all(fixture.binary.parent().unwrap()).unwrap();
    fs::write(
        &fixture.binary,
        format!("#!/bin/sh\nprintf '%s' '{}'\n", block),
    )
    .unwrap();
    fs::set_permissions(&fixture.binary, fs::Permissions::from_mode(0o700)).unwrap();
    let package = fixture.install();
    let wrapper = package.join("scripts/user-prompt-submit");
    let output = Command::new(&wrapper).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8(output.stdout).unwrap(), block);

    fs::write(
        &fixture.binary,
        "#!/bin/sh\nprintf '%s' '{\"decision\":\"block\",\"reason\":\"Collaboration message sent.\"}'\n",
    )
    .unwrap();
    fs::set_permissions(&fixture.binary, fs::Permissions::from_mode(0o700)).unwrap();
    let output = Command::new(wrapper).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
}

#[test]
fn deactivation_removes_only_entries_owned_by_namedrop() {
    let fixture = Fixture::new();
    fs::write(
        fixture.config_path(),
        "default_model = \"keep\"\n\n[[permission.rules]]\ndecision = \"allow\"\npattern = \"Read\"\n\n[[hooks]]\nevent = \"SessionStart\"\ncommand = \"user-hook\"\n",
    )
    .unwrap();
    fixture.install();
    let namespace = StateNamespace::for_space("socket-a", "workspace-1");
    let other = StateNamespace::for_space("socket-a", "workspace-2");
    StateStore::open(&fixture.state)
        .unwrap()
        .transaction(|transaction| {
            let attribution = MutationAttribution::new(ActorKind::Human, "test", 1);
            transaction.activate_worktree(&namespace, &attribution)?;
            transaction.activate_worktree(&other, &attribution)
        })
        .unwrap();
    let first = execute_lifecycle_operation("deactivate", json!({}), &fixture.runtime()).unwrap();
    assert_eq!(first["kimi_configuration"]["removed"], false);
    assert!(fixture.config().contains("Namedrop local collaboration"));
    StateStore::open(&fixture.state)
        .unwrap()
        .transaction(|transaction| {
            transaction.deactivate_worktree(
                &other,
                &MutationAttribution::new(ActorKind::Human, "test", 2),
            )
        })
        .unwrap();
    let result = execute_lifecycle_operation("deactivate", json!({}), &fixture.runtime()).unwrap();
    assert_eq!(result["kimi_configuration"]["removed"], true, "{result}");
    assert_eq!(
        result["kimi_configuration"]["removed_entries"], 21,
        "{result}"
    );
    let config = fixture.config();
    assert!(config.contains("default_model = \"keep\""));
    assert!(config.contains("pattern = \"Read\""));
    assert!(config.contains("command = \"user-hook\""));
    assert!(!config.contains("Namedrop local collaboration"));
    assert!(!config.contains("UserPromptSubmit"));
}

#[test]
fn kimi_home_override_controls_package_and_config_locations() {
    let fixture = Fixture::new();
    let custom = fixture.root.join("custom kimi home");
    let mut runtime = fixture.runtime();
    runtime.agent_config.kimi = Some(custom.as_path());
    let activated =
        execute_lifecycle_operation("activate", json!({"confirmed":true}), &runtime).unwrap();
    let package = custom.join("skills/namedrop");
    assert_eq!(
        activated["integration_packages"]["kimi-code"]["path"],
        json!(package)
    );
    assert!(package.join("SKILL.md").is_file());
    assert!(custom.join("config.toml").is_file());
    assert_eq!(
        activated["integration_packages"]["kimi-code"]["permissions"]["current"],
        true
    );
    assert!(!fixture.config_path().exists());
    assert!(!fixture.home.join(".kimi-code/skills/namedrop").exists());
}
