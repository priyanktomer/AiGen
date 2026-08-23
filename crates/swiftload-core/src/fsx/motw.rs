//! Mark-of-the-Web.
//!
//! A completed download gets a `Zone.Identifier` alternate data stream marking it as
//! internet-sourced, so SmartScreen, Office Protected View and the "this file came from
//! another computer" prompt all behave exactly as they would for a browser download.
//!
//! This is the single cheapest security feature in the product: without it, files downloaded
//! by SwiftLoad are *less* safe to open than the same file downloaded by a browser.

use std::io;
use std::path::Path;

/// Write the zone marker. `ZoneId=3` is the Internet zone.
#[cfg(windows)]
pub fn apply(path: &Path, source_url: &str, referrer: Option<&str>) -> io::Result<()> {
    use std::io::Write;

    // The ADS is addressed by appending the stream name to the path.
    let stream = format!("{}:Zone.Identifier", path.display());
    let mut content = String::from("[ZoneTransfer]\r\nZoneId=3\r\n");
    if let Some(r) = referrer {
        content.push_str(&format!("ReferrerUrl={r}\r\n"));
    }
    content.push_str(&format!("HostUrl={source_url}\r\n"));

    let mut f = std::fs::File::create(&stream)?;
    f.write_all(content.as_bytes())?;
    f.sync_all()
}

#[cfg(not(windows))]
pub fn apply(_path: &Path, _source_url: &str, _referrer: Option<&str>) -> io::Result<()> {
    // No equivalent outside Windows. Returning Ok keeps the completion path identical so the
    // engine can be exercised on a Linux CI machine.
    Ok(())
}

/// Build the marker body. Split out from the write so it is testable everywhere.
pub fn zone_identifier_content(source_url: &str, referrer: Option<&str>) -> String {
    let mut s = String::from("[ZoneTransfer]\r\nZoneId=3\r\n");
    if let Some(r) = referrer {
        s.push_str(&format!("ReferrerUrl={r}\r\n"));
    }
    s.push_str(&format!("HostUrl={source_url}\r\n"));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_the_internet_zone() {
        let c = zone_identifier_content("https://example.com/f.exe", None);
        assert!(c.starts_with("[ZoneTransfer]"));
        assert!(c.contains("ZoneId=3"), "must be the Internet zone: {c}");
        assert!(c.contains("HostUrl=https://example.com/f.exe"));
    }

    #[test]
    fn includes_the_referrer_when_known() {
        let c = zone_identifier_content(
            "https://cdn.example.com/f.exe",
            Some("https://example.com/page"),
        );
        assert!(c.contains("ReferrerUrl=https://example.com/page"));
    }

    #[test]
    fn uses_crlf_as_the_format_requires() {
        let c = zone_identifier_content("https://x/y", None);
        assert!(c.contains("\r\n"));
        assert!(!c.contains("\n\n"));
    }

    #[test]
    fn applying_is_infallible_on_a_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x.bin");
        std::fs::write(&f, b"data").unwrap();
        apply(&f, "https://example.com/x.bin", None).unwrap();
    }
}
