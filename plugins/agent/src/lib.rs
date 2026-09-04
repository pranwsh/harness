use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use arc_swap::ArcSwap;
use harness_contracts::{
    AgentCreated, AgentId, AgentState, AgentStateChanged, CH_AGENT_CREATED, CH_AGENT_STATE_CHANGED,
    KEY_AGENTS,
};
use harness_core::{Context, Result};

/// One agent: identity plus its observed lifecycle state.
#[derive(Debug, Clone)]
pub struct Agent {
    pub id: AgentId,
    pub state: AgentState,
}

/// Registry of agents. State writes go through `set_state`, which snapshots
/// into an `ArcSwap` so readers never contend on a lock.
pub struct AgentRegistry {
    states: ArcSwap<HashMap<AgentId, AgentState>>,
    next_seq: AtomicU64,
    emit: Emitter,
}

/// Emits lifecycle events; a thin owned clone of the plugin's context.
#[derive(Clone)]
struct Emitter {
    ctx: Context,
}

impl AgentRegistry {
    pub fn new(ctx: Context) -> Self {
        AgentRegistry {
            states: ArcSwap::from_pointee(HashMap::new()),
            next_seq: AtomicU64::new(1),
            emit: Emitter { ctx },
        }
    }

    /// Returns the agent, creating it (and emitting `agent.created`) if new.
    pub fn get_or_create(&self, id: &str) -> Agent {
        {
            let snapshot = self.states.load();
            if let Some(state) = snapshot.get(id) {
                return Agent {
                    id: id.to_owned(),
                    state: *state,
                };
            }
        }
        let fresh = Agent {
            id: id.to_owned(),
            state: AgentState::Idle,
        };
        self.states.rcu(|current| {
            let mut next = HashMap::clone(current);
            next.insert(fresh.id.clone(), fresh.state);
            next
        });
        let _ = self.emit.ctx.emit_key_detached(
            CH_AGENT_CREATED,
            AgentCreated {
                agent_id: fresh.id.clone(),
            },
        );
        fresh
    }

    /// Updates an agent's state and notifies listeners.
    pub fn set_state(&self, id: &str, state: AgentState) {
        let changed = {
            let current = self.states.load();
            current.get(id).is_some_and(|old| *old != state)
        };
        if !changed {
            return;
        }
        self.states.rcu(|current| {
            let mut next = HashMap::clone(current);
            if let Some(slot) = next.get_mut(id) {
                *slot = state;
            }
            next
        });
        let _ = self.emit.ctx.emit_key_detached(
            CH_AGENT_STATE_CHANGED,
            AgentStateChanged {
                agent_id: id.to_owned(),
                state,
            },
        );
    }

    /// Snapshot of an agent's state, if it exists.
    pub fn state_of(&self, id: &str) -> Option<AgentState> {
        self.states.load().get(id).copied()
    }

    /// Generates a unique agent id: `agent-<seq>`.
    pub fn generate_id(&self) -> AgentId {
        format!("agent-{}", self.next_seq.fetch_add(1, Ordering::Relaxed))
    }
}

pub struct AgentPlugin;

impl harness_core::Plugin for AgentPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("agent")
            .provides(KEY_AGENTS)
            .emits::<AgentCreated>(CH_AGENT_CREATED)
            .emits::<AgentStateChanged>(CH_AGENT_STATE_CHANGED)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        ctx.provide_key(KEY_AGENTS, Arc::new(AgentRegistry::new(ctx.clone())));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_once_and_emits_only_on_change() {
        let ctx = Context::root();
        ctx.load(AgentPlugin).unwrap();
        let reg: Arc<AgentRegistry> = ctx.inject_key(KEY_AGENTS).unwrap();

        let created = Arc::new(AtomicU64::new(0));
        ctx.on_sync_key::<AgentCreated, _>(CH_AGENT_CREATED, {
            let counter = created.clone();
            move |_| {
                counter.fetch_add(1, Ordering::Relaxed);
            }
        })
        .unwrap();

        let a = reg.get_or_create("a");
        assert_eq!(a.state, AgentState::Idle);
        reg.get_or_create("a");
        assert_eq!(created.load(Ordering::Relaxed), 1);
        assert_eq!(reg.state_of("a"), Some(AgentState::Idle));

        let changes = Arc::new(AtomicU64::new(0));
        ctx.on_sync_key::<AgentStateChanged, _>(CH_AGENT_STATE_CHANGED, {
            let counter = changes.clone();
            move |_| {
                counter.fetch_add(1, Ordering::Relaxed);
            }
        })
        .unwrap();
        reg.set_state("a", AgentState::Idle); // no-op, same state
        reg.set_state("a", AgentState::Busy);
        reg.set_state("a", AgentState::Busy); // no-op
        assert_eq!(changes.load(Ordering::Relaxed), 1);
        assert_eq!(reg.state_of("a"), Some(AgentState::Busy));
    }

    #[test]
    fn set_state_on_unknown_agent_is_ignored() {
        let ctx = Context::root();
        ctx.load(AgentPlugin).unwrap();
        let reg: Arc<AgentRegistry> = ctx.inject_key(KEY_AGENTS).unwrap();

        let changes = Arc::new(AtomicU64::new(0));
        ctx.on_sync_key::<AgentStateChanged, _>(CH_AGENT_STATE_CHANGED, {
            let counter = changes.clone();
            move |_| {
                counter.fetch_add(1, Ordering::Relaxed);
            }
        })
        .unwrap();
        reg.set_state("ghost", AgentState::Busy);
        assert_eq!(changes.load(Ordering::Relaxed), 0);
        assert_eq!(reg.state_of("ghost"), None);
    }
}
