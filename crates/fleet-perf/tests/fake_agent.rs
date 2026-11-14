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

#[test]
fn fake_agent_forever_writes_until_stdout_closes() {
    use std::io::Read;
    use std::process::Stdio;
    let mut child = Command::new(env!("CARGO_BIN_EXE_fake-agent"))
        .arg("--forever")
        .stdout(Stdio::piped())
        .spawn()
        .expect("run fake-agent");
    let mut stdout = child.stdout.take().expect("piped");
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    stdout.read_exact(&mut buf).expect("4 MiB of output");
    drop(stdout);
    // EPIPE (or SIGPIPE) ends it once its reader is gone.
    let status = child.wait().expect("wait");
    assert!(!status.success());
}

#[test]
fn fake_agent_needs_bytes_or_forever() {
    let out = Command::new(env!("CARGO_BIN_EXE_fake-agent"))
        .output()
        .expect("run fake-agent");
    assert!(!out.status.success());
}
