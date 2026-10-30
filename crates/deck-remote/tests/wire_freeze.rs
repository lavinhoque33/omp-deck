//! Protocol v1 is frozen (R5, `docs/notes/mobile-relay.md` "Protocol v1 freeze").
//!
//! Every shared vector file is pinned by the SHA-256 of its exact bytes. The generators
//! (`OMPDECK_WRITE_VECTORS=1`) and the regeneration checks catch an encoding change; this pin
//! catches a regenerated file that someone blessed. **Changing a hash here changes protocol v1**:
//! only do it for an additive change the freeze rules allow (new vectors for a feature-gated
//! variant, new reject cases), or together with a `PROTOCOL` / prologue / `/v2` decision, and
//! record it in the design doc.

use sha2::{Digest, Sha256};

const PINNED: &[(&str, &str)] = &[
    (
        "api-v1.json",
        "461095267dd7e0ae605be1cc57c1b91bfd2d8e1cf7af9615b59578773e5c0197",
    ),
    (
        "cacophony-ik.json",
        "95f24bddaabfce40922ebe6f3a06e7b2a1b0e9cdd3c6c23b87654046ac529175",
    ),
    (
        "hello-v1.json",
        "013bb93428a3344ee1909c98b921fee83aae4dc117fa6a56c7dd9587f7025566",
    ),
    (
        "invite-v1.json",
        "9ff8b33ca0d83b6ac976aefc8dfb3d6507685d5134c5563e6109c7237568eb11",
    ),
    (
        "noise-v1.json",
        "7049a5e230e87bb3e8f7e3b1a4288bf85b4858fb594f261d2828cb1d598ce9fd",
    ),
    (
        "payload-v1.json",
        "fe1dd690b5aae846d4fd591ef280b3c439ac9895071d69eb856cddf617f732aa",
    ),
    (
        "relay-v1.json",
        "47e36ca4ea838a138a2a4f9848a66c043fd32648d4db43182a4ab4a486f82190",
    ),
];

fn vectors_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vectors")
}

#[test]
fn every_vector_file_matches_its_frozen_hash() {
    let mut changed = Vec::new();
    for (file, pinned) in PINNED {
        let bytes =
            std::fs::read(vectors_dir().join(file)).unwrap_or_else(|e| panic!("read {file}: {e}"));
        let actual: String = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if actual != *pinned {
            changed.push(format!("{file}: pinned {pinned}, now {actual}"));
        }
    }
    assert!(
        changed.is_empty(),
        "protocol v1 vectors changed (see this file's header before re-pinning):\n{}",
        changed.join("\n")
    );
}

/// A new vector file must be pinned too, or it could change unnoticed.
#[test]
fn every_vector_file_is_pinned() {
    let mut unpinned: Vec<String> = std::fs::read_dir(vectors_dir())
        .expect("read vectors dir")
        .filter_map(|e| {
            let name = e.expect("dir entry").file_name().into_string().ok()?;
            let json = name.ends_with(".json");
            (json && !PINNED.iter().any(|(f, _)| *f == name)).then_some(name)
        })
        .collect();
    unpinned.sort();
    assert!(unpinned.is_empty(), "unpinned vector files: {unpinned:?}");
}
