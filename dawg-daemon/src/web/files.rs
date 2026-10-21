//! Capability-scoped file access for a single managed server.
//!
//! Browser paths never become ambient paths. A capability directory constrains
//! every operation, including when a symlink changes between validation and IO.

use super::{ApiError, AppState, run_action};
use crate::server::manager::FileOperationGuard;
use axum::{
    Json,
    body::{Body, to_bytes},
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::Response,
};
use cap_std::{ambient_authority, fs::Dir};
use dawg_core::{MAX_FRAME_BYTES, Request, Response as DaemonResponse, ServerStatus};
use http_body_util::BodyExt;
use serde::{Deserialize, Serialize};
use std::{
    io,
    path::{Component, Path as FsPath, PathBuf},
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;

const MAX_UPLOAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_LIST_ENTRIES: usize = 10_000;
const UPLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const TEMP_PREFIX: &str = ".dawg-upload-";

#[derive(Default, Deserialize)]
pub(super) struct FileQuery {
    #[serde(default)]
    pub path: String,
}

#[derive(Deserialize)]
pub(super) struct DirectoryInput {
    path: String,
}

#[derive(Deserialize)]
pub(super) struct RenameInput {
    from: String,
    to: String,
}

#[derive(Serialize)]
pub(super) struct FileEntry {
    name: String,
    path: String,
    kind: &'static str,
    size: u64,
    modified_ms: Option<u64>,
}

#[derive(Serialize)]
pub(super) struct FileListing {
    path: String,
    entries: Vec<FileEntry>,
    writable: bool,
}

pub(super) async fn list(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<FileQuery>,
) -> Result<Json<FileListing>, ApiError> {
    let server = state
        .manager
        .file_server(&id)
        .await
        .map_err(manager_error)?;
    let writable = !server.status.is_active() && server.status != ServerStatus::Installing;
    let path = normalized_path(&query.path, true)?;
    let directory = server.config.directory;
    blocking(move || list_directory(&directory, &path, writable))
        .await
        .map(Json)
}

pub(super) async fn download(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<FileQuery>,
) -> Result<Response, ApiError> {
    let server = state
        .manager
        .file_server(&id)
        .await
        .map_err(manager_error)?;
    let relative = normalized_path(&query.path, false)?;
    let filename = relative
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| bad_request("Select a regular file to download."))?
        .to_owned();
    let directory = server.config.directory;
    let (file, length) = blocking(move || open_download(&directory, &relative)).await?;
    // A running server may append logs after metadata was read. Stream only the
    // advertised prefix so the response cannot exceed its Content-Length.
    let reader = tokio::fs::File::from_std(file).take(length);
    let body = Body::from_stream(ReaderStream::new(reader));
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "attachment; filename=\"download\"; filename*=UTF-8''{}",
            encoded_filename(&filename)
        ))
        .map_err(|_| bad_request("The selected filename cannot be downloaded."))?,
    );
    Ok(response)
}

pub(super) async fn upload(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<FileQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let relative = normalized_path(&query.path, false)?;
    // The normal manager path owns the authoritative port and stopped-state
    // checks for this file. It must not be bypassed by the generic editor.
    if is_properties(&relative) {
        let bytes = tokio::time::timeout(UPLOAD_IDLE_TIMEOUT, to_bytes(body, 65_536))
            .await
            .map_err(|_| upload_timeout())?
            .map_err(|_| {
                ApiError(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "server.properties must be at most 64 KiB.".into(),
                )
            })?;
        let content = String::from_utf8(bytes.to_vec())
            .map_err(|_| bad_request("server.properties must contain UTF-8 text."))?;
        return match run_action(
            state.manager.clone(),
            Request::SaveProperties { id, content },
        )
        .await?
        {
            DaemonResponse::Ok => Ok(Json(serde_json::json!({"ok": true}))),
            DaemonResponse::Error(error) => Err(manager_error(error)),
            _ => Err(ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "The server did not save its properties.".into(),
            )),
        };
    }
    if headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > MAX_UPLOAD_BYTES)
    {
        return Err(upload_too_large());
    }
    let guard = state
        .manager
        .begin_file_operation(&id)
        .await
        .map_err(manager_error)?;
    let directory = guard.config.directory.clone();
    let (operation, mut output) = blocking(move || {
        let (pending, output) = prepare_upload(&directory, &relative)?;
        Ok((
            UploadOperation {
                pending,
                _guard: guard,
            },
            output,
        ))
    })
    .await?;
    let mut body = body;
    let mut written = 0_u64;
    loop {
        let frame = tokio::time::timeout(UPLOAD_IDLE_TIMEOUT, body.frame())
            .await
            .map_err(|_| upload_timeout())?;
        let Some(frame) = frame else {
            break;
        };
        let frame = frame.map_err(|_| {
            bad_request("The upload was interrupted. The previous file was preserved.")
        })?;
        if let Ok(chunk) = frame.into_data() {
            written = written.saturating_add(chunk.len() as u64);
            if written > MAX_UPLOAD_BYTES {
                return Err(upload_too_large());
            }
            output.write_all(&chunk).await.map_err(io_error)?;
        }
    }
    output.flush().await.map_err(io_error)?;
    output.sync_all().await.map_err(io_error)?;
    drop(output);
    // Keep the reservation inside the blocking task. A disconnected request can
    // cancel this await, but cannot let Start race a queued or ongoing rename.
    blocking(move || operation.commit()).await?;
    Ok(Json(serde_json::json!({"ok": true, "bytes": written})))
}

pub(super) async fn create_directory(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(input): Json<DirectoryInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let relative = normalized_path(&input.path, false)?;
    reject_properties_mutation(&relative)?;
    let guard = state
        .manager
        .begin_file_operation(&id)
        .await
        .map_err(manager_error)?;
    let directory = guard.config.directory.clone();
    blocking(move || {
        let _guard = guard;
        let root = open_root(&directory)?;
        let (parent, name) = open_parent(&root, &relative)?;
        parent.create_dir(name).map_err(io_error)
    })
    .await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

pub(super) async fn rename(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(input): Json<RenameInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let from = normalized_path(&input.from, false)?;
    let to = normalized_path(&input.to, false)?;
    reject_properties_mutation(&from)?;
    reject_properties_mutation(&to)?;
    let guard = state
        .manager
        .begin_file_operation(&id)
        .await
        .map_err(manager_error)?;
    if from == to {
        return Ok(Json(serde_json::json!({"ok": true})));
    }
    let directory = guard.config.directory.clone();
    blocking(move || {
        let _guard = guard;
        let root = open_root(&directory)?;
        let (from_parent, from_name) = open_parent(&root, &from)?;
        let metadata = from_parent.symlink_metadata(&from_name).map_err(io_error)?;
        if metadata.file_type().is_symlink() {
            return Err(bad_request(
                "Symbolic links can be removed, but cannot be opened or renamed.",
            ));
        }
        let (to_parent, to_name) = open_parent(&root, &to)?;
        rename_no_replace(&from_parent, &from_name, &to_parent, &to_name).map_err(io_error)
    })
    .await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

pub(super) async fn delete(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<FileQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let relative = normalized_path(&query.path, false)?;
    reject_properties_mutation(&relative)?;
    let guard = state
        .manager
        .begin_file_operation(&id)
        .await
        .map_err(manager_error)?;
    let directory = guard.config.directory.clone();
    blocking(move || {
        let _guard = guard;
        let root = open_root(&directory)?;
        let (parent, name) = open_parent(&root, &relative)?;
        let metadata = parent.symlink_metadata(&name).map_err(io_error)?;
        if metadata.file_type().is_dir() {
            parent.remove_dir_all(name).map_err(io_error)
        } else {
            // unlink removes a symlink itself; it never traverses its target.
            parent.remove_file(name).map_err(io_error)
        }
    })
    .await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

fn normalized_path(input: &str, allow_root: bool) -> Result<PathBuf, ApiError> {
    if input.len() > 4096 || input.chars().any(char::is_control) || input.contains('\\') {
        return Err(bad_request(
            "Use a relative path without control characters or backslashes.",
        ));
    }
    let mut result = PathBuf::new();
    for component in FsPath::new(input).components() {
        match component {
            Component::Normal(name) => {
                let Some(name) = name.to_str() else {
                    return Err(bad_request("Filenames must be UTF-8."));
                };
                if name.len() > 255 || name.starts_with(TEMP_PREFIX) {
                    return Err(bad_request(
                        "The filename is too long or reserved for an active upload.",
                    ));
                }
                result.push(name);
            }
            _ => {
                return Err(bad_request(
                    "Paths must stay inside the selected server directory.",
                ));
            }
        }
    }
    if result.as_os_str().is_empty() && !allow_root {
        return Err(bad_request(
            "Select a file or directory; the server root cannot be changed.",
        ));
    }
    Ok(result)
}

fn open_root(directory: &str) -> Result<Dir, ApiError> {
    Dir::open_ambient_dir(directory, ambient_authority()).map_err(io_error)
}

fn ensure_no_symlinks(root: &Dir, path: &FsPath) -> Result<(), ApiError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if root
            .symlink_metadata(&current)
            .map_err(io_error)?
            .file_type()
            .is_symlink()
        {
            return Err(ApiError(
                StatusCode::FORBIDDEN,
                "Symbolic links cannot be followed by the file manager.".into(),
            ));
        }
    }
    Ok(())
}

fn open_parent(root: &Dir, path: &FsPath) -> Result<(Dir, String), ApiError> {
    let parent = path.parent().unwrap_or_else(|| FsPath::new(""));
    ensure_no_symlinks(root, parent)?;
    let directory = if parent.as_os_str().is_empty() {
        root.try_clone()
    } else {
        root.open_dir(parent)
    }
    .map_err(io_error)?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| bad_request("Select a filename."))?
        .to_owned();
    Ok((directory, name))
}

fn list_directory(directory: &str, path: &FsPath, writable: bool) -> Result<FileListing, ApiError> {
    let root = open_root(directory)?;
    ensure_no_symlinks(&root, path)?;
    let folder = if path.as_os_str().is_empty() {
        root
    } else {
        root.open_dir(path).map_err(io_error)?
    };
    let mut entries = Vec::new();
    let display_path = path.to_string_lossy().into_owned();
    let mut response_bytes = serde_json::to_vec(&display_path)
        .map_err(|_| bad_request("Invalid directory path."))?
        .len()
        + 128;
    for entry in folder.entries().map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| bad_request("This directory contains a filename that is not UTF-8."))?;
        if name.starts_with(TEMP_PREFIX) {
            continue;
        }
        if entries.len() == MAX_LIST_ENTRIES {
            return Err(bad_request(
                "This directory has more than 10000 entries. Browse a smaller directory.",
            ));
        }
        let metadata = folder.symlink_metadata(&name).map_err(io_error)?;
        let file_type = metadata.file_type();
        let kind = if file_type.is_symlink() {
            "symlink"
        } else if file_type.is_dir() {
            "directory"
        } else if file_type.is_file() {
            "file"
        } else {
            "other"
        };
        let modified_ms = metadata
            .modified()
            .ok()
            .and_then(|time| time.into_std().duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64);
        let entry = FileEntry {
            path: path.join(&name).to_string_lossy().into_owned(),
            name,
            kind,
            size: if file_type.is_file() {
                metadata.len()
            } else {
                0
            },
            modified_ms,
        };
        response_bytes += serde_json::to_vec(&entry)
            .map_err(|_| bad_request("Invalid filename."))?
            .len()
            + 1;
        if response_bytes >= MAX_FRAME_BYTES {
            return Err(bad_request(
                "This directory listing exceeds 1 MiB. Browse a smaller directory.",
            ));
        }
        entries.push(entry);
    }
    entries.sort_by(|left, right| {
        (left.kind != "directory")
            .cmp(&(right.kind != "directory"))
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(FileListing {
        path: display_path,
        entries,
        writable,
    })
}

fn open_download(directory: &str, path: &FsPath) -> Result<(std::fs::File, u64), ApiError> {
    let root = open_root(directory)?;
    ensure_no_symlinks(&root, path)?;
    let metadata = root.symlink_metadata(path).map_err(io_error)?;
    if !metadata.is_file() {
        return Err(bad_request("Only regular files can be downloaded."));
    }
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        // Recheck the opened descriptor below. NONBLOCK also prevents a file
        // replaced with a FIFO from holding a blocking worker indefinitely.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = root.open_with(path, &options).map_err(io_error)?;
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.is_file() {
        return Err(bad_request("Only regular files can be downloaded."));
    }
    Ok((file.into_std(), metadata.len()))
}

struct PendingUpload {
    parent: Dir,
    temporary: String,
    destination: String,
    committed: bool,
}

struct UploadOperation {
    // Field order ensures an aborted upload removes its temporary file before
    // releasing the manager reservation.
    pending: PendingUpload,
    _guard: FileOperationGuard,
}

impl UploadOperation {
    fn commit(self) -> Result<(), ApiError> {
        self.pending.commit()
    }
}

impl PendingUpload {
    fn commit(mut self) -> Result<(), ApiError> {
        if let Ok(metadata) = self.parent.symlink_metadata(&self.destination)
            && !metadata.is_file()
        {
            return Err(bad_request(
                "A regular file is required at the upload destination.",
            ));
        }
        self.parent
            .rename(&self.temporary, &self.parent, &self.destination)
            .map_err(io_error)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for PendingUpload {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.parent.remove_file(&self.temporary);
        }
    }
}

fn prepare_upload(
    directory: &str,
    path: &FsPath,
) -> Result<(PendingUpload, tokio::fs::File), ApiError> {
    let root = open_root(directory)?;
    let (parent, destination) = open_parent(&root, path)?;
    match parent.symlink_metadata(&destination) {
        Ok(metadata) if !metadata.is_file() => {
            return Err(bad_request(
                "Choose a regular file, not a directory or symbolic link.",
            ));
        }
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(io_error(error)),
        _ => {}
    }
    let temporary = format!("{TEMP_PREFIX}{}.part", uuid::Uuid::new_v4());
    let mut options = cap_std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let output = parent.open_with(&temporary, &options).map_err(io_error)?;
    let pending = PendingUpload {
        parent,
        temporary,
        destination,
        committed: false,
    };
    Ok((pending, tokio::fs::File::from_std(output.into_std())))
}

#[cfg(target_os = "linux")]
fn rename_no_replace(from: &Dir, from_name: &str, to: &Dir, to_name: &str) -> io::Result<()> {
    use std::{ffi::CString, os::fd::AsRawFd};
    let from_name =
        CString::new(from_name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let to_name =
        CString::new(to_name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // Both descriptors are already capabilities and names are single validated
    // components. NOREPLACE prevents data loss if a destination appears now.
    let result = unsafe {
        libc::renameat2(
            from.as_raw_fd(),
            from_name.as_ptr(),
            to.as_raw_fd(),
            to_name.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
fn rename_no_replace(_from: &Dir, _from_name: &str, _to: &Dir, _to_name: &str) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Atomic rename without replacement is unavailable on this platform",
    ))
}

fn is_properties(path: &FsPath) -> bool {
    path == FsPath::new("server.properties")
}

fn reject_properties_mutation(path: &FsPath) -> Result<(), ApiError> {
    if is_properties(path) {
        Err(bad_request(
            "Edit server.properties through its editor so Dawg can validate the server port.",
        ))
    } else {
        Ok(())
    }
}

async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(operation).await.map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "The file operation could not complete.".into(),
        )
    })?
}

fn manager_error(message: String) -> ApiError {
    let status = if message == "Server not found" {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::CONFLICT
    };
    ApiError(status, message)
}

fn io_error(error: io::Error) -> ApiError {
    let status = match error.kind() {
        io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
        io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
        io::ErrorKind::AlreadyExists => StatusCode::CONFLICT,
        io::ErrorKind::InvalidInput => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    ApiError(status, format!("File operation failed: {error}"))
}

fn bad_request(message: &str) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, message.into())
}
fn upload_too_large() -> ApiError {
    ApiError(
        StatusCode::PAYLOAD_TOO_LARGE,
        "Uploads are limited to 2 GiB. The previous file was preserved.".into(),
    )
}
fn upload_timeout() -> ApiError {
    ApiError(
        StatusCode::REQUEST_TIMEOUT,
        "The upload stopped sending data for 30 seconds.".into(),
    )
}

fn encoded_filename(name: &str) -> String {
    let mut output = String::new();
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            output.push(char::from(byte));
        } else {
            use std::fmt::Write;
            let _ = write!(output, "%{byte:02X}");
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        base: PathBuf,
        root: PathBuf,
        outside: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let base = std::env::temp_dir().join(format!("dawg-files-{}", uuid::Uuid::new_v4()));
            let root = base.join("server");
            let outside = base.join("outside");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("private"), b"must survive").unwrap();
            Self {
                base,
                root,
                outside,
            }
        }
        fn directory(&self) -> &str {
            self.root.to_str().unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn paths_cannot_escape_or_target_the_server_root() {
        for path in [
            "../secret",
            "/etc/passwd",
            "a/../../secret",
            "a\\..\\secret",
            "file\0name",
            "./server.properties",
            ".dawg-upload-other.part",
        ] {
            assert!(normalized_path(path, false).is_err(), "{path:?}");
        }
        assert!(normalized_path("", true).unwrap().as_os_str().is_empty());
        assert!(normalized_path("", false).is_err());
        assert_eq!(
            normalized_path("world/region/a.mca", false).unwrap(),
            PathBuf::from("world/region/a.mca")
        );
        assert!(is_properties(
            &normalized_path("server.properties", false).unwrap()
        ));
        assert!(normalized_path("folder/../server.properties", false).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn symlink_parents_and_external_downloads_are_rejected() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        symlink(&fixture.outside, fixture.root.join("escape")).unwrap();
        symlink(fixture.outside.join("private"), fixture.root.join("link")).unwrap();
        assert!(open_download(fixture.directory(), FsPath::new("escape/private")).is_err());
        assert!(open_download(fixture.directory(), FsPath::new("link")).is_err());
        assert!(prepare_upload(fixture.directory(), FsPath::new("escape/changed")).is_err());
        let listing = list_directory(fixture.directory(), FsPath::new(""), true).unwrap();
        assert!(listing.entries.iter().all(|entry| entry.kind == "symlink"));
        assert_eq!(
            std::fs::read(fixture.outside.join("private")).unwrap(),
            b"must survive"
        );
    }

    #[tokio::test]
    async fn aborted_upload_preserves_existing_file_and_removes_partial() {
        let fixture = Fixture::new();
        std::fs::write(fixture.root.join("server.jar"), b"old jar").unwrap();
        let (pending, mut output) =
            prepare_upload(fixture.directory(), FsPath::new("server.jar")).unwrap();
        output.write_all(b"partial new jar").await.unwrap();
        drop(output);
        drop(pending);
        assert_eq!(
            std::fs::read(fixture.root.join("server.jar")).unwrap(),
            b"old jar"
        );
        assert_eq!(std::fs::read_dir(&fixture.root).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn completed_upload_is_atomic_and_regular_file_download_is_streamable() {
        let fixture = Fixture::new();
        std::fs::write(fixture.root.join("notes.txt"), b"old").unwrap();
        let (pending, mut output) =
            prepare_upload(fixture.directory(), FsPath::new("notes.txt")).unwrap();
        output.write_all(b"new bytes").await.unwrap();
        output.flush().await.unwrap();
        drop(output);
        pending.commit().unwrap();
        let (mut file, length) =
            open_download(fixture.directory(), FsPath::new("notes.txt")).unwrap();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut bytes).unwrap();
        assert_eq!(length, 9);
        assert_eq!(bytes, b"new bytes");
        assert_eq!(std::fs::read_dir(&fixture.root).unwrap().count(), 1);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn renaming_never_overwrites_an_existing_destination() {
        let fixture = Fixture::new();
        std::fs::write(fixture.root.join("one"), b"one").unwrap();
        std::fs::write(fixture.root.join("two"), b"two").unwrap();
        let root = open_root(fixture.directory()).unwrap();
        assert!(rename_no_replace(&root, "one", &root, "two").is_err());
        assert_eq!(std::fs::read(fixture.root.join("two")).unwrap(), b"two");
        rename_no_replace(&root, "one", &root, "three").unwrap();
        assert!(!fixture.root.join("one").exists());
        assert_eq!(std::fs::read(fixture.root.join("three")).unwrap(), b"one");
    }

    #[test]
    fn protected_properties_cannot_be_renamed_or_deleted() {
        assert!(reject_properties_mutation(FsPath::new("server.properties")).is_err());
        assert!(reject_properties_mutation(FsPath::new("world/server.properties")).is_ok());
        assert_eq!(
            encoded_filename("world \"name\".zip"),
            "world%20%22name%22.zip"
        );
    }
}
