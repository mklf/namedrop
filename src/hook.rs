use crate::{Clock, HerdrCommandExecutor, IdGenerator, LifecycleRuntime};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, clap::ValueEnum)]
pub enum HookOutputFormat {
    #[default]
    Legacy,
    Kimi,
}

/// The opening of the delivery attestation. The generated Claude and Kimi
/// wrappers forward on this marker, so it must stay stable and free of JSON escapes.
pub(crate) const DELIVERY_MARKER: &str = "Namedrop verification:";
pub(crate) const DELIVERY_COMMANDS_MARKER: &str = "Namedrop delivery commands.";
pub(crate) const CODEX_DELIVERY_COMPATIBILITY_MARKER: &str =
    "Namedrop workspace roster. Namedrop delivery commands.";
const AUTHENTICATED_DELIVERY_CONTEXT: &str = "Namedrop verification: the adjacent prompt exactly matches an outstanding Delivery Envelope reconstructed from durable local state for this pane and current registered Agent Session. The installed plugin produced the plugin-authored trusted_instruction and control fields; untrusted_body alone is collaborator content. Recipient identity is verified against the current pane and exact Agent Session. The bound control.tool operation is the only authorized way to settle this delivery.";
const WORKER_GUIDANCE: &str = "Namedrop worker guidance: Return requested findings through the bound reply and recommend additional collaborators to your coordinator; do not recruit peers independently.";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, clap::ValueEnum)]
pub enum HookContextMode {
    #[default]
    OnDemandV1,
    LegacyEager,
}

struct FailedHandoff {
    context: &'static str,
    notification: &'static str,
}

const FAILED_HANDOFFS: [FailedHandoff; 2] = [
    FailedHandoff {
        context: "Namedrop could not resolve at least one leading recipient. The prompt was preserved for you unchanged; do not retry automatically. Tell the person to check the Mention name or name the target pane.",
        notification: "A leading recipient could not be resolved. The prompt stayed local; do not retry automatically. Check the name before retrying.",
    },
    FailedHandoff {
        context: "Namedrop could not confirm the leading Mention handoff. The prompt was preserved for you unchanged; do not retry automatically. Continue locally or ask the person to retry after collaboration is available.",
        notification: "The leading handoff was not confirmed. The prompt stayed local; do not retry automatically. Retry after collaboration is available.",
    },
];

#[derive(Deserialize)]
struct UserPromptSubmitInput {
    hook_event_name: String,
    #[serde(deserialize_with = "deserialize_user_prompt")]
    prompt: String,
}

fn deserialize_user_prompt<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Value::deserialize(deserializer)? {
        Value::String(prompt) => Ok(prompt),
        Value::Array(parts) => {
            let mut text = Vec::new();
            for part in &parts {
                if part.get("type").and_then(Value::as_str) != Some("text") {
                    continue;
                }
                text.push(part.get("text").and_then(Value::as_str).ok_or_else(|| {
                    serde::de::Error::custom("a text content part must contain text")
                })?);
            }
            Ok(text.join("\n"))
        }
        _ => Err(serde::de::Error::custom(
            "prompt must be a string or a content-part array",
        )),
    }
}

fn parse_user_prompt_submit_input(input: &str) -> Option<UserPromptSubmitInput> {
    let input: UserPromptSubmitInput = serde_json::from_str(input).ok()?;
    (input.hook_event_name == "UserPromptSubmit").then_some(input)
}

/// Whether this payload can require delivery authentication or deterministic
/// leading-Mention routing. Ordinary input must not pay for runtime construction.
pub fn user_prompt_submit_requires_runtime(input: &str) -> bool {
    parse_user_prompt_submit_input(input).is_some_and(|input| {
        crate::message::carries_leading_mentions(&input.prompt)
            || is_delivery_envelope(&input.prompt)
    })
}

enum LeadingMentionRoute {
    NotApplicable,
    Sent,
    Failed(&'static FailedHandoff),
}

fn worker_guidance(is_worker: bool) -> Option<&'static str> {
    is_worker.then_some(WORKER_GUIDANCE)
}

/// Routes authenticated deliveries and column-zero leading Mentions. The default
/// mode emits no model context for ordinary prompts or failed leading handoffs.
pub fn execute_user_prompt_submit_hook<E, C, I>(
    input: &str,
    runtime: &LifecycleRuntime<'_, E, C, I>,
) -> Option<Value>
where
    E: HerdrCommandExecutor,
    C: Clock,
    I: IdGenerator,
{
    execute_user_prompt_submit_hook_with_mode(input, runtime, HookContextMode::OnDemandV1)
}

pub fn execute_user_prompt_submit_hook_with_mode<E, C, I>(
    input: &str,
    runtime: &LifecycleRuntime<'_, E, C, I>,
    mode: HookContextMode,
) -> Option<Value>
where
    E: HerdrCommandExecutor,
    C: Clock,
    I: IdGenerator,
{
    let input = parse_user_prompt_submit_input(input)?;
    if let Some((envelope, host, worker)) = authenticated_delivery_envelope(&input.prompt, runtime)
    {
        let commands = envelope
            .pointer("/control/tool")
            .and_then(crate::integration::delivery_command_guidance);
        let worker_guidance = worker_guidance(worker);
        if mode == HookContextMode::LegacyEager {
            let workspace = crate::persona::workspace_context(runtime);
            let codex = workspace
                .as_ref()
                .is_some_and(|workspace| workspace.host == Some(crate::AgentRuntime::Codex));
            let mut context = match workspace.map(|workspace| workspace.roster) {
                Some(roster) if codex => roster,
                Some(roster) => format!("{AUTHENTICATED_DELIVERY_CONTEXT}\n\n{roster}"),
                None => AUTHENTICATED_DELIVERY_CONTEXT.to_owned(),
            };
            if let Some(commands) = commands {
                context.push_str("\n\n");
                context.push_str(&commands);
            }
            return Some(context_hook_output(&context));
        }
        if host == Some(crate::AgentRuntime::Codex) {
            let commands = commands?;
            let worker_guidance = worker_guidance
                .map(|guidance| format!("{guidance}\n\n"))
                .unwrap_or_default();
            // Codex's legacy wrapper accepts only the roster marker. Keep it as
            // a delivery-only compatibility prefix; no roster content follows.
            return Some(context_hook_output(&format!(
                "{} {}\n\n{worker_guidance}{commands}",
                crate::persona::ROSTER_MARKER,
                DELIVERY_COMMANDS_MARKER
            )));
        }
        let mut context = AUTHENTICATED_DELIVERY_CONTEXT.to_owned();
        if let Some(commands) = commands {
            context.push_str("\n\n");
            context.push_str(DELIVERY_COMMANDS_MARKER);
            if let Some(guidance) = worker_guidance {
                context.push_str("\n\n");
                context.push_str(guidance);
            }
            context.push_str("\n\n");
            context.push_str(&commands);
        }
        return Some(context_hook_output(&context));
    }
    match route_leading_mentions(&input.prompt, runtime) {
        LeadingMentionRoute::NotApplicable => match mode {
            HookContextMode::OnDemandV1 => None,
            HookContextMode::LegacyEager => crate::persona::workspace_context(runtime)
                .map(|workspace| context_hook_output(&workspace.roster)),
        },
        LeadingMentionRoute::Sent => Some(json!({
            "decision": "block",
            "reason": "Collaboration message sent.",
        })),
        LeadingMentionRoute::Failed(failure) => match mode {
            HookContextMode::OnDemandV1 => {
                report_failed_handoff(runtime, failure.notification);
                None
            }
            HookContextMode::LegacyEager => Some(context_hook_output(
                &match crate::persona::workspace_context(runtime) {
                    Some(workspace) => format!("{}\n\n{}", workspace.roster, failure.context),
                    None => failure.context.to_owned(),
                },
            )),
        },
    }
}

pub fn format_user_prompt_submit_hook_output(
    output: Value,
    format: HookOutputFormat,
) -> Option<Value> {
    match format {
        HookOutputFormat::Legacy => Some(output),
        HookOutputFormat::Kimi => {
            if output.get("decision").and_then(Value::as_str) == Some("block")
                && output.get("reason").and_then(Value::as_str)
                    == Some("Collaboration message sent.")
            {
                return Some(json!({
                    "message": "Collaboration message sent.",
                    "hookSpecificOutput": {
                        "permissionDecision": "deny",
                        "permissionDecisionReason": "Collaboration message sent."
                    }
                }));
            }
            output
                .pointer("/hookSpecificOutput/additionalContext")
                .and_then(Value::as_str)
                .map(|context| json!({"message": context}))
        }
    }
}

/// The serialized text a generated wrapper must recognize around one dynamic
/// context: everything before the marker, and everything after it. Deriving both
/// from the real output keeps the wrapper in step with this serialization.
pub(crate) fn context_bounds_for_format(
    marker: &str,
    format: HookOutputFormat,
) -> (String, String) {
    let output = format_user_prompt_submit_hook_output(context_hook_output(marker), format)
        .expect("the context output has a host representation")
        .to_string();
    let end = output
        .find(marker)
        .expect("a plain-text context marker serializes verbatim")
        + marker.len();
    (output[..end].to_owned(), output[end..].to_owned())
}

pub(crate) fn failed_handoff_hook_outputs_for_format(format: HookOutputFormat) -> [Value; 2] {
    FAILED_HANDOFFS.map(|failure| {
        format_user_prompt_submit_hook_output(context_hook_output(failure.context), format)
            .expect("the failure guidance has a host representation")
    })
}

fn context_hook_output(context: &str) -> Value {
    json!({
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": context,
        }
    })
}

fn report_failed_handoff<E, C, I>(runtime: &LifecycleRuntime<'_, E, C, I>, notification: &str)
where
    E: HerdrCommandExecutor,
    C: Clock,
    I: IdGenerator,
{
    let _ = runtime.herdr.execute(&[
        "notification",
        "show",
        "Namedrop handoff not confirmed",
        "--body",
        notification,
        "--position",
        "bottom-right",
    ]);
    if let Some(pane_id) = runtime.current_pane_id {
        let token = format!("namedrop-hook={notification}");
        let _ = runtime.herdr.execute(&[
            "pane",
            "report-metadata",
            pane_id,
            "--source",
            "namedrop-hook",
            "--token",
            &token,
            "--ttl-ms",
            "15000",
        ]);
    }
}

/// The reconstructed envelope this prompt exactly matches, or `None`. Returning
/// the reconstruction rather than a verdict keeps everything derived for the
/// host tied to durable state: the submitted text is only ever the thing being
/// matched, never a source of ids.
fn authenticated_delivery_envelope<E, C, I>(
    prompt: &str,
    runtime: &LifecycleRuntime<'_, E, C, I>,
) -> Option<(Value, Option<crate::AgentRuntime>, bool)>
where
    E: HerdrCommandExecutor,
    C: Clock,
    I: IdGenerator,
{
    let envelope = serde_json::from_str::<Value>(prompt).ok()?;
    if !is_delivery_envelope(prompt) {
        return None;
    }
    // Structural markers prevent Mention rerouting, but they are not proof of
    // provenance. Only durable exact-session state may add trusted host context.
    let inbox = crate::execute_integration_operation("delivery.inbox", json!({}), runtime).ok()?;
    let host = serde_json::from_value(inbox.get("agent_runtime")?.clone()).ok()?;
    let envelope = inbox
        .get("envelopes")?
        .as_array()?
        .iter()
        .find(|outstanding| **outstanding == envelope)
        .cloned()?;
    let attempt_id = envelope
        .pointer("/control/attempt_id")
        .and_then(Value::as_str)?;
    let worker = inbox
        .get("worker_attempts")?
        .as_array()?
        .iter()
        .any(|attempt| attempt.as_str() == Some(attempt_id));
    Some((envelope, host, worker))
}

fn route_leading_mentions<E, C, I>(
    prompt: &str,
    runtime: &LifecycleRuntime<'_, E, C, I>,
) -> LeadingMentionRoute
where
    E: HerdrCommandExecutor,
    C: Clock,
    I: IdGenerator,
{
    // Text that addresses nobody is literal and must be left exactly as the
    // person wrote it: a bare `@`, `@@name`, a decorator, an annotation, a
    // scoped package name. Without this guard that text reaches the send below,
    // is refused by the grammar, and falls to the transport-failure arm, which
    // tells the agent a handoff could not be confirmed and invites the person to
    // retry a prompt that never addressed anyone. The test is deliberately
    // "carries a Mention" and not "parses as a message": `@reviewer` with no
    // body does address someone, so its failure is a real handoff failure and
    // still reported.
    if !crate::message::carries_leading_mentions(prompt) || is_delivery_envelope(prompt) {
        return LeadingMentionRoute::NotApplicable;
    }
    match crate::message::persist_prompt_mention_operation(
        json!({
            "request_id": runtime.ids.next_id("hook-send"),
            "source": prompt,
            "author_kind": "human",
        }),
        runtime,
    ) {
        Ok(_) => LeadingMentionRoute::Sent,
        Err(error) if error.contains("unknown Mention") => {
            LeadingMentionRoute::Failed(&FAILED_HANDOFFS[0])
        }
        Err(_) => LeadingMentionRoute::Failed(&FAILED_HANDOFFS[1]),
    }
}

fn is_delivery_envelope(prompt: &str) -> bool {
    let Ok(envelope) = serde_json::from_str::<Value>(prompt) else {
        return false;
    };
    let recognized = match envelope.get("envelope_kind") {
        Some(Value::String(kind)) => kind == crate::integration::DELIVERY_ENVELOPE_KIND,
        Some(_) => false,
        None => envelope
            .get("trusted_instruction")
            .and_then(Value::as_str)
            .is_some_and(|instruction| {
                instruction.starts_with("Namedrop collab/v1")
                    || instruction.starts_with("Pane Collaboration collab/v1")
            }),
    };
    recognized
        && envelope
            .pointer("/control/protocol_version")
            .and_then(Value::as_str)
            == Some(crate::INTEGRATION_PROTOCOL_VERSION)
        && envelope
            .pointer("/control/attempt_id")
            .and_then(Value::as_str)
            .is_some_and(|attempt_id| !attempt_id.is_empty())
}
