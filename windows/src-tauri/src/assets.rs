//! Port of `AssetsManager.swift` + `AssetURLSchemeHandler.swift`.
//!
//! On-disk layout is identical to the macOS app so documents interoperate:
//!   <doc-dir>/assets/<sha256-hex>.<ext>     (saved document)
//!   %TEMP%/com.shampoo.donemd/Untitled-*/   (untitled staging)
//!
//! Runtime URLs differ by platform: WKWebView serves a true custom scheme
//! (`donemd-asset://<file>`) while Tauri on Windows maps custom protocols to
//! `http://donemd-asset.localhost/<file>`. Every URL construction goes through
//! [`asset_url`]; the Markdown on disk always stores `./assets/<file>`.

use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tauri::http::{Request, Response, StatusCode};
use tauri::{AppHandle, Manager};

use crate::state::AppState;

pub const SCHEME: &str = "donemd-asset";

/// Build the runtime URL the webview uses for an asset filename.
/// On Windows the webview reaches the protocol at `http://donemd-asset.localhost`;
/// keep the custom-scheme form elsewhere (e.g. if this code is ever reused on macOS).
pub fn asset_url(filename: &str) -> String {
    if cfg!(windows) {
        format!("http://{SCHEME}.localhost/{filename}")
    } else {
        format!("{SCHEME}://{filename}")
    }
}

/// MIME → file extension, mirroring `AssetsManager.filenameExtension(forMimeType:)`.
pub fn extension_for_mime(mime: &str) -> &'static str {
    match mime.to_ascii_lowercase().as_str() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/heic" | "image/heif" => "heic",
        "image/svg+xml" => "svg",
        "image/tiff" => "tiff",
        "image/bmp" => "bmp",
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        "video/x-m4v" => "m4v",
        "video/webm" => "webm",
        _ => "bin",
    }
}

pub fn mime_type_for_filename(filename: &str) -> &'static str {
    let ext = filename
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" | "heif" => "image/heic",
        "svg" => "image/svg+xml",
        "tiff" => "image/tiff",
        "bmp" => "image/bmp",
        "mp4" => "video/mp4",
        "mov" | "qt" => "video/quicktime",
        "m4v" => "video/x-m4v",
        "webm" => "video/webm",
        _ => "application/octet-stream",
    }
}

/// The assets directory currently in effect: next to the document when saved,
/// the untitled staging dir otherwise.
fn current_assets_dir(state: &AppState) -> PathBuf {
    let doc = state.doc.lock().unwrap();
    match &doc.file_path {
        Some(path) => path
            .parent()
            .map(|p| p.join("assets"))
            .unwrap_or_else(|| doc.staging_dir.join("assets")),
        None => doc.staging_dir.join("assets"),
    }
}

/// Import raw bytes; idempotent on identical bytes (sha256-named). Returns
/// `(storage_filename, asset_url, markdown_path)` like the Swift `ImportedImage`.
pub fn import_asset(
    state: &AppState,
    bytes: &[u8],
    mime: &str,
) -> Result<(String, String, String), String> {
    let hash = hex_sha256(bytes);
    let filename = format!("{hash}.{}", extension_for_mime(mime));
    let dir = current_assets_dir(state);
    fs::create_dir_all(&dir).map_err(|e| format!("create assets dir: {e}"))?;
    let target = dir.join(&filename);
    if !target.exists() {
        // Atomic-ish: write to a temp sibling then rename, matching the
        // `.atomic` write option on macOS.
        let tmp = dir.join(format!(".{filename}.part"));
        fs::write(&tmp, bytes).map_err(|e| format!("write {filename}: {e}"))?;
        fs::rename(&tmp, &target).map_err(|e| format!("commit {filename}: {e}"))?;
    }
    Ok((
        filename.clone(),
        asset_url(&filename),
        format!("./assets/{filename}"),
    ))
}

/// Resolve a stored asset filename to a disk path (current dir first, then the
/// staging dir — covers assets imported before the document was saved).
pub fn stored_file_path(state: &AppState, filename: &str) -> Option<PathBuf> {
    let primary = current_assets_dir(state).join(filename);
    if primary.is_file() {
        return Some(primary);
    }
    let staged = state.doc.lock().unwrap().staging_dir.join("assets").join(filename);
    if staged.is_file() {
        return Some(staged);
    }
    None
}

/// Move staged assets next to the document after its first save
/// (`AssetsManager.migrateAssets`). No-op when there is nothing staged.
pub fn migrate_assets_to_document(state: &AppState) -> Result<(), String> {
    let (from, to) = {
        let doc = state.doc.lock().unwrap();
        let Some(path) = &doc.file_path else { return Ok(()) };
        let from = doc.staging_dir.join("assets");
        let to = path
            .parent()
            .ok_or("document has no parent directory")?
            .join("assets");
        (from, to)
    };
    if !from.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(&to).map_err(|e| format!("create assets dir: {e}"))?;
    for entry in fs::read_dir(&from).map_err(|e| format!("read staging assets: {e}"))? {
        let entry = entry.map_err(|e| format!("read staging entry: {e}"))?;
        let dest = to.join(entry.file_name());
        if !dest.exists() {
            fs::rename(entry.path(), &dest)
                .or_else(|_| fs::copy(entry.path(), &dest).map(|_| ()))
                .map_err(|e| format!("migrate {:?}: {e}", entry.file_name()))?;
        }
    }
    Ok(())
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

// MARK: - URI scheme protocol

/// `register_uri_scheme_protocol` handler. The webview fetches
/// `http://donemd-asset.localhost/<filename>` (Windows mapping); we answer
/// from the document's assets dir. Supports single HTTP Range requests so
/// `<video>` can seek without loading the whole file (ADR-0009 parity).
pub fn handle_asset_request(
    ctx: tauri::UriSchemeContext<'_, tauri::Wry>,
    request: Request<Vec<u8>>,
) -> Response<Vec<u8>> {
    let app: &AppHandle = ctx.app_handle();
    let state = app.state::<AppState>();
    let filename = asset_filename_from_uri(request.uri());

    // Path-traversal guard: a malicious document could embed
    // `http://donemd-asset.localhost/..%2f..%2fsecret`; after percent-decoding
    // that becomes `../../secret` and `stored_file_path` would join it OUTSIDE
    // the document's assets dir. Real asset names are always `<sha256>.<ext>`
    // (no separators, no `..`), so rejecting anything else is zero-false-positive.
    if !is_safe_asset_filename(&filename) {
        eprintln!("[asset] rejected unsafe filename: {filename:?}");
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(format!("no asset named {filename}").into_bytes())
            .unwrap();
    }

    let Some(path) = stored_file_path(&state, &filename) else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(format!("no asset named {filename}").into_bytes())
            .unwrap();
    };

    let range_header = request
        .headers()
        .get("Range")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    respond_with_file(&path, &filename, range_header.as_deref())
}

/// Extract the asset filename from the request URI. On Windows it arrives as
/// the path component; strip query/fragment the same way
/// `AssetURLSchemeHandler.filename(from:)` does and percent-decode
/// (filenames are hex + ext today, but be safe).
fn asset_filename_from_uri(uri: &tauri::http::Uri) -> String {
    let raw = uri.path().trim_start_matches('/');
    let trimmed = raw.split(['?', '#']).next().unwrap_or(raw).trim_matches('/');
    percent_decode(trimmed)
}

/// Reject anything that isn't a plain, single-segment file name. Guards the
/// asset protocol against directory traversal: a decoded name containing a
/// path separator, a `..` segment, a leading `~`, a drive/UNC prefix, or a NUL
/// could escape the assets dir once joined. Stored names are `<sha256>.<ext>`,
/// so this never rejects a legitimate asset. Pure — unit-tested below.
fn is_safe_asset_filename(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return false;
    }
    // `.` / `..` (and any name that is only dots) never name a real asset.
    if name.chars().all(|c| c == '.') {
        return false;
    }
    // A single path component can't be absolute, but a decoded `C:foo` or a
    // leading `~` would still be surprising input — treat both as unsafe.
    if name.starts_with('~') {
        return false;
    }
    if let Some((prefix, _)) = name.split_once(':') {
        // `C:...` drive-relative form; a bare colon isn't valid on Windows anyway.
        if prefix.len() == 1 && prefix.chars().next().unwrap().is_ascii_alphabetic() {
            return false;
        }
    }
    true
}

/// Build the HTTP response for an on-disk asset. A satisfiable Range header
/// gets a 206 served via seek + bounded read — a multi-hundred-MB clip never
/// loads into memory whole (FileHandle.seek parity on macOS, #88). Anything
/// else (no header, malformed, unsatisfiable) falls through to a plain 200.
fn respond_with_file(path: &Path, filename: &str, range_header: Option<&str>) -> Response<Vec<u8>> {
    let total = match fs::metadata(path) {
        Ok(m) => m.len() as usize,
        Err(e) => return internal_error(filename, &e.to_string()),
    };
    let mime = mime_type_for_filename(filename);

    if let Some((start, end)) = range_header.and_then(|h| parse_byte_range(h, total)) {
        let slice = match read_range(path, start as u64, end - start + 1) {
            Ok(s) => s,
            Err(e) => return internal_error(filename, &e.to_string()),
        };
        return Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header("Content-Type", mime)
            .header("Content-Length", slice.len().to_string())
            .header("Content-Range", format!("bytes {start}-{end}/{total}"))
            .header("Accept-Ranges", "bytes")
            .header("Access-Control-Allow-Origin", "*")
            .body(slice)
            .unwrap();
    }

    match fs::read(path) {
        Ok(bytes) => Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", mime)
            .header("Content-Length", bytes.len().to_string())
            .header("Accept-Ranges", "bytes")
            .header("Access-Control-Allow-Origin", "*")
            .body(bytes)
            .unwrap(),
        Err(e) => internal_error(filename, &e.to_string()),
    }
}

/// Read `len` bytes starting at `offset` — the `FileHandle.seek(toOffset:)` +
/// `readData(ofLength:)` equivalent the Mac handler uses for 206 responses.
fn read_range(path: &Path, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::with_capacity(len);
    file.take(len as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

fn internal_error(filename: &str, message: &str) -> Response<Vec<u8>> {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(format!("read {filename}: {message}").into_bytes())
        .unwrap()
}

/// Port of `AssetURLSchemeHandler.parseByteRange` — single-range only,
/// `bytes=START-END` / `bytes=START-` / `bytes=-SUFFIX`, clamped; `None` for
/// malformed / multipart / unsatisfiable (caller then serves the full 200).
fn parse_byte_range(header: &str, total: usize) -> Option<(usize, usize)> {
    if total == 0 {
        return None;
    }
    let spec = header.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None; // no multipart ranges
    }
    let (start_str, end_str) = spec.split_once('-')?;
    let (start, end) = if start_str.trim().is_empty() {
        let suffix: usize = end_str.trim().parse().ok()?;
        if suffix == 0 {
            return None;
        }
        (total.saturating_sub(suffix), total - 1)
    } else {
        let s: usize = start_str.trim().parse().ok()?;
        if s >= total {
            return None;
        }
        let e = if end_str.trim().is_empty() {
            total - 1
        } else {
            end_str.trim().parse::<usize>().ok()?.min(total - 1)
        };
        (s, e)
    };
    if start > end {
        return None;
    }
    Some((start, end))
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&input[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    //! Ported from `AssetsManagerTests.swift` +
    //! `AssetURLSchemeHandlerTests.swift` (M4). The Swift URL-parsing cases map
    //! onto `asset_filename_from_uri` (Windows receives the filename as the
    //! request path, so "rejects foreign schemes" has no Windows counterpart).

    use super::*;
    use std::time::Duration;

    /// Per-test temp working dir that simulates the document's folder.
    /// Removes itself on drop so a failing assertion doesn't litter %TEMP%.
    struct Sandbox(PathBuf);

    impl Sandbox {
        fn new() -> Self {
            let dir = std::env::temp_dir()
                .join("com.shampoo.donemd")
                .join(format!("assets-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// AppState whose document "lives" at `<sandbox>/doc.md`; returns the
    /// auto-created staging dir too so the test can clean it up.
    fn state_with_doc(sandbox: &Sandbox) -> (AppState, PathBuf) {
        let state = AppState::new();
        let staging = state.doc.lock().unwrap().staging_dir.clone();
        state.doc.lock().unwrap().file_path = Some(sandbox.path().join("doc.md"));
        (state, staging)
    }

    // MARK: - parse_byte_range (#88 — <video> seek issues HTTP Range requests)

    #[test]
    fn range_closed() {
        assert_eq!(parse_byte_range("bytes=0-499", 1000), Some((0, 499)));
    }

    #[test]
    fn range_open_ended_clamps_to_last_byte() {
        assert_eq!(parse_byte_range("bytes=500-", 1000), Some((500, 999)));
    }

    #[test]
    fn range_suffix() {
        assert_eq!(parse_byte_range("bytes=-200", 1000), Some((800, 999)));
    }

    #[test]
    fn range_suffix_larger_than_file_clamps_to_start() {
        assert_eq!(parse_byte_range("bytes=-5000", 1000), Some((0, 999)));
    }

    #[test]
    fn range_end_beyond_file_clamps_to_last_byte() {
        assert_eq!(parse_byte_range("bytes=990-100000", 1000), Some((990, 999)));
    }

    #[test]
    fn range_whitespace_tolerant() {
        assert_eq!(parse_byte_range("  bytes=10-20  ", 1000), Some((10, 20)));
    }

    #[test]
    fn range_rejects_multipart() {
        assert_eq!(parse_byte_range("bytes=0-99,200-299", 1000), None);
    }

    #[test]
    fn range_rejects_missing_bytes_prefix() {
        assert_eq!(parse_byte_range("0-499", 1000), None);
        assert_eq!(parse_byte_range("items=0-499", 1000), None);
    }

    #[test]
    fn range_rejects_start_beyond_file() {
        // Unsatisfiable → None (caller serves the full 200).
        assert_eq!(parse_byte_range("bytes=1000-1500", 1000), None);
        assert_eq!(parse_byte_range("bytes=2000-", 1000), None);
    }

    #[test]
    fn range_rejects_inverted() {
        assert_eq!(parse_byte_range("bytes=500-100", 1000), None);
    }

    #[test]
    fn range_rejects_malformed_and_empty() {
        assert_eq!(parse_byte_range("bytes=", 1000), None);
        assert_eq!(parse_byte_range("bytes=abc-def", 1000), None);
        assert_eq!(parse_byte_range("bytes=-", 1000), None);
        assert_eq!(parse_byte_range("bytes=-0", 1000), None);
    }

    #[test]
    fn range_rejects_any_range_on_empty_file() {
        assert_eq!(parse_byte_range("bytes=0-0", 0), None);
    }

    // MARK: - mime type mapping

    #[test]
    fn mime_type_for_common_extensions() {
        let cases = [
            ("foo.png", "image/png"),
            ("foo.PNG", "image/png"),
            ("foo.jpg", "image/jpeg"),
            ("foo.jpeg", "image/jpeg"),
            ("foo.gif", "image/gif"),
            ("foo.webp", "image/webp"),
            ("foo.heic", "image/heic"),
            ("foo.heif", "image/heic"),
            ("foo.svg", "image/svg+xml"),
            ("foo.tiff", "image/tiff"),
            ("foo.bmp", "image/bmp"),
            // Local video (#88).
            ("foo.mp4", "video/mp4"),
            ("foo.MP4", "video/mp4"),
            ("foo.mov", "video/quicktime"),
            ("foo.qt", "video/quicktime"),
            ("foo.m4v", "video/x-m4v"),
            ("foo.webm", "video/webm"),
            ("noext", "application/octet-stream"),
            ("foo.bin", "application/octet-stream"),
        ];
        for (filename, expected) in cases {
            assert_eq!(mime_type_for_filename(filename), expected, "{filename}");
        }
    }

    #[test]
    fn mime_to_extension_mapping() {
        let cases = [
            ("image/png", "png"),
            ("IMAGE/PNG", "png"),
            ("image/jpeg", "jpg"),
            ("image/jpg", "jpg"),
            ("image/gif", "gif"),
            ("image/webp", "webp"),
            ("image/heic", "heic"),
            ("image/heif", "heic"),
            ("image/svg+xml", "svg"),
            ("application/octet-stream", "bin"),
            ("", "bin"),
        ];
        for (mime, expected) in cases {
            assert_eq!(extension_for_mime(mime), expected, "{mime}");
        }
    }

    // MARK: - request filename extraction

    #[test]
    fn filename_from_uri_plain() {
        let uri = "http://donemd-asset.localhost/abc123.png".parse().unwrap();
        assert_eq!(asset_filename_from_uri(&uri), "abc123.png");
    }

    #[test]
    fn filename_from_uri_strips_leading_slashes() {
        let uri = "http://donemd-asset.localhost///abc.png".parse().unwrap();
        assert_eq!(asset_filename_from_uri(&uri), "abc.png");
    }

    #[test]
    fn filename_from_uri_strips_query_and_fragment() {
        let uri = "http://donemd-asset.localhost/abc.png?v=2".parse().unwrap();
        assert_eq!(asset_filename_from_uri(&uri), "abc.png");
        let uri = "http://donemd-asset.localhost/abc.png#frag".parse().unwrap();
        assert_eq!(asset_filename_from_uri(&uri), "abc.png");
    }

    #[test]
    fn filename_from_uri_percent_decodes() {
        let uri = "http://donemd-asset.localhost/my%20file.png".parse().unwrap();
        assert_eq!(asset_filename_from_uri(&uri), "my file.png");
    }

    // MARK: - path-traversal guard (M9-A)

    #[test]
    fn safe_filename_accepts_real_asset_names() {
        let sha = "a".repeat(64);
        assert!(is_safe_asset_filename(&format!("{sha}.png")));
        assert!(is_safe_asset_filename(&format!("{sha}.mp4")));
        assert!(is_safe_asset_filename("my file.png")); // spaces are fine
    }

    #[test]
    fn safe_filename_rejects_traversal_and_separators() {
        // The decoded forms of `..%2f..%2fsecret`, backslash variants, etc.
        assert!(!is_safe_asset_filename("../secret"));
        assert!(!is_safe_asset_filename("..\\secret"));
        assert!(!is_safe_asset_filename("a/b.png"));
        assert!(!is_safe_asset_filename("a\\b.png"));
        assert!(!is_safe_asset_filename(".."));
        assert!(!is_safe_asset_filename("."));
        assert!(!is_safe_asset_filename(""));
        assert!(!is_safe_asset_filename("~/secret"));
        assert!(!is_safe_asset_filename("C:secret.png"));
        assert!(!is_safe_asset_filename("with\0nul.png"));
    }

    #[test]
    fn handle_request_rejects_traversal_with_404() {
        // Full request path: a `%2e%2e%2f` (../) name must 404, not read a
        // sibling file. Uses respond_with_file's sibling: we assert the guard
        // via is_safe_asset_filename since handle_asset_request needs an
        // AppHandle; the decode + guard is the security-relevant seam.
        let uri = "http://donemd-asset.localhost/..%2f..%2fsecret.md".parse().unwrap();
        let decoded = asset_filename_from_uri(&uri);
        assert_eq!(decoded, "../../secret.md");
        assert!(!is_safe_asset_filename(&decoded));
    }

    // MARK: - respond_with_file (200 / 206 behavior)

    /// Bytes 0..1000 with a non-trivial pattern so slices are checkable.
    fn sample_file(sandbox: &Sandbox, name: &str, len: usize) -> (PathBuf, Vec<u8>) {
        let bytes: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let path = sandbox.path().join(name);
        fs::write(&path, &bytes).unwrap();
        (path, bytes)
    }

    #[test]
    fn responds_full_200_without_range() {
        let sandbox = Sandbox::new();
        let (path, bytes) = sample_file(&sandbox, "clip.mp4", 1000);
        let resp = respond_with_file(&path, "clip.mp4", None);
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()["Content-Type"], "video/mp4");
        assert_eq!(resp.headers()["Accept-Ranges"], "bytes");
        assert_eq!(resp.headers()["Content-Length"], "1000");
        assert_eq!(resp.body(), &bytes);
    }

    #[test]
    fn responds_206_with_requested_slice() {
        let sandbox = Sandbox::new();
        let (path, bytes) = sample_file(&sandbox, "clip.mp4", 1000);
        let resp = respond_with_file(&path, "clip.mp4", Some("bytes=100-199"));
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(resp.headers()["Content-Range"], "bytes 100-199/1000");
        assert_eq!(resp.headers()["Content-Length"], "100");
        assert_eq!(resp.body(), &&bytes[100..=199]);
    }

    #[test]
    fn responds_206_open_ended_and_suffix() {
        let sandbox = Sandbox::new();
        let (path, bytes) = sample_file(&sandbox, "clip.mp4", 1000);

        let resp = respond_with_file(&path, "clip.mp4", Some("bytes=990-"));
        assert_eq!(resp.headers()["Content-Range"], "bytes 990-999/1000");
        assert_eq!(resp.body(), &&bytes[990..=999]);

        let resp = respond_with_file(&path, "clip.mp4", Some("bytes=-10"));
        assert_eq!(resp.headers()["Content-Range"], "bytes 990-999/1000");
        assert_eq!(resp.body(), &&bytes[990..=999]);
    }

    #[test]
    fn unsatisfiable_range_falls_back_to_full_200() {
        let sandbox = Sandbox::new();
        let (path, bytes) = sample_file(&sandbox, "clip.mp4", 1000);
        let resp = respond_with_file(&path, "clip.mp4", Some("bytes=2000-"));
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.body(), &bytes);
    }

    #[test]
    fn range_on_empty_file_falls_back_to_full_200() {
        let sandbox = Sandbox::new();
        let (path, bytes) = sample_file(&sandbox, "empty.mp4", 0);
        let resp = respond_with_file(&path, "empty.mp4", Some("bytes=0-0"));
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()["Content-Length"], "0");
        assert_eq!(resp.body(), &bytes);
    }

    // MARK: - import_asset

    #[test]
    fn import_writes_file_to_doc_adjacent_assets() {
        let sandbox = Sandbox::new();
        let (state, staging) = state_with_doc(&sandbox);

        let (name, url, md) = import_asset(&state, b"hello world", "image/png").unwrap();

        let target = sandbox.path().join("assets").join(&name);
        assert!(target.is_file());
        assert_eq!(fs::read(&target).unwrap(), b"hello world");
        assert_eq!(md, format!("./assets/{name}"));
        assert!(url.ends_with(&name), "asset_url should carry the filename");
        assert!(name.ends_with(".png"));
        assert_eq!(name.split('.').next().unwrap().len(), 64); // sha256 hex

        let _ = fs::remove_dir_all(&staging);
    }

    #[test]
    fn import_dedups_identical_bytes_without_rewrite() {
        let sandbox = Sandbox::new();
        let (state, staging) = state_with_doc(&sandbox);

        let first = import_asset(&state, b"dedup me", "image/png").unwrap();
        let written = sandbox.path().join("assets").join(&first.0);
        let first_mtime = fs::metadata(&written).unwrap().modified().unwrap();

        // Long enough for the mtime to differ if the file were rewritten.
        std::thread::sleep(Duration::from_millis(50));

        let second = import_asset(&state, b"dedup me", "image/png").unwrap();
        assert_eq!(first.0, second.0);
        let second_mtime = fs::metadata(&written).unwrap().modified().unwrap();
        assert_eq!(first_mtime, second_mtime, "identical bytes must not rewrite");

        let _ = fs::remove_dir_all(&staging);
    }

    #[test]
    fn import_creates_assets_dir_on_demand() {
        let sandbox = Sandbox::new();
        let (state, staging) = state_with_doc(&sandbox);
        assert!(!sandbox.path().join("assets").exists());

        import_asset(&state, b"x", "image/png").unwrap();

        assert!(sandbox.path().join("assets").is_dir());
        let _ = fs::remove_dir_all(&staging);
    }

    #[test]
    fn import_untitled_writes_to_staging_dir() {
        let state = AppState::new(); // no file_path → untitled
        let staging = state.doc.lock().unwrap().staging_dir.clone();

        let (name, _, _) = import_asset(&state, b"untitled", "image/jpeg").unwrap();
        assert!(name.ends_with(".jpg"));
        assert!(staging.join("assets").join(&name).is_file());
        assert_eq!(stored_file_path(&state, &name), Some(staging.join("assets").join(&name)));

        let _ = fs::remove_dir_all(&staging);
    }

    #[test]
    fn import_video_uses_video_extension() {
        let sandbox = Sandbox::new();
        let (state, staging) = state_with_doc(&sandbox);

        let (name, _, md) = import_asset(&state, b"fake mp4 bytes", "video/mp4").unwrap();
        assert!(name.ends_with(".mp4"));
        assert_eq!(md, format!("./assets/{name}"));
        assert!(sandbox.path().join("assets").join(&name).is_file());

        let (name, _, _) = import_asset(&state, b"quicktime", "video/quicktime").unwrap();
        assert!(name.ends_with(".mov"));

        let _ = fs::remove_dir_all(&staging);
    }

    // MARK: - stored_file_path

    #[test]
    fn stored_file_path_returns_none_for_unknown() {
        let sandbox = Sandbox::new();
        let (state, staging) = state_with_doc(&sandbox);
        assert_eq!(stored_file_path(&state, "does-not-exist.png"), None);
        let _ = fs::remove_dir_all(&staging);
    }

    #[test]
    fn stored_file_path_falls_back_to_staging() {
        // Imported while untitled, then file_path set without migrating:
        // the staged copy must still resolve (pre-first-save window).
        let state = AppState::new();
        let staging = state.doc.lock().unwrap().staging_dir.clone();
        let (name, _, _) = import_asset(&state, b"staged", "image/png").unwrap();

        let sandbox = Sandbox::new();
        state.doc.lock().unwrap().file_path = Some(sandbox.path().join("doc.md"));

        assert_eq!(
            stored_file_path(&state, &name),
            Some(staging.join("assets").join(&name))
        );
        let _ = fs::remove_dir_all(&staging);
    }

    // MARK: - migrate_assets_to_document

    #[test]
    fn migrate_moves_staged_assets_next_to_document() {
        let state = AppState::new();
        let staging = state.doc.lock().unwrap().staging_dir.clone();
        let (name, _, _) = import_asset(&state, b"migrate", "image/png").unwrap();
        let original = staging.join("assets").join(&name);
        assert!(original.is_file());

        let sandbox = Sandbox::new();
        state.doc.lock().unwrap().file_path = Some(sandbox.path().join("doc.md"));
        migrate_assets_to_document(&state).unwrap();

        assert!(sandbox.path().join("assets").join(&name).is_file());
        assert!(!original.exists());
        let _ = fs::remove_dir_all(&staging);
    }

    #[test]
    fn migrate_is_noop_when_nothing_staged() {
        let sandbox = Sandbox::new();
        let (state, staging) = state_with_doc(&sandbox);
        migrate_assets_to_document(&state).unwrap();
        let _ = fs::remove_dir_all(&staging);
    }

    #[test]
    fn migrate_is_noop_while_untitled() {
        let state = AppState::new();
        let staging = state.doc.lock().unwrap().staging_dir.clone();
        migrate_assets_to_document(&state).unwrap();
        let _ = fs::remove_dir_all(&staging);
    }
}
