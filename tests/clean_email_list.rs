use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn accepts_private_domain_filters_from_the_environment() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_clean-email-list"))
        .args(["--keep-free-mail"])
        .env(
            "MAILGRAPH_CLEAN_EXCLUDE_DOMAIN_CONTAINS",
            "blocked.example,another.example",
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("clean-email-list should start");

    child
        .stdin
        .take()
        .expect("stdin should be piped")
        .write_all(b"email,name\nkept@allowed.example,Alice\ndropped@blocked.example,Service\n")
        .expect("CSV input should be written");

    let output = child
        .wait_with_output()
        .expect("clean-email-list should finish");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "email,name\nkept@allowed.example,Alice\n"
    );
}
