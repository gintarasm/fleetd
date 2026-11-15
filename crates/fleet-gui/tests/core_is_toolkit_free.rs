//! ADR 0029: `src/core` must not depend on gpui or AppKit.

use std::fs;
use std::path::Path;

#[test]
fn core_has_no_gpui_or_appkit_imports() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/core");
    let mut checked = 0;
    let mut offenders = Vec::new();
    for entry in fs::read_dir(&dir).expect("read src/core") {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        checked += 1;
        let text = fs::read_to_string(&path).unwrap();
        for (n, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            if ["use gpui", "gpui::", "objc2", "cocoa"]
                .iter()
                .any(|p| code.contains(p))
            {
                offenders.push(format!("{}:{}: {}", path.display(), n + 1, line.trim()));
            }
        }
    }
    assert!(checked > 0, "no core files found");
    assert!(
        offenders.is_empty(),
        "toolkit use in core:\n{}",
        offenders.join("\n")
    );
}
