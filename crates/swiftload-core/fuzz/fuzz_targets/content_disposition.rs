//! Fuzzing the `Content-Disposition` parser.
//!
//! A header written by the server, parsed by us, and used to name a file on disk. The parser
//! handles quoting, escaping, `filename*` with RFC 5987 encoding, and semicolons inside quoted
//! strings — enough state to be worth exploring rather than merely unit-testing.
//!
//! The contract is narrow and absolute: whatever comes back must already be safe to use as a
//! filename, because callers treat it as one.
#![no_main]

use libfuzzer_sys::fuzz_target;
use swiftload_core::util::filename::{parse_content_disposition, sanitize};

fuzz_target!(|data: &[u8]| {
    let header = String::from_utf8_lossy(data);
    if let Some(name) = parse_content_disposition(&header) {
        // Whatever the header claimed, the result must survive sanitising unchanged — if it
        // does not, the parser is handing callers a name that still needs cleaning, and one of
        // them will eventually forget.
        assert_eq!(
            sanitize(&name),
            name,
            "parser returned a name that is not already safe: {name:?} from {header:?}"
        );
    }
});
