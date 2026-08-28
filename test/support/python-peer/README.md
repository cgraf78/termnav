# Frozen Python relay peer

This directory contains one deliberately minimal frozen Python v2 relay peer.
It implements only prepare, commit, abort, and tmux forwarding so mixed-version
tests exercise independent JSON/socket framing without retaining the deleted
Python navigation, focus, SSH, sweep, or hot-service implementations.

Termnav hosts may update independently across an SSH path even when deployment
normally upgrades a fleet together. The fixture is not packaged or installed
and supports
only the unambiguous single-client topology constructed by its compatibility
test. Production policy and multi-client behavior remain owned by Rust.
