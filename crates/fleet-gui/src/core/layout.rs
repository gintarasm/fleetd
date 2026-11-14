use serde::{Deserialize, Serialize};

/// Saved split tree for one target. Opaque placeholder; the split-tree task replaces it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SavedLayout {}
