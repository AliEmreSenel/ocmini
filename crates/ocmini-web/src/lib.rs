//! Browser interface for `ocmini serve`.
//!
//! The page is embedded in the binary and talks to the existing v2 HTTP/SSE
//! routes for sessions, prompts, approvals and events. The additions are a few
//! file routes for the editor, plus settings and sandbox selection
//! ([`settings`]). File routes run as the server's OS user, like the terminal
//! client's operator, and reuse the tools' atomic write and per-file edit locks
//! so a browser save serializes with agent edits to the same file.
use axum::{
    body::Body,
    extract::Query,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use futures::StreamExt;
use ocmini_core::tools::{changes, files};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

mod settings;

const INDEX: &str = include_str!("../assets/index.html");
const SCRIPT: &str = include_str!("../assets/app.js");
const STYLE: &str = include_str!("../assets/app.css");
/// Bytes inspected to tell text from binary before streaming the rest.
const SNIFF: usize = 8192;

/// One ETag covers all assets; they only change together with the binary.
static ETAG: LazyLock<HeaderValue> = LazyLock::new(|| {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in [INDEX, SCRIPT, STYLE].iter().flat_map(|s| s.bytes()) {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
    }
    HeaderValue::from_str(&format!("\"{hash:016x}\"")).expect("ASCII ETag")
});

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data: blob:; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// Routes for the page and its APIs. Merge them into the server router before
/// its layers so they share the password gate and body limit.
pub fn router() -> Router<ocmini_server::server::AppState> {
    files().merge(settings::router())
}

/// The page and its file API, which need no server state.
fn files<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/", get(|h| asset(h, INDEX, "text/html; charset=utf-8")))
        .route(
            "/web/app.js",
            get(|h| asset(h, SCRIPT, "text/javascript; charset=utf-8")),
        )
        .route(
            "/web/app.css",
            get(|h| asset(h, STYLE, "text/css; charset=utf-8")),
        )
        .route("/web/api/info", get(info))
        .route("/web/api/fs/list", get(list))
        .route("/web/api/fs/read", get(read))
        .route("/web/api/fs/write", put(write))
        .route("/web/api/fs/mkdir", post(mkdir))
        .route("/web/api/fs/rename", post(rename))
        .route("/web/api/fs/delete", post(remove))
}

async fn asset(headers: HeaderMap, body: &'static str, kind: &'static str) -> Response {
    let mut response = if headers
        .get(header::IF_NONE_MATCH)
        .is_some_and(|tag| tag == *ETAG)
    {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        ([(header::CONTENT_TYPE, kind)], body).into_response()
    };
    let headers = response.headers_mut();
    headers.insert(header::ETAG, ETAG.clone());
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    response
}

struct WebError {
    status: StatusCode,
    tag: &'static str,
    message: String,
    extra: Value,
}
type Result<T> = std::result::Result<T, WebError>;

impl WebError {
    fn new(status: StatusCode, tag: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            tag,
            message: message.into(),
            extra: json!({}),
        }
    }
    fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "InvalidRequestError", message)
    }
    fn io(path: &Path, error: std::io::Error) -> Self {
        let status = match error.kind() {
            std::io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
            std::io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
            std::io::ErrorKind::AlreadyExists => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self::new(
            status,
            "FileSystemError",
            format!("{}: {error}", path.display()),
        )
    }
}
impl From<String> for WebError {
    fn from(message: String) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "FileSystemError",
            message,
        )
    }
}
impl From<ocmini_core::tools::types::OperationError> for WebError {
    fn from(error: ocmini_core::tools::types::OperationError) -> Self {
        Self::invalid(error.message)
    }
}
impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        let mut body = json!({"_tag": self.tag, "message": self.message});
        if let (Some(body), Some(extra)) = (body.as_object_mut(), self.extra.as_object()) {
            body.extend(extra.clone());
        }
        (self.status, Json(body)).into_response()
    }
}

fn absolute(path: &str) -> Result<PathBuf> {
    if path.is_empty() || path.contains('\0') {
        return Err(WebError::invalid("path must be a nonempty string"));
    }
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(WebError::invalid("path must be absolute"));
    }
    Ok(path)
}

/// Modification time and length identify the revision a client loaded, so a
/// save can refuse to overwrite a change made by an agent or another editor.
fn version(meta: &std::fs::Metadata) -> String {
    let modified = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |time| time.as_nanos());
    format!("{modified:x}-{:x}", meta.len())
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| WebError::from(error.to_string()))?
}

/// Takes the same in-process path locks as the agent's file tools.
async fn locked(paths: &[PathBuf]) -> Result<Vec<tokio::sync::OwnedMutexGuard<()>>> {
    Ok(changes::coordinate_commit(paths, &CancellationToken::new()).await?)
}

async fn info() -> Json<Value> {
    let cwd = std::env::current_dir()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|path| path.to_string_lossy().into_owned());
    Json(json!({
        "version": ocmini_core::OPENCODE_COMPAT_VERSION,
        "cwd": cwd,
        "home": home,
        "separator": std::path::MAIN_SEPARATOR_STR,
    }))
}

#[derive(Deserialize)]
struct PathQuery {
    path: String,
    #[serde(default)]
    raw: bool,
}

async fn list(Query(query): Query<PathQuery>) -> Result<Json<Value>> {
    let path = absolute(&query.path)?;
    blocking(move || {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&path).map_err(|e| WebError::io(&path, e))? {
            let entry = entry.map_err(|e| WebError::io(&path, e))?;
            let link = entry.file_type().is_ok_and(|kind| kind.is_symlink());
            // Follow links to show what they open; a dangling link lists as a file.
            let meta = std::fs::metadata(entry.path()).ok();
            let dir = meta.as_ref().is_some_and(std::fs::Metadata::is_dir);
            let mut item = json!({
                "name": entry.file_name().to_string_lossy(),
                "path": entry.path().to_string_lossy(),
                "type": if dir { "directory" } else { "file" },
            });
            if link {
                item["link"] = json!(true);
            }
            if let Some(meta) = meta.filter(|meta| meta.is_file()) {
                item["size"] = json!(meta.len());
            }
            entries.push((
                dir,
                entry.file_name().to_string_lossy().to_lowercase(),
                item,
            ));
        }
        entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let parent = path.parent().map(|p| p.to_string_lossy().into_owned());
        Ok(Json(json!({
            "path": path.to_string_lossy(),
            "parent": parent,
            "entries": entries.into_iter().map(|e| e.2).collect::<Vec<_>>(),
        })))
    })
    .await
}

fn binary(prefix: &[u8]) -> bool {
    prefix.contains(&0)
        || std::str::from_utf8(prefix).is_err_and(|error| error.error_len().is_some())
}

/// Streams a file so a large one is not buffered in server memory. Binary
/// files are refused unless `raw` is set, since the editor only handles text.
/// `If-None-Match` with the loaded version answers 304 without reading.
async fn read(headers: HeaderMap, Query(query): Query<PathQuery>) -> Result<Response> {
    let path = absolute(&query.path)?;
    let mut file = tokio::fs::File::open(&path)
        .await
        .map_err(|e| WebError::io(&path, e))?;
    let meta = file.metadata().await.map_err(|e| WebError::io(&path, e))?;
    if !meta.is_file() {
        return Err(WebError::invalid(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let revision = HeaderValue::from_str(&version(&meta)).expect("ASCII version");
    if headers
        .get(header::IF_NONE_MATCH)
        .is_some_and(|tag| tag == revision)
    {
        return Ok((
            StatusCode::NOT_MODIFIED,
            [(
                header::HeaderName::from_static("x-ocmini-version"),
                revision,
            )],
        )
            .into_response());
    }
    let mut prefix = vec![0; SNIFF.min(usize::try_from(meta.len()).unwrap_or(SNIFF))];
    let mut filled = 0;
    while filled < prefix.len() {
        match file.read(&mut prefix[filled..]).await {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(error) => return Err(WebError::io(&path, error)),
        }
    }
    prefix.truncate(filled);
    if !query.raw && binary(&prefix) {
        let mut error = WebError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "BinaryFileError",
            format!("{} is not a text file", path.display()),
        );
        error.extra = json!({"size": meta.len()});
        return Err(error);
    }
    let body = futures::stream::once(async move { Ok(prefix.into()) })
        .chain(tokio_util::io::ReaderStream::new(file));
    let kind = if query.raw {
        "application/octet-stream"
    } else {
        "text/plain; charset=utf-8"
    };
    let mut response = (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(kind)),
            (
                header::HeaderName::from_static("x-ocmini-version"),
                revision,
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        Body::from_stream(body),
    )
        .into_response();
    if query.raw {
        response.headers_mut().insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("sandbox"),
        );
        response.headers_mut().insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        response.headers_mut().insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment"),
        );
    }
    Ok(response)
}

#[derive(Deserialize)]
struct WriteBody {
    path: String,
    content: String,
    /// Revision the client loaded; omitted to overwrite unconditionally.
    version: Option<String>,
    /// Refuse to replace an existing file.
    #[serde(default)]
    create: bool,
}

async fn write(Json(body): Json<WriteBody>) -> Result<Json<Value>> {
    let target = changes::target(&absolute(&body.path)?)?;
    let guards = locked(std::slice::from_ref(&target)).await?;
    let result = blocking(move || {
        let current = match std::fs::metadata(&target) {
            Ok(meta) if meta.is_file() => Some(version(&meta)),
            Ok(_) => {
                return Err(WebError::invalid(format!(
                    "{} is not a regular file",
                    target.display()
                )))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(WebError::io(&target, error)),
        };
        if body.create && current.is_some() {
            return Err(WebError::new(
                StatusCode::CONFLICT,
                "FileExistsError",
                format!("{} already exists", target.display()),
            ));
        }
        if body.version.is_some() && body.version != current {
            let mut error = WebError::new(
                StatusCode::CONFLICT,
                "FileChangedError",
                format!("{} changed since it was opened", target.display()),
            );
            error.extra = json!({"version": current});
            return Err(error);
        }
        files::atomic_write(&target, body.content.as_bytes())?;
        let meta = std::fs::metadata(&target).map_err(|e| WebError::io(&target, e))?;
        Ok(Json(json!({
            "path": target.to_string_lossy(),
            "version": version(&meta),
        })))
    })
    .await;
    drop(guards);
    result
}

#[derive(Deserialize)]
struct PathBody {
    path: String,
}

async fn mkdir(Json(body): Json<PathBody>) -> Result<StatusCode> {
    let path = absolute(&body.path)?;
    blocking(move || {
        std::fs::create_dir_all(&path).map_err(|e| WebError::io(&path, e))?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

#[derive(Deserialize)]
struct RenameBody {
    from: String,
    to: String,
}

async fn rename(Json(body): Json<RenameBody>) -> Result<StatusCode> {
    let from = changes::target(&absolute(&body.from)?)?;
    let to = changes::target(&absolute(&body.to)?)?;
    let guards = locked(&[from.clone(), to.clone()]).await?;
    let result = blocking(move || {
        // std::fs::rename replaces an existing file; the browser asks for a move.
        if to.symlink_metadata().is_ok() {
            return Err(WebError::new(
                StatusCode::CONFLICT,
                "FileExistsError",
                format!("{} already exists", to.display()),
            ));
        }
        std::fs::rename(&from, &to).map_err(|e| WebError::io(&from, e))?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await;
    drop(guards);
    result
}

/// Deletes a file, a link (not its target) or a directory tree.
async fn remove(Json(body): Json<PathBody>) -> Result<StatusCode> {
    let path = absolute(&body.path)?;
    let guards = locked(std::slice::from_ref(&changes::target(&path)?)).await?;
    let result = blocking(move || {
        let meta = path
            .symlink_metadata()
            .map_err(|e| WebError::io(&path, e))?;
        if meta.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        }
        .map_err(|e| WebError::io(&path, e))?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await;
    drop(guards);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn call(method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Vec<u8>) {
        let mut request = Request::builder().method(method).uri(uri);
        let body = match body {
            Some(body) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        let response = files::<()>()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        (
            status,
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
    }
    fn query(path: &Path) -> String {
        let mut encoded = String::new();
        for byte in path.to_string_lossy().bytes() {
            if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
                encoded.push(byte as char);
            } else {
                encoded.push_str(&format!("%{byte:02X}"));
            }
        }
        encoded
    }

    #[tokio::test]
    async fn raw_download_is_sandboxed_and_editor_text_is_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("test.html");
        std::fs::write(&file, "<script>alert(1)</script>").unwrap();
        for raw in [false, true] {
            let uri = format!("/web/api/fs/read?path={}&raw={raw}", query(&file));
            let response = files::<()>()
                .oneshot(Request::get(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            if raw {
                assert_eq!(
                    response.headers()[header::CONTENT_SECURITY_POLICY],
                    "sandbox"
                );
                assert_eq!(
                    response.headers()[header::X_CONTENT_TYPE_OPTIONS],
                    "nosniff"
                );
                assert_eq!(
                    response.headers()[header::CONTENT_DISPOSITION],
                    "attachment"
                );
            } else {
                assert_eq!(
                    response.headers()[header::CONTENT_TYPE],
                    "text/plain; charset=utf-8"
                );
                assert!(!response.headers().contains_key(header::CONTENT_DISPOSITION));
            }
            assert_eq!(
                to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap()
                    .as_ref(),
                b"<script>alert(1)</script>"
            );
        }
    }

    #[tokio::test]
    async fn page_is_served_with_a_restrictive_policy() {
        let response = files::<()>()
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let tag = response.headers()[header::ETAG].clone();
        assert!(response.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'"));
        let cached = files::<()>()
            .oneshot(
                Request::get("/web/app.js")
                    .header(header::IF_NONE_MATCH, tag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cached.status(), StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn saves_refuse_to_overwrite_a_newer_revision() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("nested").join("main.rs");
        let path = file.to_string_lossy().to_string();
        let (status, body) = call(
            "PUT",
            "/web/api/fs/write",
            Some(json!({"path": path, "content": "one", "create": true})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let loaded: Value = serde_json::from_slice(&body).unwrap();
        let (status, _) = call(
            "PUT",
            "/web/api/fs/write",
            Some(json!({"path": path, "content": "two", "create": true})),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        std::fs::write(&file, "external change").unwrap();
        let (status, body) = call(
            "PUT",
            "/web/api/fs/write",
            Some(json!({"path": path, "content": "stale", "version": loaded["version"]})),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "external change");
        let current: Value = serde_json::from_slice(&body).unwrap();
        let (status, _) = call(
            "PUT",
            "/web/api/fs/write",
            Some(json!({"path": path, "content": "merged", "version": current["version"]})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = call(
            "GET",
            &format!("/web/api/fs/read?path={}", query(&file)),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"merged");
    }

    #[tokio::test]
    async fn listing_reading_and_tree_operations() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("b.txt"), "text").unwrap();
        std::fs::write(root.path().join("image.bin"), [0u8, 1, 2]).unwrap();
        std::fs::create_dir(root.path().join("a")).unwrap();
        let (status, body) = call(
            "GET",
            &format!("/web/api/fs/list?path={}", query(root.path())),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let listing: Value = serde_json::from_slice(&body).unwrap();
        let names: Vec<_> = listing["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["a", "b.txt", "image.bin"]);
        let (status, _) = call(
            "GET",
            &format!(
                "/web/api/fs/read?path={}",
                query(&root.path().join("image.bin"))
            ),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let (status, _) = call("GET", "/web/api/fs/list?path=relative", None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let moved = root.path().join("a").join("c.txt");
        let (status, _) = call(
            "POST",
            "/web/api/fs/rename",
            Some(json!({"from": root.path().join("b.txt"), "to": root.path().join("image.bin")})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "a move must not replace a file"
        );
        let (status, _) = call(
            "POST",
            "/web/api/fs/rename",
            Some(json!({"from": root.path().join("b.txt"), "to": moved})),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(std::fs::read_to_string(&moved).unwrap(), "text");
        let (status, _) = call(
            "POST",
            "/web/api/fs/mkdir",
            Some(json!({"path": root.path().join("x").join("y")})),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = call(
            "POST",
            "/web/api/fs/delete",
            Some(json!({"path": root.path().join("a")})),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(!root.path().join("a").exists());
    }
}
