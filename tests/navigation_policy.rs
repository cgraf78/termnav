use std::collections::HashMap;

use termnav::navigation::{
    Action, Backend, Client, Direction, Navigator, Outcome, Scope, choose_client,
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
