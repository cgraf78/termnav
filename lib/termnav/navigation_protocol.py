"""Stable navigation vocabulary shared by routers and transports."""

ACTION_DIRECTIONS = {
    "pane-select": frozenset(("left", "down", "up", "right")),
    "tab-select": frozenset(("next", "previous")),
    "tab-move": frozenset(("left", "right")),
}

WIRE_ACTIONS = {
    "pane-select": "pane",
    "tab-select": "window",
    "tab-move": "move",
}

ACTION_FROM_WIRE = {wire: action for action, wire in WIRE_ACTIONS.items()}


def validate_request(action: str, direction: str) -> None:
    """Reject vocabulary outside Termnav's stable semantic interface."""

    directions = ACTION_DIRECTIONS.get(action)
    if directions is None:
        raise ValueError(f"invalid navigation action: {action}")
    if direction not in directions:
        raise ValueError(f"invalid {action} direction: {direction}")


def valid_wire_request(scope: str, direction: str) -> bool:
    """Return whether a relay wire request maps to a public action."""

    action = ACTION_FROM_WIRE.get(scope)
    return action is not None and direction in ACTION_DIRECTIONS[action]
