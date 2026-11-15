//! The workspace release profile (issue #19): whole-program optimisation keeps
//! `fleetd`'s resident `__TEXT`, and so its idle RSS, down. `panic` must stay
//! `unwind`: the restart handoff (`src/restart.rs`) relies on `catch_unwind`.

fn release_profile() -> toml::Table {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml");
    let manifest: toml::Table = std::fs::read_to_string(path)
        .expect("read workspace Cargo.toml")
        .parse()
        .expect("parse workspace Cargo.toml");
    manifest
        .get("profile")
        .and_then(|p| p.get("release"))
        .and_then(|r| r.as_table())
        .cloned()
        .expect("workspace Cargo.toml has [profile.release]")
}

#[test]
fn release_uses_fat_lto_and_one_codegen_unit() {
    let release = release_profile();
    assert_eq!(release.get("lto").and_then(|v| v.as_str()), Some("fat"));
    assert_eq!(
        release.get("codegen-units").and_then(|v| v.as_integer()),
        Some(1)
    );
}

#[test]
fn release_keeps_unwinding_panics() {
    let release = release_profile();
    let panic = release.get("panic").and_then(|v| v.as_str());
    assert!(
        panic.is_none() || panic == Some("unwind"),
        "restart relies on catch_unwind; panic = {panic:?}"
    );
}
