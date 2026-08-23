//! Fuzzing the filename sanitiser.
//!
//! This is the highest-consequence pure function in the engine: it turns a name chosen by
//! whoever runs the server into a path we then write to. A single escape is arbitrary file
//! write. Property tests already cover it over random ASCII-ish input; this target adds
//! coverage-guided exploration over arbitrary *bytes*, which is where the interesting cases
//! live — lone surrogates, overlong sequences, bidi controls, embedded NULs.
//!
//! Every assertion below is an invariant the caller relies on, not a description of what the
//! implementation currently happens to do.
#![no_main]

use libfuzzer_sys::fuzz_target;
use std::path::Path;
use swiftload_core::util::filename::{resolve_destination, sanitize};

fuzz_target!(|data: &[u8]| {
    let raw = String::from_utf8_lossy(data);
    let name = sanitize(&raw);

    // 1. Always a usable name. An empty result would be silently written to the directory
    //    itself.
    assert!(!name.is_empty(), "empty name from {raw:?}");

    // 2. Exactly one path component. Anything else is traversal or an absolute path.
    assert!(
        !name.contains('/') && !name.contains('\\'),
        "separator survived: {name:?} from {raw:?}"
    );
    assert!(name != "." && name != "..", "dot name: {name:?}");

    // 3. No NTFS alternate data stream. `report.pdf:evil.exe` writes a hidden executable.
    assert!(!name.contains(':'), "stream separator survived: {name:?}");

    // 4. No trailing dot or space: Windows silently strips them, so `evil.exe.` and `evil.exe`
    //    are the same file, and a check against the former protects nothing.
    assert!(
        !name.ends_with('.') && !name.ends_with(' '),
        "trailing dot or space: {name:?}"
    );

    // 5. No control characters, which corrupt terminals and log lines.
    assert!(
        !name.chars().any(|c| c.is_control()),
        "control character survived: {name:?}"
    );

    // 6. Bounded, so a long name cannot break the path limit on its own.
    //
    //    Counted in UTF-16 code units, not bytes: NTFS's 255-unit component limit is a UTF-16
    //    limit, and a name of 600 bytes can be 200 units. Asserting on bytes fails on any name
    //    with non-ASCII characters in it — as fuzzing this promptly demonstrated.
    let units = name.encode_utf16().count();
    assert!(units <= 255, "overlong name: {units} UTF-16 units");

    // 7. Idempotent. A name compared against its own re-sanitised form must match itself, and
    //    resume, delete and overwrite detection all rest on that comparison.
    assert_eq!(sanitize(&name), name, "sanitize is not idempotent for {raw:?}");

    // 8. The invariant that actually matters: the resolved destination stays inside the
    //    directory it was given, for every input, always.
    let base = Path::new("/downloads/base");
    if let Ok(dest) = resolve_destination(base, &name) {
        assert!(
            dest.starts_with(base),
            "escaped the destination: {dest:?} from {raw:?}"
        );
    }
});
