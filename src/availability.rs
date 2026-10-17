use crate::{DiscoveredPane, PaneAddressRecord, PaneType, WorktreeState};
use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessConstraint {
    RecipientRemoved,
    SpaceInactive,
    SpacePaused,
    ReconciliationRequired,
    RecipientDisabled,
    RecipientUnbound,
    RecipientDisconnected,
    TerminalContainsAgent,
    ShellBusyOrUnknown,
    SessionChanged,
    RegistrationRequired,
    IntegrationRepairRequired,
    RecipientBusyOrUnknown,
    EarlierDeliveryPending,
}

/// A read-only projection of what an explicit send, automatic selection, and
/// immediate dispatch would do with one recipient. None of these fields acquires
/// a lease or proves that a later dispatch will still see the same state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RecipientReadiness {
    pub constraints: Vec<ReadinessConstraint>,
    pub earlier_deliveries: u64,
    pub held_deliveries: u64,
    pub explicit_send_queueable: bool,
    pub automatic_selection_suitable: bool,
    pub dispatch_eligible: bool,
}

impl RecipientReadiness {
    pub(crate) fn has(&self, constraint: ReadinessConstraint) -> bool {
        self.constraints.contains(&constraint)
    }

    pub(crate) fn delivery_expectation(&self, pane_type: PaneType) -> &'static str {
        if !self.constraints.is_empty() {
            "queued until constraints clear"
        } else if pane_type == PaneType::Terminal {
            "command executes after dispatch verifies an available shell"
        } else {
            "eligible after dispatch revalidates a settled session"
        }
    }
}

pub(crate) struct RecipientReadinessFacts<'a> {
    pub address: Option<&'a PaneAddressRecord>,
    pub live: Option<&'a DiscoveredPane>,
    pub worktree_state: Option<WorktreeState>,
    pub reconciliation_completed: bool,
    pub registration_required: bool,
    pub integration_repair_required: bool,
    pub shell_available: Option<bool>,
    pub earlier_deliveries: u64,
    pub held_deliveries: u64,
}

pub(crate) fn project(facts: RecipientReadinessFacts<'_>) -> RecipientReadiness {
    let Some(address) = facts.address else {
        return RecipientReadiness {
            constraints: vec![ReadinessConstraint::RecipientRemoved],
            earlier_deliveries: 0,
            held_deliveries: 0,
            explicit_send_queueable: false,
            automatic_selection_suitable: false,
            dispatch_eligible: false,
        };
    };

    let mut constraints = Vec::new();
    let active = match facts.worktree_state {
        Some(state) if state.activated && !state.deactivated => {
            if state.paused {
                constraints.push(ReadinessConstraint::SpacePaused);
            }
            true
        }
        _ => {
            constraints.push(ReadinessConstraint::SpaceInactive);
            false
        }
    };
    if !facts.reconciliation_completed {
        constraints.push(ReadinessConstraint::ReconciliationRequired);
    }
    if address.collaboration_disabled {
        constraints.push(ReadinessConstraint::RecipientDisabled);
    }
    if address.pane_id.is_none() {
        constraints.push(ReadinessConstraint::RecipientUnbound);
    }
    if address.pane_type == PaneType::Terminal {
        if facts.live.is_none() {
            constraints.push(ReadinessConstraint::RecipientDisconnected);
        } else if facts
            .live
            .is_some_and(|pane| pane.agent.is_some() || pane.agent_session_id.is_some())
        {
            constraints.push(ReadinessConstraint::TerminalContainsAgent);
        } else if facts.shell_available == Some(false) {
            constraints.push(ReadinessConstraint::ShellBusyOrUnknown);
        }
    } else {
        match facts.live.and_then(|pane| pane.agent_session_id.as_ref()) {
            None => constraints.push(ReadinessConstraint::RecipientDisconnected),
            Some(session) if address.agent_session_id.as_ref() != Some(session) => {
                constraints.push(ReadinessConstraint::SessionChanged);
            }
            Some(_) => {
                if facts.registration_required {
                    constraints.push(ReadinessConstraint::RegistrationRequired);
                }
                if facts.integration_repair_required {
                    constraints.push(ReadinessConstraint::IntegrationRepairRequired);
                }
                if !matches!(
                    facts.live.and_then(|pane| pane.agent_status.as_deref()),
                    Some("idle" | "done")
                ) {
                    constraints.push(ReadinessConstraint::RecipientBusyOrUnknown);
                }
            }
        }
    }
    if facts.earlier_deliveries > 0 {
        constraints.push(ReadinessConstraint::EarlierDeliveryPending);
    }

    let dispatch_eligible = constraints.is_empty() && !address.retired;
    RecipientReadiness {
        constraints,
        earlier_deliveries: facts.earlier_deliveries,
        held_deliveries: facts.held_deliveries,
        explicit_send_queueable: active && !address.retired,
        automatic_selection_suitable: dispatch_eligible && address.pane_type == PaneType::Agent,
        dispatch_eligible,
    }
}
