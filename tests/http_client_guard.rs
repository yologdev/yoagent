//! `reqwest::Client::new()` panics when the HTTP client cannot be built (a CA
//! setting that loads nothing, a host with no CA bundle), which took down the
//! task running an agent (yoyo-evolve#1138). Library code builds clients with
//! `Client::builder().build()` and returns the error: providers through
//! `provider::http_client()`, decision backends through
//! `decision::systemone::new_client()`. This fails if a `Client::new()` or
//! `Client::default()` comes back outside tests.

use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("readable dir") {
        let path = entry.expect("readable entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_panicking_http_client_outside_tests() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    let mut found = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).expect("readable source");
        // Test modules come last in this crate's files: stop at the first.
        for (i, line) in text.lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with("#[cfg(test)]") {
                break;
            }
            if code.starts_with("//") {
                continue;
            }
            if code.contains("Client::new()") || code.contains("Client::default()") {
                found.push(format!("{}:{}: {}", file.display(), i + 1, code));
            }
        }
    }
    assert!(
        found.is_empty(),
        "build HTTP clients with `Client::builder().build()` and return the error \
         (`provider::http_client()`, `decision::systemone::new_client()`):\n{}",
        found.join("\n")
    );
}
