use std::{
    collections::HashMap,
    net::SocketAddr,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart, Path as AxumPath, State, multipart::Field},
    http::{
        StatusCode,
        header::{
            AUTHORIZATION, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE, HeaderMap,
            HeaderValue, SET_COOKIE, WWW_AUTHENTICATE,
        },
    },
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post, put},
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE_NO_PAD},
};
use chrono::{DateTime, Utc};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{fs, io::AsyncWriteExt, sync::RwLock};
use tower_http::trace::TraceLayer;

const DEFAULT_ADMIN: &str = "admin";
const DEFAULT_PASSWORD: &str = "admin";
const DEFAULT_UPLOAD_LIMIT_BYTES: usize = 10 * 1024 * 1024 * 1024;

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::from_env()?;
    fs::create_dir_all(&config.files_dir).await?;
    fs::create_dir_all(&config.data_dir).await?;
    fs::create_dir_all(config.files_dir.join("public")).await?;

    let store = Store::load(config.data_dir.join("state.json")).await?;
    let audit_logs = load_audit_logs(config.data_dir.join("audit.json")).await?;
    let fresh_setup = store.fresh_setup;
    let state = AppState {
        config,
        store: Arc::new(RwLock::new(store)),
        audit_logs: Arc::new(RwLock::new(audit_logs)),
    };

    let upload_limit_bytes = state.config.upload_limit_bytes;
    let app = Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/api/session", get(api_session))
        .route("/api/login", post(api_login))
        .route("/api/logout", post(api_logout))
        .route(
            "/api/files",
            get(api_files).put(api_upload_root).post(api_mkdir_root),
        )
        .route(
            "/api/files/*path",
            get(api_download)
                .put(api_upload)
                .post(api_mkdir)
                .delete(api_delete),
        )
        .route("/api/admin/users", get(api_users).post(api_create_user))
        .route("/api/admin/audit", get(api_audit_logs))
        .route(
            "/api/admin/users/:username",
            put(api_update_user).delete(api_delete_user),
        )
        .route(
            "/api/admin/public/*path",
            put(api_publish).delete(api_unpublish),
        )
        .route("/api/grants/*path", put(api_grant_file))
        .route("/public/*path", get(public_download))
        .route("/p/*path", get(public_download))
        .layer(DefaultBodyLimit::max(upload_limit_bytes))
        .layer(TraceLayer::new_for_http())
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(state.config.bind).await?;
    println!("Remote File is running at http://{}", state.config.bind);
    if fresh_setup {
        println!("Default admin: {} / {}", DEFAULT_ADMIN, DEFAULT_PASSWORD);
    }
    axum::serve(listener, app).await?;
    Ok(())
}

#[derive(Clone)]
struct Config {
    bind: SocketAddr,
    files_dir: PathBuf,
    data_dir: PathBuf,
    upload_limit_bytes: usize,
}

impl Config {
    fn from_env() -> Result<Self> {
        let bind = std::env::var("REMOTE_FILE_BIND")
            .unwrap_or_else(|_| "127.0.0.1:8080".to_string())
            .parse()
            .context("REMOTE_FILE_BIND must be a socket address")?;
        let files_dir = std::env::var("REMOTE_FILE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("storage/files"));
        let data_dir = std::env::var("REMOTE_FILE_DATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("storage/meta"));
        let upload_limit_bytes = std::env::var("REMOTE_FILE_UPLOAD_LIMIT_BYTES")
            .ok()
            .map(|value| parse_upload_limit(&value))
            .transpose()?
            .unwrap_or(DEFAULT_UPLOAD_LIMIT_BYTES);
        Ok(Self {
            bind,
            files_dir,
            data_dir,
            upload_limit_bytes,
        })
    }
}

#[derive(Clone)]
struct AppState {
    config: Config,
    store: Arc<RwLock<Store>>,
    audit_logs: Arc<RwLock<Vec<AuditLog>>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Store {
    users: HashMap<String, User>,
    sessions: HashMap<String, Session>,
    public_files: HashMap<String, PublicFile>,
    // Legacy state from the old copy-based publish flow. New publishes store
    // the source path directly in public_files and leave this map empty.
    #[serde(default)]
    public_copies: HashMap<String, String>,
    #[serde(default)]
    file_grants: HashMap<String, FileGrant>,
    #[serde(default)]
    download_counts: HashMap<String, u64>,
    #[serde(default)]
    fresh_setup: bool,
    #[serde(skip)]
    path: PathBuf,
}

impl Store {
    async fn load(path: PathBuf) -> Result<Self> {
        if fs::try_exists(&path).await? {
            let bytes = fs::read(&path).await?;
            let mut store: Store = serde_json::from_slice(&bytes)?;
            store.path = path;
            Ok(store)
        } else {
            let mut users = HashMap::new();
            users.insert(
                DEFAULT_ADMIN.to_string(),
                User::new(DEFAULT_ADMIN, DEFAULT_PASSWORD, Role::Admin),
            );
            let store = Store {
                users,
                sessions: HashMap::new(),
                public_files: HashMap::new(),
                public_copies: HashMap::new(),
                file_grants: HashMap::new(),
                download_counts: HashMap::new(),
                fresh_setup: true,
                path,
            };
            store.save().await?;
            Ok(store)
        }
    }

    async fn save(&self) -> Result<()> {
        let data = serde_json::to_vec_pretty(self)?;
        fs::write(&self.path, data).await?;
        Ok(())
    }
}

async fn load_audit_logs(path: PathBuf) -> Result<Vec<AuditLog>> {
    if fs::try_exists(&path).await? {
        let bytes = fs::read(&path).await?;
        Ok(serde_json::from_slice(&bytes).unwrap_or_default())
    } else {
        Ok(Vec::new())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct User {
    username: String,
    password_hash: String,
    role: Role,
    permissions: Permissions,
    created_at: DateTime<Utc>,
}

impl User {
    fn new(username: &str, password: &str, role: Role) -> Self {
        let mut permissions = Permissions {
            roots: vec!["/".to_string()],
            can_upload: true,
            can_delete: matches!(role, Role::Admin),
            can_publish: matches!(role, Role::Admin),
        };
        if matches!(role, Role::Admin) {
            permissions.can_delete = true;
            permissions.can_publish = true;
        }
        Self {
            username: username.to_string(),
            password_hash: hash_password(password),
            role,
            permissions,
            created_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Role {
    Admin,
    User,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Permissions {
    roots: Vec<String>,
    can_upload: bool,
    can_delete: bool,
    can_publish: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Session {
    username: String,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PublicFile {
    path: String,
    published_by: String,
    published_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct FileGrant {
    path: String,
    users: Vec<String>,
    updated_by: String,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuditLog {
    at: DateTime<Utc>,
    username: Option<String>,
    action: String,
    path: Option<String>,
    status: String,
    detail: String,
    ip: Option<String>,
}

#[derive(Debug, Serialize)]
struct SessionView {
    authenticated: bool,
    user: Option<UserView>,
    fresh_setup: bool,
}

#[derive(Debug, Serialize)]
struct UserView {
    username: String,
    role: Role,
    permissions: Permissions,
    created_at: DateTime<Utc>,
}

impl From<&User> for UserView {
    fn from(user: &User) -> Self {
        Self {
            username: user.username.clone(),
            role: user.role,
            permissions: user.permissions.clone(),
            created_at: user.created_at,
        }
    }
}

#[derive(Debug, Serialize)]
struct FileList {
    path: String,
    entries: Vec<FileEntry>,
}

#[derive(Debug, Serialize)]
struct FileEntry {
    name: String,
    path: String,
    kind: FileKind,
    size: u64,
    modified: Option<DateTime<Utc>>,
    download_url: String,
    download_count: u64,
    public: bool,
    public_url: Option<String>,
    granted_users: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum FileKind {
    File,
    Directory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UploadConflict {
    Error,
    Overwrite,
    Rename,
}

impl UploadConflict {
    fn from_query(query: &HashMap<String, String>) -> Self {
        match query.get("conflict").map(String::as_str) {
            Some("overwrite") => Self::Overwrite,
            Some("rename") => Self::Rename,
            _ => Self::Error,
        }
    }
}

#[derive(Debug, Deserialize)]
struct LoginPayload {
    username: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct UserPayload {
    username: String,
    password: Option<String>,
    role: Role,
    roots: Vec<String>,
    can_upload: bool,
    can_delete: bool,
    can_publish: bool,
}

#[derive(Debug, Deserialize)]
struct GrantPayload {
    users: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct MkdirPayload {
    name: String,
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn app_js() -> Response {
    (
        [(
            CONTENT_TYPE,
            HeaderValue::from_static("application/javascript"),
        )],
        APP_JS,
    )
        .into_response()
}

async fn api_session(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let fresh_setup = state.store.read().await.fresh_setup;
    match current_user(&state, &headers).await {
        Some(user) => Json(SessionView {
            authenticated: true,
            user: Some(UserView::from(&user)),
            fresh_setup,
        }),
        None => Json(SessionView {
            authenticated: false,
            user: None,
            fresh_setup,
        }),
    }
}

async fn api_login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<LoginPayload>,
) -> impl IntoResponse {
    let mut store = state.store.write().await;
    let Some(user) = store.users.get(&payload.username) else {
        drop(store);
        record_audit(
            &state,
            &headers,
            Some(payload.username),
            "login",
            None,
            StatusCode::UNAUTHORIZED,
            "用户名不存在",
        )
        .await;
        return error(StatusCode::UNAUTHORIZED, "用户名或密码错误");
    };
    if !verify_password(&payload.password, &user.password_hash) {
        let username = user.username.clone();
        drop(store);
        record_audit(
            &state,
            &headers,
            Some(username),
            "login",
            None,
            StatusCode::UNAUTHORIZED,
            "密码错误",
        )
        .await;
        return error(StatusCode::UNAUTHORIZED, "用户名或密码错误");
    }
    let user_view = UserView::from(user);
    let username = user.username.clone();

    let token = random_token();
    store.sessions.insert(
        token.clone(),
        Session {
            username,
            created_at: Utc::now(),
        },
    );
    if store.fresh_setup {
        store.fresh_setup = false;
    }
    let fresh_setup = store.fresh_setup;
    if let Err(err) = store.save().await {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("保存会话失败: {err}"),
        );
    }
    drop(store);
    record_audit(
        &state,
        &headers,
        Some(user_view.username.clone()),
        "login",
        None,
        StatusCode::OK,
        "登录成功",
    )
    .await;
    let mut headers = HeaderMap::new();
    headers.insert(
        SET_COOKIE,
        HeaderValue::from_str(&format!(
            "rf_session={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age=604800"
        ))
        .unwrap(),
    );
    (
        headers,
        Json(SessionView {
            authenticated: true,
            user: Some(user_view),
            fresh_setup,
        }),
    )
        .into_response()
}

async fn api_logout(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let username = current_user(&state, &headers)
        .await
        .map(|user| user.username);
    if let Some(token) = session_token(&headers) {
        let mut store = state.store.write().await;
        store.sessions.remove(&token);
        let _ = store.save().await;
    }
    record_audit(
        &state,
        &headers,
        username,
        "logout",
        None,
        StatusCode::NO_CONTENT,
        "退出登录",
    )
    .await;
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        SET_COOKIE,
        HeaderValue::from_static("rf_session=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0"),
    );
    response
}

async fn api_files(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: axum::extract::Query<HashMap<String, String>>,
) -> Response {
    let Some(user) = current_user_or_basic(&state, &headers).await else {
        return error(StatusCode::UNAUTHORIZED, "请先登录");
    };
    let path = normalize_path(query.get("path").map(String::as_str).unwrap_or("/"));
    if !can_read(&user, &path) {
        return error(StatusCode::FORBIDDEN, "没有访问此目录的权限");
    }
    let full_path = match safe_join(&state.config.files_dir, &path) {
        Ok(path) => path,
        Err(err) => return error(StatusCode::BAD_REQUEST, &err.to_string()),
    };
    let Ok(mut dir) = fs::read_dir(&full_path).await else {
        return error(StatusCode::NOT_FOUND, "目录不存在");
    };

    let store = state.store.read().await;
    let mut entries = Vec::new();
    loop {
        let entry = match dir.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
        };
        let name = entry.file_name().to_string_lossy().to_string();
        let entry_path = join_virtual(&path, &name);
        if !can_access_path(&store, &user, &entry_path) {
            continue;
        }
        let meta = match entry.metadata().await {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        let public_path = public_path_for_source(&store, &entry_path);
        let public = public_path.is_some();
        let granted_users = store
            .file_grants
            .get(&entry_path)
            .map(|grant| grant.users.clone())
            .unwrap_or_default();
        let download_count = store.download_counts.get(&entry_path).copied().unwrap_or(0);
        let modified = meta.modified().ok().map(DateTime::<Utc>::from);
        entries.push(FileEntry {
            name,
            path: entry_path.clone(),
            kind: if meta.is_dir() {
                FileKind::Directory
            } else {
                FileKind::File
            },
            size: meta.len(),
            modified,
            download_url: format!("/api/files{}", encode_path(&entry_path)),
            download_count,
            public,
            public_url: public_path.as_deref().map(public_url_for_source_path),
            granted_users,
        });
    }
    entries.sort_by(|a, b| match (&a.kind, &b.kind) {
        (FileKind::Directory, FileKind::File) => std::cmp::Ordering::Less,
        (FileKind::File, FileKind::Directory) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
    Json(FileList { path, entries }).into_response()
}

async fn api_download(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(path): AxumPath<String>,
    query: axum::extract::Query<HashMap<String, String>>,
) -> Response {
    let Some(user) = current_user_or_basic(&state, &headers).await else {
        record_audit(
            &state,
            &headers,
            None,
            "download",
            Some(normalize_path(&path)),
            StatusCode::UNAUTHORIZED,
            "未认证",
        )
        .await;
        return basic_auth_required();
    };
    let path = normalize_path(&path);
    let store = state.store.read().await;
    if !can_access_path(&store, &user, &path) {
        record_audit(
            &state,
            &headers,
            Some(user.username),
            "download",
            Some(path),
            StatusCode::FORBIDDEN,
            "下载权限不足",
        )
        .await;
        return basic_auth_required();
    }
    drop(store);
    let username = user.username.clone();
    let force_download = query
        .get("download")
        .is_some_and(|value| value == "1" || value == "true");
    let response = serve_counted_file(&state, &path, force_download).await;
    record_audit(
        &state,
        &headers,
        Some(username),
        "download",
        Some(path),
        response.status(),
        "登录下载",
    )
    .await;
    response
}

async fn public_download(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(path): AxumPath<String>,
    query: axum::extract::Query<HashMap<String, String>>,
) -> Response {
    let path = normalize_public_request_path(&path);
    let store = state.store.read().await;
    let Some(source_path) = public_source_path(&store, &path) else {
        drop(store);
        record_audit(
            &state,
            &headers,
            None,
            "public_download",
            Some(path),
            StatusCode::NOT_FOUND,
            "公开文件不存在",
        )
        .await;
        return error(StatusCode::NOT_FOUND, "公开文件不存在");
    };
    drop(store);
    let force_download = query
        .get("download")
        .is_some_and(|value| value == "1" || value == "true");
    let response = serve_counted_public_file(&state, &source_path, &path, force_download).await;
    record_audit(
        &state,
        &headers,
        None,
        "public_download",
        Some(path),
        response.status(),
        "公开下载",
    )
    .await;
    response
}

async fn api_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(path): AxumPath<String>,
    query: axum::extract::Query<HashMap<String, String>>,
    mut multipart: Multipart,
) -> Response {
    upload_to_path(
        state,
        headers,
        normalize_path(&path),
        UploadConflict::from_query(&query),
        &mut multipart,
    )
    .await
}

async fn api_upload_root(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: axum::extract::Query<HashMap<String, String>>,
    mut multipart: Multipart,
) -> Response {
    upload_to_path(
        state,
        headers,
        "/".to_string(),
        UploadConflict::from_query(&query),
        &mut multipart,
    )
    .await
}

async fn upload_to_path(
    state: AppState,
    headers: HeaderMap,
    path: String,
    conflict: UploadConflict,
    multipart: &mut Multipart,
) -> Response {
    let Some(user) = current_user_or_basic(&state, &headers).await else {
        return error(StatusCode::UNAUTHORIZED, "请先登录");
    };
    if !user.permissions.can_upload {
        record_audit(
            &state,
            &headers,
            Some(user.username),
            "upload",
            Some(path),
            StatusCode::FORBIDDEN,
            "上传权限不足",
        )
        .await;
        return error(StatusCode::FORBIDDEN, "没有上传权限");
    }
    if !can_read(&user, &path) {
        record_audit(
            &state,
            &headers,
            Some(user.username),
            "upload",
            Some(path),
            StatusCode::FORBIDDEN,
            "上传目录权限不足",
        )
        .await;
        return error(StatusCode::FORBIDDEN, "不能上传到这个目录");
    }
    let full_dir = match safe_join(&state.config.files_dir, &path) {
        Ok(path) => path,
        Err(err) => return error(StatusCode::BAD_REQUEST, &err.to_string()),
    };
    if let Err(err) = fs::create_dir_all(&full_dir).await {
        return error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string());
    }

    let mut uploaded = 0_u64;
    while let Some(field) = match multipart.next_field().await {
        Ok(field) => field,
        Err(err) => {
            return error(StatusCode::BAD_REQUEST, &format!("解析上传文件失败：{err}"));
        }
    } {
        let Some(file_name) = field.file_name().map(clean_file_name) else {
            continue;
        };
        let target = match upload_target_path(&full_dir, &file_name, conflict).await {
            Ok(path) => path,
            Err(response) => return response,
        };
        if let Err(err) = write_upload_field(field, &target).await {
            return error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string());
        }
        uploaded += 1;
    }
    record_audit(
        &state,
        &headers,
        Some(user.username),
        "upload",
        Some(path),
        StatusCode::NO_CONTENT,
        format!("上传 {uploaded} 个文件"),
    )
    .await;
    StatusCode::NO_CONTENT.into_response()
}

async fn write_upload_field(mut field: Field<'_>, target: &Path) -> Result<()> {
    let temp_path = upload_temp_path(target);
    let mut file = fs::File::create(&temp_path)
        .await
        .with_context(|| format!("创建临时上传文件失败：{}", temp_path.display()))?;
    while let Some(chunk) = field.chunk().await.context("读取上传文件分片失败")? {
        file.write_all(&chunk).await.context("写入上传文件失败")?;
    }
    drop(file);
    if let Err(err) = fs::rename(&temp_path, target).await {
        let _ = fs::remove_file(&temp_path).await;
        return Err(err).with_context(|| format!("保存上传文件失败：{}", target.display()));
    }
    Ok(())
}

async fn api_mkdir(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(path): AxumPath<String>,
    Json(payload): Json<MkdirPayload>,
) -> Response {
    create_directory_at(state, headers, normalize_path(&path), payload).await
}

async fn api_mkdir_root(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<MkdirPayload>,
) -> Response {
    create_directory_at(state, headers, "/".to_string(), payload).await
}

async fn create_directory_at(
    state: AppState,
    headers: HeaderMap,
    parent_path: String,
    payload: MkdirPayload,
) -> Response {
    let Some(user) = current_user(&state, &headers).await else {
        return error(StatusCode::UNAUTHORIZED, "请先登录");
    };
    if !user.permissions.can_upload {
        record_audit(
            &state,
            &headers,
            Some(user.username),
            "mkdir",
            Some(parent_path),
            StatusCode::FORBIDDEN,
            "创建文件夹权限不足",
        )
        .await;
        return error(StatusCode::FORBIDDEN, "没有创建文件夹权限");
    }
    if !can_read(&user, &parent_path) {
        record_audit(
            &state,
            &headers,
            Some(user.username),
            "mkdir",
            Some(parent_path),
            StatusCode::FORBIDDEN,
            "目录权限不足",
        )
        .await;
        return error(StatusCode::FORBIDDEN, "不能在这个目录创建文件夹");
    }
    let Some(name) = clean_directory_name(&payload.name) else {
        return error(StatusCode::BAD_REQUEST, "文件夹名称不合法");
    };
    let target_path = join_virtual(&parent_path, &name);
    if !can_read(&user, &target_path) {
        return error(StatusCode::FORBIDDEN, "不能创建这个文件夹");
    }
    let full_path = match safe_join(&state.config.files_dir, &target_path) {
        Ok(path) => path,
        Err(err) => return error(StatusCode::BAD_REQUEST, &err.to_string()),
    };
    if fs::try_exists(&full_path).await.unwrap_or(false) {
        return error(StatusCode::CONFLICT, "文件夹已存在");
    }
    if let Err(err) = fs::create_dir_all(full_path).await {
        return error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string());
    }
    record_audit(
        &state,
        &headers,
        Some(user.username),
        "mkdir",
        Some(target_path.clone()),
        StatusCode::CREATED,
        "创建文件夹",
    )
    .await;
    (
        StatusCode::CREATED,
        Json(serde_json::json!({ "path": target_path })),
    )
        .into_response()
}

async fn api_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(path): AxumPath<String>,
) -> Response {
    let Some(user) = current_user(&state, &headers).await else {
        return error(StatusCode::UNAUTHORIZED, "请先登录");
    };
    if !user.permissions.can_delete {
        record_audit(
            &state,
            &headers,
            Some(user.username),
            "delete",
            Some(path),
            StatusCode::FORBIDDEN,
            "删除权限不足",
        )
        .await;
        return error(StatusCode::FORBIDDEN, "没有删除权限");
    }
    let path = normalize_path(&path);
    if !can_read(&user, &path) {
        record_audit(
            &state,
            &headers,
            Some(user.username),
            "delete",
            Some(path),
            StatusCode::FORBIDDEN,
            "路径权限不足",
        )
        .await;
        return error(StatusCode::FORBIDDEN, "不能删除这个文件");
    }
    let full_path = match safe_join(&state.config.files_dir, &path) {
        Ok(path) => path,
        Err(err) => return error(StatusCode::BAD_REQUEST, &err.to_string()),
    };
    let result = match fs::metadata(&full_path).await {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(&full_path).await,
        Ok(_) => fs::remove_file(&full_path).await,
        Err(err) => Err(err),
    };
    if let Err(err) = result {
        return error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string());
    }
    let mut store = state.store.write().await;
    if let Some(public_path) = public_path_for_source(&store, &path) {
        store.public_files.remove(&public_path);
        store.download_counts.remove(&public_path);
    }
    store.public_files.remove(&path);
    store.public_copies.remove(&path);
    store.file_grants.remove(&path);
    store.download_counts.remove(&path);
    let _ = store.save().await;
    record_audit(
        &state,
        &headers,
        Some(user.username),
        "delete",
        Some(path),
        StatusCode::NO_CONTENT,
        "删除文件或目录",
    )
    .await;
    StatusCode::NO_CONTENT.into_response()
}

async fn api_users(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(user) = current_user(&state, &headers).await else {
        return error(StatusCode::UNAUTHORIZED, "请先登录");
    };
    if user.role != Role::Admin {
        return error(StatusCode::FORBIDDEN, "只有管理员可以访问后台");
    }
    let store = state.store.read().await;
    let mut users: Vec<_> = store.users.values().map(UserView::from).collect();
    users.sort_by(|a, b| a.username.cmp(&b.username));
    Json(users).into_response()
}

async fn api_audit_logs(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(user) = current_user(&state, &headers).await else {
        return error(StatusCode::UNAUTHORIZED, "请先登录");
    };
    if user.role != Role::Admin {
        return error(StatusCode::FORBIDDEN, "只有管理员可以查看活动记录");
    }
    let logs = state.audit_logs.read().await;
    let logs: Vec<_> = logs.iter().rev().take(200).cloned().collect();
    Json(logs).into_response()
}

async fn api_create_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<UserPayload>,
) -> Response {
    let Some(admin) = current_user(&state, &headers).await else {
        return error(StatusCode::UNAUTHORIZED, "请先登录");
    };
    if admin.role != Role::Admin {
        return error(StatusCode::FORBIDDEN, "只有管理员可以创建用户");
    }
    if payload.username.trim().is_empty() || payload.password.as_deref().unwrap_or("").is_empty() {
        return error(StatusCode::BAD_REQUEST, "用户名和密码不能为空");
    }
    let mut store = state.store.write().await;
    if store.users.contains_key(&payload.username) {
        return error(StatusCode::CONFLICT, "用户已存在");
    }
    let user = user_from_payload(payload);
    let view = UserView::from(&user);
    for root in &user.permissions.roots {
        let full_path = match safe_join(&state.config.files_dir, root) {
            Ok(path) => path,
            Err(err) => return error(StatusCode::BAD_REQUEST, &err.to_string()),
        };
        if let Err(err) = fs::create_dir_all(full_path).await {
            return error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string());
        }
    }
    store.users.insert(user.username.clone(), user);
    if let Err(err) = store.save().await {
        return error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string());
    }
    record_audit(
        &state,
        &headers,
        Some(admin.username),
        "create_user",
        None,
        StatusCode::CREATED,
        format!("创建用户 {}", view.username),
    )
    .await;
    (StatusCode::CREATED, Json(view)).into_response()
}

async fn api_update_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(username): AxumPath<String>,
    Json(payload): Json<UserPayload>,
) -> Response {
    let Some(admin) = current_user(&state, &headers).await else {
        return error(StatusCode::UNAUTHORIZED, "请先登录");
    };
    if admin.role != Role::Admin {
        return error(StatusCode::FORBIDDEN, "只有管理员可以修改用户");
    }
    let new_username = payload.username.trim().to_string();
    if new_username.is_empty() {
        return error(StatusCode::BAD_REQUEST, "用户名不能为空");
    }
    let mut store = state.store.write().await;
    if new_username != username && store.users.contains_key(&new_username) {
        return error(StatusCode::CONFLICT, "用户名已存在");
    }
    let Some(mut user) = store.users.remove(&username) else {
        return error(StatusCode::NOT_FOUND, "用户不存在");
    };
    if user.role == Role::Admin
        && payload.role != Role::Admin
        && !store.users.values().any(|user| user.role == Role::Admin)
    {
        store.users.insert(username, user);
        return error(StatusCode::BAD_REQUEST, "至少需要保留一个管理员");
    }
    let new_role = payload.role;
    let new_permissions = permissions_from_payload(&payload);
    for root in &new_permissions.roots {
        let full_path = match safe_join(&state.config.files_dir, root) {
            Ok(path) => path,
            Err(err) => {
                store.users.insert(username, user);
                return error(StatusCode::BAD_REQUEST, &err.to_string());
            }
        };
        if let Err(err) = fs::create_dir_all(full_path).await {
            store.users.insert(username, user);
            return error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string());
        }
    }
    user.username = new_username.clone();
    user.role = new_role;
    user.permissions = new_permissions;
    if let Some(password) = payload.password.filter(|value| !value.is_empty()) {
        user.password_hash = hash_password(&password);
    }
    for session in store.sessions.values_mut() {
        if session.username == username {
            session.username = new_username.clone();
        }
    }
    let view = UserView::from(&user);
    store.users.insert(new_username, user);
    if let Err(err) = store.save().await {
        return error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string());
    }
    record_audit(
        &state,
        &headers,
        Some(admin.username),
        "update_user",
        None,
        StatusCode::OK,
        format!("修改用户 {}", view.username),
    )
    .await;
    Json(view).into_response()
}

async fn api_delete_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(username): AxumPath<String>,
) -> Response {
    let Some(admin) = current_user(&state, &headers).await else {
        return error(StatusCode::UNAUTHORIZED, "请先登录");
    };
    if admin.role != Role::Admin {
        return error(StatusCode::FORBIDDEN, "只有管理员可以删除用户");
    }
    let mut store = state.store.write().await;
    let Some(user) = store.users.get(&username) else {
        return error(StatusCode::NOT_FOUND, "用户不存在");
    };
    if user.role == Role::Admin
        && store
            .users
            .values()
            .filter(|user| user.role == Role::Admin)
            .count()
            <= 1
    {
        return error(StatusCode::BAD_REQUEST, "至少需要保留一个管理员");
    }
    store.users.remove(&username);
    store
        .sessions
        .retain(|_, session| session.username != username);
    if let Err(err) = store.save().await {
        return error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string());
    }
    record_audit(
        &state,
        &headers,
        Some(admin.username),
        "delete_user",
        None,
        StatusCode::NO_CONTENT,
        format!("删除用户 {username}"),
    )
    .await;
    StatusCode::NO_CONTENT.into_response()
}

async fn api_publish(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(path): AxumPath<String>,
) -> Response {
    let Some(user) = current_user(&state, &headers).await else {
        return error(StatusCode::UNAUTHORIZED, "请先登录");
    };
    let path = normalize_path(&path);
    if !can_read(&user, &path) {
        record_audit(
            &state,
            &headers,
            Some(user.username),
            "publish",
            Some(path),
            StatusCode::FORBIDDEN,
            "路径权限不足",
        )
        .await;
        return error(StatusCode::FORBIDDEN, "不能公开这个文件");
    }
    let source_path = match safe_join(&state.config.files_dir, &path) {
        Ok(path) => path,
        Err(err) => return error(StatusCode::BAD_REQUEST, &err.to_string()),
    };
    let Ok(meta) = fs::metadata(&source_path).await else {
        return error(StatusCode::NOT_FOUND, "文件不存在");
    };
    if meta.is_dir() {
        return error(StatusCode::BAD_REQUEST, "暂不支持公开整个目录");
    }
    let mut store = state.store.write().await;
    store.public_copies.remove(&path);
    store.public_files.insert(
        path.clone(),
        PublicFile {
            path: path.clone(),
            published_by: user.username.clone(),
            published_at: Utc::now(),
        },
    );
    if let Err(err) = store.save().await {
        return error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string());
    }
    record_audit(
        &state,
        &headers,
        Some(user.username),
        "publish",
        Some(path.clone()),
        StatusCode::OK,
        "公开文件链接",
    )
    .await;
    Json(serde_json::json!({
        "public_path": path,
        "public_url": public_url_for_source_path(&path)
    }))
    .into_response()
}

async fn api_unpublish(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(path): AxumPath<String>,
) -> Response {
    let Some(_user) = current_user(&state, &headers).await else {
        return error(StatusCode::UNAUTHORIZED, "请先登录");
    };
    let path = normalize_path(&path);
    let mut store = state.store.write().await;
    let public_path = path.clone();
    store.public_copies.remove(&path);
    store.public_files.remove(&path);
    store.download_counts.remove(&path);
    if let Err(err) = store.save().await {
        return error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string());
    }
    record_audit(
        &state,
        &headers,
        None,
        "unpublish",
        Some(path),
        StatusCode::NO_CONTENT,
        format!("取消公开链接 {public_path}"),
    )
    .await;
    Json(serde_json::json!({ "public_path": public_path })).into_response()
}

async fn api_grant_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(path): AxumPath<String>,
    Json(payload): Json<GrantPayload>,
) -> Response {
    let Some(user) = current_user(&state, &headers).await else {
        return error(StatusCode::UNAUTHORIZED, "请先登录");
    };
    let path = normalize_path(&path);
    let full_path = match safe_join(&state.config.files_dir, &path) {
        Ok(path) => path,
        Err(err) => return error(StatusCode::BAD_REQUEST, &err.to_string()),
    };
    let Ok(meta) = fs::metadata(&full_path).await else {
        return error(StatusCode::NOT_FOUND, "文件不存在");
    };
    if meta.is_dir() {
        return error(StatusCode::BAD_REQUEST, "暂不支持授权整个目录");
    }

    let mut store = state.store.write().await;
    if !can_access_path(&store, &user, &path) {
        drop(store);
        record_audit(
            &state,
            &headers,
            Some(user.username),
            "grant",
            Some(path),
            StatusCode::FORBIDDEN,
            "授权路径权限不足",
        )
        .await;
        return error(StatusCode::FORBIDDEN, "不能授权这个文件");
    }
    let mut granted_users = Vec::new();
    for username in payload.users {
        let username = username.trim().to_string();
        if username.is_empty() || username == user.username {
            continue;
        }
        if !store.users.contains_key(&username) {
            return error(StatusCode::BAD_REQUEST, &format!("用户 {username} 不存在"));
        }
        if !granted_users.contains(&username) {
            granted_users.push(username);
        }
    }
    granted_users.sort();
    if granted_users.is_empty() {
        store.file_grants.remove(&path);
    } else {
        store.file_grants.insert(
            path.clone(),
            FileGrant {
                path: path.clone(),
                users: granted_users.clone(),
                updated_by: user.username.clone(),
                updated_at: Utc::now(),
            },
        );
    }
    if let Err(err) = store.save().await {
        return error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string());
    }
    record_audit(
        &state,
        &headers,
        Some(user.username),
        "grant",
        Some(path.clone()),
        StatusCode::OK,
        if granted_users.is_empty() {
            "清空授权".to_string()
        } else {
            format!("授权给 {}", granted_users.join(", "))
        },
    )
    .await;
    Json(serde_json::json!({
        "download_url": format!("/api/files{}", encode_path(&path)),
        "granted_users": granted_users
    }))
    .into_response()
}

async fn current_user(state: &AppState, headers: &HeaderMap) -> Option<User> {
    let token = session_token(headers)?;
    let store = state.store.read().await;
    let session = store.sessions.get(&token)?;
    store.users.get(&session.username).cloned()
}

async fn current_user_or_basic(state: &AppState, headers: &HeaderMap) -> Option<User> {
    if let Some(user) = current_user(state, headers).await {
        return Some(user);
    }
    let (username, password) = basic_credentials(headers)?;
    let store = state.store.read().await;
    let user = store.users.get(&username)?;
    verify_password(&password, &user.password_hash).then(|| user.clone())
}

fn basic_credentials(headers: &HeaderMap) -> Option<(String, String)> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let encoded = value.strip_prefix("Basic ")?;
    let decoded = STANDARD.decode(encoded).ok()?;
    let credentials = String::from_utf8(decoded).ok()?;
    let (username, password) = credentials.split_once(':')?;
    Some((username.to_string(), password.to_string()))
}

async fn increment_download_count(state: &AppState, path: &str) {
    let mut store = state.store.write().await;
    *store.download_counts.entry(path.to_string()).or_insert(0) += 1;
    let _ = store.save().await;
}

async fn record_audit(
    state: &AppState,
    headers: &HeaderMap,
    username: Option<String>,
    action: &str,
    path: Option<String>,
    status: StatusCode,
    detail: impl Into<String>,
) {
    let mut logs = state.audit_logs.write().await;
    logs.push(AuditLog {
        at: Utc::now(),
        username,
        action: action.to_string(),
        path,
        status: status.as_u16().to_string(),
        detail: detail.into(),
        ip: client_ip(headers),
    });
    if logs.len() > 1000 {
        let overflow = logs.len() - 1000;
        logs.drain(0..overflow);
    }
    let path = state.config.data_dir.join("audit.json");
    if let Ok(data) = serde_json::to_vec_pretty(&*logs) {
        let _ = fs::write(path, data).await;
    }
}

fn client_ip(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            headers
                .get("x-real-ip")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        })
}

async fn serve_counted_file(state: &AppState, path: &str, force_download: bool) -> Response {
    let response = serve_file(&state.config.files_dir, path, force_download).await;
    if response.status().is_success() {
        increment_download_count(state, path).await;
    }
    response
}

async fn serve_counted_public_file(
    state: &AppState,
    source_path: &str,
    public_path: &str,
    force_download: bool,
) -> Response {
    let response = serve_file(&state.config.files_dir, source_path, force_download).await;
    if response.status().is_success() {
        let mut store = state.store.write().await;
        *store
            .download_counts
            .entry(public_path.to_string())
            .or_insert(0) += 1;
        if source_path != public_path {
            *store
                .download_counts
                .entry(source_path.to_string())
                .or_insert(0) += 1;
        }
        let _ = store.save().await;
    }
    response
}

fn session_token(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get("cookie")?.to_str().ok()?;
    cookie.split(';').find_map(|part| {
        let (key, value) = part.trim().split_once('=')?;
        (key == "rf_session").then(|| value.to_string())
    })
}

async fn serve_file(root: &Path, path: &str, force_download: bool) -> Response {
    let full_path = match safe_join(root, path) {
        Ok(path) => path,
        Err(err) => return error(StatusCode::BAD_REQUEST, &err.to_string()),
    };
    let Ok(meta) = fs::metadata(&full_path).await else {
        return error(StatusCode::NOT_FOUND, "文件不存在");
    };
    if meta.is_dir() {
        return Redirect::temporary(&format!("/?path={}", urlencoding::encode(path)))
            .into_response();
    }
    let Ok(bytes) = fs::read(&full_path).await else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "读取文件失败");
    };
    let file_name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("download");
    let mime = mime_guess::from_path(&full_path).first_or_octet_stream();
    let mut headers = HeaderMap::new();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_str(mime.as_ref())
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "{}; filename=\"{}\"",
            if force_download {
                "attachment"
            } else {
                "inline"
            },
            file_name.replace('"', "")
        ))
        .unwrap(),
    );
    (headers, bytes).into_response()
}

fn can_read(user: &User, path: &str) -> bool {
    user.role == Role::Admin
        || user
            .permissions
            .roots
            .iter()
            .map(|root| normalize_path(root))
            .any(|root| {
                root == "/"
                    || path == root
                    || path.starts_with(&format!("{}/", root.trim_end_matches('/')))
            })
}

fn can_access_path(store: &Store, user: &User, path: &str) -> bool {
    can_read(user, path)
        || store
            .file_grants
            .get(path)
            .is_some_and(|grant| grant.users.contains(&user.username))
}

fn is_public_path(path: &str) -> bool {
    path == "/public" || path.starts_with("/public/")
}

fn public_path_for_source(store: &Store, source_path: &str) -> Option<String> {
    store
        .public_files
        .contains_key(source_path)
        .then(|| source_path.to_string())
}

fn public_source_path(store: &Store, public_path: &str) -> Option<String> {
    let source_path = source_path_from_public_url(public_path);
    if store.public_files.contains_key(&source_path) {
        Some(source_path)
    } else if store.public_files.contains_key(public_path) {
        Some(public_path.to_string())
    } else {
        None
    }
}

fn safe_join(root: &Path, path: &str) -> Result<PathBuf> {
    let path = normalize_path(path);
    let mut target = root.to_path_buf();
    for component in Path::new(path.trim_start_matches('/')).components() {
        match component {
            Component::Normal(part) => target.push(part),
            Component::CurDir => {}
            _ => anyhow::bail!("路径不安全"),
        }
    }
    Ok(target)
}

fn normalize_path(path: &str) -> String {
    let decoded = urlencoding::decode(path).unwrap_or_else(|_| path.into());
    let mut parts = Vec::new();
    for component in Path::new(decoded.as_ref()).components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().to_string()),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) => {}
        }
    }
    if parts.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", parts.join("/"))
    }
}

fn join_virtual(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{}/{}", parent.trim_end_matches('/'), name)
    }
}

fn encode_path(path: &str) -> String {
    path.split('/')
        .filter(|part| !part.is_empty())
        .map(urlencoding::encode)
        .collect::<Vec<_>>()
        .join("/")
        .pipe(|value| format!("/{value}"))
}

fn normalize_public_request_path(path: &str) -> String {
    let path = normalize_path(path);
    if is_public_path(&path) {
        path
    } else {
        join_virtual("/public", path.trim_start_matches('/'))
    }
}

fn public_url_for_source_path(path: &str) -> String {
    format!("/public{}", encode_path(path).trim_start_matches("/public"))
}

fn source_path_from_public_url(path: &str) -> String {
    if is_public_path(path) {
        normalize_path(path.trim_start_matches("/public"))
    } else {
        normalize_path(path)
    }
}

fn clean_file_name(file_name: &str) -> String {
    Path::new(file_name)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("upload.bin")
        .to_string()
}

fn upload_temp_path(target: &Path) -> PathBuf {
    let token = random_token();
    let safe_token = URL_SAFE_NO_PAD.encode(token.as_bytes());
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("upload.bin");
    target.with_file_name(format!(".{file_name}.{safe_token}.uploading"))
}

async fn upload_target_path(
    dir: &Path,
    file_name: &str,
    conflict: UploadConflict,
) -> std::result::Result<PathBuf, Response> {
    let target = dir.join(file_name);
    if !fs::try_exists(&target).await.unwrap_or(false) {
        return Ok(target);
    }
    match conflict {
        UploadConflict::Overwrite => Ok(target),
        UploadConflict::Error => Err(error(StatusCode::CONFLICT, "文件已存在")),
        UploadConflict::Rename => next_available_file_path(dir, file_name)
            .await
            .map_err(|err| error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string())),
    }
}

async fn next_available_file_path(dir: &Path, file_name: &str) -> Result<PathBuf> {
    let path = Path::new(file_name);
    let stem = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let extension = path.extension().and_then(|name| name.to_str());
    for index in 1..1000 {
        let candidate = if let Some(extension) = extension {
            format!("{stem}-{index}.{extension}")
        } else {
            format!("{stem}-{index}")
        };
        let target = dir.join(candidate);
        if !fs::try_exists(&target).await? {
            return Ok(target);
        }
    }
    anyhow::bail!("无法生成可用文件名")
}

fn clean_directory_name(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return None;
    }
    let cleaned = Path::new(trimmed).file_name()?.to_str()?.to_string();
    (cleaned == trimmed).then_some(cleaned)
}

fn user_from_payload(payload: UserPayload) -> User {
    let username = payload.username.trim().to_string();
    let password_hash = hash_password(payload.password.as_deref().unwrap_or(DEFAULT_PASSWORD));
    let permissions = effective_permissions(
        &username,
        payload.role,
        &payload.roots,
        payload.can_upload,
        payload.can_delete,
        payload.can_publish,
    );
    User {
        username,
        password_hash,
        role: payload.role,
        permissions,
        created_at: Utc::now(),
    }
}

fn permissions_from_payload(payload: &UserPayload) -> Permissions {
    effective_permissions(
        payload.username.trim(),
        payload.role,
        &payload.roots,
        payload.can_upload,
        payload.can_delete,
        payload.can_publish,
    )
}

fn effective_permissions(
    username: &str,
    role: Role,
    roots: &[String],
    can_upload: bool,
    can_delete: bool,
    can_publish: bool,
) -> Permissions {
    let user_root = format!("/user/{}", username);
    let normalized_roots: Vec<String> = roots
        .iter()
        .map(|root| normalize_path(root))
        .filter(|root| role == Role::Admin || root != "/")
        .collect();
    let roots = if normalized_roots.is_empty() {
        if role == Role::Admin {
            vec!["/".to_string()]
        } else {
            vec![user_root, "/public".to_string()]
        }
    } else {
        let mut roots = normalized_roots;
        if role != Role::Admin && !roots.iter().any(|root| root == "/public") {
            roots.push("/public".to_string());
        }
        roots
    };
    Permissions {
        roots,
        can_upload,
        can_delete: role == Role::Admin || can_delete,
        can_publish: role == Role::Admin || can_publish,
    }
}

fn hash_password(password: &str) -> String {
    let salt = random_token();
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(b":");
    hasher.update(password.as_bytes());
    format!("v1${salt}${:x}", hasher.finalize())
}

fn verify_password(password: &str, stored: &str) -> bool {
    let Some(("v1", rest)) = stored.split_once('$') else {
        return stored == legacy_hash_password(password);
    };
    let Some((salt, expected)) = rest.split_once('$') else {
        return false;
    };
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(b":");
    hasher.update(password.as_bytes());
    format!("{:x}", hasher.finalize()) == expected
}

fn legacy_hash_password(password: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(password.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn random_token() -> String {
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    STANDARD_NO_PAD.encode(bytes)
}

fn parse_upload_limit(value: &str) -> Result<usize> {
    let bytes = value
        .trim()
        .parse::<usize>()
        .with_context(|| "REMOTE_FILE_UPLOAD_LIMIT_BYTES must be a positive integer")?;
    anyhow::ensure!(
        bytes > 0,
        "REMOTE_FILE_UPLOAD_LIMIT_BYTES must be greater than 0"
    );
    Ok(bytes)
}

fn error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(serde_json::json!({
            "error": message
        })),
    )
        .into_response()
}

fn basic_auth_required() -> Response {
    let mut response = error(StatusCode::UNAUTHORIZED, "请先登录");
    response.headers_mut().insert(
        WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"Remote File\", charset=\"UTF-8\""),
    );
    response.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static("no-store, no-cache, must-revalidate"),
    );
    response
}

trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl<T> Pipe for T {}

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>Remote File</title>
  <style>
    :root {
      color-scheme: light;
      --bg: #f7f5ef;
      --panel: #ffffff;
      --ink: #1f2933;
      --muted: #697386;
      --line: #d8dee8;
      --accent: #176b87;
      --accent-strong: #0f4c5c;
      --ok: #2f7d4f;
      --warn: #b45309;
      --danger: #b42318;
      --shadow: 0 12px 28px rgba(32, 45, 61, 0.10);
    }
    * { box-sizing: border-box; }
    body {
      margin: 0;
      min-height: 100vh;
      color: var(--ink);
      background:
        linear-gradient(180deg, rgba(255,255,255,0.72), rgba(255,255,255,0.32)),
        var(--bg);
      font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    }
    button, input, select { font: inherit; }
    button {
      border: 1px solid var(--line);
      background: #fff;
      color: var(--ink);
      border-radius: 7px;
      min-height: 36px;
      padding: 0 12px;
      cursor: pointer;
    }
    button:hover { border-color: var(--accent); }
    button.primary { background: var(--accent); border-color: var(--accent); color: #fff; }
    button.danger { color: var(--danger); border-color: #f3b8b2; }
    button.icon { width: 36px; padding: 0; display: inline-grid; place-items: center; }
    input, select {
      width: 100%;
      min-height: 38px;
      border: 1px solid var(--line);
      border-radius: 7px;
      padding: 0 10px;
      background: #fff;
      color: var(--ink);
    }
    .hidden { display: none !important; }
    .app-shell { min-height: 100vh; display: grid; grid-template-columns: 260px 1fr; }
    .sidebar {
      border-right: 1px solid var(--line);
      background: rgba(255,255,255,0.72);
      padding: 22px 18px;
      display: flex;
      flex-direction: column;
      gap: 18px;
    }
    .brand { display: flex; align-items: center; gap: 12px; }
    .brand-mark {
      width: 40px; height: 40px; border-radius: 8px;
      background: conic-gradient(from 210deg, #176b87, #2f7d4f, #d99a2b, #176b87);
      box-shadow: var(--shadow);
    }
    .brand strong { display: block; font-size: 17px; }
    .brand span { color: var(--muted); font-size: 12px; }
    .nav { display: grid; gap: 8px; }
    .nav button { text-align: left; border-color: transparent; background: transparent; }
    .nav button.active { background: #eaf2f4; color: var(--accent-strong); border-color: #c7dce2; }
    .account { margin-top: auto; display: grid; gap: 8px; color: var(--muted); font-size: 13px; }
    .main { padding: 22px clamp(18px, 4vw, 42px); }
    .topbar { display: flex; align-items: center; justify-content: space-between; gap: 16px; margin-bottom: 22px; }
    .crumbs { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; color: var(--muted); }
    .crumbs button { min-height: 30px; padding: 0 9px; }
    .toolbar { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }
    .panel {
      background: rgba(255,255,255,0.88);
      border: 1px solid var(--line);
      border-radius: 8px;
      box-shadow: var(--shadow);
    }
    .table { overflow: hidden; }
    .row {
      display: grid;
      grid-template-columns: minmax(180px, 1fr) 110px 90px 190px 210px;
      gap: 14px;
      align-items: center;
      min-height: 58px;
      padding: 0 16px;
      border-bottom: 1px solid var(--line);
    }
    .row.header { min-height: 42px; color: var(--muted); font-size: 12px; text-transform: uppercase; letter-spacing: 0; background: #fbfcfd; }
    .row.audit { grid-template-columns: 180px 120px 120px minmax(180px, 1fr) 220px; }
    .row:last-child { border-bottom: 0; }
    .file-name { display: flex; align-items: center; gap: 10px; min-width: 0; }
    .file-name button { border: 0; background: transparent; padding: 0; min-height: auto; text-align: left; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    .badge { display: inline-flex; align-items: center; min-height: 24px; padding: 0 8px; border-radius: 999px; font-size: 12px; border: 1px solid var(--line); color: var(--muted); }
    .badge.public { color: var(--ok); border-color: #bfe0cd; background: #eef8f1; }
    .actions { display: flex; gap: 8px; justify-content: flex-end; flex-wrap: wrap; }
    .upload-progress {
      display: grid;
      grid-template-columns: minmax(0, 1fr) auto;
      gap: 10px 14px;
      align-items: center;
      margin: -8px 0 14px;
      padding: 12px 14px;
      color: var(--muted);
      font-size: 13px;
    }
    .upload-progress strong { color: var(--ink); font-weight: 600; }
    .progress-track {
      grid-column: 1 / -1;
      height: 8px;
      overflow: hidden;
      border-radius: 999px;
      background: #e2eaed;
    }
    .progress-bar {
      width: 0%;
      height: 100%;
      background: var(--accent);
      transition: width 120ms ease-out;
    }
    .login {
      min-height: 100vh;
      display: grid;
      place-items: center;
      padding: 24px;
    }
    .login form {
      width: min(420px, 100%);
      display: grid;
      gap: 14px;
      padding: 28px;
    }
    .login h1 { margin: 0; font-size: 28px; }
    .login p { margin: 0 0 8px; color: var(--muted); }
    .admin-grid { display: grid; grid-template-columns: minmax(260px, 380px) 1fr; gap: 18px; align-items: start; }
    .form { display: grid; gap: 12px; padding: 16px; }
    .form label { display: grid; gap: 6px; color: var(--muted); font-size: 13px; }
    .checks { display: grid; grid-template-columns: repeat(3, 1fr); gap: 8px; }
    .checks label { display: flex; align-items: center; gap: 8px; color: var(--ink); }
    .checks input { width: auto; min-height: auto; }
    .empty { padding: 42px 16px; text-align: center; color: var(--muted); }
    .toast {
      position: fixed; right: 18px; bottom: 18px; min-width: 240px;
      padding: 12px 14px; background: var(--ink); color: #fff; border-radius: 8px;
      box-shadow: var(--shadow);
    }
    @media (max-width: 860px) {
      .app-shell { grid-template-columns: 1fr; }
      .sidebar { position: sticky; top: 0; z-index: 2; flex-direction: row; align-items: center; }
      .nav { display: flex; }
      .account { margin-top: 0; margin-left: auto; }
      .row { grid-template-columns: 1fr; gap: 6px; padding: 12px; }
      .row.header { display: none; }
      .actions { justify-content: flex-start; }
      .admin-grid { grid-template-columns: 1fr; }
    }
  </style>
</head>
<body>
  <div id="login" class="login hidden">
    <form class="panel" id="login-form">
      <div class="brand">
        <div class="brand-mark"></div>
        <div><strong>Remote File</strong><span>权限化文件系统</span></div>
      </div>
      <h1>登录</h1>
      <p id="setup-hint" class="hidden">默认管理员账号 admin / admin，首次部署后请立即修改密码。</p>
      <input id="login-user" autocomplete="username" placeholder="用户名" />
      <input id="login-pass" autocomplete="current-password" placeholder="密码" type="password" />
      <button class="primary" type="submit">登录</button>
    </form>
  </div>
  <div id="app" class="app-shell hidden">
    <aside class="sidebar">
      <div class="brand">
        <div class="brand-mark"></div>
        <div><strong>Remote File</strong><span id="role-label">文件系统</span></div>
      </div>
      <nav class="nav">
        <button id="nav-files" class="active">文件</button>
        <button id="nav-admin">后台</button>
      </nav>
      <div class="account">
        <span id="account-name"></span>
        <button id="logout">退出登录</button>
      </div>
    </aside>
    <main class="main">
      <section id="files-view">
        <div class="topbar">
          <div class="crumbs" id="crumbs"></div>
          <div class="toolbar">
            <input id="upload-input" type="file" multiple hidden />
            <button id="upload-btn" class="primary">上传</button>
            <button id="mkdir-btn">新建文件夹</button>
            <button id="refresh-btn">刷新</button>
          </div>
        </div>
        <div id="upload-progress" class="panel upload-progress hidden">
          <strong id="upload-progress-title">正在上传</strong>
          <span id="upload-progress-detail">0%</span>
          <div class="progress-track"><div id="upload-progress-bar" class="progress-bar"></div></div>
        </div>
        <div class="panel table" id="files-table"></div>
      </section>
      <section id="admin-view" class="hidden">
        <div class="topbar">
          <div><h2 style="margin:0">管理员后台</h2><p style="margin:4px 0 0;color:var(--muted)">管理用户、目录授权和公开直链能力。</p></div>
        </div>
        <div class="admin-grid">
          <form class="panel form" id="user-form">
            <input id="edit-username" type="hidden" />
            <label>用户名<input id="user-username" required /></label>
            <label>密码<input id="user-password" type="password" placeholder="编辑用户时留空则不修改" /></label>
            <label>角色<select id="user-role"><option value="user">普通用户</option><option value="admin">管理员</option></select></label>
            <label>可访问目录<input id="user-roots" placeholder="/user/alice, /team" /></label>
            <div class="checks">
              <label><input id="perm-upload" type="checkbox" checked />上传</label>
              <label><input id="perm-delete" type="checkbox" />删除</label>
              <label><input id="perm-publish" type="checkbox" />公开</label>
            </div>
            <button class="primary" type="submit">保存用户</button>
            <button type="button" id="reset-user-form">新建用户</button>
          </form>
          <div class="panel table" id="users-table"></div>
        </div>
        <div class="panel table" id="audit-table" style="margin-top:18px"></div>
      </section>
    </main>
  </div>
  <div id="toast" class="toast hidden"></div>
  <script src="/app.js"></script>
</body>
</html>"#;

const APP_JS: &str = r#"const state = {
  user: null,
  path: new URLSearchParams(location.search).get('path') || '/',
  view: 'files',
  entries: [],
  upload: null,
};

const $ = (id) => document.getElementById(id);

async function api(url, options = {}) {
  const res = await fetch(url, {
    credentials: 'same-origin',
    headers: options.body instanceof FormData ? {} : { 'content-type': 'application/json' },
    ...options,
  });
  if (!res.ok) {
    let message = '请求失败';
    try { message = (await res.json()).error || message; } catch {}
    throw new Error(message);
  }
  if (res.status === 204) return null;
  return res.json();
}

function toast(message) {
  $('toast').textContent = message;
  $('toast').classList.remove('hidden');
  clearTimeout(window.__toast);
  window.__toast = setTimeout(() => $('toast').classList.add('hidden'), 2600);
}

function show(view) {
  $('login').classList.toggle('hidden', view !== 'login');
  $('app').classList.toggle('hidden', view === 'login');
}

async function boot() {
  const session = await api('/api/session');
  if (!session.authenticated) {
    showSetupHint(session.fresh_setup);
    show('login');
    return;
  }
  state.user = session.user;
  $('account-name').textContent = session.user.username;
  $('role-label').textContent = session.user.role === 'admin' ? '管理员' : '文件系统';
  $('nav-admin').classList.toggle('hidden', session.user.role !== 'admin');
  if (!state.path || state.path === '/') {
    state.path = defaultPathForUser(session.user);
    history.replaceState(null, '', `/?path=${encodeURIComponent(state.path)}`);
  }
  show('app');
  await loadFiles();
}

function showSetupHint(freshSetup) {
  $('setup-hint').classList.toggle('hidden', !freshSetup);
  if (freshSetup) {
    $('login-user').value = 'admin';
    $('login-pass').value = 'admin';
  } else {
    $('login-user').value = '';
    $('login-pass').value = '';
  }
}

function defaultPathForUser(user) {
  return user.role === 'admin' ? '/' : `/user/${user.username}`;
}

function browseRoot() {
  const path = normalizeClientPath(state.path || defaultPathForUser(state.user));
  const roots = (state.user.permissions?.roots || [defaultPathForUser(state.user)])
    .map(normalizeClientPath)
    .filter((root) => root !== '/')
    .sort((a, b) => b.length - a.length);
  return roots.find((root) => path === root || path.startsWith(`${root}/`)) || defaultPathForUser(state.user);
}

function isAtBrowseRoot(path) {
  return normalizeClientPath(path) === browseRoot();
}

function normalizeClientPath(path) {
  const parts = path.split('/').filter(Boolean);
  return parts.length ? `/${parts.join('/')}` : '/';
}

function setView(view) {
  state.view = view;
  $('files-view').classList.toggle('hidden', view !== 'files');
  $('admin-view').classList.toggle('hidden', view !== 'admin');
  $('nav-files').classList.toggle('active', view === 'files');
  $('nav-admin').classList.toggle('active', view === 'admin');
  if (view === 'admin') {
    loadUsers();
    loadAuditLogs();
  }
}

function formatSize(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let size = bytes / 1024;
  let unit = 0;
  while (size >= 1024 && unit < units.length - 1) { size /= 1024; unit++; }
  return `${size.toFixed(size >= 10 ? 1 : 2)} ${units[unit]}`;
}

function icon(kind) {
  return kind === 'directory' ? '📁' : '📄';
}

function renderCrumbs(path) {
  const crumbs = $('crumbs');
  crumbs.innerHTML = '';
  const rootPath = browseRoot();
  const root = document.createElement('button');
  root.textContent = '全部文件';
  root.onclick = () => navigate(rootPath);
  crumbs.append(root);
  let current = rootPath === '/' ? '' : rootPath;
  const visibleParts = rootPath === '/'
    ? path.split('/').filter(Boolean)
    : path.slice(rootPath.length).split('/').filter(Boolean);
  visibleParts.forEach((part) => {
    current += `/${part}`;
    const sep = document.createElement('span');
    sep.textContent = '/';
    const btn = document.createElement('button');
    btn.textContent = part;
    btn.onclick = () => navigate(current);
    crumbs.append(sep, btn);
  });
}

async function loadFiles() {
  renderCrumbs(state.path);
  const data = await api(`/api/files?path=${encodeURIComponent(state.path)}`);
  state.entries = data.entries;
  const table = $('files-table');
  table.innerHTML = '<div class="row header"><span>名称</span><span>大小</span><span>下载</span><span>修改时间</span><span></span></div>';
  if (!isAtBrowseRoot(data.path)) {
    table.append(fileRow({ name: '..', path: parentPath(data.path), kind: 'directory', size: 0, modified: null }, true));
  }
  if (!data.entries.length) {
    const empty = document.createElement('div');
    empty.className = 'empty';
    empty.textContent = '这个目录还没有文件';
    table.append(empty);
  }
  data.entries.forEach((entry) => table.append(fileRow(entry)));
}

function fileRow(entry, up = false) {
  const row = document.createElement('div');
  row.className = 'row';
  const name = document.createElement('div');
  name.className = 'file-name';
  const glyph = document.createElement('span');
  glyph.textContent = up ? '↩' : icon(entry.kind);
  const open = document.createElement('button');
  open.textContent = entry.name;
  open.onclick = () => entry.kind === 'directory' ? navigate(entry.path) : location.assign(`/api/files${encodePath(entry.path)}`);
  name.append(glyph, open);

  const size = document.createElement('span');
  size.textContent = entry.kind === 'directory' ? '-' : formatSize(entry.size);
  const downloads = document.createElement('span');
  downloads.textContent = entry.kind === 'directory' ? '-' : String(entry.download_count || 0);
  const modified = document.createElement('span');
  modified.textContent = entry.modified ? new Date(entry.modified).toLocaleString() : '-';
  const actions = document.createElement('div');
  actions.className = 'actions';
  if (!up && entry.kind === 'file') {
    const download = document.createElement('button');
    download.textContent = '下载';
    download.onclick = () => location.assign(`${entry.download_url || `/api/files${encodePath(entry.path)}`}?download=1`);
    actions.append(download);
    const link = document.createElement('button');
    link.textContent = '复制链接';
    link.onclick = () => copyDownloadLink(entry);
    actions.append(link);
    if (entry.public) {
      const badge = document.createElement('button');
      badge.className = 'badge public';
      badge.textContent = '复制直链';
      badge.onclick = () => copyPublic(entry.public_url);
      actions.append(badge);
    }
    const grant = document.createElement('button');
    grant.textContent = entry.granted_users?.length ? `授权(${entry.granted_users.length})` : '授权';
    grant.onclick = () => grantFile(entry);
    actions.append(grant);
    const pub = document.createElement('button');
    pub.textContent = entry.public ? '取消公开' : '公开';
    pub.onclick = () => togglePublic(entry);
    actions.append(pub);
  }
  if (!up && state.user.permissions.can_delete) {
    const del = document.createElement('button');
    del.className = 'danger';
    del.textContent = '删除';
    del.onclick = () => deletePath(entry.path);
    actions.append(del);
  }
  row.append(name, size, downloads, modified, actions);
  return row;
}

function navigate(path) {
  const normalized = normalizeClientPath(path);
  state.path = normalized;
  history.replaceState(null, '', `/?path=${encodeURIComponent(normalized)}`);
  loadFiles().catch((err) => toast(err.message));
}

function parentPath(path) {
  if (isAtBrowseRoot(path)) return browseRoot();
  const parts = path.split('/').filter(Boolean);
  parts.pop();
  const parent = parts.length ? `/${parts.join('/')}` : '/';
  const root = browseRoot();
  if (root !== '/' && !parent.startsWith(`${root}/`) && parent !== root) {
    return root;
  }
  return parent;
}

function encodePath(path) {
  return path.split('/').filter(Boolean).map(encodeURIComponent).join('/').replace(/^/, '/');
}

function encodeApiPath(path) {
  const encoded = encodePath(path);
  return encoded === '/' ? '' : encoded;
}

function isPublicPath(path) {
  return path === '/public' || path.startsWith('/public/');
}

async function togglePublic(entry) {
  if (entry.public) {
    await api(`/api/admin/public${encodePath(entry.path)}`, { method: 'DELETE' });
    entry.public = false;
    entry.public_url = null;
    toast('已取消公开');
    if (isPublicPath(state.path)) {
      state.entries = (state.entries || []).filter((item) => item.path !== entry.path);
    }
  } else {
    const res = await api(`/api/admin/public${encodePath(entry.path)}`, { method: 'PUT' });
    entry.public = true;
    entry.public_url = res.public_url;
    copyPublic(res.public_url).catch(() => {
      toast(`已公开：${location.origin}${res.public_url}`);
    });
  }
  await loadFiles();
}

async function copyDownloadLink(entry) {
  const base = entry.download_url || `/api/files${encodePath(entry.path)}`;
  const url = `${location.origin}${base}${base.includes('?') ? '&' : '?'}download=1`;
  await copyText(url);
  toast('登录链接已复制');
}

async function copyPublic(publicUrl) {
  const url = `${location.origin}${publicUrl}`;
  await copyText(url);
  toast('直链已复制');
}

async function copyText(url) {
  if (navigator.clipboard && window.isSecureContext) {
    await navigator.clipboard.writeText(url);
  } else {
    fallbackCopy(url);
  }
}

function fallbackCopy(text) {
  const input = document.createElement('textarea');
  input.value = text;
  input.setAttribute('readonly', '');
  input.style.position = 'fixed';
  input.style.left = '-9999px';
  input.style.top = '0';
  document.body.append(input);
  input.select();
  const copied = document.execCommand('copy');
  input.remove();
  if (!copied) throw new Error('复制失败');
}

async function grantFile(entry) {
  const current = (entry.granted_users || []).join(', ');
  const value = prompt('输入要授权的用户名，多个用户用逗号分隔。留空会清空授权。', current);
  if (value === null) return;
  const users = value.split(',').map((name) => name.trim()).filter(Boolean);
  const res = await api(`/api/grants${encodePath(entry.path)}`, {
    method: 'PUT',
    body: JSON.stringify({ users }),
  });
  entry.granted_users = res.granted_users;
  toast(users.length ? '授权已更新' : '授权已清空');
  await loadFiles();
}

async function deletePath(path) {
  if (!confirm(`删除 ${path}？`)) return;
  await api(`/api/files${encodePath(path)}`, { method: 'DELETE' });
  toast('已删除');
  await loadFiles();
}

async function uploadFiles(files) {
  if (!files.length) return;
  const fileList = [...files];
  const existing = new Set((state.entries || []).map((entry) => entry.name));
  const hasConflict = fileList.some((file) => existing.has(file.name));
  let conflict = 'error';
  if (hasConflict) {
    conflict = confirm('检测到同名文件。确定覆盖已有文件？取消则自动改名上传。')
      ? 'overwrite'
      : 'rename';
  }
  const data = new FormData();
  fileList.forEach((file) => data.append('file', file));
  showUploadProgress(fileList);
  $('upload-btn').disabled = true;
  try {
    await uploadWithProgress(
      `/api/files${encodeApiPath(state.path)}?conflict=${conflict}`,
      data,
      (event) => updateUploadProgress(event, fileList)
    );
    updateUploadProgress({ lengthComputable: true, loaded: totalUploadSize(fileList), total: totalUploadSize(fileList) }, fileList);
    toast('上传完成');
    await loadFiles();
  } finally {
    $('upload-btn').disabled = false;
    $('upload-input').value = '';
    setTimeout(hideUploadProgress, 500);
  }
}

function uploadWithProgress(url, data, onProgress) {
  return new Promise((resolve, reject) => {
    const request = new XMLHttpRequest();
    request.open('PUT', url);
    request.withCredentials = true;
    request.upload.onprogress = onProgress;
    request.onload = () => {
      if (request.status >= 200 && request.status < 300) {
        resolve();
        return;
      }
      reject(new Error(uploadErrorMessage(request)));
    };
    request.onerror = () => reject(new Error('上传失败，请检查网络后重试'));
    request.onabort = () => reject(new Error('上传已取消'));
    request.send(data);
  });
}

function uploadErrorMessage(request) {
  const fallback = `上传失败（HTTP ${request.status || '网络错误'}）`;
  try {
    return JSON.parse(request.responseText).error || fallback;
  } catch {
    return request.responseText || fallback;
  }
}

function showUploadProgress(files) {
  state.upload = {
    startedAt: performance.now(),
    lastAt: performance.now(),
    lastLoaded: 0,
    speed: 0,
    saveTimer: null,
  };
  $('upload-progress').classList.remove('hidden');
  $('upload-progress-title').textContent = `正在上传 ${files.length} 个文件`;
  $('upload-progress-detail').textContent = '准备上传';
  $('upload-progress-bar').style.width = '0%';
}

function updateUploadProgress(event, files) {
  const total = event.lengthComputable ? event.total : totalUploadSize(files);
  const loaded = event.lengthComputable ? event.loaded : 0;
  if (!total) {
    $('upload-progress-detail').textContent = `正在上传 · ${formatUploadSpeed(loaded)}`;
    return;
  }
  const percent = Math.min(100, Math.round((loaded / total) * 100));
  if (percent >= 100) {
    showUploadSaving(loaded, total);
  } else {
    $('upload-progress-title').textContent = `正在上传 ${files.length} 个文件`;
    $('upload-progress-detail').textContent = `${percent}% · ${formatSize(loaded)} / ${formatSize(total)} · ${formatUploadSpeed(loaded)}`;
  }
  $('upload-progress-bar').style.width = `${percent}%`;
}

function hideUploadProgress() {
  $('upload-progress').classList.add('hidden');
  if (state.upload?.saveTimer) clearTimeout(state.upload.saveTimer);
  state.upload = null;
}

function totalUploadSize(files) {
  return files.reduce((total, file) => total + file.size, 0);
}

function formatUploadSpeed(loaded) {
  const now = performance.now();
  const upload = state.upload || { startedAt: now, lastAt: now, lastLoaded: loaded, speed: 0 };
  const elapsedSinceLast = Math.max(0, now - upload.lastAt);
  if (elapsedSinceLast >= 250 && loaded >= upload.lastLoaded) {
    const instantSpeed = ((loaded - upload.lastLoaded) * 1000) / elapsedSinceLast;
    upload.speed = upload.speed ? upload.speed * 0.65 + instantSpeed * 0.35 : instantSpeed;
    upload.lastAt = now;
    upload.lastLoaded = loaded;
    state.upload = upload;
  }
  const elapsed = Math.max(1, now - upload.startedAt);
  const averageSpeed = (loaded * 1000) / elapsed;
  const speed = upload.speed || averageSpeed;
  return `${formatSize(speed)}/s`;
}

function showUploadSaving(loaded, total) {
  $('upload-progress-title').textContent = '服务器保存中';
  $('upload-progress-detail').textContent = `100% · ${formatSize(loaded)} / ${formatSize(total)} · 等待服务器确认`;
  if (state.upload && !state.upload.saveTimer) {
    state.upload.saveTimer = setTimeout(() => {
      $('upload-progress-detail').textContent = `100% · ${formatSize(loaded)} / ${formatSize(total)} · 仍在等待服务器保存，请稍候`;
    }, 15000);
  }
}

async function createFolder() {
  const name = prompt('输入文件夹名称');
  if (name === null) return;
  const trimmed = name.trim();
  if (!trimmed) return;
  await api(`/api/files${encodeApiPath(state.path)}`, {
    method: 'POST',
    body: JSON.stringify({ name: trimmed }),
  });
  toast('文件夹已创建');
  await loadFiles();
}

async function loadUsers() {
  const users = await api('/api/admin/users');
  const table = $('users-table');
  table.innerHTML = '<div class="row header"><span>用户</span><span>角色</span><span>目录</span><span></span></div>';
  users.forEach((user) => {
    const row = document.createElement('div');
    row.className = 'row';
    row.innerHTML = `<strong>${user.username}</strong><span>${user.role}</span><span>${user.permissions.roots.join(', ')}</span>`;
    const actions = document.createElement('div');
    actions.className = 'actions';
    const edit = document.createElement('button');
    edit.textContent = '编辑';
    edit.onclick = () => fillUser(user);
    actions.append(edit);
    const del = document.createElement('button');
    del.className = 'danger';
    del.textContent = '删除';
    del.onclick = async () => {
      await api(`/api/admin/users/${encodeURIComponent(user.username)}`, { method: 'DELETE' });
      toast('用户已删除');
      loadUsers();
    };
    actions.append(del);
    row.append(actions);
    table.append(row);
  });
}

async function loadAuditLogs() {
  const logs = await api('/api/admin/audit');
  const table = $('audit-table');
  table.innerHTML = '<div class="row header audit"><span>时间</span><span>用户</span><span>动作</span><span>路径</span><span>结果</span></div>';
  if (!logs.length) {
    const empty = document.createElement('div');
    empty.className = 'empty';
    empty.textContent = '暂无活动记录';
    table.append(empty);
    return;
  }
  logs.forEach((log) => {
    const row = document.createElement('div');
    row.className = 'row audit';
    const time = document.createElement('span');
    time.textContent = new Date(log.at).toLocaleString();
    const user = document.createElement('span');
    user.textContent = log.username || '-';
    const action = document.createElement('span');
    action.textContent = auditActionLabel(log.action);
    const path = document.createElement('span');
    path.textContent = log.path || log.ip || '-';
    const status = document.createElement('span');
    status.textContent = auditResultLabel(log);
    row.append(time, user, action, path, status);
    table.append(row);
  });
}

function auditActionLabel(action) {
  return ({
    login: '登录',
    logout: '退出登录',
    download: '下载文件',
    public_download: '公开下载',
    upload: '上传文件',
    mkdir: '新建文件夹',
    delete: '删除',
    publish: '公开文件',
    unpublish: '取消公开',
    grant: '文件授权',
    create_user: '创建用户',
    update_user: '修改用户',
    delete_user: '删除用户',
  })[action] || action;
}

function auditResultLabel(log) {
  const code = Number(log.status);
  const outcome = code >= 200 && code < 300 ? '成功' : '失败';
  return log.detail ? `${outcome}：${log.detail}` : outcome;
}

function fillUser(user) {
  $('edit-username').value = user.username;
  $('user-username').value = user.username;
  $('user-username').disabled = false;
  $('user-password').value = '';
  $('user-role').value = user.role;
  $('user-roots').value = user.permissions.roots.join(', ');
  $('perm-upload').checked = user.permissions.can_upload;
  $('perm-delete').checked = user.permissions.can_delete;
  $('perm-publish').checked = user.permissions.can_publish;
}

function resetUserForm() {
  $('edit-username').value = '';
  $('user-username').disabled = false;
  $('user-form').reset();
  $('user-role').value = 'user';
  $('user-roots').value = '';
  $('perm-upload').checked = true;
  $('perm-delete').checked = true;
  $('perm-publish').checked = true;
  updateDefaultUserRoot();
}

function updateDefaultUserRoot() {
  if ($('edit-username').value) return;
  if ($('user-role').value === 'admin') {
    $('user-roots').value = '/';
    return;
  }
  const username = $('user-username').value.trim();
  $('user-roots').value = username ? `/user/${username}` : '';
}

async function saveUser(event) {
  event.preventDefault();
  const editing = $('edit-username').value;
  const payload = {
    username: $('user-username').value.trim(),
    password: $('user-password').value,
    role: $('user-role').value,
    roots: $('user-roots').value.split(',').map((v) => v.trim()).filter(Boolean),
    can_upload: $('perm-upload').checked,
    can_delete: $('perm-delete').checked,
    can_publish: $('perm-publish').checked,
  };
  await api(editing ? `/api/admin/users/${encodeURIComponent(editing)}` : '/api/admin/users', {
    method: editing ? 'PUT' : 'POST',
    body: JSON.stringify(payload),
  });
  toast(editing ? '用户已保存' : '用户已创建');
  resetUserForm();
  await loadUsers();
}

$('login-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  try {
    const session = await api('/api/login', {
      method: 'POST',
      body: JSON.stringify({ username: $('login-user').value, password: $('login-pass').value }),
    });
    state.user = session.user;
    await boot();
  } catch (err) {
    toast(err.message);
  }
});
$('logout').onclick = async () => { await api('/api/logout', { method: 'POST' }); location.reload(); };
$('nav-files').onclick = () => setView('files');
$('nav-admin').onclick = () => setView('admin');
$('refresh-btn').onclick = () => loadFiles().catch((err) => toast(err.message));
$('upload-btn').onclick = () => $('upload-input').click();
$('mkdir-btn').onclick = () => createFolder().catch((err) => toast(err.message));
$('upload-input').onchange = (event) => uploadFiles(event.target.files).catch((err) => toast(err.message));
$('user-form').addEventListener('submit', (event) => saveUser(event).catch((err) => toast(err.message)));
$('reset-user-form').onclick = resetUserForm;
$('user-username').addEventListener('input', updateDefaultUserRoot);
$('user-role').addEventListener('change', updateDefaultUserRoot);

boot().catch((err) => toast(err.message));
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_upload_limit_rejects_zero_and_invalid_values() {
        assert_eq!(parse_upload_limit("1024").unwrap(), 1024);
        assert!(parse_upload_limit("0").is_err());
        assert!(parse_upload_limit("not-a-number").is_err());
    }

    #[test]
    fn public_url_maps_back_to_source_path() {
        assert_eq!(
            public_url_for_source_path("/docs/report.pdf"),
            "/public/docs/report.pdf"
        );
        assert_eq!(
            source_path_from_public_url("/public/docs/report.pdf"),
            "/docs/report.pdf"
        );
    }
}
