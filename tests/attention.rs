//! Real tmux ancestry test, isolated from every user session and state root.

use std::process::Command;

#[test]
fn attention_follows_the_exact_nested_client_into_its_parent_pane() {
    let status = Command::new("python3")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/test/attention.py"))
        .arg(env!("CARGO_BIN_EXE_termnav"))
        .status()
        .expect("run isolated attention integration");
    assert!(status.success());
}
