use crate::lifecycle::{DirectorySetup, load_directory_setup};
use crate::{
    Clock, HerdrCommandExecutor, IdGenerator, LifecycleRuntime, PaneAddressRecord, PaneType,
    StateNamespace, StateStore, StateTransaction, StoreError, StoreErrorKind, discover_worktree,
    discover_worktree_for_workspace,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

const MAX_RESOLVE_ALIASES: usize = 32;
const MAX_ALIAS_CHARS: usize = 128;
const DEFAULT_CANDIDATE_LIMIT: u32 = 32;
const MAX_CANDIDATE_LIMIT: u32 = 64;
const MAX_CURSOR_CHARS: usize = 256;
const CURSOR_VERSION: &str = "directory-v1";
pub const DIRECTORY_PERSONA_EXCERPT_CHARS: usize = 240;
pub const DIRECTORY_PERSONA_FULL_CHARS: usize = crate::MAX_PERSONA_CHARS;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolveInput {
    aliases: Option<Vec<String>>,
    #[serde(rename = "self")]
    self_request: Option<bool>,
    #[serde(default)]
    include_persona: bool,
}

enum ResolveRequest {
    Aliases(Vec<String>),
    SelfAddress,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidatesInput {
    #[serde(default = "default_candidate_limit")]
    limit: u32,
    cursor: Option<String>,
    pane_type: Option<PaneType>,
    #[serde(default)]
    availability: AvailabilityFilter,
    #[serde(default)]
    include_self: bool,
}

fn default_candidate_limit() -> u32 {
    DEFAULT_CANDIDATE_LIMIT
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum AvailabilityFilter {
    #[default]
    All,
    Selectable,
    Queueable,
    DispatchEligible,
}

#[derive(Serialize)]
struct CandidateQuery<'a> {
    pane_type: Option<PaneType>,
    availability: AvailabilityFilter,
    include_self: bool,
    self_address_id: Option<&'a str>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyInput {}

pub fn execute_directory_operation<E, C, I>(
    operation: &str,
    input: Value,
    runtime: &LifecycleRuntime<'_, E, C, I>,
) -> Result<Value, String>
where
    E: HerdrCommandExecutor,
    C: Clock,
    I: IdGenerator,
{
    match operation {
        "address.list" => {
            let _: EmptyInput = serde_json::from_value(input).map_err(crate::input_schema_error)?;
            with_directory_setup(runtime, |_, _, _, setup| {
                address_list_projection(&setup).map_err(StoreError::invalid_input)
            })
        }
        "address.resolve" => {
            let input: ResolveInput =
                serde_json::from_value(input).map_err(crate::input_schema_error)?;
            let include_persona = input.include_persona;
            let request = resolve_request(input)?;
            resolve(include_persona, request, runtime)
        }
        "address.candidates" => {
            let input: CandidatesInput =
                serde_json::from_value(input).map_err(crate::input_schema_error)?;
            validate_candidates_input(&input)?;
            candidates(input, runtime)
        }
        _ => Err(format!("{operation} is not a directory operation")),
    }
}

fn resolve<E, C, I>(
    include_persona: bool,
    request: ResolveRequest,
    runtime: &LifecycleRuntime<'_, E, C, I>,
) -> Result<Value, String>
where
    E: HerdrCommandExecutor,
    C: Clock,
    I: IdGenerator,
{
    with_directory_setup(runtime, |transaction, _, namespace, setup| {
        let fingerprint = directory_fingerprint(namespace, &setup)?;
        let availability_observation = availability_fingerprint(&setup)?;
        let self_address_id = runtime.current_pane_id.and_then(|pane_id| {
            setup
                .addresses
                .iter()
                .find(|address| address.pane_id.as_deref() == Some(pane_id))
                .map(|address| address.id.clone())
        });
        let results = match &request {
            ResolveRequest::Aliases(aliases) => aliases
                .iter()
                .map(|alias| {
                    resolve_alias(
                        transaction,
                        namespace,
                        &setup,
                        alias,
                        include_persona,
                        self_address_id
                            .as_ref()
                            .map(|address_id| address_id.as_str()),
                    )
                })
                .collect::<Result<Vec<_>, _>>()?,
            ResolveRequest::SelfAddress => {
                let pane_id = runtime.current_pane_id.ok_or_else(|| {
                    StoreError::invalid_input("self resolution requires the current Herdr pane")
                })?;
                vec![resolve_self(
                    transaction,
                    namespace,
                    &setup,
                    pane_id,
                    include_persona,
                )?]
            }
        };
        let (expansion, expansion_error) = match &request {
            ResolveRequest::Aliases(aliases) => match self_address_id.as_ref() {
                Some(author) => {
                    match transaction.resolve_message_recipients(namespace, aliases, author) {
                        Ok(recipients) => {
                            let address_ids = recipients
                                .iter()
                                .map(|recipient| recipient.address_id.clone())
                                .collect::<Vec<_>>();
                            (
                                json!(crate::group_membership_fingerprint(&address_ids)),
                                Value::Null,
                            )
                        }
                        Err(error) => (Value::Null, json!(error.to_string())),
                    }
                }
                None => (
                    Value::Null,
                    json!("recipient expansion requires a current pane address"),
                ),
            },
            ResolveRequest::SelfAddress => (Value::Null, Value::Null),
        };
        Ok(json!({
            "results": results,
            "workspace_id": namespace.space_id,
            "observed_at_ms": runtime.clock.now_unix_ms(),
            "directory_fingerprint": fingerprint,
            "availability_observation": availability_observation,
            "persona_provenance": "untrusted_user_data",
            "recipient_expansion_fingerprint": expansion,
            "recipient_expansion_error": expansion_error,
        }))
    })
}

fn resolve_request(input: ResolveInput) -> Result<ResolveRequest, String> {
    match (input.aliases, input.self_request) {
        (Some(aliases), None) => {
            if aliases.is_empty() || aliases.len() > MAX_RESOLVE_ALIASES {
                return Err(format!(
                    "aliases must contain 1 to {MAX_RESOLVE_ALIASES} exact aliases"
                ));
            }
            let aliases = aliases
                .into_iter()
                .map(|alias| {
                    if alias.is_empty() || alias.chars().count() > MAX_ALIAS_CHARS {
                        return Err(format!(
                            "each alias must contain 1 to {MAX_ALIAS_CHARS} characters"
                        ));
                    }
                    let alias = alias.strip_prefix('@').unwrap_or(&alias).to_owned();
                    if alias.is_empty() {
                        return Err("an alias must include a name after @".to_owned());
                    }
                    Ok(alias)
                })
                .collect::<Result<Vec<_>, String>>()?;
            Ok(ResolveRequest::Aliases(aliases))
        }
        (None, Some(true)) => Ok(ResolveRequest::SelfAddress),
        _ => Err("provide exactly one of a nonempty aliases list or self:true".to_owned()),
    }
}

fn validate_candidates_input(input: &CandidatesInput) -> Result<(), String> {
    if !(1..=MAX_CANDIDATE_LIMIT).contains(&input.limit) {
        return Err(format!("limit must be between 1 and {MAX_CANDIDATE_LIMIT}"));
    }
    if input
        .cursor
        .as_ref()
        .is_some_and(|cursor| cursor.is_empty() || cursor.chars().count() > MAX_CURSOR_CHARS)
    {
        return Err(format!(
            "cursor must contain 1 to {MAX_CURSOR_CHARS} characters"
        ));
    }
    Ok(())
}

fn with_directory_setup<E, C, I, T>(
    runtime: &LifecycleRuntime<'_, E, C, I>,
    read: impl FnOnce(
        &StateTransaction<'_>,
        &crate::DiscoveredWorktree,
        &StateNamespace,
        DirectorySetup,
    ) -> Result<T, StoreError>,
) -> Result<T, String>
where
    E: HerdrCommandExecutor,
    C: Clock,
    I: IdGenerator,
{
    let mut store = StateStore::open_read_only(runtime.state_directory)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Namedrop has no saved state".to_owned())?;
    let discovered = match runtime.workspace_id {
        Some(workspace_id) => discover_worktree_for_workspace(runtime.herdr, workspace_id)?,
        None => discover_worktree(runtime.herdr, runtime.cwd)?,
    };
    let namespace = discovered.namespace(runtime.session_socket_id);
    let addresses = store
        .transaction(|transaction| transaction.list_pane_addresses(&namespace))
        .map_err(|error| error.to_string())?;
    let shell_availability = crate::terminal::shell_availability(runtime.herdr, &addresses);
    store
        .transaction(|transaction| {
            let setup = load_directory_setup(
                transaction,
                &discovered,
                runtime.session_socket_id,
                &shell_availability,
            )?;
            if !setup
                .worktree_state
                .as_ref()
                .is_some_and(|state| state.activated && !state.deactivated)
            {
                return Err(StoreError::invalid_input(
                    "Namedrop is not activated for this space",
                ));
            }
            read(transaction, &discovered, &namespace, setup)
        })
        .map_err(|error| error.to_string())
}

fn resolve_alias(
    transaction: &StateTransaction<'_>,
    namespace: &StateNamespace,
    setup: &DirectorySetup,
    alias: &str,
    include_persona: bool,
    self_address_id: Option<&str>,
) -> Result<Value, StoreError> {
    if let Some(group) = setup.groups.iter().find(|group| group["alias"] == alias) {
        return resolve_group(
            transaction,
            namespace,
            setup,
            alias,
            group,
            include_persona,
            self_address_id,
        );
    }
    match transaction.resolve_routing_alias(namespace, alias) {
        Ok(resolved) => {
            let address = setup_address(setup, &resolved.address_id)?;
            resolved_address(setup, address, include_persona, Some(alias.to_owned()))
        }
        Err(error) if error.kind() == StoreErrorKind::InvalidInput => {
            if let Some((address_id, retired, replacement_alias)) =
                transaction.alias_resolution_metadata(namespace, alias)?
                && retired
            {
                let replacement = replacement_alias.map(|alias| json!({"alias": alias}));
                return Ok(json!({
                    "query": alias,
                    "status": "retired",
                    "type": "address",
                    "address_id": address_id,
                    "replacement": replacement,
                }));
            }
            Ok(json!({"query": alias, "status": "unknown"}))
        }
        Err(error) => Err(error),
    }
}

fn resolve_self(
    transaction: &StateTransaction<'_>,
    namespace: &StateNamespace,
    setup: &DirectorySetup,
    pane_id: &str,
    include_persona: bool,
) -> Result<Value, StoreError> {
    match transaction.routing_author_for_pane(namespace, pane_id) {
        Ok(resolved) => {
            let address = setup_address(setup, &resolved.address_id)?;
            let mut result = resolved_address(setup, address, include_persona, None)?;
            result["self"] = Value::Bool(true);
            Ok(result)
        }
        Err(error) if error.kind() == StoreErrorKind::InvalidInput => {
            Ok(json!({"query": "self", "self": true, "status": "unknown"}))
        }
        Err(error) => Err(error),
    }
}

fn resolve_group(
    transaction: &StateTransaction<'_>,
    namespace: &StateNamespace,
    setup: &DirectorySetup,
    alias: &str,
    group: &Value,
    include_persona: bool,
    self_address_id: Option<&str>,
) -> Result<Value, StoreError> {
    let mut seen = BTreeSet::new();
    let mut members = Vec::new();
    for member in group["members"].as_array().into_iter().flatten() {
        let Some(address_id) = member["address_id"].as_str() else {
            continue;
        };
        if !seen.insert(address_id.to_owned()) {
            continue;
        }
        let address_id = crate::PaneAddressId::new(address_id);
        match transaction.routing_address_by_id(namespace, &address_id) {
            Ok(_) => {
                let address = setup_address(setup, &address_id)?;
                members.push(resolved_address(setup, address, include_persona, None)?);
            }
            Err(error) if error.kind() == StoreErrorKind::InvalidInput => {
                members.push(json!({
                    "status": "unknown",
                    "type": "address",
                    "address_id": address_id,
                }));
            }
            Err(error) => return Err(error),
        }
    }
    // A later send from this pane excludes its author from the expanded recipients.
    let address_ids = seen
        .into_iter()
        .filter(|address_id| self_address_id != Some(address_id.as_str()))
        .map(crate::PaneAddressId::new)
        .collect::<Vec<_>>();
    Ok(json!({
        "query": alias,
        "status": "found",
        "type": "group",
        "alias": alias,
        "members": members,
        "group_fingerprint": crate::group_membership_fingerprint(&address_ids),
    }))
}

fn resolved_address(
    setup: &DirectorySetup,
    address: &PaneAddressRecord,
    include_persona: bool,
    query: Option<String>,
) -> Result<Value, StoreError> {
    let mut result = directory_address(
        setup,
        address,
        include_persona,
        DIRECTORY_PERSONA_FULL_CHARS,
    )?;
    result["status"] = Value::String("found".to_owned());
    result["type"] = Value::String("address".to_owned());
    result["query"] = query.map(Value::String).unwrap_or(Value::Null);
    Ok(result)
}

fn setup_address<'a>(
    setup: &'a DirectorySetup,
    address_id: &crate::PaneAddressId,
) -> Result<&'a PaneAddressRecord, StoreError> {
    setup
        .addresses
        .iter()
        .find(|address| &address.id == address_id)
        .ok_or_else(|| {
            StoreError::invalid_input("resolved address is absent from the directory snapshot")
        })
}

fn candidates<E, C, I>(
    input: CandidatesInput,
    runtime: &LifecycleRuntime<'_, E, C, I>,
) -> Result<Value, String>
where
    E: HerdrCommandExecutor,
    C: Clock,
    I: IdGenerator,
{
    with_directory_setup(runtime, |_, _, namespace, setup| {
        let fingerprint = directory_fingerprint(namespace, &setup)?;
        let availability_observation = availability_fingerprint(&setup)?;
        let self_address_id = runtime.current_pane_id.and_then(|pane_id| {
            setup
                .addresses
                .iter()
                .find(|address| address.pane_id.as_deref() == Some(pane_id))
                .map(|address| address.id.as_str())
        });
        let query = CandidateQuery {
            pane_type: input.pane_type,
            availability: input.availability,
            include_self: input.include_self,
            self_address_id,
        };
        let query_fingerprint = value_fingerprint(&query)?;
        let cursor_fingerprint = if input.availability == AvailabilityFilter::All {
            query_fingerprint.clone()
        } else {
            value_fingerprint(&json!({
                "query": query_fingerprint,
                "availability_observation": availability_observation,
            }))?
        };
        let offset = match input.cursor.as_deref() {
            Some(cursor) => decode_cursor(cursor, &fingerprint, &cursor_fingerprint)?,
            None => 0,
        };
        let mut addresses = setup.addresses.iter().collect::<Vec<_>>();
        addresses.sort_by(|left, right| {
            left.alias
                .cmp(&right.alias)
                .then_with(|| left.id.cmp(&right.id))
        });
        let candidates = addresses
            .into_iter()
            .filter(|address| input.include_self || Some(address.id.as_str()) != self_address_id)
            .filter(|address| {
                input
                    .pane_type
                    .is_none_or(|pane_type| address.pane_type == pane_type)
            })
            .filter(|address| {
                let readiness = &setup.readiness[&address.id];
                match input.availability {
                    AvailabilityFilter::All => true,
                    AvailabilityFilter::Selectable => readiness.automatic_selection_suitable,
                    AvailabilityFilter::Queueable => readiness.explicit_send_queueable,
                    AvailabilityFilter::DispatchEligible => readiness.dispatch_eligible,
                }
            })
            .collect::<Vec<_>>();
        if offset > candidates.len() {
            return Err(StoreError::invalid_input(
                "cursor offset is beyond the filtered directory",
            ));
        }
        let limit = input.limit as usize;
        let end = offset.saturating_add(limit).min(candidates.len());
        let page = candidates[offset..end]
            .iter()
            .map(|address| {
                directory_address(&setup, address, true, DIRECTORY_PERSONA_EXCERPT_CHARS)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = end < candidates.len();
        let next_cursor = has_more.then(|| encode_cursor(&fingerprint, &cursor_fingerprint, end));
        Ok(json!({
            "candidates": page,
            "workspace_id": namespace.space_id,
            "observed_at_ms": runtime.clock.now_unix_ms(),
            "directory_fingerprint": fingerprint,
            "availability_observation": availability_observation,
            "persona_provenance": "untrusted_user_data",
            "offset": offset,
            "limit": input.limit,
            "total": candidates.len(),
            "has_more": has_more,
            "next_cursor": next_cursor,
        }))
    })
}

pub(crate) fn address_list_projection(setup: &DirectorySetup) -> Result<Value, String> {
    let records = setup
        .addresses
        .iter()
        .map(|address| base_address(setup, address))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    serde_json::to_value(records).map_err(|error| error.to_string())
}

fn base_address(setup: &DirectorySetup, address: &PaneAddressRecord) -> Result<Value, StoreError> {
    let readiness = setup.readiness.get(&address.id).ok_or_else(|| {
        StoreError::invalid_input("address readiness is absent from the directory snapshot")
    })?;
    let registered = setup.registered_address_ids.contains(&address.id);
    let mut record = serde_json::to_value(address)
        .map_err(|error| StoreError::invalid_input(error.to_string()))?;
    record["registered"] = Value::Bool(registered);
    record["selection"] = Value::String(crate::persona::selection_note(address, readiness));
    Ok(record)
}

fn directory_address(
    setup: &DirectorySetup,
    address: &PaneAddressRecord,
    include_persona: bool,
    max_persona_chars: usize,
) -> Result<Value, StoreError> {
    let readiness = setup.readiness.get(&address.id).ok_or_else(|| {
        StoreError::invalid_input("address readiness is absent from the directory snapshot")
    })?;
    let mut record = base_address(setup, address)?;
    record["pane_binding_id"] = setup
        .bindings
        .iter()
        .find(|binding| binding.address_id == address.id)
        .map(|binding| json!(binding.id))
        .unwrap_or(Value::Null);
    record["readiness"] = serde_json::to_value(readiness)
        .map_err(|error| StoreError::invalid_input(error.to_string()))?;
    let live = address
        .pane_id
        .as_ref()
        .and_then(|pane_id| setup.panes.iter().find(|pane| &pane.pane_id == pane_id));
    record["live_agent_session_id"] = live
        .and_then(|pane| pane.agent_session_id.as_ref())
        .map(|session| json!(session))
        .unwrap_or(Value::Null);
    record["live_agent_status"] = live
        .and_then(|pane| pane.agent_status.as_ref())
        .map(|status| json!(status))
        .unwrap_or(Value::Null);
    record["collaboration_features"] = json!(
        setup
            .capabilities
            .get(&address.id)
            .cloned()
            .unwrap_or_default()
    );
    if let Some(object) = record.as_object_mut() {
        for field in [
            "role_id",
            "role_name",
            "role_description",
            "pane_label",
            "occupant_label",
        ] {
            object.remove(field);
        }
    }
    if include_persona {
        let (persona, truncated) =
            crate::persona::excerpt(address.persona.as_deref(), max_persona_chars);
        record["persona"] = persona.map(Value::String).unwrap_or(Value::Null);
        record["persona_truncated"] = Value::Bool(truncated);
    } else if let Some(object) = record.as_object_mut() {
        object.remove("persona");
    }
    Ok(record)
}

fn directory_fingerprint(
    namespace: &StateNamespace,
    setup: &DirectorySetup,
) -> Result<String, StoreError> {
    value_fingerprint(&json!({
        "namespace": namespace,
        "worktree_state": setup.worktree_state,
        "addresses": setup.addresses.iter().map(|address| json!({
            "id": address.id,
            "alias": address.alias,
            "pane_type": address.pane_type,
            "persona": address.persona,
            "lifecycle": address.lifecycle,
            "pane_id": address.pane_id,
            "retired": address.retired,
            "collaboration_disabled": address.collaboration_disabled,
        })).collect::<Vec<_>>(),
        "groups": setup.groups,
        "bindings": setup.bindings,
    }))
}

fn availability_fingerprint(setup: &DirectorySetup) -> Result<String, StoreError> {
    value_fingerprint(&json!({
        "registered_address_ids": setup.registered_address_ids,
        "readiness": setup.readiness,
        "capabilities": setup.capabilities,
        "panes": setup.panes.iter().map(|pane| json!({
            "pane_id": pane.pane_id,
            "agent": pane.agent,
            "agent_session_id": pane.agent_session_id,
            "agent_status": pane.agent_status,
        })).collect::<Vec<_>>(),
    }))
}

fn value_fingerprint(value: &impl Serialize) -> Result<String, StoreError> {
    let bytes =
        serde_json::to_vec(value).map_err(|error| StoreError::invalid_input(error.to_string()))?;
    Ok(crate::lower_hex(&Sha256::digest(bytes)))
}

fn encode_cursor(fingerprint: &str, query_fingerprint: &str, offset: usize) -> String {
    format!("{CURSOR_VERSION}:{fingerprint}:{query_fingerprint}:{offset}")
}

fn decode_cursor(
    cursor: &str,
    fingerprint: &str,
    query_fingerprint: &str,
) -> Result<usize, StoreError> {
    let parts = cursor.split(':').collect::<Vec<_>>();
    let valid = parts.len() == 4
        && parts[0] == CURSOR_VERSION
        && parts[1] == fingerprint
        && parts[2] == query_fingerprint;
    if !valid {
        return Err(StoreError::invalid_input(
            "cursor is invalid, stale, or belongs to different directory filters",
        ));
    }
    parts[3]
        .parse::<usize>()
        .map_err(|_| StoreError::invalid_input("cursor offset is invalid"))
}
