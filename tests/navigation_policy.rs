use std::collections::HashMap;

use termnav::navigation::{
    Action, Backend, Client, Direction, Navigator, Outcome, Scope, choose_client, reselect_client,
};

#[derive(Default)]
struct FakeBackend {
    current: Option<Scope>,
    results: HashMap<String, Outcome>,
    clients: HashMap<String, Option<Client>>,
    parents: HashMap<u32, Option<Scope>>,
    relays: HashMap<u32, Outcome>,
    valid: HashMap<u32, bool>,
    events: Vec<String>,
}

impl Backend for FakeBackend {
    fn current_scope(&mut self) -> Option<Scope> {
        self.events.push("current".to_owned());
        self.current.clone()
    }

    fn execute(&mut self, scope: &Scope, action: Action, direction: Direction) -> Outcome {
        self.events.push(format!(
            "execute:{}:{}:{}",
            scope.identity(),
            action.as_str(),
            direction.as_str()
        ));
        self.results
            .get(&scope.identity())
            .copied()
            .unwrap_or(Outcome::Declined)
    }

    fn resolve_client(&mut self, scope: &Scope, started_at: u64) -> Option<Client> {
        self.events
            .push(format!("resolve:{}:{started_at}", scope.identity()));
        self.clients.get(&scope.identity()).cloned().flatten()
    }

    fn inspect_scope(&mut self, scope: &Scope, started_at: u64) -> (Option<Scope>, Option<Client>) {
        self.events
            .push(format!("inspect:{}:{started_at}", scope.identity()));
        let client = self.clients.get(&scope.identity()).cloned().flatten();
        let mut resolved = scope.clone();
        if resolved.session.is_none() {
            resolved.session = client.as_ref().map(|item| item.session.clone());
        }
        let available = resolved.session.is_some();
        (available.then_some(resolved), client)
    }

    fn validate_client(&mut self, client: &Client, _started_at: u64) -> bool {
        self.events.push(format!("validate:{}", client.pid));
        self.valid.get(&client.pid).copied().unwrap_or(true)
    }

    fn parent_scope(&mut self, client: &Client) -> Option<Scope> {
        self.events.push(format!("parent:{}", client.pid));
        self.parents.get(&client.pid).cloned().flatten()
    }

    fn relay(&mut self, client: &Client, action: Action, direction: Direction) -> Outcome {
        self.events.push(format!(
            "relay:{}:{}:{}",
            client.pid,
            action.as_str(),
            direction.as_str()
        ));
        self.relays
            .get(&client.pid)
            .copied()
            .unwrap_or(Outcome::Declined)
    }

    fn terminal(
        &mut self,
        client: Option<&Client>,
        action: Action,
        direction: Direction,
    ) -> Outcome {
        self.events.push(format!(
            "terminal:{}:{}:{}",
            client.map_or(0, |item| item.pid),
            action.as_str(),
            direction.as_str()
        ));
        Outcome::Handled
    }
}

fn scope(name: &str) -> Scope {
    Scope {
        socket: format!("/tmp/{name}.sock"),
        pane: format!("%{}", name.len()),
        session: Some(format!("${name}")),
    }
}

fn client(pid: u32) -> Client {
    Client {
        activity: 100,
        pid,
        tty: format!("/dev/pts/{pid}"),
        termtype: "tmux-256color".to_owned(),
        session: "$session".to_owned(),
        pane: "%1".to_owned(),
        focused: false,
        control: false,
        socket: "/tmp/source.sock".to_owned(),
        exact: false,
        created: 80,
    }
}

#[test]
fn unique_focused_client_beats_newer_activity() {
    let mut focused = client(10);
    focused.activity = 90;
    focused.focused = true;
    let newer = client(11);

    assert_eq!(
        choose_client(&[focused.clone(), newer], 100, 2),
        Some(focused)
    );
}

fn focused_client(pid: u32, activity: u64) -> Client {
    let mut client = client(pid);
    client.activity = activity;
    client.focused = true;
    client
}

#[test]
fn freshest_of_several_focused_clients_wins() {
    // A terminal on another machine can keep claiming focus long after its
    // user walked away; the client that just carried the keystroke is fresh.
    let current = focused_client(10, 99);
    let abandoned = focused_client(11, 10);

    assert_eq!(
        choose_client(&[abandoned, current.clone()], 100, 2),
        Some(current)
    );
}

#[test]
fn several_focused_clients_ignore_fresher_unfocused_client() {
    let current = focused_client(10, 99);
    let abandoned = focused_client(11, 10);
    let mut unfocused = client(12);
    unfocused.activity = 100;

    assert_eq!(
        choose_client(&[abandoned, unfocused, current.clone()], 100, 2),
        Some(current)
    );
}

#[test]
fn several_stale_focused_clients_fail_closed() {
    let first = focused_client(10, 90);
    let second = focused_client(11, 10);

    assert_eq!(choose_client(&[first, second], 100, 2), None);
}

#[test]
fn several_equally_fresh_focused_clients_fail_closed() {
    let first = focused_client(10, 99);
    let second = focused_client(11, 99);

    assert_eq!(choose_client(&[first, second], 100, 2), None);
}

#[test]
fn several_focused_clients_with_future_activity_fail_closed() {
    let first = focused_client(10, 101);
    let second = focused_client(11, 10);

    assert_eq!(choose_client(&[first, second], 100, 2), None);
}

#[test]
fn several_stale_focused_clients_ignore_fresh_unfocused_client() {
    let first = focused_client(10, 90);
    let second = focused_client(11, 10);
    let mut unfocused = client(12);
    unfocused.activity = 100;

    assert_eq!(choose_client(&[first, second, unfocused], 100, 2), None);
}

#[test]
fn focused_client_at_the_freshness_bound_is_fresh() {
    let current = focused_client(10, 98);
    let abandoned = focused_client(11, 10);
    assert_eq!(
        choose_client(&[abandoned.clone(), current.clone()], 100, 2),
        Some(current)
    );

    let expired = focused_client(10, 97);
    assert_eq!(choose_client(&[abandoned, expired], 100, 2), None);
}

#[test]
fn reselection_keeps_client_whose_activity_advanced() {
    // A held or repeated chord advances the source client's activity past the
    // gesture's start before dispatch revalidates the selection.
    let abandoned = focused_client(11, 10);
    let selected = choose_client(&[abandoned.clone(), focused_client(10, 100)], 100, 2);
    assert_eq!(selected.map(|client| client.pid), Some(10));

    let advanced = focused_client(10, 102);
    assert_eq!(
        reselect_client(&[abandoned, advanced.clone()], 100, 2),
        Some(advanced)
    );
}

#[test]
fn reselection_rejects_newly_fresher_competitor() {
    let selected = focused_client(10, 100);
    let competitor = focused_client(11, 102);

    assert_eq!(
        reselect_client(&[selected, competitor.clone()], 100, 2).map(|client| client.pid),
        Some(competitor.pid)
    );
}

#[test]
fn reselection_fails_closed_on_tied_activity() {
    let first = focused_client(10, 102);
    let second = focused_client(11, 102);

    assert_eq!(reselect_client(&[first, second], 100, 2), None);
}

#[test]
fn ambiguous_or_stale_clients_fail_closed() {
    let mut first = client(10);
    first.activity = 90;
    let mut second = client(11);
    second.activity = 89;
    assert_eq!(choose_client(&[first, second], 100, 1), None);

    let tied = [client(10), client(11)];
    assert_eq!(choose_client(&tied, 100, 1), None);
}

#[test]
fn local_parent_precedes_an_available_relay() {
    let mut backend = FakeBackend::default();
    let inner = scope("inner");
    let parent = scope("parent");
    let origin = client(10);
    backend.current = Some(inner.clone());
    backend
        .clients
        .insert(inner.identity(), Some(origin.clone()));
    backend.parents.insert(origin.pid, Some(parent.clone()));
    backend.results.insert(parent.identity(), Outcome::Handled);
    backend.relays.insert(origin.pid, Outcome::Handled);

    let outcome = Navigator::new(&mut backend, || 100).navigate(
        Action::TabSelect,
        Direction::Next,
        true,
        None,
    );

    assert_eq!(outcome, Outcome::Handled);
    assert!(backend.events.iter().any(|event| event == "parent:10"));
    assert!(
        !backend
            .events
            .iter()
            .any(|event| event.starts_with("relay:"))
    );
}

#[test]
fn cycle_is_an_error_instead_of_repeating_a_gesture() {
    let mut backend = FakeBackend::default();
    let repeated = scope("same");
    let first = client(10);
    backend.current = Some(repeated.clone());
    backend
        .clients
        .insert(repeated.identity(), Some(first.clone()));
    backend.parents.insert(first.pid, Some(repeated.clone()));

    let outcome = Navigator::new(&mut backend, || 100).navigate(
        Action::PaneSelect,
        Direction::Left,
        true,
        None,
    );

    assert_eq!(outcome, Outcome::Error);
}

#[test]
fn pane_move_bubbles_through_arbitrary_local_tmux_nesting() {
    let mut backend = FakeBackend::default();
    let scopes = (0..32)
        .map(|depth| scope(&format!("level-{depth}")))
        .collect::<Vec<_>>();
    backend.current = scopes.first().cloned();
    for (depth, pair) in scopes.windows(2).enumerate() {
        let route = client(10 + depth as u32);
        backend
            .clients
            .insert(pair[0].identity(), Some(route.clone()));
        backend.parents.insert(route.pid, Some(pair[1].clone()));
    }
    backend.results.insert(
        scopes.last().expect("outer scope").identity(),
        Outcome::Handled,
    );

    let outcome = Navigator::new(&mut backend, || 100).navigate(
        Action::PaneMove,
        Direction::Right,
        true,
        None,
    );

    assert_eq!(outcome, Outcome::Handled);
    assert_eq!(
        backend
            .events
            .iter()
            .filter(|event| event.starts_with("execute:"))
            .count(),
        scopes.len()
    );
    assert!(
        !backend
            .events
            .iter()
            .any(|event| event.starts_with("relay:"))
    );
    assert!(
        !backend
            .events
            .iter()
            .any(|event| event.starts_with("terminal:"))
    );
}

#[test]
fn pane_move_stops_before_ssh_relay_and_terminal_boundaries() {
    let mut backend = FakeBackend::default();
    let inner = scope("inner");
    let origin = client(10);
    backend.current = Some(inner.clone());
    backend
        .clients
        .insert(inner.identity(), Some(origin.clone()));
    backend.parents.insert(origin.pid, None);
    backend.relays.insert(origin.pid, Outcome::Handled);

    let outcome = Navigator::new(&mut backend, || 100).navigate(
        Action::PaneMove,
        Direction::Left,
        true,
        None,
    );

    assert_eq!(outcome, Outcome::Declined);
    assert!(
        !backend
            .events
            .iter()
            .any(|event| event.starts_with("relay:"))
    );
    assert!(
        !backend
            .events
            .iter()
            .any(|event| event.starts_with("terminal:"))
    );
}

#[test]
fn pane_move_without_a_tmux_scope_is_a_boundary_noop() {
    let mut backend = FakeBackend::default();

    let outcome =
        Navigator::new(&mut backend, || 100).navigate(Action::PaneMove, Direction::Up, true, None);

    assert_eq!(outcome, Outcome::Declined);
    assert_eq!(backend.events, vec!["current"]);
}

#[test]
fn pane_move_declines_when_client_identity_is_ambiguous() {
    let mut backend = FakeBackend {
        current: Some(scope("ambiguous")),
        ..FakeBackend::default()
    };

    let outcome = Navigator::new(&mut backend, || 100).navigate(
        Action::PaneMove,
        Direction::Down,
        true,
        None,
    );

    assert_eq!(outcome, Outcome::Declined);
    assert!(
        !backend
            .events
            .iter()
            .any(|event| event.starts_with("relay:"))
    );
    assert!(
        !backend
            .events
            .iter()
            .any(|event| event.starts_with("terminal:"))
    );
}

#[test]
fn pane_move_declines_when_selected_client_turns_stale() {
    let mut backend = FakeBackend::default();
    let current = scope("stale");
    let selected = client(42);
    backend.current = Some(current.clone());
    backend
        .clients
        .insert(current.identity(), Some(selected.clone()));
    backend.valid.insert(selected.pid, false);

    let outcome =
        Navigator::new(&mut backend, || 100).navigate(Action::PaneMove, Direction::Up, true, None);

    assert_eq!(outcome, Outcome::Declined);
    assert!(
        !backend
            .events
            .iter()
            .any(|event| event.starts_with("parent:"))
    );
}
