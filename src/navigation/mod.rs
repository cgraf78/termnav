//! Topology-independent navigation policy.
//!
//! This module intentionally does not know how to invoke tmux, contact an SSH
//! relay, or emit a terminal escape sequence. Those mechanisms live behind
//! [`Backend`]. Keeping the traversal policy pure matters for two reasons:
//!
//! - every integration surface follows the same local-to-outer ordering;
//! - tests can exhaust unusual shared-client and arbitrary-depth topologies
//!   without constructing fragile process trees for each policy branch.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

mod system;

pub use system::{SystemBackend, parse_clients, process_tmux_parent};

/// Stable outcome values used by tmux, Neovim, relay, and terminal adapters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum Outcome {
    /// One scope accepted the gesture.
    Handled = 0,
    /// Identity or transport uncertainty made routing unsafe.
    Error = 1,
    /// This scope cannot perform the gesture, so its parent may try.
    Declined = 3,
}

/// Semantic operation requested by a key chord.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    /// Select an adjacent editor or tmux pane.
    PaneSelect,
    /// Swap the active pane with one directional neighbor on this host.
    PaneMove,
    /// Select the previous or next tab/window.
    TabSelect,
    /// Reorder the current tab/window.
    TabMove,
}

impl Action {
    /// Parse the canonical public command spelling.
    ///
    /// Relay protocol spellings deliberately live at the relay boundary. They
    /// predate the unified CLI vocabulary and remain wire details needed while
    /// independently updated hosts share one SSH path; accepting them here
    /// would accidentally turn that transport compatibility into public API.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "pane-select" => Ok(Self::PaneSelect),
            "pane-move" => Ok(Self::PaneMove),
            "tab-select" => Ok(Self::TabSelect),
            "tab-move" => Ok(Self::TabMove),
            _ => Err(format!("invalid navigation action: {value}")),
        }
    }

    /// Return the canonical CLI spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PaneSelect => "pane-select",
            Self::PaneMove => "pane-move",
            Self::TabSelect => "tab-select",
            Self::TabMove => "tab-move",
        }
    }
}

/// Direction vocabulary shared by every navigation scope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    /// Move left.
    Left,
    /// Move down.
    Down,
    /// Move up.
    Up,
    /// Move right.
    Right,
    /// Select the next tab/window.
    Next,
    /// Select the previous tab/window.
    Previous,
}

impl Direction {
    /// Parse a direction and reject combinations that the action cannot mean.
    pub fn parse(action: Action, value: &str) -> Result<Self, String> {
        let direction = match value {
            "left" => Self::Left,
            "down" => Self::Down,
            "up" => Self::Up,
            "right" => Self::Right,
            "next" => Self::Next,
            "previous" => Self::Previous,
            _ => return Err(format!("invalid {} direction: {value}", action.as_str())),
        };

        let valid = match action {
            Action::PaneSelect | Action::PaneMove => {
                matches!(direction, Self::Left | Self::Down | Self::Up | Self::Right)
            }
            Action::TabSelect => matches!(direction, Self::Next | Self::Previous),
            Action::TabMove => matches!(direction, Self::Left | Self::Right),
        };
        valid
            .then_some(direction)
            .ok_or_else(|| format!("invalid {} direction: {value}", action.as_str()))
    }

    /// Return the canonical CLI and wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Down => "down",
            Self::Up => "up",
            Self::Right => "right",
            Self::Next => "next",
            Self::Previous => "previous",
        }
    }
}

/// One tmux pane together with the optional session that gives tabs meaning.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Scope {
    /// Explicit tmux server socket.
    pub socket: String,
    /// Globally stable pane identifier.
    pub pane: String,
    /// Session identity required for session-relative window operations.
    pub session: Option<String>,
}

impl Scope {
    /// Return the cycle identity shared by linked sessions.
    ///
    /// Pane IDs are server-global, whereas a session-qualified target is not:
    /// one window can be linked into multiple sessions. Using only socket plus
    /// pane prevents a linked-session loop from executing a gesture twice.
    #[must_use]
    pub fn identity(&self) -> String {
        format!("{}:{}", self.socket, self.pane)
    }

    /// Return the target form tmux needs for session-relative operations.
    #[must_use]
    pub fn target(&self) -> String {
        self.session.as_ref().map_or_else(
            || self.pane.clone(),
            |session| format!("{session}:.{}", self.pane),
        )
    }
}

/// Physical tmux client identity capable of carrying a gesture outward.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Client {
    /// Last activity timestamp reported by tmux.
    pub activity: u64,
    /// Client process ID.
    pub pid: u32,
    /// Client tty path.
    pub tty: String,
    /// Client terminal type.
    pub termtype: String,
    /// Session currently displayed by the client.
    pub session: String,
    /// Active pane currently displayed by the client.
    pub pane: String,
    /// Whether tmux reports this client focused.
    pub focused: bool,
    /// Whether this is a control-mode client with no interactive UI ownership.
    pub control: bool,
    /// Explicit tmux server socket.
    pub socket: String,
    /// Whether the caller supplied exact physical provenance.
    pub exact: bool,
    /// Client creation timestamp used to reject PID/tty reuse.
    pub created: u64,
}

/// Result plus provenance safe to retain for the next item in a short burst.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteResult {
    /// Routing result.
    pub outcome: Outcome,
    /// Physical client selected at the outer edge, if one was unambiguous.
    pub client: Option<Client>,
    /// Logical tmux scope that handled the gesture, if applicable.
    pub scope: Option<Scope>,
}

/// Side-effect boundary for the topology-independent router.
///
/// Implementations must fail closed when identity cannot be revalidated. A
/// stale focus snapshot is never permission to route through whichever client
/// happens to look newest later.
pub trait Backend {
    /// Return the tmux scope containing the caller, if any.
    fn current_scope(&mut self) -> Option<Scope>;

    /// Try the operation inside one tmux scope.
    fn execute(&mut self, scope: &Scope, action: Action, direction: Direction) -> Outcome;

    /// Select the one safe physical client displaying a logical scope.
    fn resolve_client(&mut self, scope: &Scope, started_at: u64) -> Option<Client>;

    /// Refresh previously retained physical provenance for a queued request.
    fn refresh_client(&mut self, client: &Client) -> Option<Client> {
        Some(client.clone())
    }

    /// Resolve session ownership and optional physical provenance in one read.
    fn inspect_scope(&mut self, scope: &Scope, started_at: u64) -> (Option<Scope>, Option<Client>);

    /// Follow a retained logical scope to its currently active pane.
    fn refresh_scope(&mut self, scope: &Scope) -> Option<Scope> {
        Some(scope.clone())
    }

    /// Revalidate a client immediately before following its process ancestry.
    fn validate_client(&mut self, client: &Client, started_at: u64) -> bool;

    /// Return the nearest enclosing tmux scope in the client's process tree.
    fn parent_scope(&mut self, client: &Client) -> Option<Scope>;

    /// Ask the parent SSH transport to continue routing.
    fn relay(&mut self, client: &Client, action: Action, direction: Direction) -> Outcome;

    /// Offer the gesture to the outer terminal application.
    fn terminal(
        &mut self,
        client: Option<&Client>,
        action: Action,
        direction: Direction,
    ) -> Outcome;
}

/// Select a physical client without guessing through ambiguous attachments.
#[must_use]
pub fn choose_client(
    clients: &[Client],
    started_at: u64,
    freshness_seconds: u64,
) -> Option<Client> {
    let eligible = clients
        .iter()
        .filter(|client| !client.control)
        .collect::<Vec<_>>();
    if eligible.len() == 1 {
        return Some(eligible[0].clone());
    }
    if eligible.is_empty() {
        return None;
    }

    // Focus is stronger than activity. When more than one terminal claims
    // focus, picking either would turn inconsistent external state into a
    // potentially destructive navigation action, so ambiguity stops here.
    let focused = eligible
        .iter()
        .filter(|client| client.focused)
        .copied()
        .collect::<Vec<_>>();
    if focused.len() == 1 {
        return Some(focused[0].clone());
    }
    if !focused.is_empty() {
        return None;
    }

    // Some terminals briefly omit focus during handoff. Recency is therefore
    // allowed only as a bounded compatibility fallback, never as an unbounded
    // "most recently used" heuristic that could select a stale attachment.
    let newest_activity = eligible.iter().map(|client| client.activity).max()?;
    let newest = eligible
        .iter()
        .filter(|client| client.activity == newest_activity)
        .copied()
        .collect::<Vec<_>>();
    if newest.len() != 1
        || newest_activity > started_at
        || started_at.saturating_sub(newest_activity) > freshness_seconds
    {
        return None;
    }
    Some(newest[0].clone())
}

/// Walk local scopes, process ancestry, SSH relays, and the terminal once each.
pub struct Navigator<'a, B, C>
where
    B: Backend,
    C: Fn() -> u64,
{
    backend: &'a mut B,
    now: C,
}

impl<'a, B, C> Navigator<'a, B, C>
where
    B: Backend,
    C: Fn() -> u64,
{
    /// Create a router around one concrete side-effect backend.
    pub fn new(backend: &'a mut B, now: C) -> Self {
        Self { backend, now }
    }

    /// Route one semantic action and return only its stable exit outcome.
    pub fn navigate(
        &mut self,
        action: Action,
        direction: Direction,
        include_current: bool,
        exact_client: Option<Client>,
    ) -> Outcome {
        self.route(action, direction, include_current, exact_client, None, None)
            .outcome
    }

    /// Route one action while retaining provenance for an ordered successor.
    ///
    /// The caller may retain the returned identities only for a bounded burst.
    /// Every reuse goes through the backend refresh hooks so a focus handoff or
    /// pane replacement cannot redirect a delayed key press.
    #[allow(clippy::too_many_arguments)]
    pub fn route(
        &mut self,
        action: Action,
        direction: Direction,
        include_current: bool,
        exact_client: Option<Client>,
        continuing_client: Option<Client>,
        continuing_scope: Option<Scope>,
    ) -> RouteResult {
        let started_at = (self.now)();
        let mut visited = HashSet::new();
        let mut client = exact_client;

        if let Some(scope) = continuing_scope {
            let Some(current) = self.backend.refresh_scope(&scope) else {
                return RouteResult::uncertain(action, None);
            };
            if let Some(previous) = continuing_client {
                client = self.backend.refresh_client(&previous);
                if client
                    .as_ref()
                    .is_none_or(|candidate| !client_displays(candidate, &current))
                {
                    return RouteResult::uncertain(action, client);
                }
            }
            let (outcome, current, selected) = self.enter_scope(
                current,
                action,
                direction,
                started_at,
                &mut visited,
                client,
                true,
            );
            client = selected;
            if outcome != Outcome::Declined {
                return RouteResult {
                    outcome,
                    client,
                    scope: Some(current),
                };
            }
            if client.is_none() {
                return RouteResult::uncertain(action, None);
            }
        } else if let Some(previous) = continuing_client {
            let Some(selected) = self.backend.refresh_client(&previous) else {
                return RouteResult::uncertain(action, None);
            };
            let current = Scope {
                socket: selected.socket.clone(),
                pane: selected.pane.clone(),
                session: Some(selected.session.clone()),
            };
            let (outcome, current, selected) = self.enter_scope(
                current,
                action,
                direction,
                started_at,
                &mut visited,
                Some(selected),
                false,
            );
            client = selected;
            if outcome != Outcome::Declined {
                return RouteResult {
                    outcome,
                    client,
                    scope: Some(current),
                };
            }
        } else if include_current {
            let current = if let Some(source) = client.as_ref().filter(|source| source.exact) {
                // A tmux binding can name the physical client and logical pane
                // that generated a gesture. Treat that complete identity as
                // the authoritative current scope, but revalidate it before a
                // layout mutation so a delayed command cannot act after the
                // client moved, detached, or reused its PID/tty.
                if !self.backend.validate_client(source, started_at) {
                    return RouteResult::uncertain(action, client);
                }
                Scope {
                    socket: source.socket.clone(),
                    pane: source.pane.clone(),
                    session: (!source.session.is_empty()).then(|| source.session.clone()),
                }
            } else if let Some(current) = self.backend.current_scope() {
                current
            } else {
                return self.finish_without_current(action, direction, client);
            };
            let (outcome, current, selected) = self.enter_scope(
                current,
                action,
                direction,
                started_at,
                &mut visited,
                client,
                true,
            );
            client = selected;
            if outcome != Outcome::Declined {
                return RouteResult {
                    outcome,
                    client,
                    scope: Some(current),
                };
            }
            if client.is_none() {
                return RouteResult::uncertain(action, None);
            }
        }

        let Some(mut selected) = client else {
            if action == Action::PaneMove {
                return RouteResult {
                    outcome: Outcome::Declined,
                    client: None,
                    scope: None,
                };
            }
            return RouteResult {
                outcome: self.backend.terminal(None, action, direction),
                client: None,
                scope: None,
            };
        };

        loop {
            // Client selection is a snapshot, not a lease. Revalidate at each
            // outward boundary so a delayed gesture cannot follow a different
            // terminal merely because the same tmux server is still alive.
            if !self.backend.validate_client(&selected, started_at) {
                return RouteResult::uncertain(action, Some(selected));
            }
            if let Some(parent) = self.backend.parent_scope(&selected) {
                let (outcome, parent, parent_client) = self.enter_scope(
                    parent,
                    action,
                    direction,
                    started_at,
                    &mut visited,
                    None,
                    false,
                );
                if outcome != Outcome::Declined {
                    return RouteResult {
                        outcome,
                        client: None,
                        scope: Some(parent),
                    };
                }
                let Some(parent_client) = parent_client else {
                    return RouteResult::uncertain(action, None);
                };
                selected = parent_client;
                continue;
            }

            // Pane layout is host-local state. Crossing an SSH relay or asking
            // a terminal application to reinterpret the gesture could move a
            // different host's UI, so exhaustion of local tmux ancestry is a
            // definitive decline rather than an outward-routing opportunity.
            if action == Action::PaneMove {
                return RouteResult {
                    outcome: Outcome::Declined,
                    client: Some(selected),
                    scope: None,
                };
            }

            if !self.backend.validate_client(&selected, started_at) {
                return RouteResult::error();
            }
            let relay = self.backend.relay(&selected, action, direction);
            if relay != Outcome::Declined {
                return RouteResult {
                    outcome: relay,
                    client: Some(selected),
                    scope: None,
                };
            }
            if !self.backend.validate_client(&selected, started_at) {
                return RouteResult::error();
            }
            let outcome = self.backend.terminal(Some(&selected), action, direction);
            return RouteResult {
                outcome,
                client: Some(selected),
                scope: None,
            };
        }
    }

    fn finish_without_current(
        &mut self,
        action: Action,
        direction: Direction,
        client: Option<Client>,
    ) -> RouteResult {
        if action == Action::PaneMove {
            return RouteResult {
                outcome: Outcome::Declined,
                client,
                scope: None,
            };
        }
        RouteResult {
            outcome: self.backend.terminal(client.as_ref(), action, direction),
            client,
            scope: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn enter_scope(
        &mut self,
        mut scope: Scope,
        action: Action,
        direction: Direction,
        started_at: u64,
        visited: &mut HashSet<String>,
        mut client: Option<Client>,
        preserve_client: bool,
    ) -> (Outcome, Scope, Option<Client>) {
        let mut inspected = false;
        if action != Action::PaneSelect && scope.session.is_none() {
            let (resolved, discovered) = self.backend.inspect_scope(&scope, started_at);
            inspected = true;
            let Some(resolved) = resolved else {
                let outcome = if action == Action::PaneMove {
                    Outcome::Declined
                } else {
                    Outcome::Error
                };
                return (outcome, scope, None);
            };
            if client.is_none() {
                client = discovered;
            }
            scope = resolved;
        } else if preserve_client && client.is_none() {
            let (_, discovered) = self.backend.inspect_scope(&scope, started_at);
            inspected = true;
            client = discovered;
        }

        let outcome = self.execute_once(&scope, action, direction, visited);
        if outcome != Outcome::Declined || client.is_some() {
            return (outcome, scope, client);
        }

        // Physical client selection is deliberately deferred until the action
        // must leave this logical scope. Directly attached multi-client tmux
        // sessions therefore keep working for local moves while ambiguous
        // ancestry still fails closed before crossing a boundary.
        if inspected {
            return (outcome, scope, None);
        }
        client = self.backend.resolve_client(&scope, started_at);
        (outcome, scope, client)
    }

    fn execute_once(
        &mut self,
        scope: &Scope,
        action: Action,
        direction: Direction,
        visited: &mut HashSet<String>,
    ) -> Outcome {
        if !visited.insert(scope.identity()) {
            return Outcome::Error;
        }
        self.backend.execute(scope, action, direction)
    }
}

impl RouteResult {
    fn error() -> Self {
        Self {
            outcome: Outcome::Error,
            client: None,
            scope: None,
        }
    }

    fn uncertain(action: Action, client: Option<Client>) -> Self {
        if action == Action::PaneMove {
            // Movement changes layout, so uncertainty owns a quiet no-op. In
            // contrast, selection requests preserve the long-standing error
            // signal callers use to detect stale route identity.
            Self {
                outcome: Outcome::Declined,
                client,
                scope: None,
            }
        } else {
            Self::error()
        }
    }
}

fn client_displays(client: &Client, scope: &Scope) -> bool {
    client.socket == scope.socket
        && client.pane == scope.pane
        && scope
            .session
            .as_ref()
            .is_none_or(|session| &client.session == session)
}
