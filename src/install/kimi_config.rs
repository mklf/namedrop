use super::{
    ReplacementError, collaboration_permission_patterns, conflict, replace_if_changed,
    replace_with_status, shell_single_quote,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::ErrorKind,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table};

const RECORD_FILE: &str = "kimi-config.json";
pub(super) const REPAIR_GUIDANCE: &str = "Kimi Code hooks and permissions are unavailable until config.toml is fixed and Namedrop is reactivated. Fix the reported conflict, then reactivate Namedrop. Loading the skill grants no permissions or hooks.";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct HookEntry {
    event: String,
    command: String,
    timeout: u64,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct OwnedConfiguration {
    config_path: PathBuf,
    rules: Vec<String>,
    hook: Option<HookEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending: Option<PendingUpdate>,
}

#[derive(Clone, Serialize, Deserialize)]
struct PendingUpdate {
    next: Box<OwnedConfiguration>,
    updates: Vec<JournalUpdate>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalUpdate {
    path: PathBuf,
    before_hash: Option<String>,
    after_hash: Option<String>,
    added_rules: Vec<String>,
    removed_rules: Vec<String>,
    added_hook: Option<HookEntry>,
    removed_hook: Option<HookEntry>,
    mode: u32,
}

#[derive(Clone)]
struct ConfigUpdate {
    path: PathBuf,
    before: Option<String>,
    after: Option<String>,
    added_rules: Vec<String>,
    removed_rules: Vec<String>,
    added_hook: Option<HookEntry>,
    removed_hook: Option<HookEntry>,
    mode: u32,
}

fn snapshot_hash(text: &Option<String>) -> Option<String> {
    text.as_ref()
        .map(|text| crate::lower_hex(&Sha256::digest(text.as_bytes())))
}

pub(super) fn config_path(directory: &Path) -> Result<PathBuf, String> {
    if !directory.is_absolute() {
        return Err("KIMI_CODE_HOME must be absolute to install Namedrop hooks".to_owned());
    }
    Ok(directory.join("config.toml"))
}

fn verify_path(path: &Path) -> Result<(), String> {
    for parent in path.ancestors().take(2) {
        match fs::symlink_metadata(parent) {
            Ok(metadata)
                if metadata.is_symlink()
                    || (parent != path && !metadata.is_dir())
                    || (parent == path && !metadata.is_file()) =>
            {
                return Err(conflict(parent, "is not a regular file or directory"));
            }
            Err(error) if error.kind() != ErrorKind::NotFound => {
                return Err(conflict(parent, &error.to_string()));
            }
            _ => {}
        }
    }
    Ok(())
}

fn read_text(path: &Path) -> Result<Option<String>, String> {
    verify_path(path)?;
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(conflict(path, &error.to_string())),
    }
}

fn read_record(state: &Path) -> Result<OwnedConfiguration, String> {
    let path = state.join(RECORD_FILE);
    let Some(text) = read_text(&path)? else {
        return Ok(OwnedConfiguration::default());
    };
    let record: OwnedConfiguration = serde_json::from_str(&text)
        .map_err(|error| conflict(&path, &format!("has invalid ownership JSON: {error}")))?;
    if ((!record.rules.is_empty() || record.hook.is_some()) && !record.config_path.is_absolute())
        || record.pending.as_ref().is_some_and(|pending| {
            !pending.next.config_path.is_absolute()
                || pending.next.pending.is_some()
                || pending
                    .updates
                    .iter()
                    .any(|update| !update.path.is_absolute())
        })
    {
        return Err(conflict(
            &path,
            "ownership config paths must be absolute and pending updates cannot nest",
        ));
    }
    Ok(record)
}

fn write_record(state: &Path, record: &OwnedConfiguration) -> Result<(), String> {
    replace_if_changed(
        &state.join(RECORD_FILE),
        &format!(
            "{}\n",
            serde_json::to_string_pretty(record).map_err(|error| error.to_string())?
        ),
        Some(0o600),
    )
}

struct Settings {
    path: PathBuf,
    original: Option<String>,
    document: DocumentMut,
    mode: u32,
    dirty: bool,
    added_rules: Vec<String>,
    removed_rules: Vec<String>,
    added_hook: Option<HookEntry>,
    removed_hook: Option<HookEntry>,
}

impl Settings {
    fn read(path: &Path) -> Result<Self, String> {
        let original = read_text(path)?;
        let document = original
            .as_deref()
            .unwrap_or("")
            .parse::<DocumentMut>()
            .map_err(|error| conflict(path, &format!("is not valid TOML: {error}")))?;
        let mode = fs::metadata(path)
            .map(|metadata| metadata.permissions().mode() & 0o7777)
            .unwrap_or(0o600);
        Ok(Self {
            path: path.to_owned(),
            original,
            document,
            mode,
            dirty: false,
            added_rules: Vec::new(),
            removed_rules: Vec::new(),
            added_hook: None,
            removed_hook: None,
        })
    }

    fn rule_tables(&self) -> Result<Option<&ArrayOfTables>, String> {
        let Some(permission) = self.document.get("permission") else {
            return Ok(None);
        };
        let permission = permission
            .as_table()
            .ok_or_else(|| conflict(&self.path, "permission must be a table"))?;
        match permission.get("rules") {
            None => Ok(None),
            Some(rules) => rules
                .as_array_of_tables()
                .map(Some)
                .ok_or_else(|| conflict(&self.path, "permission.rules must be an array of tables")),
        }
    }

    fn rule_tables_mut(&mut self) -> Result<&mut ArrayOfTables, String> {
        if self.document.get("permission").is_none() {
            self.document["permission"] = Item::Table(Table::new());
        }
        let permission = self
            .document
            .get_mut("permission")
            .and_then(Item::as_table_mut)
            .ok_or_else(|| conflict(&self.path, "permission must be a table"))?;
        if permission.get("rules").is_none() {
            permission["rules"] = Item::ArrayOfTables(ArrayOfTables::new());
        }
        permission
            .get_mut("rules")
            .and_then(Item::as_array_of_tables_mut)
            .ok_or_else(|| conflict(&self.path, "permission.rules must be an array of tables"))
    }

    fn contains_rule(&self, rule: &str) -> Result<bool, String> {
        Ok(self.rule_tables()?.into_iter().flatten().any(|table| {
            table.get("decision").and_then(Item::as_str) == Some("allow")
                && table.get("pattern").and_then(Item::as_str) == Some(rule)
        }))
    }

    fn add_rule(&mut self, rule: &str) -> Result<(), String> {
        if self.contains_rule(rule)? {
            return Ok(());
        }
        if self.rule_tables()?.into_iter().flatten().any(|table| {
            table.get("pattern").and_then(Item::as_str) == Some(rule)
                && table.get("decision").and_then(Item::as_str) != Some("allow")
        }) {
            return Err(conflict(
                &self.path,
                "has a conflicting Kimi permission rule for a Namedrop command",
            ));
        }
        let mut table = Table::new();
        table["decision"] = Item::from("allow");
        table["pattern"] = Item::from(rule);
        table["reason"] = Item::from("Namedrop local collaboration");
        self.rule_tables_mut()?.push(table);
        self.dirty = true;
        self.added_rules.push(rule.to_owned());
        Ok(())
    }

    fn remove_rule(&mut self, rule: &str) -> Result<bool, String> {
        if self.rule_tables()?.is_none() {
            return Ok(false);
        }
        let tables = self.rule_tables_mut()?;
        let Some(index) = tables.iter().position(|table| {
            table.get("decision").and_then(Item::as_str) == Some("allow")
                && table.get("pattern").and_then(Item::as_str) == Some(rule)
        }) else {
            return Ok(false);
        };
        tables.remove(index);
        self.dirty = true;
        self.removed_rules.push(rule.to_owned());
        Ok(true)
    }

    fn hook_tables(&self) -> Result<Option<&ArrayOfTables>, String> {
        match self.document.get("hooks") {
            None => Ok(None),
            Some(hooks) => hooks
                .as_array_of_tables()
                .map(Some)
                .ok_or_else(|| conflict(&self.path, "hooks must be an array of tables")),
        }
    }

    fn hook_tables_mut(&mut self) -> Result<&mut ArrayOfTables, String> {
        if self.document.get("hooks").is_none() {
            self.document["hooks"] = Item::ArrayOfTables(ArrayOfTables::new());
        }
        self.document
            .get_mut("hooks")
            .and_then(Item::as_array_of_tables_mut)
            .ok_or_else(|| conflict(&self.path, "hooks must be an array of tables"))
    }

    fn contains_hook(&self, hook: &HookEntry) -> Result<bool, String> {
        Ok(self
            .hook_tables()?
            .into_iter()
            .flatten()
            .any(|table| hook_matches(table, hook)))
    }

    fn add_hook(&mut self, hook: &HookEntry) -> Result<(), String> {
        if self.contains_hook(hook)? {
            return Ok(());
        }
        let mut table = Table::new();
        table["event"] = Item::from(hook.event.as_str());
        table["command"] = Item::from(hook.command.as_str());
        table["timeout"] = Item::from(hook.timeout as i64);
        self.hook_tables_mut()?.push(table);
        self.dirty = true;
        self.added_hook = Some(hook.clone());
        Ok(())
    }

    fn remove_hook(&mut self, hook: &HookEntry) -> Result<bool, String> {
        if self.hook_tables()?.is_none() {
            return Ok(false);
        }
        let tables = self.hook_tables_mut()?;
        let Some(index) = tables.iter().position(|table| hook_matches(table, hook)) else {
            return Ok(false);
        };
        tables.remove(index);
        self.dirty = true;
        self.removed_hook = Some(hook.clone());
        Ok(true)
    }

    fn update(&self) -> Result<ConfigUpdate, String> {
        let after = if self.dirty {
            Some(self.document.to_string())
        } else {
            self.original.clone()
        };
        Ok(ConfigUpdate {
            path: self.path.clone(),
            before: self.original.clone(),
            after,
            added_rules: self.added_rules.clone(),
            removed_rules: self.removed_rules.clone(),
            added_hook: self.added_hook.clone(),
            removed_hook: self.removed_hook.clone(),
            mode: self.mode,
        })
    }
}

fn hook_matches(table: &Table, hook: &HookEntry) -> bool {
    table.get("event").and_then(Item::as_str) == Some(hook.event.as_str())
        && table.get("command").and_then(Item::as_str) == Some(hook.command.as_str())
        && table.get("timeout").and_then(Item::as_integer) == Some(hook.timeout as i64)
        && table.get("matcher").is_none()
}

impl ConfigUpdate {
    fn journal(&self) -> JournalUpdate {
        JournalUpdate {
            path: self.path.clone(),
            before_hash: snapshot_hash(&self.before),
            after_hash: snapshot_hash(&self.after),
            added_rules: self.added_rules.clone(),
            removed_rules: self.removed_rules.clone(),
            added_hook: self.added_hook.clone(),
            removed_hook: self.removed_hook.clone(),
            mode: self.mode,
        }
    }

    fn publish(&self) -> Result<bool, ReplacementError> {
        let actual = read_text(&self.path)?;
        if actual != self.before {
            return Err(conflict(
                &self.path,
                "changed during the pending Kimi configuration update",
            )
            .into());
        }
        if self.before == self.after {
            return Ok(false);
        }
        self.write(&self.after)?;
        Ok(true)
    }

    fn write(&self, text: &Option<String>) -> Result<(), ReplacementError> {
        match text {
            Some(text) => {
                fs::create_dir_all(self.path.parent().expect("config parent"))
                    .map_err(|error| conflict(&self.path, &error.to_string()))?;
                replace_with_status(&self.path, text, Some(self.mode))
            }
            None => match fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
                Err(error) => Err(conflict(&self.path, &error.to_string()).into()),
            },
        }
    }

    fn rollback(&self) -> Result<(), String> {
        let actual = read_text(&self.path)?;
        if actual == self.before {
            return Ok(());
        }
        if actual != self.after {
            return Err(conflict(
                &self.path,
                "changed before rollback; the pending record was retained",
            ));
        }
        self.write(&self.before).map_err(|error| error.message)
    }
}

fn lock(state: &Path) -> Result<fs::File, String> {
    let path = state.join("kimi-config.lock");
    verify_path(&path)?;
    fs::create_dir_all(state).map_err(|error| conflict(state, &error.to_string()))?;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .map_err(|error| conflict(&path, &error.to_string()))?;
    file.lock()
        .map_err(|error| conflict(&path, &error.to_string()))?;
    Ok(file)
}

fn recover(state: &Path) -> Result<OwnedConfiguration, String> {
    let record = read_record(state)?;
    let Some(pending) = &record.pending else {
        return Ok(record);
    };
    let mut changed = false;
    for update in &pending.updates {
        let hash = snapshot_hash(&read_text(&update.path)?);
        changed |= hash != update.before_hash && hash != update.after_hash;
    }
    if changed {
        return discard_pending(state, record);
    }
    for update in &pending.updates {
        if snapshot_hash(&read_text(&update.path)?) == update.after_hash {
            continue;
        }
        let mut settings = Settings::read(&update.path)?;
        for rule in &update.removed_rules {
            settings.remove_rule(rule)?;
        }
        for rule in &update.added_rules {
            settings.add_rule(rule)?;
        }
        if let Some(hook) = &update.removed_hook {
            settings.remove_hook(hook)?;
        }
        if let Some(hook) = &update.added_hook {
            settings.add_hook(hook)?;
        }
        let mut replacement = settings.update()?;
        replacement.mode = update.mode;
        if snapshot_hash(&replacement.after) != update.after_hash {
            return discard_pending(state, record);
        }
        replacement.publish().map_err(|error| error.message)?;
    }
    write_record(state, &pending.next)?;
    Ok(*pending.next.clone())
}

fn discard_pending(
    state: &Path,
    mut record: OwnedConfiguration,
) -> Result<OwnedConfiguration, String> {
    record.pending = None;
    if !record.rules.is_empty() || record.hook.is_some() {
        let settings = Settings::read(&record.config_path)?;
        record
            .rules
            .retain(|rule| settings.contains_rule(rule).unwrap_or(false));
        if record
            .hook
            .as_ref()
            .is_some_and(|hook| !settings.contains_hook(hook).unwrap_or(false))
        {
            record.hook = None;
        }
    }
    write_record(state, &record)?;
    eprintln!(
        "Namedrop discarded a pending Kimi configuration update that cannot be replayed; leftover rules or hooks may need review."
    );
    Ok(record)
}

fn publish(
    state: &Path,
    next: OwnedConfiguration,
    updates: Vec<ConfigUpdate>,
) -> Result<(), String> {
    let old = read_record(state)?;
    let changed = updates.iter().any(|update| update.before != update.after);
    if !changed
        && old.config_path == next.config_path
        && old.rules == next.rules
        && old.hook == next.hook
        && old.pending.is_none()
    {
        return Ok(());
    }
    let mut pending = old.clone();
    pending.pending = Some(PendingUpdate {
        next: Box::new(next.clone()),
        updates: updates.iter().map(ConfigUpdate::journal).collect(),
    });
    write_record(state, &pending)?;
    let mut applied: Vec<&ConfigUpdate> = Vec::new();
    for update in &updates {
        match update.publish() {
            Ok(true) => applied.push(update),
            Ok(false) => {}
            Err(error) => {
                if error.applied {
                    return Err(error.message);
                }
                for previous in applied.into_iter().rev() {
                    if let Err(rollback) = previous.rollback() {
                        return Err(format!("{error}; rollback also failed: {rollback}"));
                    }
                }
                write_record(state, &old)?;
                return Err(error.message);
            }
        }
    }
    write_record(state, &next)
}

fn kimi_allow_rules(executable: &Path, state: &Path) -> Vec<String> {
    collaboration_permission_patterns(executable, state)
        .into_iter()
        .map(|pattern| {
            format!(
                "Bash({} --plugin-state {} {} *{{,/**/*}})",
                shell_single_quote(&kimi_glob_literal(&pattern[0])),
                shell_single_quote(&kimi_glob_literal(&pattern[2])),
                pattern[3..].join(" ")
            )
        })
        .collect()
}

fn kimi_glob_literal(value: &str) -> String {
    let mut escaped = String::new();
    for character in value.chars() {
        if matches!(
            character,
            '\\' | '*' | '?' | '[' | ']' | '{' | '}' | '(' | ')' | '!' | '+' | '@' | '|'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn desired_hook(package: &Path) -> Result<HookEntry, String> {
    let script = package.join(super::HOOK_SCRIPT_FILE);
    let script = script.to_str().ok_or_else(|| {
        format!(
            "the Kimi Code hook path is not valid UTF-8: {}",
            script.display()
        )
    })?;
    Ok(HookEntry {
        event: "UserPromptSubmit".to_owned(),
        command: shell_single_quote(script),
        timeout: 10,
    })
}

pub(super) fn refresh(
    kimi_home: &Path,
    package: &Path,
    executable: &Path,
    state: &Path,
) -> Result<(), String> {
    let desired_rules = kimi_allow_rules(executable, state);
    let desired_hook = desired_hook(package)?;
    let path = config_path(kimi_home)?;
    let _guard = lock(state)?;
    let owned = recover(state)?;
    let mut settings = Settings::read(&path)?;
    let mut updates = Vec::new();
    let mut next = OwnedConfiguration {
        config_path: path.clone(),
        rules: Vec::new(),
        hook: None,
        pending: None,
    };
    if owned.config_path != path && (!owned.rules.is_empty() || owned.hook.is_some()) {
        let mut previous = Settings::read(&owned.config_path)?;
        for rule in &owned.rules {
            previous.remove_rule(rule)?;
        }
        if let Some(hook) = &owned.hook {
            previous.remove_hook(hook)?;
        }
        updates.push(previous.update()?);
    } else {
        for rule in &owned.rules {
            if desired_rules.contains(rule) && settings.contains_rule(rule)? {
                next.rules.push(rule.clone());
            } else {
                settings.remove_rule(rule)?;
            }
        }
        if owned.hook.as_ref() == Some(&desired_hook) && settings.contains_hook(&desired_hook)? {
            next.hook = Some(desired_hook.clone());
        } else if let Some(hook) = &owned.hook {
            settings.remove_hook(hook)?;
        }
    }
    for rule in desired_rules {
        if !settings.contains_rule(&rule)? {
            settings.add_rule(&rule)?;
            next.rules.push(rule);
        }
    }
    if !settings.contains_hook(&desired_hook)? {
        settings.add_hook(&desired_hook)?;
        next.hook = Some(desired_hook);
    }
    updates.push(settings.update()?);
    publish(state, next, updates)
}

pub(crate) fn remove_kimi_configuration(state: &Path) -> Result<usize, String> {
    let (_package_guard, current) = crate::installed_runtime::package_guard()?;
    if !current {
        return Ok(0);
    }
    let _guard = lock(state)?;
    if crate::StateStore::open(state)
        .map_err(|error| error.to_string())?
        .transaction(|tx| tx.any_activated_worktrees())
        .map_err(|error| error.to_string())?
    {
        return Ok(0);
    }
    let mut owned = recover(state)?;
    if owned.rules.is_empty() && owned.hook.is_none() {
        return Ok(0);
    }
    let mut settings = Settings::read(&owned.config_path)?;
    let mut removed = 0;
    for rule in &owned.rules {
        removed += usize::from(settings.remove_rule(rule)?);
    }
    if let Some(hook) = &owned.hook {
        removed += usize::from(settings.remove_hook(hook)?);
    }
    owned.rules.clear();
    owned.hook = None;
    publish(state, owned, vec![settings.update()?])?;
    Ok(removed)
}

pub(super) fn inspect(
    kimi_home: &Path,
    package: &Path,
    executable: &Path,
    state: &Path,
) -> Result<(), String> {
    inspect_rules(kimi_home, package, executable, state)
        .map_err(|error| format!("{error}; {REPAIR_GUIDANCE}"))
}

fn inspect_rules(
    kimi_home: &Path,
    package: &Path,
    executable: &Path,
    state: &Path,
) -> Result<(), String> {
    let desired_rules = kimi_allow_rules(executable, state);
    let desired_hook = desired_hook(package)?;
    let path = config_path(kimi_home)?;
    let owned = read_record(state)?;
    if owned.pending.is_some() {
        return Err(conflict(
            &path,
            "has a pending Kimi configuration update; activate again to reconcile it",
        ));
    }
    let settings = Settings::read(&path)?;
    if ((!owned.rules.is_empty() || owned.hook.is_some()) && owned.config_path != path)
        || owned.rules.iter().any(|rule| !desired_rules.contains(rule))
        || owned
            .hook
            .as_ref()
            .is_some_and(|hook| *hook != desired_hook)
    {
        return Err(conflict(
            &path,
            "has stale managed Kimi collaboration configuration; activate again to refresh it",
        ));
    }
    if desired_rules
        .iter()
        .any(|rule| !settings.contains_rule(rule).unwrap_or(false))
    {
        return Err(conflict(
            &path,
            "is missing Kimi collaboration permissions; activate again to repair them",
        ));
    }
    if !settings.contains_hook(&desired_hook)? {
        return Err(conflict(
            &path,
            "is missing the Kimi UserPromptSubmit hook; activate again to repair it",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::kimi_glob_literal;

    #[test]
    fn glob_literals_escape_every_kimi_special_character() {
        assert_eq!(
            kimi_glob_literal(r"/tmp/a\b*c?d[e]f{g}h(i)j!k+l@m|n"),
            r"/tmp/a\\b\*c\?d\[e\]f\{g\}h\(i\)j\!k\+l\@m\|n"
        );
    }
}
