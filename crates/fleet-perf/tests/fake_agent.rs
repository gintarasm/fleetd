use std::process::Command;

#[test]
fn fake_agent_writes_exact_bytes() {
    let out = Command::new(env!("CARGO_BIN_EXE_fake-agent"))
        .args(["--bytes", "1000000"])
        .output()
        .expect("run fake-agent");
    assert!(out.status.success());
    assert_eq!(out.stdout.len(), 1_000_000);
}
