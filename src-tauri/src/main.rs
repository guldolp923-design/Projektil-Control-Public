#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
#![recursion_limit = "256"]
mod oca;
use base64::{engine::general_purpose, Engine as _};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager,
};
use std::fs;
use std::fs::OpenOptions;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::io::{Write, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use chrono::{Local, TimeZone};
use futures_util::{SinkExt, StreamExt};
use tiny_http::{Header, Method, Response, Server, StatusCode};
use tokio::time::timeout;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

struct CameraStreamHandle {
    child: std::process::Child,
    latest_frame: Arc<Mutex<Option<Vec<u8>>>>,
    last_frame_at: Arc<Mutex<Instant>>,
    last_used: Instant,
}

static CAMERA_STREAMS: OnceLock<Mutex<HashMap<String, CameraStreamHandle>>> = OnceLock::new();
static APP_HANDLE: OnceLock<AppHandle> = OnceLock::new();
static FFMPEG_SETUP_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static SCHEDULER_MODULE_RESOLVED: OnceLock<Mutex<String>> = OnceLock::new();
const CAMERA_MJPEG_PORT: u16 = 41777;
// Preferred: short URL without a port suffix. Falls back to LAN_WEB_PORT_FALLBACK
// if 80 is already taken (router admin UI, IIS, Skype, etc. commonly use it).
const LAN_WEB_PORT_PREFERRED: u16 = 80;
const LAN_WEB_PORT_FALLBACK: u16 = 41778;
static ACTIVE_LAN_WEB_PORT: OnceLock<u16> = OnceLock::new();
const OSC_LISTENER_PORT: u16 = 9001;
const OSC_LISTENER_ADDR: &str = "0.0.0.0:9001";
const OSC_BUFFER_SIZE: usize = 256;
const OSC_EMERGENCY_CMD: &[u8] = b"/emergency_pressed";
const OSC_EMERGENCY_CMD_LEN: usize = 18;
const DEFAULT_PIXERA_SCHEDULER_MODULE: &str = "Projektil_EventScheduler_V2_7";
const FALLBACK_PIXERA_SCHEDULER_MODULES: &[&str] = &[
    DEFAULT_PIXERA_SCHEDULER_MODULE,
    "Projektil_EventScheduler_V27",
    "Projektil_EventScheduler_V21",
    "EventScheduler_Projektil_V17",
];

// ============================================================
// QUERY CACHE & DEVICE HEALTH
// ============================================================
const QUERY_CACHE_TTL_MS: u64 = 30000;   // 30 seconds
const DEVICE_HEALTH_CHECK_INTERVAL_MS: u64 = 60000;  // 1 minute
const DEVICE_OFFLINE_THRESHOLD: u64 = 180000;  // 3 minutes

#[derive(Debug, Clone)]
struct CachedQuery {
    data: String,
    timestamp_ms: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct DeviceHealthStatus {
    pub device_id: String,
    pub is_online: bool,
    pub last_seen_ms: u64,
    pub consecutive_failures: u32,
    pub last_error: Option<String>,
}

/// Error categories for better logging and monitoring
#[derive(Debug, Clone, Copy)]
enum ErrorCategory {
    ConnectionTimeout,
    DeviceOffline,
    InvalidResponse,
    AuthenticationFailed,
    ConfigurationError,
    InternalError,
}

impl ErrorCategory {
    fn as_str(&self) -> &'static str {
        match self {
            Self::ConnectionTimeout => "CONNECTION_TIMEOUT",
            Self::DeviceOffline => "DEVICE_OFFLINE",
            Self::InvalidResponse => "INVALID_RESPONSE",
            Self::AuthenticationFailed => "AUTH_FAILED",
            Self::ConfigurationError => "CONFIG_ERROR",
            Self::InternalError => "INTERNAL_ERROR",
        }
    }
}

static QUERY_CACHE: OnceLock<Mutex<HashMap<String, CachedQuery>>> = OnceLock::new();
static DEVICE_HEALTH: OnceLock<Mutex<HashMap<String, DeviceHealthStatus>>> = OnceLock::new();

fn query_cache() -> &'static Mutex<HashMap<String, CachedQuery>> {
    QUERY_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn device_health() -> &'static Mutex<HashMap<String, DeviceHealthStatus>> {
    DEVICE_HEALTH.get_or_init(|| Mutex::new(HashMap::new()))
}

fn log_error_with_category(category: ErrorCategory, message: &str, device_id: Option<&str>, app: Option<&AppHandle>) {
    let log_msg = format!("[{}] {}", category.as_str(), message);
    
    if let Some(d_id) = device_id {
        if let Ok(mut health) = device_health().lock() {
            let entry = health.entry(d_id.to_string()).or_insert_with(|| {
                DeviceHealthStatus {
                    device_id: d_id.to_string(),
                    is_online: true,
                    last_seen_ms: now_timestamp_ms(),
                    consecutive_failures: 0,
                    last_error: None,
                }
            });
            
            let was_online = entry.is_online;
            // Only increment when transitioning online->offline, not on every repeated poll failure
            if was_online {
                entry.consecutive_failures += 1;
            }
            entry.last_error = Some(message.to_string());
            
            if entry.consecutive_failures >= 3 {
                entry.is_online = false;
            }
            
            // Only print to stderr on state change to avoid log spam
            if was_online {
                eprintln!("{}", log_msg);
            }
        }
    } else {
        eprintln!("{}", log_msg);
    }
    
    if let Some(app) = app {
        let _ = write_app_log("error", &log_msg, now_timestamp_ms(), Some(app));
    }
}

fn get_or_cache_query(key: &str, fetch_fn: impl FnOnce() -> Option<String>) -> Option<String> {
    // Check cache
    if let Ok(cache) = query_cache().lock() {
        if let Some(cached) = cache.get(key) {
            let age_ms = now_timestamp_ms().saturating_sub(cached.timestamp_ms);
            if age_ms < QUERY_CACHE_TTL_MS {
                return Some(cached.data.clone());
            }
        }
    }
    
    // Fetch fresh data
    let result = fetch_fn()?;
    
    // Store in cache
    if let Ok(mut cache) = query_cache().lock() {
        cache.insert(key.to_string(), CachedQuery {
            data: result.clone(),
            timestamp_ms: now_timestamp_ms(),
        });
    }
    
    Some(result)
}

fn mark_device_online(device_id: &str) {
    if let Ok(mut health) = device_health().lock() {
        let entry = health.entry(device_id.to_string()).or_insert_with(|| {
            DeviceHealthStatus {
                device_id: device_id.to_string(),
                is_online: true,
                last_seen_ms: now_timestamp_ms(),
                consecutive_failures: 0,
                last_error: None,
            }
        });
        
        entry.is_online = true;
        entry.last_seen_ms = now_timestamp_ms();
        entry.consecutive_failures = 0;
        entry.last_error = None;
    }
}
const WEBGUI_INDEX_HTML: &str = include_str!("../../frontend/index.html");
const WEBGUI_UTILS_JS: &str = include_str!("../../frontend/js/utils.js");
const WEBGUI_TAURI_BRIDGE_JS: &str = include_str!("../../frontend/js/tauri-bridge.js");
const WEBGUI_FAVICON_ICO: &[u8] = include_bytes!("../../frontend/favicon.ico");
const CAMERA_STREAM_IDLE_TIMEOUT_SECS: u64 = 90;
const CAMERA_STREAM_STALE_TIMEOUT_SECS: u64 = 60;
const CAMERA_STREAM_FIRST_FRAME_TIMEOUT_SECS: u64 = 20;
const CAMERA_STREAM_WRITE_TIMEOUT_MS: u64 = 25000;
const FFMPEG_RUNTIME_DOWNLOAD_URL: &str = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip";
const LOG_RETENTION_DAYS: u64 = 90;
const LOG_RETENTION_MS: u64 = LOG_RETENTION_DAYS * 24 * 60 * 60 * 1000;
const LOG_PRUNE_INTERVAL_MS: u64 = 12 * 60 * 60 * 1000;
const LOG_ROTATION_SIZE_BYTES: u64 = 50 * 1024 * 1024;  // 50MB per log file
const LOG_COMPRESSION_THRESHOLD_DAYS: u64 = 7;  // Compress logs older than 7 days

static APP_LOG_DIR: OnceLock<PathBuf> = OnceLock::new();
static LAST_LOG_PRUNE_MS: OnceLock<Mutex<u64>> = OnceLock::new();
static TELEGRAM_LAST_SENT: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
static CONFIG_VERSIONS: OnceLock<Mutex<Vec<ConfigVersion>>> = OnceLock::new();

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ConfigVersion {
    pub timestamp_ms: u64,
    pub version_num: u32,
    pub hash: String,
    pub backup_path: String,
}

fn config_versions() -> &'static Mutex<Vec<ConfigVersion>> {
    CONFIG_VERSIONS.get_or_init(|| Mutex::new(Vec::new()))
}

// ============================================================
// CONNECTION POOLING
// ============================================================
#[derive(Debug, Clone)]
struct PooledConnection {
    pub host: String,
    pub port: u16,
    pub last_used_ms: u64,
    pub connection_type: String,  // "snmp", "modbus_tcp"
}

static CONNECTION_POOL: OnceLock<Mutex<HashMap<String, PooledConnection>>> = OnceLock::new();

fn connection_pool() -> &'static Mutex<HashMap<String, PooledConnection>> {
    CONNECTION_POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

fn get_pooled_connection(host: &str, port: u16, conn_type: &str) -> String {
    let key = format!("{}:{}:{}", conn_type, host, port);
    
    if let Ok(mut pool) = connection_pool().lock() {
        if let Some(conn) = pool.get(&key) {
            // Reuse if used in last 5 minutes
            if now_timestamp_ms().saturating_sub(conn.last_used_ms) < 300000 {
                return key;
            }
        }
        
        // Add/update connection in pool
        pool.insert(key.clone(), PooledConnection {
            host: host.to_string(),
            port,
            last_used_ms: now_timestamp_ms(),
            connection_type: conn_type.to_string(),
        });
    }
    key
}

// ============================================================
// RATE LIMITING
// ============================================================
static RATE_LIMIT_TRACKER: OnceLock<Mutex<HashMap<String, Vec<u64>>>> = OnceLock::new();
const RATE_LIMIT_WINDOW_MS: u64 = 60000;  // 1 minute
const RATE_LIMIT_MAX_REQUESTS: usize = 100;  // Max 100 requests per minute per endpoint

fn rate_limit_tracker() -> &'static Mutex<HashMap<String, Vec<u64>>> {
    RATE_LIMIT_TRACKER.get_or_init(|| Mutex::new(HashMap::new()))
}

fn check_rate_limit(endpoint: &str) -> Result<(), String> {
    let now = now_timestamp_ms();
    
    if let Ok(mut tracker) = rate_limit_tracker().lock() {
        let timestamps = tracker.entry(endpoint.to_string()).or_insert_with(Vec::new);
        
        // Remove old timestamps outside window
        timestamps.retain(|t| now.saturating_sub(*t) < RATE_LIMIT_WINDOW_MS);
        
        // Check if limit exceeded
        if timestamps.len() >= RATE_LIMIT_MAX_REQUESTS {
            return Err(format!("Rate limit exceeded for {}: {} requests/min", endpoint, RATE_LIMIT_MAX_REQUESTS));
        }
        
        // Add current timestamp
        timestamps.push(now);
    }
    
    Ok(())
}

// ============================================================
// REQUEST DEDUPLICATION
// ============================================================
#[derive(Debug, Clone)]
struct DeduplicatedRequest {
    pub result: String,
    pub timestamp_ms: u64,
}

static DEDUP_REQUESTS: OnceLock<Mutex<HashMap<String, DeduplicatedRequest>>> = OnceLock::new();
const DEDUP_WINDOW_MS: u64 = 5000;  // 5 seconds

fn dedup_requests() -> &'static Mutex<HashMap<String, DeduplicatedRequest>> {
    DEDUP_REQUESTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn get_deduplicated_request(key: &str, fetch_fn: impl FnOnce() -> Option<String>) -> Option<String> {
    let now = now_timestamp_ms();
    
    if let Ok(dedup) = dedup_requests().lock() {
        if let Some(cached) = dedup.get(key) {
            if now.saturating_sub(cached.timestamp_ms) < DEDUP_WINDOW_MS {
                return Some(cached.result.clone());
            }
        }
    }
    
    // Fetch fresh data
    let result = fetch_fn()?;
    
    if let Ok(mut dedup) = dedup_requests().lock() {
        dedup.insert(key.to_string(), DeduplicatedRequest {
            result: result.clone(),
            timestamp_ms: now,
        });
        
        // Cleanup old entries
        dedup.retain(|_, v| now.saturating_sub(v.timestamp_ms) < DEDUP_WINDOW_MS * 2);
    }
    
    Some(result)
}

fn current_webgui_index_html() -> String {
    let disk_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../frontend/index.html");
    fs::read_to_string(disk_path).unwrap_or_else(|_| WEBGUI_INDEX_HTML.to_string())
}

// Serves the small set of static JS assets the LAN mobile/webgui view depends on
// (index.html only references js/utils.js today). Falls back to the compiled-in
// copy when the frontend folder isn't shipped next to the release binary.
fn current_webgui_asset(rel_path: &str) -> Option<(String, &'static str)> {
    let (embedded, content_type): (&'static str, &'static str) = match rel_path {
        "js/utils.js" => (WEBGUI_UTILS_JS, "application/javascript; charset=utf-8"),
        "js/tauri-bridge.js" => (WEBGUI_TAURI_BRIDGE_JS, "application/javascript; charset=utf-8"),
        _ => return None,
    };
    let disk_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../frontend").join(rel_path);
    let body = fs::read_to_string(disk_path).unwrap_or_else(|_| embedded.to_string());
    Some((body, content_type))
}

// Same Windows shortcut icon as the desktop app, served as the browser favicon.
fn current_webgui_favicon() -> Vec<u8> {
    let disk_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../frontend/favicon.ico");
    fs::read(disk_path).unwrap_or_else(|_| WEBGUI_FAVICON_ICO.to_vec())
}

const TELEGRAM_MIN_REPEAT_MS: u64 = 60_000;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct AppLogEntry {
    timestamp_ms: u64,
    level: String,
    message: String,
}

fn now_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn cutoff_timestamp_ms() -> u64 {
    now_timestamp_ms().saturating_sub(LOG_RETENTION_MS)
}

fn resolve_log_dir(app: Option<&AppHandle>) -> Result<PathBuf, String> {
    if let Some(dir) = APP_LOG_DIR.get() {
        return Ok(dir.clone());
    }

    if let Some(handle) = app {
        let dir = handle
            .path()
            .app_data_dir()
            .map_err(|e| format!("App data dir konnte nicht ermittelt werden: {}", e))?
            .join("logs");
        fs::create_dir_all(&dir)
            .map_err(|e| format!("Log-Verzeichnis konnte nicht erstellt werden: {}", e))?;
        let _ = APP_LOG_DIR.set(dir.clone());
        return Ok(dir);
    }

    Err("Log-Verzeichnis ist nicht initialisiert".to_string())
}

fn system_log_path(app: Option<&AppHandle>) -> Result<PathBuf, String> {
    Ok(resolve_log_dir(app)?.join("system.log"))
}

fn error_log_path(app: Option<&AppHandle>) -> Result<PathBuf, String> {
    Ok(resolve_log_dir(app)?.join("error.log"))
}

fn read_log_entries(path: &PathBuf) -> Vec<AppLogEntry> {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    let cutoff = cutoff_timestamp_ms();
    BufReader::new(file)
        .lines()
        .filter_map(|line| {
            let text = line.ok()?;
            let entry = serde_json::from_str::<AppLogEntry>(&text).ok()?;
            if entry.timestamp_ms >= cutoff {
                Some(entry)
            } else {
                None
            }
        })
        .collect()
}

fn rewrite_log_entries(path: &PathBuf, entries: &[AppLogEntry]) -> Result<(), String> {
    let mut out = String::new();
    for entry in entries {
        let line = serde_json::to_string(entry)
            .map_err(|e| format!("Log-Serialisierung fehlgeschlagen: {}", e))?;
        out.push_str(&line);
        out.push('\n');
    }
    fs::write(path, out).map_err(|e| format!("Log-Datei konnte nicht geschrieben werden: {}", e))
}

fn append_log_entry(path: &PathBuf, entry: &AppLogEntry) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Ungültiger Log-Dateipfad".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|e| format!("Log-Verzeichnis konnte nicht erstellt werden: {}", e))?;

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("Log-Datei konnte nicht geöffnet werden: {}", e))?;

    let line = serde_json::to_string(entry)
        .map_err(|e| format!("Log-Serialisierung fehlgeschlagen: {}", e))?;
    writeln!(file, "{}", line).map_err(|e| format!("Log-Eintrag konnte nicht geschrieben werden: {}", e))
}

fn prune_old_logs(app: Option<&AppHandle>) -> Result<(), String> {
    let sys_path = system_log_path(app)?;
    let err_path = error_log_path(app)?;

    let sys_entries = read_log_entries(&sys_path);
    let err_entries = read_log_entries(&err_path);

    rewrite_log_entries(&sys_path, &sys_entries)?;
    rewrite_log_entries(&err_path, &err_entries)?;
    Ok(())
}

fn maybe_prune_logs(app: Option<&AppHandle>) {
    let now = now_timestamp_ms();
    let gate = LAST_LOG_PRUNE_MS.get_or_init(|| Mutex::new(0));
    let mut should_prune = false;

    if let Ok(mut last) = gate.lock() {
        if now.saturating_sub(*last) >= LOG_PRUNE_INTERVAL_MS {
            *last = now;
            should_prune = true;
        }
    }

    if should_prune {
        let _ = prune_old_logs(app);
        let _ = rotate_logs_if_needed(app);
    }
}

fn rotate_logs_if_needed(app: Option<&AppHandle>) -> Result<(), String> {
    let log_dir = resolve_log_dir(app)?;
    
    for log_file in &["system.log", "error.log"] {
        let path = log_dir.join(log_file);
        if let Ok(metadata) = fs::metadata(&path) {
            if metadata.len() > LOG_ROTATION_SIZE_BYTES {
                let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
                let rotated = log_dir.join(format!("{}.{}", log_file, timestamp));
                let _ = fs::rename(&path, &rotated);
                
                // Try to compress older logs
                let _ = compress_old_logs(&log_dir, LOG_COMPRESSION_THRESHOLD_DAYS);
            }
        }
    }
    Ok(())
}

fn compress_old_logs(log_dir: &PathBuf, threshold_days: u64) -> Result<(), String> {
    use std::fs::File;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    
    let cutoff_ms = now_timestamp_ms().saturating_sub(threshold_days * 24 * 60 * 60 * 1000);
    
    for entry in fs::read_dir(log_dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        
        if path.extension().map_or(false, |ext| ext == "log") {
            if let Ok(metadata) = fs::metadata(&path) {
                let file_time_ms = metadata.modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                
                // Compress if older than threshold and not already compressed
                if file_time_ms < cutoff_ms && !path.to_string_lossy().ends_with(".gz") {
                    let gz_path = format!("{}.gz", path.to_string_lossy());
                    
                    match File::open(&path) {
                        Ok(input) => {
                            match File::create(&gz_path) {
                                Ok(output) => {
                                    let mut encoder = GzEncoder::new(output, Compression::default());
                                    match std::io::copy(&mut std::io::BufReader::new(input), &mut encoder) {
                                        Ok(_) => {
                                            if encoder.finish().is_ok() {
                                                let _ = fs::remove_file(&path);
                                                eprintln!("[LOG-COMPRESS] Compressed: {}", path.display());
                                            }
                                        }
                                        Err(e) => eprintln!("[LOG-COMPRESS] Copy failed: {}", e),
                                    }
                                }
                                Err(e) => eprintln!("[LOG-COMPRESS] Create gz failed: {}", e),
                            }
                        }
                        Err(e) => eprintln!("[LOG-COMPRESS] Open log failed: {}", e),
                    }
                }
            }
        }
    }
    Ok(())
}

fn backup_config_on_change(config_path: &Path, config_content: &str) -> Result<(), String> {
    let config_dir = config_path.parent().ok_or_else(|| "Invalid config path".to_string())?;
    let backups_dir = config_dir.join("config_backups");
    fs::create_dir_all(&backups_dir)
        .map_err(|e| format!("Failed to create backups dir: {}", e))?;
    
    // Calculate hash of current config
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    config_content.hash(&mut hasher);
    let hash = format!("{:x}", hasher.finish());
    
    // Check if this is a new version (hash changed)
    let versions = config_versions().lock().ok();
    let is_new_version = versions.as_ref().map_or(true, |v| {
        v.last().map_or(true, |last| last.hash != hash)
    });
    
    if is_new_version {
        let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
        let version_num = versions.as_ref().map_or(1, |v| v.len() as u32 + 1);
        let backup_path = backups_dir.join(format!("config.v{}.{}.json", version_num, timestamp));
        drop(versions);
        
        fs::write(&backup_path, config_content)
            .map_err(|e| format!("Failed to backup config: {}", e))?;
        
        // Record in version history
        if let Ok(mut versions) = config_versions().lock() {
            versions.push(ConfigVersion {
                timestamp_ms: now_timestamp_ms(),
                version_num,
                hash: hash.clone(),
                backup_path: backup_path.to_string_lossy().to_string(),
            });
            
            // Keep only last 50 versions
            if versions.len() > 50 {
                versions.remove(0);
            }
        }
    }
    
    Ok(())
}

fn write_app_log(level: &str, message: &str, timestamp_ms: u64, app: Option<&AppHandle>) -> Result<(), String> {
    let clean_level = if level.eq_ignore_ascii_case("error") {
        "error".to_string()
    } else {
        "info".to_string()
    };
    let clean_message = message.replace('\n', " ").replace('\r', " ");
    let entry = AppLogEntry {
        timestamp_ms,
        level: clean_level.clone(),
        message: clean_message,
    };

    let sys_path = system_log_path(app)?;
    append_log_entry(&sys_path, &entry)?;

    if clean_level == "error" {
        let err_path = error_log_path(app)?;
        append_log_entry(&err_path, &entry)?;
    }

    maybe_prune_logs(app);
    Ok(())
}

fn classify_error_category(message: &str) -> &'static str {
    let m = message.to_ascii_lowercase();
    if m.contains("[ups]") || m.contains("batterie") || m.contains("ups") {
        "ups"
    } else if m.contains("[janitza]") || m.contains("spannung") || m.contains("frequenz") {
        "power"
    } else if m.contains("[pixera") || m.contains("timeline") || m.contains("scheduler") {
        "pixera"
    } else if m.contains("pj-link") || m.contains("projector") || m.contains("projektor") {
        "projector"
    } else if m.contains("cam 0") || m.contains("camera") {
        "camera"
    } else if m.contains("[startup]") {
        "startup"
    } else {
        "system"
    }
}

fn normalize_text(input: &str) -> String {
    input
    .to_lowercase()
        .replace('ä', "ae")
        .replace('ö', "oe")
        .replace('ü', "ue")
        .replace('ß', "ss")
}

fn detect_country_code_from_location(location: &str) -> Option<&'static str> {
    let l = normalize_text(location);

    let city_matchers: [(&str, &[&str]); 11] = [
        ("CH", &["luzern", "lucerne", "zuerich", "zurich", "bern", "basel", "lausanne", "lugano", "st. gallen", "winterthur"]),
        ("FR", &["paris", "lyon", "marseille", "toulouse", "lille", "strasbourg"]),
        ("DE", &["berlin", "muenchen", "munich", "hamburg", "koeln", "cologne", "frankfurt"]),
        ("AT", &["wien", "vienna", "salzburg", "innsbruck", "graz", "linz"]),
        ("IT", &["roma", "rome", "milano", "milan", "florence", "firenze", "venice", "venezia"]),
        ("ES", &["madrid", "barcelona", "sevilla", "seville", "valencia"]),
        ("NL", &["amsterdam", "rotterdam", "den haag", "the hague", "utrecht"]),
        ("BE", &["brussels", "bruessel", "bruxelles", "antwerp", "antwerpen", "gent", "ghent"]),
        ("GB", &["london", "manchester", "birmingham", "edinburgh", "glasgow"]),
        ("CZ", &["prague", "praha", "prag"]),
        ("US", &["new york", "nyc", "los angeles", "chicago"]),
    ];

    for (cc, cities) in city_matchers {
        if cities.iter().any(|c| l.contains(c)) {
            return Some(cc);
        }
    }

    let country_matchers: [(&str, &[&str]); 18] = [
        ("CH", &["schweiz", "switzerland", "suisse", "svizzera", " ch ", " ch,"]),
        ("DE", &["deutschland", "germany", " de ", " de,"]),
        ("AT", &["oesterreich", "austria", " at ", " at,"]),
        ("FR", &["frankreich", "france", " fr ", " fr,"]),
        ("IT", &["italien", "italy", " it ", " it,"]),
        ("US", &["usa", "united states", " us ", " us,"]),
        ("GB", &["uk", "united kingdom", "great britain", " gb ", " gb,"]),
        ("ES", &["spanien", "spain", " es ", " es,"]),
        ("NL", &["niederlande", "netherlands", "holland", " nl ", " nl,"]),
        ("BE", &["belgien", "belgium", " be ", " be,"]),
        ("IE", &["irland", "ireland", " ie ", " ie,"]),
        ("PT", &["portugal", " pt ", " pt,"]),
        ("PL", &["polen", "poland", " pl ", " pl,"]),
        ("CZ", &["tschechien", "czech", " cz ", " cz,"]),
        ("DK", &["daenemark", "denmark", " dk ", " dk,"]),
        ("SE", &["schweden", "sweden", " se ", " se,"]),
        ("NO", &["norwegen", "norway", " no ", " no,"]),
        ("FI", &["finnland", "finland", " fi ", " fi,"]),
    ];

    for (cc, keys) in country_matchers {
        if keys.iter().any(|k| l.contains(k)) {
            return Some(cc);
        }
    }

    None
}

fn country_code_to_flag_emoji(code: &str) -> Option<String> {
    let cc = code.trim().to_ascii_uppercase();
    if cc.len() != 2 || !cc.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let mut out = String::new();
    for ch in cc.chars() {
        let base = 0x1F1E6u32;
        let offset = (ch as u32).saturating_sub('A' as u32);
        out.push(char::from_u32(base + offset)?);
    }
    Some(out)
}

fn format_telegram_location_label(location: &str) -> String {
    let country = detect_country_code_from_location(location);
    let flag = country
        .and_then(country_code_to_flag_emoji)
        .unwrap_or_else(|| "📍".to_string());

    if cfg!(target_os = "windows") {
        if let Some(cc) = country {
            return format!("{} [{}] {}", flag, cc, location);
        }
    }

    format!("{}{}", flag, location)
}

fn format_human_datetime(timestamp_ms: u64) -> String {
    Local
        .timestamp_millis_opt(timestamp_ms as i64)
        .single()
        .unwrap_or_else(Local::now)
        .format("%d.%m.%Y, %H:%M:%S")
        .to_string()
}

fn extract_runtime_from_message(message: &str) -> Option<String> {
    let lower = message.to_ascii_lowercase();
    let idx = lower.find("runtime:")?;
    let rest = message.get(idx + "runtime:".len()..)?.trim();
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_string())
    }
}

fn strip_runtime_from_message(message: &str) -> String {
    let lower = message.to_ascii_lowercase();
    if let Some(idx) = lower.find("runtime:") {
        return message[..idx].trim().trim_end_matches('-').trim().to_string();
    }
    message.trim().to_string()
}

fn is_resolution_message(message: &str) -> bool {
    message.trim_start().starts_with("[RESOLVED]")
}

fn strip_resolution_prefix(message: &str) -> String {
    message
        .trim_start()
        .strip_prefix("[RESOLVED]")
        .unwrap_or(message)
        .trim()
        .to_string()
}

fn contains_critical_keyword(message: &str, cfg: &serde_json::Value) -> bool {
    let m = message.to_ascii_lowercase();

    let built_in_keywords = [
        "sofortalarm",
        "akku warnung",
        "akkustand unter 20",
        "ups hat auf batteriebetrieb gewechselt",
        "batterie mode aktiviert",
        "panic:",
        "emergency",
        "ueberfrequenz",
        "unterfrequenz",
        "offline!",
    ];

    if built_in_keywords.iter().any(|k| m.contains(k)) {
        return true;
    }

    cfg["telegram"]["critical_error_keywords"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .any(|kw| m.contains(&kw))
        })
        .unwrap_or(false)
}

fn telegram_event_enabled(cfg: &serde_json::Value, event_key: &str) -> bool {
    cfg["telegram"]["alert_events"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim().to_ascii_lowercase())
                .any(|v| {
                    v == event_key
                        || (event_key.ends_with("_offline") && v == "offline")
                })
        })
        .unwrap_or(true)
}

fn detect_telegram_alert_events(message: &str, cfg: &serde_json::Value) -> Vec<String> {
    let m = normalize_text(message);
    let mut out: Vec<String> = Vec::new();

    let mut push = |key: &str| {
        if !out.iter().any(|k| k == key) {
            out.push(key.to_string());
        }
    };

    if m.contains("missed event")
        || m.contains("not fired")
        || m.contains("nicht ausgeloest")
        || m.contains("nicht ausgelost")
    {
        push("trigger_missed");
    }

    if m.contains("timeline error") || m.contains("ontimelineerror") {
        push("timeline_error");
    }

    if m.contains("offline!") || m.contains(" ist offline") {
        push("offline");
        if m.contains("janitza") {
            push("janitza_offline");
        }
        if m.contains("ups") {
            push("ups_offline");
        }
        if m.contains("nas") {
            push("nas_offline");
        }
        if m.contains("poe") || m.contains("switch") {
            push("poe_switch_offline");
        }
        if m.contains("rutx") || m.contains("router") {
            push("rutx_offline");
        }
        if m.contains("pixera") || m.contains("director") || m.contains("octo") {
            push("pixera_offline");
        }
    }

    if m.contains("snmp-werte von switch") || m.contains("snmp values") && m.contains("switch") {
        push("poe_switch_offline");
    }
    if m.contains("snmp-werte von router") || m.contains("snmp values") && (m.contains("router") || m.contains("rutx")) {
        push("rutx_offline");
    }
    if m.contains("snmp-werte von nas") || m.contains("snmp values") && m.contains("nas") {
        push("nas_offline");
    }

    if m.contains("phasen-unsymmetrie") || m.contains("asymmetrie") || m.contains("asymmetry") {
        push("janitza_asymmetry");
    }

    if m.contains("ueberfrequenz") || m.contains("uberfrequenz") || m.contains("overfrequency") {
        push("janitza_overfrequency");
    }

    if m.contains("unterfrequenz") || m.contains("underfrequency") {
        push("janitza_underfrequency");
    }

    if m.contains("batteriebetrieb") || m.contains("battery mode") || m.contains("akku warnung") {
        push("ups_battery");
    }

    if m.contains("emergency") || m.contains("sofortalarm") {
        push("emergency");
    }

    if m.contains("panic:") {
        push("panic");
    }

    if contains_critical_keyword(message, cfg) {
        push("keyword_match");
    }

    out
}

fn should_send_telegram_for_error(level: &str, message: &str, cfg: &serde_json::Value) -> bool {
    if !level.eq_ignore_ascii_case("error") {
        return false;
    }
    detect_telegram_alert_events(message, cfg)
        .iter()
        .any(|k| telegram_event_enabled(cfg, k))
}

fn telegram_rate_limited(fingerprint: &str, timestamp_ms: u64) -> bool {
    let gate = TELEGRAM_LAST_SENT.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = match gate.lock() {
        Ok(m) => m,
        Err(_) => return true,
    };

    if let Some(last) = map.get(fingerprint) {
        if timestamp_ms.saturating_sub(*last) < TELEGRAM_MIN_REPEAT_MS {
            return true;
        }
    }

    map.insert(fingerprint.to_string(), timestamp_ms);
    false
}

fn build_telegram_alarm_text(cfg: &serde_json::Value, message: &str, timestamp_ms: u64) -> String {
    let location = cfg["location_name"].as_str().unwrap_or("Standort unbekannt").trim();
    let location = if location.is_empty() { "Standort unbekannt" } else { location };
    let location_label = format_telegram_location_label(location);
    let anydesk = cfg["anydesk_address"].as_str().unwrap_or("").trim();

    let message_clean = strip_runtime_from_message(message);
    let mut lines = vec![
        format!("🚨{}🚨", location_label),
        format_human_datetime(timestamp_ms),
        message_clean,
    ];

    let is_ups_battery = message.to_ascii_lowercase().contains("batteriebetrieb");
    if is_ups_battery {
        if let Some(rt) = extract_runtime_from_message(message) {
            lines.push(format!("Runtime: {}", rt));
        }
    }

    if !anydesk.is_empty() {
        lines.push(format!(
            "Anydeskadresse: <a href=\"anydesk://{}\">{}</a>",
            anydesk, anydesk
        ));
    }

    lines.join("\n")
}

fn build_telegram_resolved_text(cfg: &serde_json::Value, message: &str, timestamp_ms: u64) -> String {
    let location = cfg["location_name"].as_str().unwrap_or("Standort unbekannt").trim();
    let location = if location.is_empty() { "Standort unbekannt" } else { location };
    let location_label = format_telegram_location_label(location);

    let resolved = strip_resolution_prefix(message);
    let text = if resolved.is_empty() {
        "Error geloest".to_string()
    } else {
        resolved
    };

    format!(
        "✅{}✅\n{}\n{}",
        location_label,
        format_human_datetime(timestamp_ms),
        text
    )
}

fn send_telegram_message_async(bot_token: String, chat_id: String, text: String) {
    thread::spawn(move || {
        let url = format!("https://api.telegram.org/bot{}/sendMessage", bot_token);
        let client = match reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(6))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = write_app_log("error", &format!("Telegram client init failed: {}", e), now_timestamp_ms(), None);
                return;
            }
        };

        let form = [
            ("chat_id", chat_id.clone()),
            ("text", text),
            ("parse_mode", "HTML".to_string()),
            ("disable_web_page_preview", "true".to_string()),
        ];

        match client.post(url).form(&form).send() {
            Ok(resp) => {
                let status = resp.status();
                if !status.is_success() {
                    let body = resp.text().unwrap_or_else(|_| "no body".to_string());
                    let _ = write_app_log(
                        "error",
                        &format!("Telegram send to {} failed with status {}. Response: {}", chat_id, status, body),
                        now_timestamp_ms(),
                        None,
                    );
                } else {
                    let _ = write_app_log(
                        "info",
                        &format!("Telegram message sent successfully to {}", chat_id),
                        now_timestamp_ms(),
                        None,
                    );
                }
            }
            Err(e) => {
                let _ = write_app_log("error", &format!("Telegram send to {} request failed: {}", chat_id, e), now_timestamp_ms(), None);
            }
        }
    });
}

fn maybe_send_critical_telegram(level: &str, message: &str, timestamp_ms: u64) {
    let cfg = get_config();
    if !cfg["telegram"]["enabled"].as_bool().unwrap_or(false) {
        return;
    }

    let bot_token = cfg["telegram"]["bot_token"].as_str().unwrap_or("").trim().to_string();
    let chat_id = cfg["telegram"]["chat_id"].as_str().unwrap_or("").trim().to_string();
    let channel_id = cfg["telegram"]["channel_id"].as_str().unwrap_or("").trim().to_string();
    if bot_token.is_empty() || chat_id.is_empty() {
        return;
    }

    if is_resolution_message(message) && level.eq_ignore_ascii_case("info") {
        let resolved = strip_resolution_prefix(message);
        if classify_error_category(&resolved) != "ups" {
            return;
        }
        if !telegram_event_enabled(&cfg, "ups_battery") {
            return;
        }
        let fingerprint = format!("resolved:{}", resolved);
        if telegram_rate_limited(&fingerprint, timestamp_ms) {
            return;
        }
        let text = build_telegram_resolved_text(&cfg, message, timestamp_ms);
        send_telegram_message_async(bot_token.clone(), chat_id.clone(), text.clone());
        if !channel_id.is_empty() {
            send_telegram_message_async(bot_token.clone(), channel_id, text);
        }
        return;
    }

    if !should_send_telegram_for_error(level, message, &cfg) {
        return;
    }

    let category = classify_error_category(message);
    let fingerprint = format!("{}:{}", category, message);
    if telegram_rate_limited(&fingerprint, timestamp_ms) {
        return;
    }

    let text = build_telegram_alarm_text(&cfg, message, timestamp_ms);
    send_telegram_message_async(bot_token.clone(), chat_id.clone(), text.clone());
    if !channel_id.is_empty() {
        send_telegram_message_async(bot_token, channel_id, text);
    }
}

fn install_panic_logging_hook() {
    std::panic::set_hook(Box::new(|panic_info| {
        let location = panic_info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "unbekannter Ort".to_string());
        let payload = if let Some(msg) = panic_info.payload().downcast_ref::<&str>() {
            (*msg).to_string()
        } else if let Some(msg) = panic_info.payload().downcast_ref::<String>() {
            msg.clone()
        } else {
            "Unbekannter Panic-Fehler".to_string()
        };
        let msg = format!("PANIC: {} @ {}", payload, location);
        let _ = write_app_log("error", &msg, now_timestamp_ms(), None);
    }));
}

fn camera_streams() -> &'static Mutex<HashMap<String, CameraStreamHandle>> {
    CAMERA_STREAMS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn parse_query_param<'a>(path: &'a str, key: &str) -> Option<&'a str> {
    let query = path.split('?').nth(1)?;
    query.split('&').find_map(|pair| {
        let mut parts = pair.splitn(2, '=');
        let k = parts.next()?;
        let v = parts.next().unwrap_or("");
        if k == key { Some(v) } else { None }
    })
}

fn find_jpeg_marker(data: &[u8], a: u8, b: u8) -> Option<usize> {
    data.windows(2).position(|w| w[0] == a && w[1] == b)
}

fn ffmpeg_setup_lock() -> &'static Mutex<()> {
    FFMPEG_SETUP_LOCK.get_or_init(|| Mutex::new(()))
}

fn ffmpeg_runtime_root(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_data_dir().ok().map(|dir| dir.join("runtime").join("ffmpeg"))
}

fn find_ffmpeg_recursive(root: &Path) -> Option<String> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Some(path) = find_ffmpeg_in_dir(&dir) {
            return Some(path);
        }
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            }
        }
    }
    None
}

fn set_ffmpeg_process_env(ffmpeg_path: &str) {
    std::env::set_var("PROJEKTIL_FFMPEG", ffmpeg_path);
    if let Some(bin_dir) = Path::new(ffmpeg_path).parent() {
        let current_path = std::env::var("PATH").unwrap_or_default();
        let bin_dir_str = bin_dir.to_string_lossy().to_string();
        let already_in_path = current_path
            .split(';')
            .any(|segment| segment.eq_ignore_ascii_case(&bin_dir_str));
        if !already_in_path {
            let new_path = if current_path.is_empty() {
                bin_dir_str
            } else {
                format!("{};{}", bin_dir_str, current_path)
            };
            std::env::set_var("PATH", new_path);
        }
    }
}

fn ffmpeg_on_path() -> Option<String> {
    let mut cmd = Command::new("ffmpeg");
    #[cfg(target_os = "windows")]
    {
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let output = cmd
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if output.status.success() {
        Some("ffmpeg".to_string())
    } else {
        None
    }
}

fn resolve_ffmpeg_binary_candidate() -> Option<String> {
    if let Ok(path) = std::env::var("PROJEKTIL_FFMPEG") {
        let trimmed = path.trim();
        if !trimmed.is_empty() && Path::new(trimmed).exists() {
            return Some(trimmed.to_string());
        }
    }

    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(dir) = exe_path.parent() {
            let mut search_dirs: Vec<PathBuf> = vec![dir.to_path_buf()];

            if let Some(parent) = dir.parent() {
                search_dirs.push(parent.to_path_buf());
                search_dirs.push(parent.join("resources"));
                search_dirs.push(parent.join("Resources"));
            }

            search_dirs.push(dir.join("resources"));
            search_dirs.push(dir.join("Resources"));

            if let Ok(cwd) = std::env::current_dir() {
                search_dirs.push(cwd);
            }

            for candidate_dir in search_dirs {
                if let Some(path) = find_ffmpeg_in_dir(&candidate_dir) {
                    return Some(path);
                }
            }
        }
    }

    if let Some(app) = APP_HANDLE.get() {
        if let Some(runtime_root) = ffmpeg_runtime_root(app) {
            if let Some(path) = find_ffmpeg_recursive(&runtime_root) {
                return Some(path);
            }
        }
    }

    ffmpeg_on_path()
}

fn install_ffmpeg_runtime(app: &AppHandle) -> Result<String, String> {
    let runtime_root = ffmpeg_runtime_root(app)
        .ok_or_else(|| "FFmpeg runtime directory konnte nicht ermittelt werden".to_string())?;
    fs::create_dir_all(&runtime_root)
        .map_err(|e| format!("FFmpeg runtime directory konnte nicht erstellt werden: {}", e))?;

    if let Some(path) = find_ffmpeg_recursive(&runtime_root) {
        set_ffmpeg_process_env(&path);
        return Ok(path);
    }

    let zip_path = runtime_root.join("ffmpeg-runtime.zip");
    let extract_root = runtime_root.join("extracted");
    let zip_path_str = zip_path.to_string_lossy().to_string();
    let extract_root_str = extract_root.to_string_lossy().to_string();

    let download_script = format!(
        "$ProgressPreference='SilentlyContinue'; Invoke-WebRequest -Uri '{}' -OutFile '{}'",
        FFMPEG_RUNTIME_DOWNLOAD_URL,
        zip_path_str.replace('\\', "\\\\")
    );
    let mut download_cmd = Command::new("powershell");
    #[cfg(target_os = "windows")]
    {
        download_cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let download = download_cmd
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &download_script])
        .output()
        .map_err(|e| format!("FFmpeg Download konnte nicht gestartet werden: {}", e))?;
    if !download.status.success() {
        let stderr = String::from_utf8_lossy(&download.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            "FFmpeg Download fehlgeschlagen".to_string()
        } else {
            format!("FFmpeg Download fehlgeschlagen: {}", stderr)
        });
    }

    let _ = fs::remove_dir_all(&extract_root);
    fs::create_dir_all(&extract_root)
        .map_err(|e| format!("FFmpeg Extract-Verzeichnis konnte nicht erstellt werden: {}", e))?;

    let extract_script = format!(
        "$ProgressPreference='SilentlyContinue'; Expand-Archive -Path '{}' -DestinationPath '{}' -Force",
        zip_path_str.replace('\\', "\\\\"),
        extract_root_str.replace('\\', "\\\\")
    );
    let mut extract_cmd = Command::new("powershell");
    #[cfg(target_os = "windows")]
    {
        extract_cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let extract = extract_cmd
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &extract_script])
        .output()
        .map_err(|e| format!("FFmpeg Extract konnte nicht gestartet werden: {}", e))?;
    if !extract.status.success() {
        let stderr = String::from_utf8_lossy(&extract.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            "FFmpeg Extract fehlgeschlagen".to_string()
        } else {
            format!("FFmpeg Extract fehlgeschlagen: {}", stderr)
        });
    }

    let ffmpeg_path = find_ffmpeg_recursive(&extract_root)
        .ok_or_else(|| "FFmpeg wurde nach dem Extract nicht gefunden".to_string())?;
    set_ffmpeg_process_env(&ffmpeg_path);
    Ok(ffmpeg_path)
}

fn ensure_ffmpeg_available(app: Option<&AppHandle>) -> Result<String, String> {
    if let Some(path) = resolve_ffmpeg_binary_candidate() {
        set_ffmpeg_process_env(&path);
        return Ok(path);
    }

    let app = app
        .or_else(|| APP_HANDLE.get())
        .ok_or_else(|| "FFmpeg ist nicht verfuegbar und kein App-Kontext fuer Auto-Setup vorhanden".to_string())?;

    let _guard = ffmpeg_setup_lock()
        .lock()
        .map_err(|_| "FFmpeg setup lock Fehler".to_string())?;

    if let Some(path) = resolve_ffmpeg_binary_candidate() {
        set_ffmpeg_process_env(&path);
        return Ok(path);
    }

    install_ffmpeg_runtime(app)
}

fn find_ffmpeg_in_dir(dir: &Path) -> Option<String> {
    for name in [
        "ffmpeg.exe",
        "ffmpeg",
        "ffmpeg-x86_64-pc-windows-msvc.exe",
        "ffmpeg-x86_64-pc-windows-gnu.exe",
        "bin/ffmpeg.exe",
        "ffmpeg/bin/ffmpeg.exe",
    ] {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().to_string());
        }
    }
    None
}

fn resolve_ffmpeg_binary() -> String {
    resolve_ffmpeg_binary_candidate().unwrap_or_else(|| "ffmpeg".to_string())
}

fn cleanup_idle_camera_streams(map: &mut HashMap<String, CameraStreamHandle>) {
    let now = Instant::now();
    let stale_keys: Vec<String> = map
        .iter()
        .filter(|(_, handle)| now.duration_since(handle.last_used) > Duration::from_secs(CAMERA_STREAM_IDLE_TIMEOUT_SECS))
        .map(|(k, _)| k.clone())
        .collect();

    for key in stale_keys {
        if let Some(mut handle) = map.remove(&key) {
            let _ = handle.child.kill();
            let _ = handle.child.wait();
        }
    }
}

fn spawn_camera_stream(rtsp_url: &str) -> Result<CameraStreamHandle, String> {
    let ffmpeg_bin = ensure_ffmpeg_available(APP_HANDLE.get()).unwrap_or_else(|_| resolve_ffmpeg_binary());
    let mut cmd = Command::new(&ffmpeg_bin);
    #[cfg(target_os = "windows")]
    {
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let mut child = cmd
        .args([
            "-loglevel",
            "error",
            "-rtsp_transport",
            "tcp",
            "-i",
            rtsp_url,
            "-vf",
            "scale=trunc(iw*sar):ih,setsar=1,fps=20",
            "-f",
            "image2pipe",
            "-vcodec",
            "mjpeg",
            "-q:v",
            "8",
            "-",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("ffmpeg stream konnte nicht gestartet werden ({}): {}", ffmpeg_bin, e))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "ffmpeg stdout pipe fehlt".to_string())?;

    let latest_frame: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let latest_frame_reader = Arc::clone(&latest_frame);
    let last_frame_at = Arc::new(Mutex::new(Instant::now()));
    let last_frame_reader = Arc::clone(&last_frame_at);

    thread::spawn(move || {
        let mut out = stdout;
        let mut chunk = [0u8; 32 * 1024];
        let mut buffer: Vec<u8> = Vec::with_capacity(128 * 1024);

        loop {
            let n = match out.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            buffer.extend_from_slice(&chunk[..n]);

            loop {
                let start = match find_jpeg_marker(&buffer, 0xFF, 0xD8) {
                    Some(pos) => pos,
                    None => {
                        if buffer.len() > 1 {
                            let keep = buffer.len() - 1;
                            buffer.drain(..keep);
                        }
                        break;
                    }
                };

                if start > 0 {
                    buffer.drain(..start);
                }

                let end = match find_jpeg_marker(&buffer[2..], 0xFF, 0xD9) {
                    Some(rel) => 2 + rel + 2,
                    None => {
                        if buffer.len() > 2_000_000 {
                            buffer.clear();
                        }
                        break;
                    }
                };

                let frame = buffer[..end].to_vec();
                if let Ok(mut slot) = latest_frame_reader.lock() {
                    *slot = Some(frame);
                }
                if let Ok(mut ts) = last_frame_reader.lock() {
                    *ts = Instant::now();
                }
                buffer.drain(..end);
            }
        }
    });

    Ok(CameraStreamHandle {
        child,
        latest_frame,
        last_frame_at,
        last_used: Instant::now(),
    })
}

fn acquire_camera_stream(ip: &str, stream_id: u8) -> Result<Arc<Mutex<Option<Vec<u8>>>>, String> {
    let key = format!("{}|{}", ip, stream_id);
    let rtsp_url = format!("rtsp://{}/MediaInput/h264/stream_{}", ip, stream_id);

    let mut map = camera_streams()
        .lock()
        .map_err(|_| "Camera stream lock Fehler".to_string())?;

    cleanup_idle_camera_streams(&mut map);

    let should_restart = match map.get_mut(&key) {
        Some(handle) => {
            handle.last_used = Instant::now();
            let process_dead = match handle.child.try_wait() {
                Ok(Some(_)) => true,
                Ok(None) => false,
                Err(_) => true,
            };
            let no_frame_yet_timed_out = {
                let has_frame = handle
                    .latest_frame
                    .lock()
                    .map(|slot| slot.is_some())
                    .unwrap_or(false);
                !has_frame
                    && handle
                        .last_frame_at
                        .lock()
                        .map(|ts| {
                            ts.elapsed()
                                > Duration::from_secs(CAMERA_STREAM_FIRST_FRAME_TIMEOUT_SECS + 5)
                        })
                        .unwrap_or(true)
            };
            let frame_stale = handle
                .last_frame_at
                .lock()
                .map(|ts| ts.elapsed() > Duration::from_secs(CAMERA_STREAM_STALE_TIMEOUT_SECS))
                .unwrap_or(true);
            process_dead || frame_stale || no_frame_yet_timed_out
        }
        None => true,
    };

    if should_restart {
        if let Some(mut old) = map.remove(&key) {
            let _ = old.child.kill();
            let _ = old.child.wait();
        }
        let mut handle = spawn_camera_stream(&rtsp_url)?;
        handle.last_used = Instant::now();
        map.insert(key.clone(), handle);
    }

    map.get(&key)
        .ok_or_else(|| "Camera stream nicht verfügbar".to_string())
        .map(|h| h.latest_frame.clone())
}

#[tauri::command]
fn camera_prepare_stream(app: AppHandle, ip: String, stream: Option<u8>) -> Result<bool, String> {
    let _ = ensure_ffmpeg_available(Some(&app))?;
    let stream_id = stream.unwrap_or(1).clamp(1, 4);
    let _ = acquire_camera_stream(&ip, stream_id)?;
    Ok(true)
}

#[tauri::command]
fn camera_restart_stream(ip: String, stream: Option<u8>) -> Result<bool, String> {
    let stream_id = stream.unwrap_or(1).clamp(1, 4);
    let key = format!("{}|{}", ip, stream_id);
    let mut map = camera_streams()
        .lock()
        .map_err(|_| "Camera stream lock Fehler".to_string())?;

    if let Some(mut handle) = map.remove(&key) {
        let _ = handle.child.kill();
        let _ = handle.child.wait();
    }
    Ok(true)
}

fn handle_mjpeg_client(mut conn: TcpStream) {
    let _ = conn.set_read_timeout(Some(Duration::from_millis(1500)));
    let _ = conn.set_write_timeout(Some(Duration::from_millis(CAMERA_STREAM_WRITE_TIMEOUT_MS)));

    let mut req = [0u8; 4096];
    let n = match conn.read(&mut req) {
        Ok(0) | Err(_) => return,
        Ok(n) => n,
    };

    let request = String::from_utf8_lossy(&req[..n]);
    let first_line = request.lines().next().unwrap_or("");
    let path = first_line.split_whitespace().nth(1).unwrap_or("/");

    if !path.starts_with("/camera/mjpeg") {
        let _ = conn.write_all(b"HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n");
        return;
    }

    let ip = parse_query_param(path, "ip").unwrap_or("").trim();
    if ip.is_empty() {
        let _ = conn.write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\nMissing ip");
        return;
    }

    let stream_id = parse_query_param(path, "stream")
        .and_then(|s| s.parse::<u8>().ok())
        .unwrap_or(1)
        .clamp(1, 4);

    let latest = match acquire_camera_stream(ip, stream_id) {
        Ok(v) => v,
        Err(e) => {
            let body = format!("Stream start failed: {}", e);
            let response = format!(
                "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = conn.write_all(response.as_bytes());
            return;
        }
    };

    let wait_deadline = Instant::now();
    let first_frame = loop {
        let frame = latest.lock().ok().and_then(|guard| guard.clone());
        if let Some(bytes) = frame {
            break bytes;
        }
        if wait_deadline.elapsed() > Duration::from_secs(CAMERA_STREAM_FIRST_FRAME_TIMEOUT_SECS) {
            let body = "Stream timeout: no frames";
            let response = format!(
                "HTTP/1.1 504 Gateway Timeout\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = conn.write_all(response.as_bytes());
            return;
        }
        thread::sleep(Duration::from_millis(10));
    };

    let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\nCache-Control: no-cache, no-store, must-revalidate\r\nPragma: no-cache\r\nConnection: close\r\n\r\n"
    );
    if conn.write_all(headers.as_bytes()).is_err() {
        return;
    }

    let first_part_head = format!(
        "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
        first_frame.len()
    );
    if conn.write_all(first_part_head.as_bytes()).is_err() {
        return;
    }
    if conn.write_all(&first_frame).is_err() {
        return;
    }
    if conn.write_all(b"\r\n").is_err() {
        return;
    }
    if conn.flush().is_err() {
        return;
    }

    loop {
        let latest = match acquire_camera_stream(ip, stream_id) {
            Ok(v) => v,
            Err(_) => break,
        };

        let frame = latest.lock().ok().and_then(|guard| guard.clone());
        if let Some(bytes) = frame {
            let part_head = format!(
                "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                bytes.len()
            );
            if conn.write_all(part_head.as_bytes()).is_err() {
                break;
            }
            if conn.write_all(&bytes).is_err() {
                break;
            }
            if conn.write_all(b"\r\n").is_err() {
                break;
            }
            if conn.flush().is_err() {
                break;
            }
        }

        thread::sleep(Duration::from_millis(10));
    }
}

fn start_camera_mjpeg_server() {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }

    thread::spawn(|| {
        let listener = match TcpListener::bind(("0.0.0.0", CAMERA_MJPEG_PORT)) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("MJPEG server bind error: {}", e);
                return;
            }
        };

        for incoming in listener.incoming() {
            if let Ok(conn) = incoming {
                thread::spawn(move || handle_mjpeg_client(conn));
            }
        }
    });
}

// ============================================================
// Anomalie-Erkennung (dynamisch via Config)
// ============================================================
fn check_janitza_anomalies(v1: f32, v2: f32, v3: f32, i1: f32, i2: f32, i3: f32, freq: f32, _power_kw: f32, cfg: &serde_json::Value) -> Vec<String> {
    let mut warnings = Vec::new();
    let t = &cfg["thresholds"];
    
    let v_min = t["v_min"].as_f64().unwrap_or(195.0) as f32;
    let v_max = t["v_max"].as_f64().unwrap_or(253.0) as f32;
    let v_imbal = t["v_imbal"].as_f64().unwrap_or(15.0) as f32;
    let f_min = t["f_min"].as_f64().unwrap_or(49.5) as f32;
    let f_max = t["f_max"].as_f64().unwrap_or(50.5) as f32;
    let i_max = t["i_max_32"].as_f64().unwrap_or(28.0) as f32; // Default auf 32A Schiene

    for (phase, v) in [("L1", v1), ("L2", v2), ("L3", v3)] {
        if v > 1.0 {
            if v < v_min {
                warnings.push(format!("UNTERSPANNUNG {} = {:.1}V (< {}V)", phase, v, v_min));
            } else if v > v_max {
                warnings.push(format!("ÜBERSPANNUNG {} = {:.1}V (> {}V)", phase, v, v_max));
            }
        }
    }

    for (phase, i) in [("L1", i1), ("L2", i2), ("L3", i3)] {
        if i > i_max {
            warnings.push(format!("HOHE LAST {} = {:.1}A (> {}A)", phase, i, i_max));
        }
    }

    if v1 > 1.0 && v2 > 1.0 && v3 > 1.0 {
        let vmax = v1.max(v2).max(v3);
        let vmin = v1.min(v2).min(v3);
        if vmax - vmin > v_imbal {
            warnings.push(format!(
                "PHASEN-UNSYMMETRIE {:.1}V (L1={:.1} L2={:.1} L3={:.1})",
                vmax - vmin, v1, v2, v3
            ));
        }
    }

    if freq > 1.0 {
        if freq < f_min {
            warnings.push(format!("UNTERFREQUENZ {:.2}Hz (< {}Hz)", freq, f_min));
        } else if freq > f_max {
            warnings.push(format!("ÜBERFREQUENZ {:.2}Hz (> {}Hz)", freq, f_max));
        }
    }

    warnings
}

fn check_ups_anomalies(data: &serde_json::Map<String, serde_json::Value>, cfg: &serde_json::Value) -> Vec<String> {
    let mut warnings = Vec::new();
    let t = &cfg["thresholds"];
    let ups_load_warn = t["ups_load_warn"].as_i64().unwrap_or(80);
    let v_min = t["v_min"].as_f64().unwrap_or(195.0) as i64;
    let v_max = t["v_max"].as_f64().unwrap_or(253.0) as i64;

    let get_i = |k: &str| -> i64 {
        data.get(k).and_then(|v| v.as_i64()).unwrap_or(0)
    };

    let bat_status    = get_i("bat_status");
    let bat_ok        = get_i("bat_ok");
    let output_load   = get_i("output_load"); // /10 = %
    let output_online = get_i("output_online");
    let input_v       = get_i("input_voltage");
    let runtime       = get_i("runtime_ticks"); // Timeticks /100 = Sekunden

    // bat_status: 2=normal (Netzstrom), 3=low (Batterie, niedrig), 4=fault (Batteriefehler)
    if bat_status == 3 {
        warnings.push("⚠ BATTERIE MODE AKTIVIERT - UPS AUF BATTERIE!".to_string());
        warnings.push("BATTERIE NIEDRIG (bat_status=3)".to_string());
    } else if bat_status == 4 {
        warnings.push("🚨 BATTERIE FEHLER (bat_status=4)".to_string());
    } else if bat_status != 2 && bat_status != 0 {
        warnings.push(format!("UNBEKANNTER BATTERIE-STATUS: {}", bat_status));
    }

    if bat_ok == 0 {
        warnings.push("bat_ok = 0 (Batterie nicht OK)".to_string());
    }

    let load_pct = normalize_ups_load_percent(output_load);
    if load_pct >= ups_load_warn {
        warnings.push(format!("UPS LAST {}% (Warnschwelle {}%)", load_pct, ups_load_warn));
    }

    if output_online != 1 {
        warnings.push(format!("⚠ OUTPUT nicht online (output_online={})", output_online));
    }

    if input_v > 0 && (input_v < v_min || input_v > v_max) {
        warnings.push(format!("UPS EINGANGSSPANNUNG {}V ausserhalb Normal ({} - {}V)", input_v, v_min, v_max));
    }

    // Laufzeit < 5 Minuten = 30000 Timeticks
    if runtime > 0 && runtime < 30000 {
        let secs = runtime / 100;
        warnings.push(format!("UPS LAUFZEIT NUR {}min {}sec", secs / 60, secs % 60));
    }

    let bat_temp_raw = get_i("bat_temp"); // Liegt in Zehntel-Grad vor (z.B. 350)
    if bat_temp_raw > 450 {
        warnings.push(format!("UPS BATTERIE ÜBERHITZUNG: {:.1}°C", bat_temp_raw as f32 / 10.0));
    }

    let replace = get_i("replace_bat");
    if replace == 2 {
        warnings.push("UPS MELDET: BATTERIE TAUSCHEN!".to_string());
    }

    warnings
}

fn normalize_ups_load_percent(raw: i64) -> i64 {
    if raw <= 0 {
        return 0;
    }
    if raw <= 100 {
        return raw;
    }
    if raw <= 1000 {
        return raw / 10;
    }
    if raw <= 10000 {
        return raw / 100;
    }
    raw / 10
}

// ============================================================
// TCP Ping
// ============================================================
#[tauri::command]
async fn http_ping(ip: String, port: u16) -> Result<String, String> {
    let addr = format!("{}:{}", ip, port);
    match TcpStream::connect_timeout(
        &addr.parse::<std::net::SocketAddr>().map_err(|e| e.to_string())?,
        Duration::from_millis(3000),
    ) {
        Ok(_) => {
            mark_device_online(&format!("tcp:{}:{}", ip, port));
            Ok("OK".to_string())
        }
        Err(e) => {
            let err_str = e.to_string().to_lowercase();
            // Windows Fehler 10061 = WSAECONNREFUSED
            if err_str.contains("10061") || err_str.contains("connection refused") || err_str.contains("verweigerte") {
                Ok("REFUSED".to_string())
            } else if err_str.contains("timed out") || err_str.contains("timeout") {
                Ok("TIMEOUT".to_string())
            } else if err_str.contains("host") || err_str.contains("network") || err_str.contains("unreachable") || err_str.contains("erreichbar") {
                Ok("UNREACHABLE".to_string())
            } else {
                Ok("REFUSED".to_string())
            }
        }
    }
}

// ============================================================
// ICMP Ping (Windows)
// ============================================================
#[tauri::command]
fn icmp_ping(ip: String) -> Result<bool, String> {
    let mut cmd = Command::new("ping");
    cmd.args(&["-n", "1", "-w", "1000", &ip]);
    
    #[cfg(target_os = "windows")]
    {
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    
    let output = cmd.output()
        .map_err(|e| format!("Ping command fehlgeschlagen: {}", e))?;

    let success = output.status.success();
    if success {
        mark_device_online(&format!("icmp:{}", ip));
    }
    Ok(success)
}

#[tauri::command]
fn system_get_battery_status() -> Result<serde_json::Value, String> {
    #[cfg(target_os = "windows")]
    {
        let mut battery_cmd = Command::new("powershell");
        battery_cmd.args([
            "-NoProfile",
            "-Command",
            "Get-CimInstance Win32_Battery | Select-Object EstimatedChargeRemaining,BatteryStatus | ConvertTo-Json -Compress",
        ]);
        #[cfg(target_os = "windows")]
        {
            battery_cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        let output = battery_cmd.output().map_err(|e| e.to_string())?;

        if !output.status.success() {
            return Err("Battery query failed".to_string());
        }

        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if stdout.is_empty() || stdout == "null" {
            return Ok(serde_json::json!({
                "available": false
            }));
        }

        let parsed: serde_json::Value = serde_json::from_str(&stdout).map_err(|e| e.to_string())?;
        let obj = if let Some(arr) = parsed.as_array() {
            arr.first().cloned().unwrap_or(serde_json::Value::Null)
        } else {
            parsed
        };

        if obj.is_null() {
            return Ok(serde_json::json!({
                "available": false
            }));
        }

        let pct = obj
            .get("EstimatedChargeRemaining")
            .and_then(|v| v.as_i64())
            .unwrap_or(-1);
        let status = obj
            .get("BatteryStatus")
            .and_then(|v| v.as_i64())
            .unwrap_or(-1);

        return Ok(serde_json::json!({
            "available": true,
            "percent": pct,
            "status": status
        }));
    }

    #[cfg(not(target_os = "windows"))]
    {
        Ok(serde_json::json!({
            "available": false,
            "reason": "unsupported-platform"
        }))
    }
}


// ============================================================
// Panasonic AW-UE40/50 PTZ CGI proxy
// Example: /cgi-bin/aw_ptz?cmd=%23R01&res=1
// ============================================================
#[tauri::command]
async fn camera_ptz_command(ip: String, command: String) -> Result<String, String> {
    let addr = format!("{}:80", ip);
    let mut stream = TcpStream::connect_timeout(
        &addr.parse::<std::net::SocketAddr>().map_err(|e| e.to_string())?,
        Duration::from_millis(2000),
    )
    .map_err(|e| format!("Camera connect error: {}", e))?;
    stream.set_read_timeout(Some(Duration::from_millis(2500))).ok();

    let encoded_cmd = if command.starts_with('#') {
        format!("%23{}", &command[1..])
    } else {
        command
    };
    let path = format!("/cgi-bin/aw_ptz?cmd={}&res=1", encoded_cmd);
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        path, ip
    );

    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("Camera request error: {}", e))?;

    let mut buf = Vec::new();
    stream
        .read_to_end(&mut buf)
        .map_err(|e| format!("Camera read error: {}", e))?;

    let response = String::from_utf8_lossy(&buf);
    if !response.contains("200 OK") {
        return Err("Camera command failed (no HTTP 200 response)".to_string());
    }

    let body = response
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or("")
        .trim()
        .to_string();

    if body.is_empty() {
        Ok("OK".to_string())
    } else {
        Ok(body.lines().next().unwrap_or("OK").to_string())
    }
}

#[tauri::command]
async fn camera_snapshot(app: AppHandle, ip: String, stream: Option<u8>) -> Result<String, String> {
    let stream_id = stream.unwrap_or(1).clamp(1, 4);
    let rtsp_url = format!("rtsp://{}/MediaInput/h264/stream_{}", ip, stream_id);

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis();
    let out_path = std::env::temp_dir().join(format!("projektil-cam-{}-{}.jpg", ip.replace('.', "_"), ts));

    let out_path_str = out_path
        .to_str()
        .ok_or_else(|| "Invalid temp file path".to_string())?
        .to_string();

    let ffmpeg_bin = ensure_ffmpeg_available(Some(&app)).unwrap_or_else(|_| resolve_ffmpeg_binary());
    let mut cmd = Command::new(&ffmpeg_bin);
    #[cfg(target_os = "windows")]
    {
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let ffmpeg = cmd
        .args([
            "-y",
            "-loglevel",
            "error",
            "-rtsp_transport",
            "tcp",
            "-i",
            &rtsp_url,
            "-vf",
            "scale=trunc(iw*sar):ih,setsar=1",
            "-frames:v",
            "1",
            "-q:v",
            "5",
            &out_path_str,
        ])
        .output();

    let output = match ffmpeg {
        Ok(o) => o,
        Err(e) => {
            return Err(format!(
                "ffmpeg not available or failed to start ({}): {}",
                ffmpeg_bin,
                e
            ))
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let _ = fs::remove_file(&out_path);
        return Err(if stderr.is_empty() {
            "Snapshot capture failed".to_string()
        } else {
            format!("Snapshot capture failed: {}", stderr)
        });
    }

    let bytes = fs::read(&out_path).map_err(|e| format!("Snapshot read failed: {}", e))?;
    let _ = fs::remove_file(&out_path);

    let encoded = general_purpose::STANDARD.encode(bytes);
    Ok(format!("data:image/jpeg;base64,{}", encoded))
}

#[tauri::command]
async fn camera_stream_frame(ip: String, stream: Option<u8>) -> Result<String, String> {
    let stream_id = stream.unwrap_or(1).clamp(1, 4);
    let latest = acquire_camera_stream(&ip, stream_id)?;

    for _ in 0..10 {
        if let Ok(frame_guard) = latest.lock() {
            if let Some(bytes) = frame_guard.as_ref() {
                let encoded = general_purpose::STANDARD.encode(bytes);
                return Ok(format!("data:image/jpeg;base64,{}", encoded));
            }
        }
        thread::sleep(Duration::from_millis(30));
    }

    Err("RTSP Stream liefert noch keine Frames".to_string())
}


// ============================================================
// APC UPS — SNMPv1 UDP Port 161, Community: "projektil"
// ============================================================
#[tauri::command]
async fn ups_get_status(ip: String) -> Result<serde_json::Value, String> {
    let community = "projektil";
    let _pool_key = get_pooled_connection(&ip, 161, "snmp");  // Register in connection pool
    
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    socket.set_read_timeout(Some(Duration::from_millis(450))).ok();
    socket.connect(format!("{}:161", ip)).map_err(|e| e.to_string())?;

    // Fast-fail on unreachable UPS: only query critical OIDs first.
    let fast_bat_status = snmp_query_raw(&socket, community, &[1,3,6,1,4,1,318,1,1,1,2,1,1,0])
        .and_then(|raw| extract_snmp_value(&raw));
    let fast_output_online = snmp_query_raw(&socket, community, &[1,3,6,1,4,1,318,1,1,1,4,1,2,0])
        .and_then(|raw| extract_snmp_value(&raw));

    if fast_bat_status.is_none() && fast_output_online.is_none() {
        return Err("UPS antwortet nicht auf SNMP-Abfragen".to_string());
    }

    let queries: Vec<(&str, Vec<u32>)> = vec![
        ("bat_status",      vec![1,3,6,1,4,1,318,1,1,1,2,1,1,0]), 
        ("runtime_ticks",   vec![1,3,6,1,4,1,318,1,1,1,2,2,3,0]), 
        ("bat_capacity",    vec![1,3,6,1,4,1,318,1,1,1,2,3,1,0]), // HighPrecBatteryCapacity
        ("bat_temp",        vec![1,3,6,1,4,1,318,1,1,1,2,3,2,0]), // HighPrecBatteryTemperature
        ("bat_temp_adv",    vec![1,3,6,1,4,1,318,1,1,1,2,2,2,0]), // upsAdvBatteryTemperature
        ("bat_temp_basic",  vec![1,3,6,1,4,1,318,1,1,1,2,1,2,0]), // upsBasicBatteryTemperature
        ("bat_temp_internal", vec![1,3,6,1,4,1,318,1,1,1,4,1,4,0]), // internal UPS temperature (device-specific)
        ("replace_bat",     vec![1,3,6,1,4,1,318,1,1,1,2,2,4,0]), 
        ("bat_ok",          vec![1,3,6,1,4,1,318,1,1,1,2,2,5,0]),
        ("input_voltage",   vec![1,3,6,1,4,1,318,1,1,1,3,2,1,0]),
        ("input_freq",    vec![1,3,6,1,4,1,318,1,1,1,3,2,4,0]),
        ("output_v",      vec![1,3,6,1,4,1,318,1,1,1,3,3,1,0]),
        ("output_load_mib", vec![1,3,6,1,4,1,318,1,1,1,3,3,3,0]),
        ("hp_output_load", vec![1,3,6,1,4,1,318,1,1,1,4,3,3,0]),
        ("rfc_output_percent_load_0", vec![1,3,6,1,2,1,33,1,4,4,1,5,0]),
        ("rfc_output_percent_load_1", vec![1,3,6,1,2,1,33,1,4,4,1,5,1]),
        ("apc_output_load_1", vec![1,3,6,1,4,1,318,1,1,1,4,2,4,0]),
        ("apc_output_load_2", vec![1,3,6,1,4,1,318,1,1,1,4,2,5,0]),
        ("apc_output_load_3", vec![1,3,6,1,4,1,318,1,1,1,4,2,6,0]),
        ("hp_output_current", vec![1,3,6,1,4,1,318,1,1,1,4,3,4,0]),
        ("output_current", vec![1,3,6,1,4,1,318,1,1,1,3,3,4,0]),
        ("output_status", vec![1,3,6,1,4,1,318,1,1,1,4,1,1,0]),
        ("output_online", vec![1,3,6,1,4,1,318,1,1,1,4,1,2,0]),
    ];

    let mut result = serde_json::Map::new();
    if let Some(v) = fast_bat_status {
        result.insert("bat_status".to_string(), serde_json::json!(v));
    }
    if let Some(v) = fast_output_online {
        result.insert("output_online".to_string(), serde_json::json!(v));
    }
    for (key, oid) in &queries {
        if result.contains_key(*key) {
            continue;
        }
        let packet = snmp_get_packet(community, oid);
        if socket.send(&packet).is_ok() {
            let mut buf = [0u8; 512];
            if let Ok(n) = socket.recv(&mut buf) {
                if let Some(val) = extract_snmp_value(&buf[..n]) {
                    result.insert(key.to_string(), serde_json::json!(val));
                }
            }
        }
    }

    // Always calculate display load from current and voltage for this UPS model.
    // Do not use APC/RFC load OIDs directly in status output.
    let current_raw = result
        .get("hp_output_current").and_then(|v| v.as_i64())
        .or_else(|| result.get("output_current").and_then(|v| v.as_i64()))
        .unwrap_or(0);

    let current_a = (current_raw as f64) / 10.0;
    let output_v_raw = result.get("output_v").and_then(|v| v.as_i64()).unwrap_or(0);
    let input_v_raw = result.get("input_voltage").and_then(|v| v.as_i64()).unwrap_or(0);

    let volts = if output_v_raw >= 1000 {
        (output_v_raw as f64) / 10.0
    } else if (100..=300).contains(&output_v_raw) {
        output_v_raw as f64
    } else if (100..=300).contains(&input_v_raw) {
        input_v_raw as f64
    } else {
        230.0
    };

    let rated_watts = 1800.0;
    let watts = (current_a * volts).round() as i64;
    let load_pct = (((watts as f64) / rated_watts) * 100.0).round().clamp(0.0, 100.0) as i64;
    result.insert("output_load".to_string(), serde_json::json!(load_pct));
    result.insert("output_load_estimated_watts".to_string(), serde_json::json!(watts));
    result.insert("output_load_source".to_string(), serde_json::json!("calculated_from_current_voltage"));
    
    // Fallback für Kapazität und Temperatur, falls HighPrec 0 liefert oder fehlt
    // Wir skalieren Nicht-HighPrec Werte mit 10, damit das Frontend (das /10 macht) korrekt rechnet.
    if result.get("bat_capacity").map_or(true, |v| v.as_i64().unwrap_or(0) == 0) {
        let packet = snmp_get_packet(community, &[1,3,6,1,4,1,318,1,1,1,2,2,1,0]); // upsAdvBatteryCapacity
        if socket.send(&packet).is_ok() {
            let mut buf = [0u8; 512];
            if let Ok(n) = socket.recv(&mut buf) {
                if let Some(val) = extract_snmp_value(&buf[..n]) {
                    result.insert("bat_capacity".to_string(), serde_json::json!(val * 10));
                }
            }
        }
    }

    // Erweiterte Fallback-Kette für Temperatur: HighPrec -> Advanced -> Basic
    let bat_temp_ok = result.get("bat_temp").and_then(|v| v.as_i64()).map_or(false, |val| val >= 50 && val <= 700);
    if !bat_temp_ok {
        if let Some(val) = result.get("bat_temp_internal").and_then(|v| v.as_i64()) {
            if val > 0 && val <= 70 {
                result.insert("bat_temp".to_string(), serde_json::json!(val * 10));
            }
        }
    }
    if !bat_temp_ok {
        if let Some(val) = result.get("bat_temp_adv").and_then(|v| v.as_i64()) {
            if val > 0 && val <= 70 {
                result.insert("bat_temp".to_string(), serde_json::json!(val * 10));
            }
        }
    }
    if !bat_temp_ok && result.get("bat_temp").and_then(|v| v.as_i64()).map_or(true, |val| val == 0) {
        if let Some(val) = result.get("bat_temp_basic").and_then(|v| v.as_i64()) {
            if val > 0 && val <= 70 {
                result.insert("bat_temp".to_string(), serde_json::json!(val * 10));
            }
        }
    }
    if !bat_temp_ok && result.get("bat_temp").and_then(|v| v.as_i64()).map_or(true, |val| val == 0) {
        let temp_oids = vec![
            vec![1,3,6,1,4,1,318,1,1,1,2,2,2,0], // upsAdvBatteryTemperature (Celsius)
            vec![1,3,6,1,4,1,318,1,1,1,2,1,2,0], // upsBasicBatteryTemperature (Celsius)
        ];
        for oid in temp_oids {
            let packet = snmp_get_packet(community, &oid);
            if socket.send(&packet).is_ok() {
                let mut buf = [0u8; 512];
                if let Ok(n) = socket.recv(&mut buf) {
                    if let Some(val) = extract_snmp_value(&buf[..n]) {
                        if val > 0 && val <= 70 {
                            result.insert("bat_temp".to_string(), serde_json::json!(val * 10));
                            break;
                        }
                    }
                }
            }
        }
    }

    if result.is_empty() { return Err("SNMP keine Antwort".to_string()); }
    
    // Überprüfung auf kritische Felder: Wenn diese fehlen, ist die UPS nicht erreichbar
    if !result.contains_key("output_online") && !result.contains_key("bat_status") {
        return Err("UPS antwortet nicht auf SNMP-Abfragen".to_string());
    }

    let cfg = get_config();
    let warnings = check_ups_anomalies(&result, &cfg);
    result.insert("warnings".to_string(), serde_json::json!(warnings));

    mark_device_online(&format!("ups:{}", ip));
    Ok(serde_json::Value::Object(result))
}

#[tauri::command]
async fn ups_get_power_mode(ip: String) -> Result<serde_json::Value, String> {
    let community = "projektil";
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    socket.set_read_timeout(Some(Duration::from_millis(250))).ok();
    socket.connect(format!("{}:161", ip)).map_err(|e| e.to_string())?;

    let bat_status = snmp_query_raw(&socket, community, &[1,3,6,1,4,1,318,1,1,1,2,1,1,0])
        .and_then(|raw| extract_snmp_value(&raw));
    let output_status = snmp_query_raw(&socket, community, &[1,3,6,1,4,1,318,1,1,1,4,1,1,0])
        .and_then(|raw| extract_snmp_value(&raw));
    let output_online = snmp_query_raw(&socket, community, &[1,3,6,1,4,1,318,1,1,1,4,1,2,0])
        .and_then(|raw| extract_snmp_value(&raw));

    if bat_status.is_none() && output_status.is_none() && output_online.is_none() {
        return Err("UPS antwortet nicht auf SNMP-Abfragen".to_string());
    }

    // Priority: explicit battery state from output_status, then bat_status, then output_online fallback.
    let on_mains = match (bat_status, output_status, output_online) {
        // APC output status commonly reports 2=online, 3=onBattery
        (_, Some(3), _) | (_, Some(5), _) => false,
        (_, Some(2), _) => true,
        // RFC1628: upsBatteryStatus 2 = batteryNormal (typically mains)
        (Some(2), _, _) => true,
        // RFC1628: 3/4 indicate battery-low or battery-fault -> not on mains
        (Some(3), _, _) | (Some(4), _, _) => false,
        // Legacy fallback used by existing UI logic
        (_, _, Some(1)) => true,
        _ => false,
    };

    Ok(serde_json::json!({
        "on_mains": on_mains,
        "bat_status": bat_status,
        "output_status": output_status,
        "output_online": output_online
    }))
}

#[tauri::command]
async fn ups_get_diagnostics(ip: String) -> Result<serde_json::Value, String> {
    let community = "projektil";
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    socket.set_read_timeout(Some(Duration::from_millis(450))).ok();
    socket.connect(format!("{}:161", ip)).map_err(|e| e.to_string())?;

    let probes: Vec<(&str, Vec<u32>)> = vec![
        ("bat_status", vec![1,3,6,1,4,1,318,1,1,1,2,1,1,0]),
        ("runtime_ticks", vec![1,3,6,1,4,1,318,1,1,1,2,2,3,0]),
        ("bat_capacity", vec![1,3,6,1,4,1,318,1,1,1,2,3,1,0]),
        ("bat_temp", vec![1,3,6,1,4,1,318,1,1,1,2,3,2,0]),
        ("bat_temp_adv", vec![1,3,6,1,4,1,318,1,1,1,2,2,2,0]),
        ("bat_temp_basic", vec![1,3,6,1,4,1,318,1,1,1,2,1,2,0]),
        ("bat_temp_internal", vec![1,3,6,1,4,1,318,1,1,1,4,1,4,0]),
        ("replace_bat", vec![1,3,6,1,4,1,318,1,1,1,2,2,4,0]),
        ("bat_ok", vec![1,3,6,1,4,1,318,1,1,1,2,2,5,0]),
        ("input_voltage", vec![1,3,6,1,4,1,318,1,1,1,3,2,1,0]),
        ("input_freq", vec![1,3,6,1,4,1,318,1,1,1,3,2,4,0]),
        ("output_v", vec![1,3,6,1,4,1,318,1,1,1,3,3,1,0]),
        ("output_current_legacy", vec![1,3,6,1,4,1,318,1,1,1,3,3,4,0]),
        ("output_status", vec![1,3,6,1,4,1,318,1,1,1,4,1,1,0]),
        ("output_online", vec![1,3,6,1,4,1,318,1,1,1,4,1,2,0]),
        ("adv_output_load", vec![1,3,6,1,4,1,318,1,1,1,3,3,3,0]),
        ("adv_output_current", vec![1,3,6,1,4,1,318,1,1,1,3,3,4,0]),
        ("adv_output_active_power", vec![1,3,6,1,4,1,318,1,1,1,3,3,8,0]),
        ("adv_output_apparent_power", vec![1,3,6,1,4,1,318,1,1,1,3,3,9,0]),
        ("hp_output_load", vec![1,3,6,1,4,1,318,1,1,1,4,3,3,0]),
        ("hp_output_current", vec![1,3,6,1,4,1,318,1,1,1,4,3,4,0]),
        ("hp_output_efficiency", vec![1,3,6,1,4,1,318,1,1,1,4,3,5,0]),
        ("hp_output_energy_usage", vec![1,3,6,1,4,1,318,1,1,1,4,3,6,0]),
        ("rfc_output_current_0", vec![1,3,6,1,2,1,33,1,4,4,1,4,0]),
        ("rfc_output_current_1", vec![1,3,6,1,2,1,33,1,4,4,1,4,1]),
        ("rfc_output_percent_load_0", vec![1,3,6,1,2,1,33,1,4,4,1,5,0]),
        ("rfc_output_percent_load_1", vec![1,3,6,1,2,1,33,1,4,4,1,5,1]),
        ("rfc_output_percent_capacity_0", vec![1,3,6,1,2,1,33,1,4,4,1,6,0]),
        ("rfc_output_percent_capacity_1", vec![1,3,6,1,2,1,33,1,4,4,1,6,1]),
        ("apc_output_current_1", vec![1,3,6,1,4,1,318,1,1,1,4,2,1,0]),
        ("apc_output_current_2", vec![1,3,6,1,4,1,318,1,1,1,4,2,2,0]),
        ("apc_output_current_3", vec![1,3,6,1,4,1,318,1,1,1,4,2,3,0]),
        ("apc_output_load_1", vec![1,3,6,1,4,1,318,1,1,1,4,2,4,0]),
        ("apc_output_load_2", vec![1,3,6,1,4,1,318,1,1,1,4,2,5,0]),
        ("apc_output_load_3", vec![1,3,6,1,4,1,318,1,1,1,4,2,6,0]),
        ("ups_output_percent_load_1", vec![1,3,6,1,2,1,33,1,4,4,1,5,1]),
        ("ups_output_percent_load_2", vec![1,3,6,1,2,1,33,1,4,4,1,5,2]),
        ("ups_output_percent_load_3", vec![1,3,6,1,2,1,33,1,4,4,1,5,3]),
    ];

    let mut results = Vec::<serde_json::Value>::new();
    for (name, oid) in probes {
        let mut entry = serde_json::Map::new();
        entry.insert("name".to_string(), serde_json::json!(name));
        entry.insert("oid".to_string(), serde_json::json!(oid_to_string(&oid)));
        let packet = snmp_get_packet(community, &oid);
        if socket.send(&packet).is_err() {
            entry.insert("error".to_string(), serde_json::json!("send failed"));
            results.push(serde_json::Value::Object(entry));
            continue;
        }

        let mut buf = [0u8; 512];
        match socket.recv(&mut buf) {
            Ok(n) => {
                let raw = &buf[..n];
                entry.insert("bytes".to_string(), serde_json::json!(n));
                entry.insert("raw_hex".to_string(), serde_json::json!(bytes_to_hex_string(raw)));
                if let Some(val) = extract_snmp_value(raw) {
                    entry.insert("value".to_string(), serde_json::json!(val));
                    entry.insert("decoded_kind".to_string(), serde_json::json!("integer-like"));
                    if name.contains("load") || name.contains("percent_load") {
                        entry.insert("load_pct_guess".to_string(), serde_json::json!(normalize_ups_load_percent(val)));
                    }
                } else if let Some(txt) = extract_snmp_octet_string(raw) {
                    entry.insert("value".to_string(), serde_json::json!(txt));
                    entry.insert("decoded_kind".to_string(), serde_json::json!("octet-string"));
                } else {
                    entry.insert("error".to_string(), serde_json::json!("decode failed"));
                }
            }
            Err(_) => {
                entry.insert("error".to_string(), serde_json::json!("timeout/no response"));
            }
        }
        results.push(serde_json::Value::Object(entry));
    }

    Ok(serde_json::json!({
        "ip": ip,
        "community": community,
        "results": results
    }))
}

fn oid_to_string(oid: &[u32]) -> String {
    oid.iter().map(|part| part.to_string()).collect::<Vec<_>>().join(".")
}

fn bytes_to_hex_string(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{:02X}", byte)).collect::<Vec<_>>().join(" ")
}

fn decode_asn1_length(data: &[u8], offset: usize) -> Option<(usize, usize)> {
    if offset >= data.len() {
        return None;
    }
    let first = data[offset] as usize;
    if (first & 0x80) == 0 {
        return Some((first, 1));
    }

    let count = first & 0x7f;
    if count == 0 || count > 4 || offset + 1 + count > data.len() {
        return None;
    }

    let mut len = 0usize;
    for i in 0..count {
        len = (len << 8) | data[offset + 1 + i] as usize;
    }
    Some((len, 1 + count))
}

fn extract_snmp_value(data: &[u8]) -> Option<i64> {
    let mut last_oid_end = 0usize;
    let mut i = 0;
    while i + 2 < data.len() {
        if data[i] == 0x06 {
            if let Some((oid_len, oid_len_bytes)) = decode_asn1_length(data, i + 1) {
                let oid_start = i + 1 + oid_len_bytes;
                if oid_start + oid_len <= data.len() {
                    last_oid_end = oid_start + oid_len;
                    i = last_oid_end;
                    continue;
                }
            }
        }
        i += 1;
    }
    if last_oid_end + 2 > data.len() { return None; }
    let vtype = data[last_oid_end];
    let (vlen, vlen_bytes) = decode_asn1_length(data, last_oid_end + 1)?;
    let vstart = last_oid_end + 1 + vlen_bytes;
    if vlen == 0 || vstart + vlen > data.len() { return None; }
    let vbytes = &data[vstart .. vstart + vlen];
    match vtype {
        0x02 | 0x41 | 0x42 | 0x43 => {
            let mut val: i64 = 0;
            for b in vbytes { val = (val << 8) | (*b as i64); }
            if vtype == 0x02 && vlen < 8 && !vbytes.is_empty() && (vbytes[0] & 0x80) != 0 {
                val -= 1i64 << (vlen * 8);
            }
            Some(val)
        }
        _ => None
    }
}

fn extract_snmp_octet_string(data: &[u8]) -> Option<String> {
    let mut last_oid_end = 0usize;
    let mut i = 0;
    while i + 2 < data.len() {
        if data[i] == 0x06 {
            if let Some((oid_len, oid_len_bytes)) = decode_asn1_length(data, i + 1) {
                let oid_start = i + 1 + oid_len_bytes;
                if oid_start + oid_len <= data.len() {
                    last_oid_end = oid_start + oid_len;
                    i = last_oid_end;
                    continue;
                }
            }
        }
        i += 1;
    }
    if last_oid_end + 2 > data.len() {
        return None;
    }
    let vtype = data[last_oid_end];
    let (vlen, vlen_bytes) = decode_asn1_length(data, last_oid_end + 1)?;
    let vstart = last_oid_end + 1 + vlen_bytes;
    if vlen == 0 || vstart + vlen > data.len() {
        return None;
    }
    let vbytes = &data[vstart..vstart + vlen];
    if vtype != 0x04 {
        return None;
    }
    Some(String::from_utf8_lossy(vbytes).trim_matches(char::from(0)).trim().to_string())
}

// ============================================================
// SNMP OID Validation
// ============================================================

/// Known safe SNMP OIDs for this application
const ALLOWED_SNMP_OIDS: &[&[u32]] = &[
    // System OIDs (1.3.6.1.2.1.1.*)
    &[1, 3, 6, 1, 2, 1, 1],         // System group (sysDescr, sysName, sysUpTime, etc.)
    
    // Standard MIBs (1.3.6.1.2.1)
    &[1, 3, 6, 1, 2, 1, 2],         // Interfaces (ifIndex, ifDescr, ifSpeed, etc.)
    &[1, 3, 6, 1, 2, 1, 25],        // Host-resources (storage, devices, running software)
    &[1, 3, 6, 1, 2, 1, 33],        // UPS MIB (RFC 3621)
    &[1, 3, 6, 1, 2, 1, 105],       // Power/Energy MIB
    
    // Enterprise Specific OIDs (1.3.6.1.4.1.*)
    // APC/Schneider Electric (318)
    &[1, 3, 6, 1, 4, 1, 318],       // APC UPS, PDU, etc.
    
    // Eaton (534)
    &[1, 3, 6, 1, 4, 1, 534],       // Eaton UPS, PDU
    
    // Synology (6574)
    &[1, 3, 6, 1, 4, 1, 6574],      // Synology NAS (system, disk, storage)
    
    // Teltonika (48690)
    &[1, 3, 6, 1, 4, 1, 48690],     // Teltonika devices (RUTX50 router, etc.)
    
    // Other common vendors
    &[1, 3, 6, 1, 4, 1, 2578],      // Janitza (power meters)
];

fn validate_snmp_oid(oid: &[u32]) -> Result<(), String> {
    // Check length
    if oid.is_empty() {
        return Err("SNMP OID cannot be empty".to_string());
    }
    if oid.len() > 128 {
        return Err(format!("SNMP OID too long: {} components (max 128)", oid.len()));
    }
    
    // Check first two components (should be 1.3 for standard OIDs)
    if oid[0] > 2 {
        return Err(format!("Invalid OID root: {} (must be 0, 1, or 2)", oid[0]));
    }
    
    // Whitelist check: OID must start with an allowed prefix
    let is_allowed = ALLOWED_SNMP_OIDS.iter().any(|allowed| {
        oid.len() >= allowed.len() && &oid[..allowed.len()] == *allowed
    });
    
    if !is_allowed {
        let oid_str = oid.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(".");
        return Err(format!("SNMP OID not whitelisted: {}", oid_str));
    }
    
    Ok(())
}

fn snmp_query_raw(socket: &UdpSocket, community: &str, oid: &[u32]) -> Option<Vec<u8>> {
    // Validate OID before querying
    if let Err(e) = validate_snmp_oid(oid) {
        eprintln!("[SNMP ERROR] {}", e);
        return None;
    }
    
    let packet = snmp_get_packet(community, oid);
    if socket.send(&packet).is_err() {
        return None;
    }
    let mut buf = [0u8; 2048];
    if let Ok(n) = socket.recv(&mut buf) {
        return Some(buf[..n].to_vec());
    }
    None
}

fn snmp_query_text(socket: &UdpSocket, community: &str, oid: &[u32]) -> Option<String> {
    let raw = snmp_query_raw(socket, community, oid)?;
    if let Some(v) = extract_snmp_octet_string(&raw) {
        if !v.trim().is_empty() {
            return Some(v);
        }
    }
    extract_snmp_value(&raw).map(|v| v.to_string())
}

fn query_host_storage_volume_usage(
    socket: &UdpSocket,
    community: &str,
    volume_mount: &str,
) -> Option<(i64, i64, i64)> {
    // HOST-RESOURCES-MIB::hrStorageTable lookup by hrStorageDescr (e.g. "/volume1")
    // then read allocation unit, size and used for that index.
    let mut target_idx: Option<u32> = None;

    for idx in 1..=96u32 {
        let descr_oid = [1, 3, 6, 1, 2, 1, 25, 2, 3, 1, 3, idx];
        if let Some(raw) = snmp_query_raw(socket, community, &descr_oid) {
            if let Some(descr) = extract_snmp_octet_string(&raw) {
                if descr.trim() == volume_mount {
                    target_idx = Some(idx);
                    break;
                }
            }
        }
    }

    let idx = target_idx?;
    let alloc_oid = [1, 3, 6, 1, 2, 1, 25, 2, 3, 1, 4, idx];
    let size_oid = [1, 3, 6, 1, 2, 1, 25, 2, 3, 1, 5, idx];
    let used_oid = [1, 3, 6, 1, 2, 1, 25, 2, 3, 1, 6, idx];

    let alloc = snmp_query_raw(socket, community, &alloc_oid)
        .and_then(|raw| extract_snmp_value(&raw))?;
    let size = snmp_query_raw(socket, community, &size_oid)
        .and_then(|raw| extract_snmp_value(&raw))?;
    let used = snmp_query_raw(socket, community, &used_oid)
        .and_then(|raw| extract_snmp_value(&raw))?;

    Some((alloc, size, used))
}

#[tauri::command]
async fn nas_get_status(ip: String, community: Option<String>, port: Option<u16>) -> Result<serde_json::Value, String> {
    let community = community.unwrap_or_else(|| "projektil".to_string());
    let port = port.unwrap_or(161);

    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    socket
        .set_read_timeout(Some(Duration::from_millis(1200)))
        .ok();
    socket
        .connect(format!("{}:{}", ip, port))
        .map_err(|e| e.to_string())?;

    let mut result = serde_json::Map::new();

    let sys_name_oid = [1, 3, 6, 1, 2, 1, 1, 5, 0];
    let snmp_agent_uptime_oid = [1, 3, 6, 1, 2, 1, 1, 3, 0];
    let host_uptime_oid = [1, 3, 6, 1, 2, 1, 25, 1, 1, 0];
    let syno_system_status_oid = [1, 3, 6, 1, 4, 1, 6574, 1, 1, 0];
    let syno_system_temp_oid = [1, 3, 6, 1, 4, 1, 6574, 1, 2, 0];
    let syno_model_oid = [1, 3, 6, 1, 4, 1, 6574, 1, 5, 1, 0];
    let syno_dsm_oid = [1, 3, 6, 1, 4, 1, 6574, 1, 5, 3, 0];

    if let Some(raw) = snmp_query_raw(&socket, &community, &sys_name_oid) {
        if let Some(v) = extract_snmp_octet_string(&raw) {
            result.insert("sys_name".to_string(), serde_json::json!(v));
        }
    }
    if let Some(raw) = snmp_query_raw(&socket, &community, &snmp_agent_uptime_oid) {
        if let Some(v) = extract_snmp_value(&raw) {
            result.insert("snmp_uptime_ticks".to_string(), serde_json::json!(v));
        }
    }
    if let Some(raw) = snmp_query_raw(&socket, &community, &host_uptime_oid) {
        if let Some(v) = extract_snmp_value(&raw) {
            result.insert("sys_uptime_ticks".to_string(), serde_json::json!(v));
        }
    }
    if let Some(raw) = snmp_query_raw(&socket, &community, &syno_system_status_oid) {
        if let Some(v) = extract_snmp_value(&raw) {
            result.insert("system_status".to_string(), serde_json::json!(v));
        }
    }
    if let Some(raw) = snmp_query_raw(&socket, &community, &syno_system_temp_oid) {
        if let Some(v) = extract_snmp_value(&raw) {
            result.insert("system_temp_c".to_string(), serde_json::json!(v));
        }
    }
    if let Some(raw) = snmp_query_raw(&socket, &community, &syno_model_oid) {
        if let Some(v) = extract_snmp_octet_string(&raw) {
            result.insert("model".to_string(), serde_json::json!(v));
        }
    }
    if let Some(raw) = snmp_query_raw(&socket, &community, &syno_dsm_oid) {
        if let Some(v) = extract_snmp_octet_string(&raw) {
            result.insert("dsm_version".to_string(), serde_json::json!(v));
        }
    }

    let mut raids = Vec::<serde_json::Value>::new();
    for idx in 0..=1u32 {
        let name_oid = [1, 3, 6, 1, 4, 1, 6574, 3, 1, 1, 2, idx];
        let status_oid = [1, 3, 6, 1, 4, 1, 6574, 3, 1, 1, 3, idx];
        let name = snmp_query_raw(&socket, &community, &name_oid)
            .and_then(|raw| extract_snmp_octet_string(&raw));
        let status = snmp_query_raw(&socket, &community, &status_oid)
            .and_then(|raw| extract_snmp_value(&raw));
        if name.is_some() || status.is_some() {
            raids.push(serde_json::json!({
                "index": idx,
                "name": name.unwrap_or_else(|| format!("RAID {}", idx + 1)),
                "status": status.unwrap_or(0)
            }));
        }
    }
    if !raids.is_empty() {
        result.insert("raids".to_string(), serde_json::json!(raids));
    }

    let mut disks = Vec::<serde_json::Value>::new();
    for idx in 0..=3u32 {
        let name_oid = [1, 3, 6, 1, 4, 1, 6574, 2, 1, 1, 2, idx];
        let status_oid = [1, 3, 6, 1, 4, 1, 6574, 2, 1, 1, 5, idx];
        let temp_oid = [1, 3, 6, 1, 4, 1, 6574, 2, 1, 1, 6, idx];

        let name = snmp_query_raw(&socket, &community, &name_oid)
            .and_then(|raw| extract_snmp_octet_string(&raw));
        let status = snmp_query_raw(&socket, &community, &status_oid)
            .and_then(|raw| extract_snmp_value(&raw));
        let temp = snmp_query_raw(&socket, &community, &temp_oid)
            .and_then(|raw| extract_snmp_value(&raw));

        if name.is_some() || status.is_some() || temp.is_some() {
            disks.push(serde_json::json!({
                "index": idx,
                "name": name.unwrap_or_else(|| format!("Disk {}", idx + 1)),
                "status": status.unwrap_or(0),
                "temp_c": temp.unwrap_or(0)
            }));
        }
    }
    if !disks.is_empty() {
        result.insert("disks".to_string(), serde_json::json!(disks));
    }

    if let Some((alloc, size, used)) = query_host_storage_volume_usage(&socket, &community, "/volume1") {
        result.insert("vol1_alloc_units".to_string(), serde_json::json!(alloc));
        result.insert("vol1_size_units".to_string(), serde_json::json!(size));
        result.insert("vol1_used_units".to_string(), serde_json::json!(used));
    }
    if let Some((alloc, size, used)) = query_host_storage_volume_usage(&socket, &community, "/volume2") {
        result.insert("vol2_alloc_units".to_string(), serde_json::json!(alloc));
        result.insert("vol2_size_units".to_string(), serde_json::json!(size));
        result.insert("vol2_used_units".to_string(), serde_json::json!(used));
    }

    if result.is_empty() {
        return Err("NAS antwortet nicht auf SNMP-Abfragen".to_string());
    }

    mark_device_online(&format!("nas:{}", ip));
    Ok(serde_json::Value::Object(result))
}

#[tauri::command]
async fn poe_switch_get_status(ip: String, community: Option<String>, port: Option<u16>) -> Result<serde_json::Value, String> {
    let community = community.unwrap_or_else(|| "projektil".to_string());
    let port = port.unwrap_or(161);

    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    socket
        .set_read_timeout(Some(Duration::from_millis(2200)))
        .ok();
    socket
        .connect(format!("{}:{}", ip, port))
        .map_err(|e| e.to_string())?;

    let communities = vec![community.clone()];

    let sys_descr_oid = [1, 3, 6, 1, 2, 1, 1, 1, 0];
    let sys_name_oid = [1, 3, 6, 1, 2, 1, 1, 5, 0];
    let sys_uptime_oid = [1, 3, 6, 1, 2, 1, 1, 3, 0];

    // POWER-ETHERNET-MIB (RFC 3621) base metrics for PoE summary.
    // Group index is commonly 1 on compact switches; try 1 and fallback to 2.
    let poe_oper_status_g1_oid = [1, 3, 6, 1, 2, 1, 105, 1, 3, 1, 1, 3, 1];
    let poe_power_limit_g1_oid = [1, 3, 6, 1, 2, 1, 105, 1, 3, 1, 1, 2, 1];
    let poe_consumption_g1_oid = [1, 3, 6, 1, 2, 1, 105, 1, 3, 1, 1, 4, 1];
    let poe_oper_status_g2_oid = [1, 3, 6, 1, 2, 1, 105, 1, 3, 1, 1, 3, 2];
    let poe_power_limit_g2_oid = [1, 3, 6, 1, 2, 1, 105, 1, 3, 1, 1, 2, 2];
    let poe_consumption_g2_oid = [1, 3, 6, 1, 2, 1, 105, 1, 3, 1, 1, 4, 2];

    for community_try in communities {
        let mut result = serde_json::Map::new();

        if let Some(v) = snmp_query_text(&socket, &community_try, &sys_descr_oid) {
            result.insert("sys_descr".to_string(), serde_json::json!(v));
        }
        if let Some(v) = snmp_query_text(&socket, &community_try, &sys_name_oid) {
            result.insert("sys_name".to_string(), serde_json::json!(v));
        }
        if let Some(raw) = snmp_query_raw(&socket, &community_try, &sys_uptime_oid) {
            if let Some(v) = extract_snmp_value(&raw) {
                result.insert("sys_uptime_ticks".to_string(), serde_json::json!(v));
            }
        }

        let mut poe_oper_status = None;
        let mut poe_limit = None;
        let mut poe_used = None;

        if let Some(raw) = snmp_query_raw(&socket, &community_try, &poe_oper_status_g1_oid) {
            poe_oper_status = extract_snmp_value(&raw);
        }
        if let Some(raw) = snmp_query_raw(&socket, &community_try, &poe_power_limit_g1_oid) {
            poe_limit = extract_snmp_value(&raw);
        }
        if let Some(raw) = snmp_query_raw(&socket, &community_try, &poe_consumption_g1_oid) {
            poe_used = extract_snmp_value(&raw);
        }

        if poe_oper_status.is_none() {
            if let Some(raw) = snmp_query_raw(&socket, &community_try, &poe_oper_status_g2_oid) {
                poe_oper_status = extract_snmp_value(&raw);
            }
        }
        if poe_limit.is_none() {
            if let Some(raw) = snmp_query_raw(&socket, &community_try, &poe_power_limit_g2_oid) {
                poe_limit = extract_snmp_value(&raw);
            }
        }
        if poe_used.is_none() {
            if let Some(raw) = snmp_query_raw(&socket, &community_try, &poe_consumption_g2_oid) {
                poe_used = extract_snmp_value(&raw);
            }
        }

        if let Some(v) = poe_oper_status {
            result.insert("poe_oper_status".to_string(), serde_json::json!(v));
        }
        if let Some(v) = poe_limit {
            result.insert("poe_power_limit_w".to_string(), serde_json::json!(v));
        }
        if let Some(v) = poe_used {
            result.insert("poe_power_used_w".to_string(), serde_json::json!(v));
            if let Some(limit) = result.get("poe_power_limit_w").and_then(|x| x.as_i64()) {
                result.insert("poe_power_free_w".to_string(), serde_json::json!((limit - v).max(0)));
            }
        }

        let probe = format!(
            "{} {}",
            result.get("sys_descr").and_then(|v| v.as_str()).unwrap_or(""),
            result.get("sys_name").and_then(|v| v.as_str()).unwrap_or("")
        )
        .to_lowercase();
        let model = if probe.contains("m4250-8g2xf-poe+") {
            "Netgear M4250-8G2XF-PoE+"
        } else if probe.contains("m4250-40g8f-poe+") {
            "Netgear M4250-40G8F-PoE+"
        } else if probe.contains("m4250-26g4xf-poe+") {
            "Netgear M4250-26G4XF-PoE+"
        } else {
            "Unknown"
        };
        result.insert("detected_model".to_string(), serde_json::json!(model));

        if !result.is_empty() {
            result.insert("snmp_community_used".to_string(), serde_json::json!(community_try));
            mark_device_online(&format!("switch:{}", ip));
            return Ok(serde_json::Value::Object(result));
        }
    }

    Err("SNMP keine Antwort vom PoE-Switch".to_string())
}

#[tauri::command]
async fn rutx50_get_status(ip: String, community: Option<String>, port: Option<u16>) -> Result<serde_json::Value, String> {
    let community = community.unwrap_or_else(|| "projektil".to_string());
    let port = port.unwrap_or(161);

    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    socket
        .set_read_timeout(Some(Duration::from_millis(2200)))
        .ok();
    socket
        .connect(format!("{}:{}", ip, port))
        .map_err(|e| e.to_string())?;

    let mut result = serde_json::Map::new();

    // Standard system branch
    let sys_descr_oid = [1, 3, 6, 1, 2, 1, 1, 1, 0];
    let sys_name_oid = [1, 3, 6, 1, 2, 1, 1, 5, 0];
    let sys_uptime_oid = [1, 3, 6, 1, 2, 1, 1, 3, 0];

    // Teltonika enterprise branch: 1.3.6.1.4.1.48690
    let tel_device_name_oid = [1, 3, 6, 1, 4, 1, 48690, 1, 2, 0];
    let tel_product_code_oid = [1, 3, 6, 1, 4, 1, 48690, 1, 3, 0];
    let tel_fw_oid = [1, 3, 6, 1, 4, 1, 48690, 1, 6, 0];
    let tel_device_uptime_oid = [1, 3, 6, 1, 4, 1, 48690, 1, 7, 0];
    let tel_cpu_usage_oid = [1, 3, 6, 1, 4, 1, 48690, 1, 8, 0];
    let tel_mobile_uptime_oid = [1, 3, 6, 1, 4, 1, 48690, 2, 3, 0];

    // modemTable row 1: 1.3.6.1.4.1.48690.2.2.1.<column>.1
    let tel_modem_model_oid = [1, 3, 6, 1, 4, 1, 48690, 2, 2, 1, 4, 1];
    let tel_net_state_oid = [1, 3, 6, 1, 4, 1, 48690, 2, 2, 1, 11, 1];
    let tel_signal_oid = [1, 3, 6, 1, 4, 1, 48690, 2, 2, 1, 12, 1];
    let tel_operator_oid = [1, 3, 6, 1, 4, 1, 48690, 2, 2, 1, 13, 1];
    let tel_conn_state_oid = [1, 3, 6, 1, 4, 1, 48690, 2, 2, 1, 15, 1];
    let tel_net_type_oid = [1, 3, 6, 1, 4, 1, 48690, 2, 2, 1, 16, 1];
    let tel_cell_id_oid = [1, 3, 6, 1, 4, 1, 48690, 2, 2, 1, 18, 1];
    let tel_sinr_oid = [1, 3, 6, 1, 4, 1, 48690, 2, 2, 1, 19, 1];
    let tel_rsrp_oid = [1, 3, 6, 1, 4, 1, 48690, 2, 2, 1, 20, 1];
    let tel_rsrq_oid = [1, 3, 6, 1, 4, 1, 48690, 2, 2, 1, 21, 1];
    let tel_modem_ip_oid = [1, 3, 6, 1, 4, 1, 48690, 2, 2, 1, 24, 1];

    let string_queries: [(&str, &[u32]); 18] = [
        ("sys_descr", &sys_descr_oid),
        ("sys_name", &sys_name_oid),
        ("device_name", &tel_device_name_oid),
        ("product_code", &tel_product_code_oid),
        ("fw_version", &tel_fw_oid),
        ("device_uptime_s", &tel_device_uptime_oid),
        ("cpu_usage", &tel_cpu_usage_oid),
        ("modem_model", &tel_modem_model_oid),
        ("net_state", &tel_net_state_oid),
        ("signal", &tel_signal_oid),
        ("operator", &tel_operator_oid),
        ("connection_state", &tel_conn_state_oid),
        ("network_type", &tel_net_type_oid),
        ("cell_id", &tel_cell_id_oid),
        ("sinr", &tel_sinr_oid),
        ("rsrp", &tel_rsrp_oid),
        ("rsrq", &tel_rsrq_oid),
        ("modem_ip", &tel_modem_ip_oid),
    ];

    for (key, oid) in string_queries {
        if let Some(v) = snmp_query_text(&socket, &community, oid) {
            result.insert(key.to_string(), serde_json::json!(v));
        }
    }

    let numeric_queries: [(&str, &[u32]); 2] = [
        ("sys_uptime_ticks", &sys_uptime_oid),
        ("mobile_uptime_s", &tel_mobile_uptime_oid),
    ];

    for (key, oid) in numeric_queries {
        if let Some(raw) = snmp_query_raw(&socket, &community, oid) {
            if let Some(v) = extract_snmp_value(&raw) {
                result.insert(key.to_string(), serde_json::json!(v));
            }
        }
    }

    let descr = result
        .get("sys_descr")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    let detected = if descr.contains("rutx50") {
        "Teltonika RUTX50"
    } else {
        "Unknown"
    };
    result.insert("detected_model".to_string(), serde_json::json!(detected));

    if result.is_empty() {
        return Err("SNMP keine Antwort vom RUTX50".to_string());
    }

    mark_device_online(&format!("rutx50:{}", ip));
    Ok(serde_json::Value::Object(result))
}

fn snmp_get_packet(community: &str, oid: &[u32]) -> Vec<u8> {
    let comm = community.as_bytes();
    let oid_bytes = encode_oid(oid);
    let mut varbind = Vec::new();
    varbind.push(0x06u8);
    varbind.extend(encode_length(oid_bytes.len()));
    varbind.extend(&oid_bytes);
    varbind.extend(&[0x05, 0x00]);
    let mut varbind_seq = Vec::new();
    varbind_seq.push(0x30u8);
    varbind_seq.extend(encode_length(varbind.len()));
    varbind_seq.extend(&varbind);
    let mut varbind_list = Vec::new();
    varbind_list.push(0x30u8);
    varbind_list.extend(encode_length(varbind_seq.len()));
    varbind_list.extend(&varbind_seq);
    let mut pdu_inner = Vec::new();
    pdu_inner.extend(&[0x02, 0x01, 0x01]);
    pdu_inner.extend(&[0x02, 0x01, 0x00]);
    pdu_inner.extend(&[0x02, 0x01, 0x00]);
    pdu_inner.extend(&varbind_list);
    let mut pdu = Vec::new();
    pdu.push(0xa0u8);
    pdu.extend(encode_length(pdu_inner.len()));
    pdu.extend(&pdu_inner);
    let mut msg_inner = Vec::new();
    msg_inner.extend(&[0x02, 0x01, 0x00]);
    msg_inner.push(0x04u8);
    msg_inner.extend(encode_length(comm.len()));
    msg_inner.extend(comm);
    msg_inner.extend(&pdu);
    let mut msg = Vec::new();
    msg.push(0x30u8);
    msg.extend(encode_length(msg_inner.len()));
    msg.extend(&msg_inner);
    msg
}

fn encode_oid(oid: &[u32]) -> Vec<u8> {
    let mut bytes = vec![oid[0] as u8 * 40 + oid[1] as u8];
    for &n in &oid[2..] {
        if n < 128 {
            bytes.push(n as u8);
        } else if n < 16384 {
            bytes.push(0x80 | (n >> 7) as u8);
            bytes.push((n & 0x7f) as u8);
        } else {
            bytes.push(0x80 | (n >> 14) as u8);
            bytes.push(0x80 | ((n >> 7) & 0x7f) as u8);
            bytes.push((n & 0x7f) as u8);
        }
    }
    bytes
}

fn encode_length(len: usize) -> Vec<u8> {
    if len < 128 { vec![len as u8] } else { vec![0x81, len as u8] }
}

// ============================================================
// Janitza UMG96RM-E — Modbus TCP Port 502
// Register-Map (Float32 Big-Endian, 2 Reg = 4 Bytes):
//   19000 = U_L1_N (V)
//   19002 = U_L2_N (V)
//   19004 = U_L3_N (V)
//   19012 = I_L1 (A)
//   19014 = I_L2 (A)
//   19016 = I_L3 (A)
//   19026 = P_gesamt (W)
//   19050 = Frequenz (Hz)
// ============================================================
#[tauri::command]
async fn janitza_get_data(ip: String) -> Result<serde_json::Value, String> {
    let _pool_key = get_pooled_connection(&ip, 502, "modbus_tcp");  // Register in connection pool
    let addr = format!("{}:502", ip);
    let mut stream = TcpStream::connect_timeout(
        &addr.parse::<std::net::SocketAddr>().map_err(|e| e.to_string())?,
        Duration::from_millis(2000),
    ).map_err(|e| format!("Modbus connect: {}", e))?;
    stream.set_read_timeout(Some(Duration::from_millis(2000))).ok();

    fn modbus_read(stream: &mut TcpStream, start_reg: u16, count: u16) -> Result<Vec<u8>, String> {
        let req = [
            0x00, 0x01, 0x00, 0x00, 0x00, 0x06,
            0x01, 0x03,
            (start_reg >> 8) as u8, (start_reg & 0xFF) as u8,
            (count >> 8) as u8,     (count & 0xFF) as u8,
        ];
        stream.write_all(&req).map_err(|e| e.to_string())?;
        let mut resp = vec![0u8; 9 + count as usize * 2];
        stream.read_exact(&mut resp).map_err(|e| e.to_string())?;
        Ok(resp[9..].to_vec())
    }

    fn reg_to_f32(data: &[u8], byte_offset: usize) -> f32 {
        if byte_offset + 4 > data.len() { return 0.0; }
        f32::from_be_bytes([
            data[byte_offset], data[byte_offset+1],
            data[byte_offset+2], data[byte_offset+3],
        ])
    }

    // Wir lesen einen Block von 52 Registern (104 Bytes) ab Adresse 19000.
    // Dies deckt Spannungen (19000), Ströme (19012), Leistung (19026) und Frequenz (19050) ab.
    let data_block = modbus_read(&mut stream, 19000, 52)
        .unwrap_or_else(|e| { eprintln!("[Janitza] Lese-Fehler: {}", e); vec![0u8; 104] });

    let v_l1 = reg_to_f32(&data_block, 0);   // Reg 19000
    let v_l2 = reg_to_f32(&data_block, 4);   // Reg 19002
    let v_l3 = reg_to_f32(&data_block, 8);   // Reg 19004
    
    let i_l1 = reg_to_f32(&data_block, 24);  // Reg 19012
    let i_l2 = reg_to_f32(&data_block, 28);  // Reg 19014
    let i_l3 = reg_to_f32(&data_block, 32);  // Reg 19016

    let power_w  = reg_to_f32(&data_block, 52); // (19026-19000)*2 = 52
    let power_kw = (power_w / 1000.0).max(0.0); // Verhindert negative Werte durch Messrauschen

    let freq = reg_to_f32(&data_block, 100);    // (19050-19000)*2 = 100

    let cfg = get_config();
    let warnings = check_janitza_anomalies(v_l1, v_l2, v_l3, i_l1, i_l2, i_l3, freq, power_kw, &cfg);

    mark_device_online(&format!("janitza:{}", ip));
    Ok(serde_json::json!({
        "v_l1":      v_l1,
        "v_l2":      v_l2,
        "v_l3":      v_l3,
        "i_l1":      i_l1,
        "i_l2":      i_l2,
        "i_l3":      i_l3,
        "frequency": freq,
        "power_kw":  power_kw,
        "warnings":  warnings,
    }))
}

#[tauri::command]
async fn d40_command(ip: String, command: String) -> Result<String, String> {
    oca::send_command(&ip, &command).await.map_err(|e| e.to_string())
}
#[tauri::command]
async fn d40_ping(ip: String) -> Result<bool, String> {
    let result = oca::ping(&ip).await.map_err(|e| e.to_string())?;
    if result {
        mark_device_online(&format!("d40:{}", ip));
    }
    Ok(result)
}
#[tauri::command]
async fn d40_status(ip: String) -> Result<serde_json::Value, String> {
    oca::get_status(&ip).await.map_err(|e| e.to_string())
}
#[tauri::command]
async fn d40_set_gain(ip: String, channel: u8, current: f32, target: f32) -> Result<String, String> {
    oca::set_gain(&ip, channel as usize, current, target)
        .await
        .map_err(|e| e.to_string())
}

// ============================================================================
// INTEGRATED DEVICE QUERY COMMANDS - With caching, dedup, and health tracking
// ============================================================================

#[tauri::command]
async fn ups_get_status_managed(ip: String, app: AppHandle) -> Result<serde_json::Value, String> {
    let cache_key = format!("ups:{}", ip);
    let dedup_key = format!("ups_dedup:{}", ip);
    
    check_rate_limit("ups_query")?;
    
    // Try dedup first (5s window)
    if let Some(cached) = get_deduplicated_request(&dedup_key, || None) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&cached) {
            return Ok(value);
        }
    }
    
    // Check cache (30s window)
    if let Ok(cache) = query_cache().lock() {
        if let Some(cached) = cache.get(&cache_key) {
            let age_ms = now_timestamp_ms().saturating_sub(cached.timestamp_ms);
            if age_ms < QUERY_CACHE_TTL_MS {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&cached.data) {
                    return Ok(value);
                }
            }
        }
    }
    
    // Fetch fresh data
    match ups_get_status(ip.clone()).await {
        Ok(result) => {
            mark_device_online(&format!("ups:{}", ip));
            
            let result_str = result.to_string();
            if let Ok(mut cache) = query_cache().lock() {
                cache.insert(cache_key.clone(), CachedQuery {
                    data: result_str.clone(),
                    timestamp_ms: now_timestamp_ms(),
                });
            }
            
            let _ = get_deduplicated_request(&dedup_key, || Some(result_str));
            Ok(result)
        }
        Err(e) => {
            log_error_with_category(
                ErrorCategory::DeviceOffline,
                &e,
                Some(&format!("ups:{}", ip)),
                Some(&app)
            );
            Err(e)
        }
    }
}

#[tauri::command]
async fn janitza_get_data_managed(ip: String, app: AppHandle) -> Result<serde_json::Value, String> {
    let cache_key = format!("janitza:{}", ip);
    let dedup_key = format!("janitza_dedup:{}", ip);
    
    check_rate_limit("janitza_query")?;
    
    if let Some(cached) = get_deduplicated_request(&dedup_key, || None) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&cached) {
            return Ok(value);
        }
    }
    
    if let Ok(cache) = query_cache().lock() {
        if let Some(cached) = cache.get(&cache_key) {
            let age_ms = now_timestamp_ms().saturating_sub(cached.timestamp_ms);
            if age_ms < QUERY_CACHE_TTL_MS {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&cached.data) {
                    return Ok(value);
                }
            }
        }
    }
    
    match janitza_get_data(ip.clone()).await {
        Ok(result) => {
            mark_device_online(&format!("janitza:{}", ip));
            
            let result_str = result.to_string();
            if let Ok(mut cache) = query_cache().lock() {
                cache.insert(cache_key.clone(), CachedQuery {
                    data: result_str.clone(),
                    timestamp_ms: now_timestamp_ms(),
                });
            }
            
            let _ = get_deduplicated_request(&dedup_key, || Some(result_str));
            Ok(result)
        }
        Err(e) => {
            log_error_with_category(
                ErrorCategory::DeviceOffline,
                &e,
                Some(&format!("janitza:{}", ip)),
                Some(&app)
            );
            Err(e)
        }
    }
}

#[tauri::command]
async fn nas_get_status_managed(ip: String, community: Option<String>, port: Option<u16>, app: AppHandle) -> Result<serde_json::Value, String> {
    let cache_key = format!("nas:{}:{}:{}", ip, community.as_ref().unwrap_or(&"public".to_string()), port.unwrap_or(161));
    
    check_rate_limit("nas_query")?;
    
    if let Ok(cache) = query_cache().lock() {
        if let Some(cached) = cache.get(&cache_key) {
            let age_ms = now_timestamp_ms().saturating_sub(cached.timestamp_ms);
            if age_ms < QUERY_CACHE_TTL_MS {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&cached.data) {
                    return Ok(value);
                }
            }
        }
    }
    
    match nas_get_status(ip.clone(), community, port).await {
        Ok(result) => {
            mark_device_online(&format!("nas:{}", ip));
            
            let result_str = result.to_string();
            if let Ok(mut cache) = query_cache().lock() {
                cache.insert(cache_key, CachedQuery {
                    data: result_str,
                    timestamp_ms: now_timestamp_ms(),
                });
            }
            Ok(result)
        }
        Err(e) => {
            log_error_with_category(
                ErrorCategory::DeviceOffline,
                &e,
                Some(&format!("nas:{}", ip)),
                Some(&app)
            );
            Err(e)
        }
    }
}

#[tauri::command]
async fn poe_switch_get_status_managed(ip: String, community: Option<String>, port: Option<u16>, app: AppHandle) -> Result<serde_json::Value, String> {
    let cache_key = format!("switch:{}:{}:{}", ip, community.as_ref().unwrap_or(&"public".to_string()), port.unwrap_or(161));
    
    check_rate_limit("switch_query")?;
    
    if let Ok(cache) = query_cache().lock() {
        if let Some(cached) = cache.get(&cache_key) {
            let age_ms = now_timestamp_ms().saturating_sub(cached.timestamp_ms);
            if age_ms < QUERY_CACHE_TTL_MS {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&cached.data) {
                    return Ok(value);
                }
            }
        }
    }
    
    match poe_switch_get_status(ip.clone(), community, port).await {
        Ok(result) => {
            mark_device_online(&format!("switch:{}", ip));
            
            let result_str = result.to_string();
            if let Ok(mut cache) = query_cache().lock() {
                cache.insert(cache_key, CachedQuery {
                    data: result_str,
                    timestamp_ms: now_timestamp_ms(),
                });
            }
            Ok(result)
        }
        Err(e) => {
            log_error_with_category(
                ErrorCategory::DeviceOffline,
                &e,
                Some(&format!("switch:{}", ip)),
                Some(&app)
            );
            Err(e)
        }
    }
}

#[tauri::command]
async fn rutx50_get_status_managed(ip: String, community: Option<String>, port: Option<u16>, app: AppHandle) -> Result<serde_json::Value, String> {
    let cache_key = format!("rutx50:{}:{}:{}", ip, community.as_ref().unwrap_or(&"public".to_string()), port.unwrap_or(161));
    
    check_rate_limit("rutx50_query")?;
    
    if let Ok(cache) = query_cache().lock() {
        if let Some(cached) = cache.get(&cache_key) {
            let age_ms = now_timestamp_ms().saturating_sub(cached.timestamp_ms);
            if age_ms < QUERY_CACHE_TTL_MS {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&cached.data) {
                    return Ok(value);
                }
            }
        }
    }
    
    match rutx50_get_status(ip.clone(), community, port).await {
        Ok(result) => {
            mark_device_online(&format!("rutx50:{}", ip));
            
            let result_str = result.to_string();
            if let Ok(mut cache) = query_cache().lock() {
                cache.insert(cache_key, CachedQuery {
                    data: result_str,
                    timestamp_ms: now_timestamp_ms(),
                });
            }
            Ok(result)
        }
        Err(e) => {
            log_error_with_category(
                ErrorCategory::DeviceOffline,
                &e,
                Some(&format!("rutx50:{}", ip)),
                Some(&app)
            );
            Err(e)
        }
    }
}

#[tauri::command]
fn minimize_window(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") { let _ = w.minimize(); }
}
#[tauri::command]
fn toggle_fullscreen(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.set_fullscreen(!w.is_fullscreen().unwrap_or(false));
    }
}
#[tauri::command]
fn hide_to_tray(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") { let _ = w.hide(); }
}
#[tauri::command]
fn quit_app(app: AppHandle) { app.exit(0); }

#[tauri::command]
fn open_external_url(url: String) -> Result<bool, String> {
    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(&url)
            .spawn()
            .map_err(|e| format!("Konnte Browser nicht öffnen: {}", e))?;
        return Ok(true);
    }

    #[cfg(target_os = "windows")]
    {
        let mut open_cmd = Command::new("rundll32");
        #[cfg(target_os = "windows")]
        {
            open_cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        open_cmd
            .args(["url.dll,FileProtocolHandler", &url])
            .spawn()
            .map_err(|e| format!("Konnte Browser nicht öffnen: {}", e))?;
        return Ok(true);
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open")
            .arg(&url)
            .spawn()
            .map_err(|e| format!("Konnte Browser nicht öffnen: {}", e))?;
        return Ok(true);
    }

    #[allow(unreachable_code)]
    Err("Diese Plattform wird für URL-Open nicht unterstützt".to_string())
}

#[tauri::command]
fn companion_press_emergency_button(url: String) -> Result<bool, String> {
    let target_url = if url.trim().is_empty() {
        "http://192.168.1.42:8000/api/location/1/2/0/press".to_string()
    } else {
        url
    };

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .map_err(|e| format!("Companion-Client konnte nicht erstellt werden: {}", e))?;

    client
        .post(&target_url)
        .send()
        .and_then(|response| response.error_for_status())
        .map_err(|e| format!("Companion emergency press fehlgeschlagen: {}", e))?;

    Ok(true)
}

fn osc_padded_string_bytes(value: &str) -> Vec<u8> {
    let mut out = value.as_bytes().to_vec();
    out.push(0);
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out
}

fn build_notaus_osc_packet() -> Vec<u8> {
    let mut packet = Vec::new();
    packet.extend_from_slice(&osc_padded_string_bytes("/emergency"));
    packet.extend_from_slice(&osc_padded_string_bytes(",i"));
    packet.extend_from_slice(&1_i32.to_be_bytes());
    packet
}

#[tauri::command]
fn send_emergency_notaus_osc(target_ip: Option<String>, target_port: Option<u16>) -> Result<bool, String> {
    let ip = target_ip
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "192.168.1.31".to_string());
    let port = target_port.unwrap_or(8000);
    let addr = format!("{}:{}", ip, port);

    let msg = format!("[OSC OUT] Sending /emergency to {}", addr);
    eprintln!("{}", msg);
    if let Some(app) = APP_HANDLE.get() {
        let _ = write_app_log("info", &msg, now_timestamp_ms(), Some(&app));
    }

    let socket = UdpSocket::bind("0.0.0.0:0")
        .map_err(|e| format!("OSC Socket konnte nicht erstellt werden: {}", e))?;
    socket
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| format!("OSC Write-Timeout konnte nicht gesetzt werden: {}", e))?;

    let packet = build_notaus_osc_packet();
    let sent = socket
        .send_to(&packet, &addr)
        .map_err(|e| {
            let err_msg = format!("[OSC OUT ERROR] /emergency send failed: {}", e);
            eprintln!("{}", err_msg);
            if let Some(app) = APP_HANDLE.get() {
                let _ = write_app_log("error", &err_msg, now_timestamp_ms(), Some(&app));
            }
            err_msg
        })?;
    if sent != packet.len() {
        let err_msg = format!(
            "[OSC OUT ERROR] /emergency incomplete: {} von {} Bytes",
            sent,
            packet.len()
        );
        eprintln!("{}", err_msg);
        if let Some(app) = APP_HANDLE.get() {
            let _ = write_app_log("error", &err_msg, now_timestamp_ms(), Some(&app));
        }
        return Err(err_msg);
    }

    let success_msg = format!("[OSC OUT] /emergency sent successfully ({} bytes)", sent);
    eprintln!("{}", success_msg);
    if let Some(app) = APP_HANDLE.get() {
        let _ = write_app_log("info", &success_msg, now_timestamp_ms(), Some(&app));
    }
    Ok(true)
}

#[tauri::command]
fn send_emergency_osc_to_switch() -> Result<bool, String> {
    let addr = "192.168.1.99:9000";
    let msg = format!("[OSC OUT] Sending /projektil_control_pressed to {}", addr);
    eprintln!("{}", msg);
    if let Some(app) = APP_HANDLE.get() {
        let _ = write_app_log("info", &msg, now_timestamp_ms(), Some(&app));
    }

    let socket = UdpSocket::bind("0.0.0.0:0")
        .map_err(|e| format!("OSC Socket konnte nicht erstellt werden: {}", e))?;
    socket
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| format!("OSC Write-Timeout konnte nicht gesetzt werden: {}", e))?;

    let mut packet = Vec::new();
    packet.extend_from_slice(&osc_padded_string_bytes("/projektil_control_pressed"));
    packet.extend_from_slice(&osc_padded_string_bytes(",i"));
    packet.extend_from_slice(&1_i32.to_be_bytes());

    let sent = socket
        .send_to(&packet, addr)
        .map_err(|e| {
            let err_msg = format!("[OSC OUT ERROR] /projektil_control_pressed send failed: {}", e);
            eprintln!("{}", err_msg);
            if let Some(app) = APP_HANDLE.get() {
                let _ = write_app_log("error", &err_msg, now_timestamp_ms(), Some(&app));
            }
            err_msg
        })?;
    if sent != packet.len() {
        let err_msg = format!(
            "[OSC OUT ERROR] /projektil_control_pressed incomplete: {} von {} Bytes",
            sent,
            packet.len()
        );
        eprintln!("{}", err_msg);
        if let Some(app) = APP_HANDLE.get() {
            let _ = write_app_log("error", &err_msg, now_timestamp_ms(), Some(&app));
        }
        return Err(err_msg);
    }

    let success_msg = format!("[OSC OUT] /projektil_control_pressed sent successfully ({} bytes)", sent);
    eprintln!("{}", success_msg);
    if let Some(app) = APP_HANDLE.get() {
        let _ = write_app_log("info", &success_msg, now_timestamp_ms(), Some(&app));
    }
    Ok(true)
}

#[tauri::command]
fn send_emergency_reset_osc() -> Result<bool, String> {
    let addr = "192.168.1.99:9000";
    let msg = format!("[OSC OUT] Sending /emergency_reset to {}", addr);
    eprintln!("{}", msg);
    if let Some(app) = APP_HANDLE.get() {
        let _ = write_app_log("info", &msg, now_timestamp_ms(), Some(&app));
    }

    let socket = UdpSocket::bind("0.0.0.0:0")
        .map_err(|e| format!("OSC Socket konnte nicht erstellt werden: {}", e))?;
    socket
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| format!("OSC Write-Timeout konnte nicht gesetzt werden: {}", e))?;

    let mut packet = Vec::new();
    packet.extend_from_slice(&osc_padded_string_bytes("/emergency_reset"));
    packet.extend_from_slice(&osc_padded_string_bytes(",i"));
    packet.extend_from_slice(&1_i32.to_be_bytes());

    let sent = socket
        .send_to(&packet, addr)
        .map_err(|e| {
            let err_msg = format!("[OSC OUT ERROR] /emergency_reset send failed: {}", e);
            eprintln!("{}", err_msg);
            if let Some(app) = APP_HANDLE.get() {
                let _ = write_app_log("error", &err_msg, now_timestamp_ms(), Some(&app));
            }
            err_msg
        })?;
    if sent != packet.len() {
        let err_msg = format!(
            "[OSC OUT ERROR] /emergency_reset incomplete: {} von {} Bytes",
            sent,
            packet.len()
        );
        eprintln!("{}", err_msg);
        if let Some(app) = APP_HANDLE.get() {
            let _ = write_app_log("error", &err_msg, now_timestamp_ms(), Some(&app));
        }
        return Err(err_msg);
    }

    let success_msg = format!("[OSC OUT] /emergency_reset sent successfully ({} bytes)", sent);
    eprintln!("{}", success_msg);
    if let Some(app) = APP_HANDLE.get() {
        let _ = write_app_log("info", &success_msg, now_timestamp_ms(), Some(&app));
    }
    Ok(true)
}

fn default_config_json() -> serde_json::Value {
    serde_json::json!({
        "demo_mode": false,
        "startup_mode": "admin",
        "language": "de",
        "camera_view_mode": "snapshot",
        "demo_amp_mutes": {
            "1": [false, false, false, false],
            "2": [false, false, false, false],
            "3": [false, false, false, false]
        },
        "demo_projector_on": [false, false, false, false, false, false, false, false, false, false, false, false, false, false, false, false],
        "demo_projector_mute": [false, false, false, false, false, false, false, false, false, false, false, false, false, false, false, false],
        "projector_control_states": ["unknown", "unknown", "unknown", "unknown", "unknown", "unknown", "unknown", "unknown", "unknown", "unknown", "unknown", "unknown", "unknown", "unknown", "unknown", "unknown"],
        "projector_status_updated_at": 0,
        "pixera_ip": "192.168.1.31", "pixera_port": 1338,
        "pixera_octo1_ip": "192.168.1.32", "pixera_octo2_ip": "192.168.1.33",
        "pixera_octo_port": 4000,
        "pixera_octo_count": 2,
        "pixera_api_root": "",
        "pixera_pjlink_module": "PJ_Link__16ch",
        "pixera_scheduler_module": "Projektil_EventScheduler_V2_7",
        "d40_01_ip": "192.168.1.51", "d40_02_ip": "192.168.1.52", "d40_03_ip": "192.168.1.53", "amp_count": 2, "d40_oca_port": 50014,
        "nas_ip": "192.168.1.21", "nas_port": 5000,
        "nas_snmp_port": 161, "nas_snmp_community": "projektil",
        "poe_switch_ip": "192.168.1.11", "poe_switch_name": "", "poe_switch_ping_port": 443,
        "poe_switch_snmp_port": 161, "poe_switch_snmp_community": "projektil",
        "rutx50_ip": "192.168.1.1", "rutx50_ping_port": 443,
        "rutx50_snmp_port": 161, "rutx50_snmp_community": "public",
        "ups_ip": "192.168.1.6", "power_disp_ip": "192.168.1.5",
        "cam_01_ip": "192.168.1.22", "cam_02_ip": "192.168.1.23",
        "projector_start": 101, "projector_count": 16,
        "interactive_enabled": false,
        "interactive_scanner_count": 2,
        "emergency_switch_enabled": false,
        "hotline": "+41 44 492 51 69",
        "location_name": "",
        "anydesk_address": "",
        "hub_api_url": "",
        "hub_api_token": "",
        "hub_project_id": "",
        "hub_device_id": "",
        "telegram": {
            "enabled": false,
            "bot_token": "",
            "chat_id": "",
            "alert_events": [
                "janitza_offline",
                "janitza_asymmetry",
                "janitza_overfrequency",
                "janitza_underfrequency",
                "ups_offline",
                "ups_battery",
                "nas_offline",
                "poe_switch_offline",
                "rutx_offline",
                "pixera_offline",
                "trigger_missed",
                "timeline_error",
                "emergency",
                "panic",
                "keyword_match"
            ],
            "critical_error_keywords": [
                "sofortalarm",
                "batteriebetrieb",
                "panic",
                "emergency",
                "offline!"
            ]
        },
        "thresholds": {
            "v_min": 195.0,
            "v_max": 253.0,
            "v_imbal": 15.0,
            "f_min": 49.5,
            "f_max": 50.5,
            "i_max_32": 28.0,
            "i_max_63": 58.0,
            "ups_load_warn": 80
        }
    })
}

fn ensure_config_defaults(cfg: &mut serde_json::Value) {
    let Some(obj) = cfg.as_object_mut() else {
        *cfg = default_config_json();
        return;
    };

    if !obj.contains_key("pixera_octo_port") {
        obj.insert("pixera_octo_port".to_string(), serde_json::json!(4000));
    }
    if !obj.contains_key("pixera_octo_count") {
        obj.insert("pixera_octo_count".to_string(), serde_json::json!(2));
    }
    if !obj.contains_key("amp_count") {
        obj.insert("amp_count".to_string(), serde_json::json!(2));
    }
    if !obj.contains_key("interactive_enabled") {
        obj.insert("interactive_enabled".to_string(), serde_json::json!(false));
    }
    if !obj.contains_key("interactive_scanner_count") {
        obj.insert("interactive_scanner_count".to_string(), serde_json::json!(2));
    }
    if !obj.contains_key("emergency_switch_enabled") {
        obj.insert("emergency_switch_enabled".to_string(), serde_json::json!(false));
    }
    if !obj.contains_key("d40_03_ip") {
        obj.insert("d40_03_ip".to_string(), serde_json::json!("192.168.1.53"));
    }
    if !obj.contains_key("pixera_api_root") {
        obj.insert("pixera_api_root".to_string(), serde_json::json!(""));
    }
    if !obj.contains_key("pixera_pjlink_module") {
        obj.insert("pixera_pjlink_module".to_string(), serde_json::json!("PJ_Link__16ch"));
    }
    if !obj.contains_key("pixera_scheduler_module") {
        obj.insert(
            "pixera_scheduler_module".to_string(),
            serde_json::json!("Projektil_EventScheduler_V2_7"),
        );
    }
    if !obj.contains_key("hub_api_url") {
        obj.insert("hub_api_url".to_string(), serde_json::json!(""));
    }
    if !obj.contains_key("hub_api_token") {
        obj.insert("hub_api_token".to_string(), serde_json::json!(""));
    }
    if !obj.contains_key("hub_project_id") {
        obj.insert("hub_project_id".to_string(), serde_json::json!(""));
    }
    if !obj.contains_key("hub_device_id") {
        obj.insert("hub_device_id".to_string(), serde_json::json!(""));
    }
    if !obj.contains_key("demo_mode") {
        obj.insert("demo_mode".to_string(), serde_json::json!(false));
    }
    if !obj.contains_key("startup_mode") {
        obj.insert("startup_mode".to_string(), serde_json::json!("admin"));
    }
    if !obj.contains_key("language") {
        obj.insert("language".to_string(), serde_json::json!("de"));
    }
    if !obj.contains_key("camera_view_mode") {
        obj.insert("camera_view_mode".to_string(), serde_json::json!("snapshot"));
    }
    if !obj.contains_key("demo_amp_mutes") {
        obj.insert(
            "demo_amp_mutes".to_string(),
            serde_json::json!({
                "1": [false, false, false, false],
                "2": [false, false, false, false],
                "3": [false, false, false, false]
            }),
        );
    }
    if !obj.contains_key("demo_projector_on") {
        obj.insert(
            "demo_projector_on".to_string(),
            serde_json::json!([false, false, false, false, false, false, false, false, false, false, false, false, false, false, false, false]),
        );
    }
    if !obj.contains_key("demo_projector_mute") {
        obj.insert(
            "demo_projector_mute".to_string(),
            serde_json::json!([false, false, false, false, false, false, false, false, false, false, false, false, false, false, false, false]),
        );
    }
    if !obj.contains_key("projector_control_states") {
        obj.insert(
            "projector_control_states".to_string(),
            serde_json::json!(["unknown", "unknown", "unknown", "unknown", "unknown", "unknown", "unknown", "unknown", "unknown", "unknown", "unknown", "unknown", "unknown", "unknown", "unknown", "unknown"]),
        );
    }
    if !obj.contains_key("projector_status_updated_at") {
        obj.insert("projector_status_updated_at".to_string(), serde_json::json!(0));
    }

    let telegram = obj
        .entry("telegram".to_string())
        .or_insert_with(|| serde_json::json!({}));
    if let Some(t) = telegram.as_object_mut() {
        if !t.contains_key("enabled") {
            t.insert("enabled".to_string(), serde_json::json!(false));
        }
        if !t.contains_key("bot_token") {
            t.insert("bot_token".to_string(), serde_json::json!(""));
        }
        if !t.contains_key("chat_id") {
            t.insert("chat_id".to_string(), serde_json::json!(""));
        }
        if !t.contains_key("alert_events") {
            t.insert(
                "alert_events".to_string(),
                serde_json::json!([
                    "janitza_offline",
                    "janitza_asymmetry",
                    "janitza_overfrequency",
                    "janitza_underfrequency",
                    "ups_offline",
                    "ups_battery",
                    "nas_offline",
                    "poe_switch_offline",
                    "rutx_offline",
                    "pixera_offline",
                    "trigger_missed",
                    "timeline_error",
                    "emergency",
                    "panic",
                    "keyword_match"
                ]),
            );
        }
        if !t.contains_key("critical_error_keywords") {
            t.insert(
                "critical_error_keywords".to_string(),
                serde_json::json!(["sofortalarm", "batteriebetrieb", "panic", "emergency", "offline!"]),
            );
        }
    }
}

fn config_path_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();

    if let Some(app) = APP_HANDLE.get() {
        if let Ok(app_data) = app.path().app_data_dir() {
            out.push(app_data.join("config.json"));
        }
        if let Ok(app_cfg) = app.path().app_config_dir() {
            out.push(app_cfg.join("config.json"));
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            out.push(dir.join("config.json"));
            if let Some(parent) = dir.parent() {
                out.push(parent.join("config.json"));
            }
        }
    }

    if let Ok(cwd) = std::env::current_dir() {
        // During `tauri dev` the cwd is often `<repo>/src-tauri`.
        // Writing UI sync state into that file triggers the dev watcher and restarts endlessly.
        if cwd
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.eq_ignore_ascii_case("src-tauri"))
            .unwrap_or(false)
        {
            if let Some(parent) = cwd.parent() {
                out.push(parent.join("config.json"));
            }
        }
        out.push(cwd.join("config.json"));
    }

    out.push(PathBuf::from("config.json"));

    let mut dedup = Vec::new();
    for p in out {
        if !dedup.iter().any(|e: &PathBuf| e == &p) {
            dedup.push(p);
        }
    }
    dedup
}

fn is_src_tauri_config_path(path: &Path) -> bool {
    let is_config = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.eq_ignore_ascii_case("config.json"))
        .unwrap_or(false);
    if !is_config {
        return false;
    }
    path.parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .map(|n| n.eq_ignore_ascii_case("src-tauri"))
        .unwrap_or(false)
}

fn read_config_json_from_disk() -> Option<(PathBuf, serde_json::Value)> {
    for path in config_path_candidates() {
        // Never treat src-tauri/config.json as runtime state file.
        if is_src_tauri_config_path(&path) {
            continue;
        }
        let content = match fs::read_to_string(&path) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let json = match serde_json::from_str::<serde_json::Value>(&content) {
            Ok(v) => v,
            Err(_) => continue,
        };
        return Some((path, json));
    }
    None
}

fn resolve_config_write_path() -> PathBuf {
    if let Some((path, _)) = read_config_json_from_disk() {
        return path;
    }

    for path in config_path_candidates() {
        if is_src_tauri_config_path(&path) {
            continue;
        }
        if let Some(parent) = path.parent() {
            if parent.as_os_str().is_empty() {
                return path;
            }
            if fs::create_dir_all(parent).is_ok() {
                return path;
            }
        } else {
            return path;
        }
    }

    PathBuf::from("config.json")
}

fn write_config_json_to_disk(cfg: &serde_json::Value) -> Result<(), String> {
    let path = resolve_config_write_path();
    let body = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    fs::write(path, body).map_err(|e| e.to_string())
}

#[tauri::command]
fn save_site_metadata(location_name: String, anydesk_address: String) -> Result<bool, String> {
    let mut cfg = read_config_json_from_disk()
        .map(|(_, json)| json)
        .unwrap_or_else(default_config_json);
    ensure_config_defaults(&mut cfg);

    if let Some(obj) = cfg.as_object_mut() {
        obj.insert("location_name".to_string(), serde_json::json!(location_name.trim()));
        obj.insert("anydesk_address".to_string(), serde_json::json!(anydesk_address.trim()));
    }

    let cfg_str = cfg.to_string();
    let config_path = resolve_config_write_path();
    backup_config_on_change(&config_path, &cfg_str)?;
    write_config_json_to_disk(&cfg)?;
    Ok(true)
}

#[tauri::command]
fn save_hub_config(
    api_url: String,
    api_token: String,
    project_id: String,
    device_id: String,
) -> Result<bool, String> {
    let url = api_url.trim();
    if !url.is_empty() && !url.starts_with("https://") {
        return Err("Hub API muss eine HTTPS-URL verwenden".to_string());
    }
    if project_id.trim().len() > 120 || device_id.trim().len() > 120 {
        return Err("Projekt-ID oder Geräte-ID ist zu lang".to_string());
    }
    let mut cfg = read_config_json_from_disk()
        .map(|(_, json)| json)
        .unwrap_or_else(default_config_json);
    ensure_config_defaults(&mut cfg);
    if let Some(obj) = cfg.as_object_mut() {
        obj.insert("hub_api_url".to_string(), serde_json::json!(url));
        obj.insert("hub_api_token".to_string(), serde_json::json!(api_token.trim()));
        obj.insert("hub_project_id".to_string(), serde_json::json!(project_id.trim()));
        obj.insert("hub_device_id".to_string(), serde_json::json!(device_id.trim()));
    }
    let cfg_str = cfg.to_string();
    let config_path = resolve_config_write_path();
    backup_config_on_change(&config_path, &cfg_str)?;
    write_config_json_to_disk(&cfg)?;
    Ok(true)
}

#[tauri::command]
fn hub_post_json(
    url: String,
    api_token: String,
    payload: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let endpoint = url.trim();
    if !endpoint.starts_with("https://") {
        return Err("Hub API muss eine HTTPS-URL verwenden".to_string());
    }
    if api_token.trim().is_empty() {
        return Err("Hub API-Token fehlt".to_string());
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(12))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .post(endpoint)
        .bearer_auth(api_token.trim())
        .header("X-Projektil-Client", "projektil-control")
        .json(&payload)
        .send()
        .map_err(|e| format!("Hub API Netzwerkfehler: {}", e))?;
    let status = response.status();
    let text = response.text().map_err(|e| e.to_string())?;
    let body = serde_json::from_str::<serde_json::Value>(&text)
        .unwrap_or_else(|_| serde_json::json!({"raw": text}));
    if !status.is_success() {
        return Err(format!("Hub API Fehler {}: {}", status, body));
    }
    Ok(body)
}

#[tauri::command]
fn save_telegram_config(enabled: bool, bot_token: String, chat_id: String, channel_id: Option<String>, alert_events: Option<Vec<String>>) -> Result<bool, String> {
    let mut cfg = read_config_json_from_disk()
        .map(|(_, json)| json)
        .unwrap_or_else(default_config_json);
    ensure_config_defaults(&mut cfg);

    if let Some(obj) = cfg.as_object_mut() {
        let telegram = obj.entry("telegram").or_insert_with(|| serde_json::json!({}));
        if let Some(t) = telegram.as_object_mut() {
            t.insert("enabled".to_string(), serde_json::json!(enabled));
            t.insert("bot_token".to_string(), serde_json::json!(bot_token.trim()));
            t.insert("chat_id".to_string(), serde_json::json!(chat_id.trim()));
            if let Some(ch_id) = channel_id {
                t.insert("channel_id".to_string(), serde_json::json!(ch_id.trim()));
            }
            if let Some(events) = alert_events {
                let mut cleaned: Vec<String> = Vec::new();
                for event in events {
                    let v = event.trim().to_ascii_lowercase();
                    if v.is_empty() {
                        continue;
                    }
                    if !cleaned.iter().any(|e| e == &v) {
                        cleaned.push(v);
                    }
                }
                t.insert("alert_events".to_string(), serde_json::json!(cleaned));
            }
        }
    }

    let cfg_str = cfg.to_string();
    let config_path = resolve_config_write_path();
    backup_config_on_change(&config_path, &cfg_str)?;
    write_config_json_to_disk(&cfg)?;
    Ok(true)
}

#[tauri::command]
fn save_ui_state(
    demo_mode: Option<bool>,
    startup_mode: Option<String>,
    language: Option<String>,
    camera_view_mode: Option<String>,
    projector_count: Option<u8>,
    pixera_octo_count: Option<u8>,
    amp_count: Option<u8>,
    interactive_enabled: Option<bool>,
    interactive_scanner_count: Option<u8>,
    emergency_switch_enabled: Option<bool>,
    demo_amp1_mutes: Option<Vec<bool>>,
    demo_amp2_mutes: Option<Vec<bool>>,
    demo_amp3_mutes: Option<Vec<bool>>,
    demo_projector_on: Option<Vec<bool>>,
    demo_projector_mute: Option<Vec<bool>>,
    projector_control_states: Option<Vec<String>>,
    projector_status_updated_at: Option<u64>,
) -> Result<bool, String> {
    let mut cfg = read_config_json_from_disk()
        .map(|(_, json)| json)
        .unwrap_or_else(default_config_json);
    ensure_config_defaults(&mut cfg);

    if let Some(obj) = cfg.as_object_mut() {
        if let Some(enabled) = demo_mode {
            obj.insert("demo_mode".to_string(), serde_json::json!(enabled));
        }
        if let Some(mode_raw) = startup_mode {
            let normalized = mode_raw.trim().to_ascii_lowercase();
            let mode = if normalized == "viewer" { "viewer" } else { "admin" };
            obj.insert("startup_mode".to_string(), serde_json::json!(mode));
        }
        if let Some(lang_raw) = language {
            let normalized = lang_raw.trim().to_ascii_lowercase();
            let lang = if normalized == "en" { "en" } else { "de" };
            obj.insert("language".to_string(), serde_json::json!(lang));
        }
        if let Some(camera_mode_raw) = camera_view_mode {
            let normalized = camera_mode_raw.trim().to_ascii_lowercase();
            let mode = if normalized == "stream" { "stream" } else { "snapshot" };
            obj.insert("camera_view_mode".to_string(), serde_json::json!(mode));
        }
        if let Some(count_raw) = projector_count {
            let clamped = count_raw.clamp(1, 16);
            obj.insert("projector_count".to_string(), serde_json::json!(clamped));
        }
        if let Some(count_raw) = pixera_octo_count {
            let clamped = count_raw.clamp(0, 2);
            obj.insert("pixera_octo_count".to_string(), serde_json::json!(clamped));
        }
        if let Some(count_raw) = amp_count {
            let clamped = count_raw.clamp(1, 3);
            obj.insert("amp_count".to_string(), serde_json::json!(clamped));
        }
        if let Some(enabled) = interactive_enabled {
            obj.insert("interactive_enabled".to_string(), serde_json::json!(enabled));
        }
        if let Some(count_raw) = interactive_scanner_count {
            let clamped = count_raw.clamp(1, 2);
            obj.insert("interactive_scanner_count".to_string(), serde_json::json!(clamped));
        }
        if let Some(enabled) = emergency_switch_enabled {
            obj.insert("emergency_switch_enabled".to_string(), serde_json::json!(enabled));
        }
        if demo_amp1_mutes.is_some() || demo_amp2_mutes.is_some() || demo_amp3_mutes.is_some() {
            let entry = obj
                .entry("demo_amp_mutes".to_string())
                .or_insert_with(|| serde_json::json!({"1": [false, false, false, false], "2": [false, false, false, false], "3": [false, false, false, false]}));
            if let Some(amp) = entry.as_object_mut() {
                if let Some(v) = demo_amp1_mutes {
                    let mut out = vec![false; 4];
                    for (idx, value) in v.into_iter().take(4).enumerate() {
                        out[idx] = value;
                    }
                    amp.insert("1".to_string(), serde_json::json!(out));
                }
                if let Some(v) = demo_amp2_mutes {
                    let mut out = vec![false; 4];
                    for (idx, value) in v.into_iter().take(4).enumerate() {
                        out[idx] = value;
                    }
                    amp.insert("2".to_string(), serde_json::json!(out));
                }
                if let Some(v) = demo_amp3_mutes {
                    let mut out = vec![false; 4];
                    for (idx, value) in v.into_iter().take(4).enumerate() {
                        out[idx] = value;
                    }
                    amp.insert("3".to_string(), serde_json::json!(out));
                }
            }
        }
        if let Some(v) = demo_projector_on {
            let mut out = vec![false; 16];
            for (idx, value) in v.into_iter().take(16).enumerate() {
                out[idx] = value;
            }
            obj.insert("demo_projector_on".to_string(), serde_json::json!(out));
        }
        if let Some(v) = demo_projector_mute {
            let mut out = vec![false; 16];
            for (idx, value) in v.into_iter().take(16).enumerate() {
                out[idx] = value;
            }
            obj.insert("demo_projector_mute".to_string(), serde_json::json!(out));
        }
        if let Some(v) = projector_control_states {
            let mut out = vec!["unknown".to_string(); 16];
            for (idx, value) in v.into_iter().take(16).enumerate() {
                let normalized = value.trim().to_ascii_lowercase();
                out[idx] = match normalized.as_str() {
                    "online" | "startup" | "cooldown" | "standby" | "offline" | "error" => normalized,
                    _ => "unknown".to_string(),
                };
            }
            obj.insert("projector_control_states".to_string(), serde_json::json!(out));
        }
        if let Some(v) = projector_status_updated_at {
            obj.insert("projector_status_updated_at".to_string(), serde_json::json!(v));
        }
    }

    write_config_json_to_disk(&cfg)?;
    Ok(true)
}

#[tauri::command]
fn telegram_send_test(bot_token: String, chat_id: String) -> Result<String, String> {
    let token = bot_token.trim().to_string();
    let chat  = chat_id.trim().to_string();
    if token.is_empty() || chat.is_empty() {
        return Err("Bot-Token oder Chat-ID fehlt".to_string());
    }
    
    let mut cfg = read_config_json_from_disk()
        .map(|(_, json)| json)
        .unwrap_or_else(default_config_json);
    ensure_config_defaults(&mut cfg);

    let timestamp_ms = now_timestamp_ms();
    let location = cfg["location_name"].as_str().unwrap_or("").trim();
    let location = if location.is_empty() { "unbekannt" } else { location };
    let location_label = format_telegram_location_label(location);
    let anydesk = cfg["anydesk_address"].as_str().unwrap_or("").trim();
    let anydesk_line = if anydesk.is_empty() {
        "Anydeskadresse: nicht gesetzt".to_string()
    } else {
        format!("Anydeskadresse: <a href=\"anydesk://{}\">{}</a>", anydesk, anydesk)
    };

    let text = format!(
        "🧪{}🧪\n{}\nTestnachricht\n{}",
        location_label,
        format_human_datetime(timestamp_ms),
        anydesk_line
    );
    
    let url = format!("https://api.telegram.org/bot{}/sendMessage", token);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .map_err(|e| e.to_string())?;
    
    let form = [
        ("chat_id", chat),
        ("text", text),
        ("parse_mode", "HTML".to_string()),
        ("disable_web_page_preview", "true".to_string()),
    ];
    let resp = client.post(&url).form(&form).send().map_err(|e| e.to_string())?;
    if resp.status().is_success() {
        Ok("Testnachricht gesendet!".to_string())
    } else {
        Err(format!("Telegram API Fehler: {}", resp.status()))
    }
}

#[tauri::command]
fn append_app_log(app: AppHandle, level: String, message: String, timestamp_ms: Option<u64>) -> Result<bool, String> {
    let ts = timestamp_ms.unwrap_or_else(now_timestamp_ms);
    write_app_log(&level, &message, ts, Some(&app))?;
    maybe_send_critical_telegram(&level, &message, ts);
    Ok(true)
}

#[tauri::command]
fn load_app_logs(app: AppHandle, limit: Option<usize>) -> Result<serde_json::Value, String> {
    prune_old_logs(Some(&app))?;

    let sys_path = system_log_path(Some(&app))?;
    let err_path = error_log_path(Some(&app))?;
    let mut system_entries = read_log_entries(&sys_path);
    let mut error_entries = read_log_entries(&err_path);

    system_entries.sort_by(|a, b| b.timestamp_ms.cmp(&a.timestamp_ms));
    error_entries.sort_by(|a, b| b.timestamp_ms.cmp(&a.timestamp_ms));

    let max_items = limit.unwrap_or(2000);
    if system_entries.len() > max_items {
        system_entries.truncate(max_items);
    }
    if error_entries.len() > max_items {
        error_entries.truncate(max_items);
    }

    Ok(serde_json::json!({
        "system": system_entries,
        "errors": error_entries
    }))
}

fn pjlink_read_line(stream: &mut TcpStream) -> Result<String, String> {
    let mut out: Vec<u8> = Vec::with_capacity(256);
    let mut one = [0u8; 1];
    loop {
        match stream.read(&mut one) {
            Ok(0) => break,
            Ok(_) => {
                if one[0] == b'\r' || one[0] == b'\n' {
                    if !out.is_empty() {
                        break;
                    }
                } else {
                    out.push(one[0]);
                }
            }
            Err(e) => return Err(format!("PJLink read error: {}", e)),
        }
    }
    if out.is_empty() {
        return Err("PJLink empty response".to_string());
    }
    Ok(String::from_utf8_lossy(&out).to_string())
}

fn pjlink_connect(ip: &str) -> Result<TcpStream, String> {
    let addr = format!("{}:4352", ip);
    let stream = TcpStream::connect_timeout(
        &addr.parse::<std::net::SocketAddr>().map_err(|e| e.to_string())?,
        Duration::from_millis(700),
    )
    .map_err(|e| format!("PJLink connect {}: {}", addr, e))?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(700)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(700)));
    Ok(stream)
}

fn pjlink_auth_prefix(stream: &mut TcpStream, password: &str) -> Result<String, String> {
    let hello = pjlink_read_line(stream)?;
    if hello.starts_with("PJLINK 0") {
        return Ok(String::new());
    }
    if hello.starts_with("PJLINK 1 ") {
        let nonce = hello.trim_start_matches("PJLINK 1 ").trim();
        let input = format!("{}{}", nonce, password);
        let digest = format!("{:x}", md5::compute(input));
        return Ok(digest);
    }
    Err(format!("Unexpected PJLink hello: {}", hello))
}

fn pjlink_send_cmd(stream: &mut TcpStream, prefix: &str, cmd: &str) -> Result<String, String> {
    let payload = format!("{}{}\r", prefix, cmd);
    stream
        .write_all(payload.as_bytes())
        .map_err(|e| format!("PJLink write error: {}", e))?;
    pjlink_read_line(stream)
}

fn pjlink_parse_value(resp: &str, key: &str) -> Option<String> {
    let expected = format!("%1{}=", key);
    if let Some(v) = resp.strip_prefix(&expected) {
        return Some(v.to_string());
    }
    None
}

fn pjlink_parse_model(resp: &str) -> Option<String> {
    // INF format: %1INF=manufacturer,model,...
    if let Some(v) = pjlink_parse_value(resp, "INF") {
        let parts: Vec<&str> = v.split(',').collect();
        if parts.len() >= 2 {
            let mfr = parts[0].trim();
            let mdl = parts[1].trim();
            if !mdl.is_empty() {
                return Some(format!("{} {}", mfr, mdl));
            }
        }
    }
    None
}

fn pjlink_poll_one(ip: &str, password: &str) -> serde_json::Value {
    if ip.trim().is_empty() || ip.trim() == "0.0.0.0" {
        return serde_json::json!({
            "ip": ip,
            "hasIp": false,
            "isConnected": false,
            "powerIsOn": false,
            "errorState": "",
            "shutterMuted": false,
            "lampHours": serde_json::Value::Null
        });
    }

    let mut stream = match pjlink_connect(ip) {
        Ok(s) => s,
        Err(e) => {
            log_error_with_category(ErrorCategory::DeviceOffline, &e, Some(&format!("projector:{}", ip)), None);
            return serde_json::json!({
                "ip": ip,
                "hasIp": true,
                "isConnected": false,
                "powerIsOn": false,
                "errorState": e,
                "shutterMuted": false,
                "lampHours": serde_json::Value::Null
            });
        }
    };

    let prefix = match pjlink_auth_prefix(&mut stream, password) {
        Ok(p) => p,
        Err(e) => {
            return serde_json::json!({
                "ip": ip,
                "hasIp": true,
                "isConnected": false,
                "powerIsOn": false,
                "errorState": e,
                "shutterMuted": false,
                "lampHours": serde_json::Value::Null
            });
        }
    };

    let powr = pjlink_send_cmd(&mut stream, &prefix, "%1POWR ?");
    let erst = pjlink_send_cmd(&mut stream, &prefix, "%1ERST ?");
    let avmt = pjlink_send_cmd(&mut stream, &prefix, "%1AVMT ?");
    let lamp = pjlink_send_cmd(&mut stream, &prefix, "%1LAMP ?");

    let mut error_state = String::new();

    let power_is_on = match powr {
        Ok(ref r) => {
            let v = pjlink_parse_value(r, "POWR").unwrap_or_default();
            match v.as_str() {
                "0" => serde_json::Value::Bool(false),
                "1" => serde_json::Value::Bool(true),
                "2" => serde_json::Value::String("Cooling".to_string()),
                "3" => serde_json::Value::String("WarmUp".to_string()),
                // ERR3 = "Unavailable time" (PJLink spec): projector is transitioning
                // (either warming up or cooling down). Return a neutral "Transitioning" state
                // so the frontend can decide based on context (startup vs cooldown target).
                "ERR3" => serde_json::Value::String("Transitioning".to_string()),
                _ => {
                    if !v.is_empty() {
                        error_state = format!("POWR {}", v);
                    }
                    serde_json::Value::Bool(false)
                }
            }
        }
        Err(e) => {
            error_state = e;
            serde_json::Value::Bool(false)
        }
    };

    if let Ok(ref r) = erst {
        if let Some(v) = pjlink_parse_value(r, "ERST") {
            // ERR3 = "Unavailable time": projector is transitioning, not a real error.
            // Only report actual hardware error codes (non-zero ERST, excluding ERR codes).
            if !v.trim().is_empty() && v != "000000" && !v.starts_with("ERR") {
                error_state = format!("ERST {}", v);
            }
        }
    }

    let shutter_muted = match avmt {
        Ok(ref r) => {
            let v = pjlink_parse_value(r, "AVMT").unwrap_or_default();
            matches!(v.as_str(), "11" | "21" | "31")
        }
        Err(_) => false,
    };

    let lamp_hours = match lamp {
        Ok(ref r) => {
            let v = pjlink_parse_value(r, "LAMP").unwrap_or_default();
            let first = v.split_whitespace().next().unwrap_or("");
            match first.parse::<u64>() {
                Ok(h) => serde_json::json!(h),
                Err(_) => serde_json::Value::Null,
            }
        }
        Err(_) => serde_json::Value::Null,
    };

    mark_device_online(&format!("projector:{}", ip));
    serde_json::json!({
        "ip": ip,
        "hasIp": true,
        "isConnected": true,
        "powerIsOn": power_is_on,
        "errorState": error_state,
        "shutterMuted": shutter_muted,
        "lampHours": lamp_hours
    })
}

fn pjlink_detect_model_one(ip: &str, password: &str) -> serde_json::Value {
    if ip.trim().is_empty() || ip.trim() == "0.0.0.0" {
        return serde_json::json!({
            "ip": ip,
            "hasIp": false,
            "isConnected": false,
            "model": "Unknown",
            "errorState": ""
        });
    }

    let mut stream = match pjlink_connect(ip) {
        Ok(s) => s,
        Err(e) => {
            return serde_json::json!({
                "ip": ip,
                "hasIp": true,
                "isConnected": false,
                "model": "Unknown",
                "errorState": e
            });
        }
    };

    let prefix = match pjlink_auth_prefix(&mut stream, password) {
        Ok(p) => p,
        Err(e) => {
            return serde_json::json!({
                "ip": ip,
                "hasIp": true,
                "isConnected": false,
                "model": "Unknown",
                "errorState": e
            });
        }
    };

    let inf = pjlink_send_cmd(&mut stream, &prefix, "%1INF ?");
    let inf1 = pjlink_send_cmd(&mut stream, &prefix, "%1INF1 ?");
    let inf2 = pjlink_send_cmd(&mut stream, &prefix, "%1INF2 ?");

    let manufacturer = inf1
        .as_ref()
        .ok()
        .and_then(|r| pjlink_parse_value(r, "INF1"))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty() && !v.starts_with("ERR"));

    let model_name = inf2
        .as_ref()
        .ok()
        .and_then(|r| pjlink_parse_value(r, "INF2"))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty() && !v.starts_with("ERR"));

    let model = if let Some(name) = model_name {
        if let Some(mfr) = manufacturer {
            format!("{} {}", mfr, name)
        } else {
            name
        }
    } else {
        inf.as_ref()
            .ok()
            .and_then(|r| pjlink_parse_model(r))
            .unwrap_or_else(|| "Unknown".to_string())
    };

    serde_json::json!({
        "ip": ip,
        "hasIp": true,
        "isConnected": true,
        "model": model,
        "errorState": ""
    })
}

#[tauri::command]
fn pjlink_poll_many(ips: Vec<String>, password: Option<String>) -> Result<serde_json::Value, String> {
    let pwd = password.unwrap_or_default();
    let total = ips.len();
    let mut items: Vec<serde_json::Value> = vec![serde_json::Value::Null; total];

    let mut handles = Vec::with_capacity(total);
    for (idx, ip) in ips.into_iter().enumerate() {
        let pwd_clone = pwd.clone();
        handles.push(thread::spawn(move || {
            let row = pjlink_poll_one(&ip, &pwd_clone);
            (idx, row)
        }));
    }

    for h in handles {
        if let Ok((idx, row)) = h.join() {
            if idx < items.len() {
                items[idx] = row;
            }
        }
    }

    Ok(serde_json::json!(items))
}

#[tauri::command]
fn pjlink_detect_models(ips: Vec<String>, password: Option<String>) -> Result<serde_json::Value, String> {
    let pwd = password.unwrap_or_default();
    let total = ips.len();
    let mut items: Vec<serde_json::Value> = vec![serde_json::Value::Null; total];

    let mut handles = Vec::with_capacity(total);
    for (idx, ip) in ips.into_iter().enumerate() {
        let pwd_clone = pwd.clone();
        handles.push(thread::spawn(move || {
            let row = pjlink_detect_model_one(&ip, &pwd_clone);
            (idx, row)
        }));
    }

    for h in handles {
        if let Ok((idx, row)) = h.join() {
            if idx < items.len() {
                items[idx] = row;
            }
        }
    }

    Ok(serde_json::json!(items))
}

#[tauri::command]
fn pjlink_set_power(ip: String, on: bool, password: Option<String>) -> Result<bool, String> {
    let mut stream = pjlink_connect(&ip)?;
    let prefix = pjlink_auth_prefix(&mut stream, &password.unwrap_or_default())?;
    let cmd = if on { "%1POWR 1" } else { "%1POWR 0" };
    let resp = pjlink_send_cmd(&mut stream, &prefix, cmd)?;
    if resp.contains("=ERR") {
        return Err(format!("PJLink SetPower error: {}", resp));
    }
    Ok(true)
}

#[tauri::command]
fn pjlink_set_shutter(ip: String, muted: bool, password: Option<String>) -> Result<bool, String> {
    let mut stream = pjlink_connect(&ip)?;
    let prefix = pjlink_auth_prefix(&mut stream, &password.unwrap_or_default())?;
    let cmd = if muted { "%1AVMT 31" } else { "%1AVMT 30" };
    let resp = pjlink_send_cmd(&mut stream, &prefix, cmd)?;
    if resp.contains("=ERR") {
        return Err(format!("PJLink SetShutter error: {}", resp));
    }
    Ok(true)
}

#[tauri::command]
async fn pixera_api_request(
    address: String,
    params: Option<Vec<serde_json::Value>>,
    host: Option<String>,
    port: Option<u16>,
    timeout_ms: Option<u64>,
) -> Result<serde_json::Value, String> {
    let cfg = get_config();
    let target_host = host
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| cfg["pixera_ip"].as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "127.0.0.1".to_string());

    let target_port = port
        .or_else(|| cfg["pixera_port"].as_u64().map(|n| n as u16))
        .unwrap_or(1338);

    let timeout_value = timeout_ms.unwrap_or(3500).max(500);
    let sequence = (now_timestamp_ms() % 1_000_000_000) as u64;
    let ws_url = format!("ws://{}:{}/ws_avio", target_host, target_port);

    let mut request = ws_url
        .into_client_request()
        .map_err(|e| format!("Pixera Request-Aufbau fehlgeschlagen: {}", e))?;
    request
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", HeaderValue::from_static("ws_avio"));

    let target_host_for_tracking = target_host.clone();
    let connect = timeout(Duration::from_millis(timeout_value), connect_async(request))
        .await
        .map_err(|_| {
            pixera_track_failure(&target_host_for_tracking, &format!("Pixera API timeout: {}", address));
            format!("Pixera API timeout: {}", address)
        })?
        .map_err(|e| {
            let msg = format!("Pixera API websocket error: {}:{} ({})", target_host_for_tracking, target_port, e);
            pixera_track_failure(&target_host_for_tracking, &msg);
            msg
        })?;

    let (mut ws_stream, _) = connect;
    let payload = serde_json::json!({
        "type": "Request",
        "sequence": sequence,
        "address": address,
        "params": params.unwrap_or_default()
    });

    ws_stream
        .send(Message::Text(payload.to_string()))
        .await
        .map_err(|e| format!("Pixera API send failed: {}", e))?;

    loop {
        let message = timeout(Duration::from_millis(timeout_value), ws_stream.next())
            .await
            .map_err(|_| format!("Pixera API timeout: {}", address))?;

        let Some(frame) = message else {
            return Err("Pixera API Verbindung geschlossen".to_string());
        };

        let frame = frame.map_err(|e| format!("Pixera API receive failed: {}", e))?;

        match frame {
            Message::Text(text) => {
                let json: serde_json::Value = serde_json::from_str(&text)
                    .map_err(|e| format!("Pixera API ungültige JSON-Antwort: {}", e))?;
                if json["sequence"].as_u64() == Some(sequence) {
                    pixera_track_success(&target_host);
                    return Ok(json.get("result").cloned().unwrap_or(serde_json::Value::Null));
                }
            }
            Message::Binary(bin) => {
                let text = String::from_utf8(bin.to_vec())
                    .map_err(|e| format!("Pixera API Binary UTF-8 Fehler: {}", e))?;
                let json: serde_json::Value = serde_json::from_str(&text)
                    .map_err(|e| format!("Pixera API ungültige JSON-Antwort: {}", e))?;
                if json["sequence"].as_u64() == Some(sequence) {
                    pixera_track_success(&target_host);
                    return Ok(json.get("result").cloned().unwrap_or(serde_json::Value::Null));
                }
            }
            Message::Close(_) => return Err("Pixera API Verbindung geschlossen".to_string()),
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
        }
    }
}

// Thin wrapper to add health tracking for Pixera
fn pixera_track_success(host: &str) {
    mark_device_online(&format!("pixera:{}", host));
}

fn pixera_track_failure(host: &str, error: &str) {
    log_error_with_category(ErrorCategory::ConnectionTimeout, error, Some(&format!("pixera:{}", host)), None);
}

fn scheduler_resolved_module() -> &'static Mutex<String> {
    SCHEDULER_MODULE_RESOLVED.get_or_init(|| Mutex::new(String::new()))
}

fn pixera_config_string(cfg: &serde_json::Value, key: &str) -> String {
    cfg[key].as_str().unwrap_or("").trim().to_string()
}

fn push_unique_candidate(candidates: &mut Vec<String>, value: impl AsRef<str>) {
    let value = value.as_ref().trim();
    if value.is_empty() {
        return;
    }
    if !candidates.iter().any(|existing| existing.eq_ignore_ascii_case(value)) {
        candidates.push(value.to_string());
    }
}

fn scheduler_module_candidates(cfg: &serde_json::Value) -> Vec<String> {
    let mut candidates = Vec::new();
    push_unique_candidate(&mut candidates, DEFAULT_PIXERA_SCHEDULER_MODULE);
    if let Ok(resolved) = scheduler_resolved_module().lock() {
        push_unique_candidate(&mut candidates, resolved.as_str());
    }
    push_unique_candidate(&mut candidates, pixera_config_string(cfg, "pixera_scheduler_module"));
    for fallback in FALLBACK_PIXERA_SCHEDULER_MODULES {
        push_unique_candidate(&mut candidates, fallback);
    }
    candidates
}

fn pixera_paths(cfg: &serde_json::Value, base_path: &str) -> Vec<String> {
    let root = pixera_config_string(cfg, "pixera_api_root");
    if root.is_empty() {
        vec![base_path.to_string()]
    } else {
        vec![format!("{}.{}", root, base_path), base_path.to_string()]
    }
}

fn normalize_pixera_result(result: serde_json::Value) -> Result<serde_json::Value, String> {
    if let Some(arr) = result.as_array() {
        if arr.first().and_then(|v| v.get("_pixc")).and_then(|v| v.get("type")).and_then(|v| v.as_i64()) == Some(125) {
            let msg = arr.get(2).and_then(|v| v.as_str()).unwrap_or("Pixera exception");
            return Err(msg.to_string());
        }
        if arr.len() == 1 {
            if let Some(msg) = arr.first().and_then(|v| v.as_str()) {
                if msg.to_ascii_lowercase().contains("not authorized") {
                    return Err(msg.to_string());
                }
            }
            return Ok(arr[0].clone());
        }
    }
    if let Some(msg) = result.as_str() {
        if msg.to_ascii_lowercase().contains("not authorized") {
            return Err(msg.to_string());
        }
    }
    Ok(result)
}

fn pixera_result_is_empty(value: &serde_json::Value) -> bool {
    value.is_null() || value.as_str().map(|s| s.trim().is_empty()).unwrap_or(false)
}

fn is_pixera_connectivity_error(error: &str) -> bool {
    let msg = error.to_ascii_lowercase();
    msg.contains("timeout")
        || msg.contains("timed out")
        || msg.contains("websocket error")
        || msg.contains("verbindung geschlossen")
        || msg.contains("connection closed")
        || msg.contains("send failed")
        || msg.contains("receive failed")
        || msg.contains("refused")
        || msg.contains("10061")
        || msg.contains("host unreachable")
        || msg.contains("network is unreachable")
}

async fn scheduler_call(method_name: &str, params: Vec<serde_json::Value>, allow_empty: bool) -> Result<serde_json::Value, String> {
    let cfg = get_config();
    let host = pixera_config_string(&cfg, "pixera_ip");
    let port = cfg["pixera_port"].as_u64().map(|n| n as u16).unwrap_or(1338);
    let mut first_error: Option<String> = None;

    for module_name in scheduler_module_candidates(&cfg) {
        let base_path = format!("{}.{}", module_name, method_name);
        for address in pixera_paths(&cfg, &base_path) {
            match pixera_api_request(address, Some(params.clone()), Some(host.clone()), Some(port), Some(3500)).await {
                Ok(result) => match normalize_pixera_result(result) {
                    Ok(normalized) => {
                        if !allow_empty && pixera_result_is_empty(&normalized) {
                            continue;
                        }
                        if let Ok(mut resolved) = scheduler_resolved_module().lock() {
                            if *resolved != module_name {
                                *resolved = module_name.clone();
                            }
                        }
                        return Ok(normalized);
                    }
                    Err(err) => {
                        if first_error.is_none() {
                            first_error = Some(err);
                        }
                    }
                },
                Err(err) => {
                    if is_pixera_connectivity_error(&err) {
                        return Err(err);
                    }
                    if first_error.is_none() {
                        first_error = Some(err);
                    }
                }
            }
        }
    }

    Err(first_error.unwrap_or_else(|| format!("Scheduler call failed for {}", method_name)))
}

async fn scheduler_warmup_v21() {
    let _ = scheduler_call("ref", Vec::new(), true).await;
    let _ = scheduler_call("init", Vec::new(), true).await;
    let _ = scheduler_call("Time", Vec::new(), true).await;
}

async fn scheduler_read_with_warmup(method_name: &str) -> Result<serde_json::Value, String> {
    match scheduler_call(method_name, Vec::new(), false).await {
        Ok(value) if !pixera_result_is_empty(&value) => Ok(value),
        Err(err) if is_pixera_connectivity_error(&err) => Err(err),
        Ok(_) | Err(_) => {
            scheduler_warmup_v21().await;
            scheduler_call(method_name, Vec::new(), false).await
        }
    }
}

fn parse_countdown_ms(value: &serde_json::Value) -> Option<u64> {
    let s = value.as_str().map(str::trim).unwrap_or("");
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h = parts[0].parse::<u64>().ok()?;
    let m = parts[1].parse::<u64>().ok()?;
    let sec = parts[2].parse::<u64>().ok()?;
    if m > 59 || sec > 59 {
        return None;
    }
    Some(((h * 3600) + (m * 60) + sec) * 1000)
}

fn parse_pixera_schedule_datetime_ms(value: &serde_json::Value) -> Option<u64> {
    let s = value.as_str()?.trim();
    let mut parts = s.split_whitespace();
    let date = parts.next()?;
    let time = parts.next()?;
    let date_parts: Vec<u32> = date.split('.').filter_map(|p| p.parse::<u32>().ok()).collect();
    let time_parts: Vec<u32> = time.split(':').filter_map(|p| p.parse::<u32>().ok()).collect();
    if date_parts.len() != 3 || time_parts.len() < 2 {
        return None;
    }
    let second = *time_parts.get(2).unwrap_or(&0);
    let dt = Local
        .with_ymd_and_hms(
            date_parts[0] as i32,
            date_parts[1],
            date_parts[2],
            time_parts[0],
            time_parts[1],
            second,
        )
        .single()?;
    Some(dt.timestamp_millis().max(0) as u64)
}

fn parse_upcoming_events(raw: serde_json::Value) -> Vec<serde_json::Value> {
    let mut value = raw;
    for _ in 0..3 {
        if let Some(arr) = value.as_array() {
            return arr.clone();
        }
        if value.is_null() {
            return Vec::new();
        }
        if let Some(s) = value.as_str() {
            let s = s.trim();
            if s.is_empty() {
                return Vec::new();
            }
            value = match serde_json::from_str::<serde_json::Value>(s) {
                Ok(v) => v,
                Err(_) => return Vec::new(),
            };
            continue;
        }
        if let Some(arr) = value.get("value").and_then(|v| v.as_array()) {
            return arr.clone();
        }
        if let Some(arr) = value.get("events").and_then(|v| v.as_array()) {
            return arr.clone();
        }
        return Vec::new();
    }
    value.as_array().cloned().unwrap_or_default()
}

fn upcoming_event_countdown_ms(event: &serde_json::Value, server_time_ms: u64) -> Option<u64> {
    if let Some(in_sec) = event.get("inSec").and_then(|v| v.as_f64()) {
        if in_sec >= 0.0 {
            return Some((in_sec * 1000.0).max(0.0).round() as u64);
        }
    }
    let target_ms = parse_pixera_schedule_datetime_ms(event.get("time")?)?;
    Some(target_ms.saturating_sub(server_time_ms))
}

fn first_future_upcoming_event(events: &[serde_json::Value], server_time_ms: u64) -> Option<serde_json::Value> {
    events
        .iter()
        .filter_map(|event| upcoming_event_countdown_ms(event, server_time_ms).map(|ms| (ms, event.clone())))
        .filter(|(ms, _)| *ms > 0)
        .min_by_key(|(ms, _)| *ms)
        .map(|(_, event)| event)
}

fn no_schedule_response(server_time_ms: u64) -> serde_json::Value {
    serde_json::json!({
        "hasSchedule": false,
        "serverTimeMs": server_time_ms
    })
}

#[tauri::command]
async fn get_upcoming_cues() -> Result<serde_json::Value, String> {
    let server_time_ms = now_timestamp_ms();
    let raw = match scheduler_read_with_warmup("UpcomingEventsJson").await {
        Ok(value) => value,
        Err(err) if is_pixera_connectivity_error(&err) => return Err(err),
        Err(_) => return Ok(serde_json::json!({
            "hasSchedule": false,
            "serverTimeMs": server_time_ms,
            "events": []
        })),
    };
    let events = parse_upcoming_events(raw);
    Ok(serde_json::json!({
        "hasSchedule": !events.is_empty(),
        "serverTimeMs": server_time_ms,
        "events": events
    }))
}

#[tauri::command]
async fn get_next_cue() -> Result<serde_json::Value, String> {
    let server_time_ms = now_timestamp_ms();

    match get_upcoming_cues().await {
        Ok(upcoming) => {
            let events = upcoming
                .get("events")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if let Some(event) = first_future_upcoming_event(&events, server_time_ms) {
                let countdown_ms = upcoming_event_countdown_ms(&event, server_time_ms).unwrap_or(0);
                let next_name = event.get("name").and_then(|v| v.as_str()).unwrap_or("").trim();
                return Ok(serde_json::json!({
                    "hasSchedule": !next_name.is_empty(),
                    "nextCueName": next_name,
                    "countdownMs": countdown_ms,
                    "serverTimeMs": server_time_ms,
                    "source": "UpcomingEventsJson",
                    "event": event
                }));
            }
        }
        Err(err) if is_pixera_connectivity_error(&err) => return Err(err),
        Err(_) => {}
    }

    let next_name = match scheduler_read_with_warmup("NextEventName").await {
        Ok(value) => value,
        Err(err) if is_pixera_connectivity_error(&err) => return Err(err),
        Err(_) => return Ok(no_schedule_response(server_time_ms)),
    };
    let next_countdown = match scheduler_read_with_warmup("NextEventCountdown").await {
        Ok(value) => value,
        Err(err) if is_pixera_connectivity_error(&err) => return Err(err),
        Err(_) => return Ok(no_schedule_response(server_time_ms)),
    };

    let next_name_str = next_name.as_str().unwrap_or("").trim();
    let countdown_ms = parse_countdown_ms(&next_countdown);
    if next_name_str.is_empty() || next_name_str == "-" || countdown_ms.is_none() {
        return Ok(no_schedule_response(server_time_ms));
    }

    Ok(serde_json::json!({
        "hasSchedule": true,
        "nextCueName": next_name_str,
        "countdownMs": countdown_ms.unwrap_or(0),
        "serverTimeMs": server_time_ms,
        "source": "NextEventName/NextEventCountdown"
    }))
}

#[tauri::command]
fn get_config() -> serde_json::Value {
    if let Some((_path, mut json)) = read_config_json_from_disk() {
        ensure_config_defaults(&mut json);
        return json;
    }

    let default_config = default_config_json();
    let _ = write_config_json_to_disk(&default_config);
    default_config
}

#[tauri::command]
fn get_server_time_ms() -> u64 {
    now_timestamp_ms()
}

#[tauri::command]
fn get_device_health_status() -> Result<Vec<DeviceHealthStatus>, String> {
    device_health()
        .lock()
        .map(|health| health.values().cloned().collect())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn get_offline_mode_enabled() -> bool {
    // Check if offline mode is enabled in config
    let cfg = get_config();
    cfg.get("offline_mode_enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

#[tauri::command]
fn set_offline_mode(enabled: bool) -> Result<(), String> {
    let mut cfg = get_config();
    cfg["offline_mode_enabled"] = serde_json::json!(enabled);
    write_config_json_to_disk(&cfg)?;
    Ok(())
}

#[tauri::command]
fn clear_query_cache() -> Result<(), String> {
    query_cache()
        .lock()
        .map(|mut cache| cache.clear())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn reset_all_device_failures() -> Result<(), String> {
    device_health()
        .lock()
        .map(|mut health| {
            for device in health.values_mut() {
                device.consecutive_failures = 0;
                device.last_error = None;
            }
        })
        .map_err(|e| e.to_string())
}

fn app_handle_required() -> Result<AppHandle, String> {
    APP_HANDLE
        .get()
        .cloned()
        .ok_or_else(|| "AppHandle ist nicht initialisiert".to_string())
}

fn arg_value<'a>(args: &'a serde_json::Value, keys: &[&str]) -> Option<&'a serde_json::Value> {
    keys.iter().find_map(|k| args.get(*k))
}

fn arg_string(args: &serde_json::Value, keys: &[&str]) -> Result<String, String> {
    arg_value(args, keys)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("Fehlender String-Parameter: {}", keys.join("/")))
}

fn arg_optional_string(args: &serde_json::Value, keys: &[&str]) -> Option<String> {
    arg_value(args, keys)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn arg_bool(args: &serde_json::Value, keys: &[&str]) -> Result<bool, String> {
    arg_value(args, keys)
        .and_then(|v| v.as_bool())
        .ok_or_else(|| format!("Fehlender Bool-Parameter: {}", keys.join("/")))
}

fn arg_u8(args: &serde_json::Value, keys: &[&str]) -> Result<u8, String> {
    arg_value(args, keys)
        .and_then(|v| v.as_u64())
        .map(|n| n as u8)
        .ok_or_else(|| format!("Fehlender u8-Parameter: {}", keys.join("/")))
}

fn arg_optional_u8(args: &serde_json::Value, keys: &[&str]) -> Option<u8> {
    arg_value(args, keys)
        .and_then(|v| v.as_u64())
        .map(|n| n as u8)
}

fn arg_u16(args: &serde_json::Value, keys: &[&str]) -> Result<u16, String> {
    arg_value(args, keys)
        .and_then(|v| v.as_u64())
        .map(|n| n as u16)
        .ok_or_else(|| format!("Fehlender u16-Parameter: {}", keys.join("/")))
}

fn arg_optional_u16(args: &serde_json::Value, keys: &[&str]) -> Option<u16> {
    arg_value(args, keys)
        .and_then(|v| v.as_u64())
        .map(|n| n as u16)
}

fn arg_optional_u64(args: &serde_json::Value, keys: &[&str]) -> Option<u64> {
    arg_value(args, keys)
        .and_then(|v| v.as_u64())
}

fn arg_f32(args: &serde_json::Value, keys: &[&str]) -> Result<f32, String> {
    arg_value(args, keys)
        .and_then(|v| v.as_f64())
        .map(|n| n as f32)
        .ok_or_else(|| format!("Fehlender f32-Parameter: {}", keys.join("/")))
}

fn arg_optional_usize(args: &serde_json::Value, keys: &[&str]) -> Option<usize> {
    arg_value(args, keys)
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
}

fn arg_vec_string(args: &serde_json::Value, keys: &[&str]) -> Result<Vec<String>, String> {
    arg_value(args, keys)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        })
        .ok_or_else(|| format!("Fehlender Array-Parameter: {}", keys.join("/")))
}

fn arg_optional_vec_string(args: &serde_json::Value, keys: &[&str]) -> Option<Vec<String>> {
    arg_value(args, keys)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        })
}

fn arg_optional_vec_bool(args: &serde_json::Value, keys: &[&str]) -> Option<Vec<bool>> {
    arg_value(args, keys)
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().map(|v| v.as_bool().unwrap_or(false)).collect::<Vec<_>>())
}

fn block_on_command<F, T>(future: F) -> Result<T, String>
where
    F: std::future::Future<Output = Result<T, String>>,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("Tokio Runtime Fehler: {}", e))?;
    rt.block_on(future)
}

fn remote_invoke_dispatch(cmd: &str, args: &serde_json::Value) -> Result<serde_json::Value, String> {
    match cmd {
        "get_config" => Ok(get_config()),
        "get_server_time_ms" => Ok(serde_json::json!(get_server_time_ms())),

        "save_site_metadata" => Ok(serde_json::json!(save_site_metadata(
            arg_string(args, &["locationName", "location_name"])? ,
            arg_string(args, &["anydeskAddress", "anydesk_address"])?
        )?)),

        "save_hub_config" => Ok(serde_json::json!(save_hub_config(
            arg_string(args, &["apiUrl", "api_url"])? ,
            arg_string(args, &["apiToken", "api_token"])? ,
            arg_string(args, &["projectId", "project_id"])? ,
            arg_string(args, &["deviceId", "device_id"])?
        )?)),

        "hub_post_json" => Ok(hub_post_json(
            arg_string(args, &["url"])? ,
            arg_string(args, &["apiToken", "api_token"])? ,
            arg_value(args, &["payload"]).cloned().unwrap_or_else(|| serde_json::json!({}))
        )?),

        "get_device_health_status" => Ok(serde_json::json!(get_device_health_status()?)),
        "get_offline_mode_enabled" => Ok(serde_json::json!(get_offline_mode_enabled())),
        "set_offline_mode" => {
            set_offline_mode(arg_bool(args, &["enabled"])?)?;
            Ok(serde_json::json!(true))
        }
        "clear_query_cache" => {
            clear_query_cache()?;
            Ok(serde_json::json!(true))
        }
        "reset_all_device_failures" => {
            reset_all_device_failures()?;
            Ok(serde_json::json!(true))
        }

        "save_telegram_config" => Ok(serde_json::json!(save_telegram_config(
            arg_bool(args, &["enabled"])? ,
            arg_string(args, &["botToken", "bot_token"])? ,
            arg_string(args, &["chatId", "chat_id"])? ,
            arg_optional_string(args, &["channelId", "channel_id"]),
            arg_optional_vec_string(args, &["alertEvents", "alert_events"])
        )?)),

        "telegram_send_test" => Ok(serde_json::json!(telegram_send_test(
            arg_string(args, &["botToken", "bot_token"])? ,
            arg_string(args, &["chatId", "chat_id"])?
        )?)),

        "append_app_log" => {
            let app = app_handle_required()?;
            Ok(serde_json::json!(append_app_log(
                app,
                arg_string(args, &["level"])? ,
                arg_string(args, &["message"])? ,
                arg_optional_u64(args, &["timestampMs", "timestamp_ms"])
            )?))
        },

        "save_ui_state" => Ok(serde_json::json!(save_ui_state(
            arg_value(args, &["demoMode", "demo_mode"]).and_then(|v| v.as_bool()),
            arg_optional_string(args, &["startupMode", "startup_mode"]),
            arg_optional_string(args, &["language", "lang"]),
            arg_optional_string(args, &["cameraViewMode", "camera_view_mode"]),
            arg_value(args, &["projectorCount", "projector_count"]).and_then(|v| v.as_u64()).map(|n| n.clamp(1,16) as u8),
            arg_value(args, &["pixeraOctoCount", "pixera_octo_count"]).and_then(|v| v.as_u64()).map(|n| n.clamp(0,2) as u8),
            arg_value(args, &["ampCount", "amp_count"]).and_then(|v| v.as_u64()).map(|n| n.clamp(1,3) as u8),
            arg_value(args, &["interactiveEnabled", "interactive_enabled"]).and_then(|v| v.as_bool()),
            arg_value(args, &["interactiveScannerCount", "interactive_scanner_count"]).and_then(|v| v.as_u64()).map(|n| n.clamp(1,2) as u8),
            arg_value(args, &["emergencySwitchEnabled", "emergency_switch_enabled"]).and_then(|v| v.as_bool()),
            arg_optional_vec_bool(args, &["demoAmp1Mutes", "demo_amp1_mutes"]),
            arg_optional_vec_bool(args, &["demoAmp2Mutes", "demo_amp2_mutes"]),
            arg_optional_vec_bool(args, &["demoAmp3Mutes", "demo_amp3_mutes"]),
            arg_optional_vec_bool(args, &["demoProjectorOn", "demo_projector_on"]),
            arg_optional_vec_bool(args, &["demoProjectorMute", "demo_projector_mute"]),
            arg_optional_vec_string(args, &["projectorControlStates", "projector_control_states"]),
            arg_optional_u64(args, &["projectorStatusUpdatedAt", "projector_status_updated_at"])
        )?)),

        "load_app_logs" => {
            let app = app_handle_required()?;
            load_app_logs(app, arg_optional_usize(args, &["limit"]))
        }

        "open_external_url" => Ok(serde_json::json!(open_external_url(
            arg_string(args, &["url"])?
        )?)),

        "http_ping" => Ok(serde_json::json!(block_on_command(http_ping(
            arg_string(args, &["ip"])? ,
            arg_u16(args, &["port"])?
        ))?)),

        "camera_ptz_command" => Ok(serde_json::json!(block_on_command(camera_ptz_command(
            arg_string(args, &["ip"])? ,
            arg_string(args, &["command"])?
        ))?)),

        "camera_snapshot" => {
            let app = app_handle_required()?;
            Ok(serde_json::json!(block_on_command(camera_snapshot(
                app,
                arg_string(args, &["ip"])? ,
                arg_optional_u8(args, &["stream"])
            ))?))
        }

        "camera_stream_frame" => Ok(serde_json::json!(block_on_command(camera_stream_frame(
            arg_string(args, &["ip"])? ,
            arg_optional_u8(args, &["stream"])
        ))?)),

        "camera_prepare_stream" => {
            let app = app_handle_required()?;
            Ok(serde_json::json!(camera_prepare_stream(
                app,
                arg_string(args, &["ip"])? ,
                arg_optional_u8(args, &["stream"])
            )?))
        }

        "camera_restart_stream" => Ok(serde_json::json!(camera_restart_stream(
            arg_string(args, &["ip"])? ,
            arg_optional_u8(args, &["stream"])
        )?)),

        "d40_command" => Ok(serde_json::json!(block_on_command(d40_command(
            arg_string(args, &["ip"])? ,
            arg_string(args, &["command"])?
        ))?)),

        "d40_ping" => Ok(serde_json::json!(block_on_command(d40_ping(
            arg_string(args, &["ip"])?
        ))?)),

        "d40_status" => block_on_command(d40_status(arg_string(args, &["ip"])?)),

        "d40_set_gain" => Ok(serde_json::json!(block_on_command(d40_set_gain(
            arg_string(args, &["ip"])? ,
            arg_u8(args, &["channel"])? ,
            arg_f32(args, &["current"])? ,
            arg_f32(args, &["target"])?
        ))?)),

        "send_emergency_notaus_osc" => Ok(serde_json::json!(send_emergency_notaus_osc(
            arg_optional_string(args, &["targetIp", "target_ip"]),
            arg_optional_u16(args, &["targetPort", "target_port"])
        )?)),

        "system_get_battery_status" => system_get_battery_status(),

        "ups_get_status" => block_on_command(ups_get_status(arg_string(args, &["ip"])?)),
        "ups_get_power_mode" => block_on_command(ups_get_power_mode(arg_string(args, &["ip"])?)),
        "ups_get_diagnostics" => block_on_command(ups_get_diagnostics(arg_string(args, &["ip"])?)),
        "janitza_get_data" => block_on_command(janitza_get_data(arg_string(args, &["ip"])?)),

        "poe_switch_get_status" => block_on_command(poe_switch_get_status(
            arg_string(args, &["ip"])? ,
            arg_optional_string(args, &["community"]),
            arg_optional_u16(args, &["port"])
        )),

        "rutx50_get_status" => block_on_command(rutx50_get_status(
            arg_string(args, &["ip"])? ,
            arg_optional_string(args, &["community"]),
            arg_optional_u16(args, &["port"])
        )),

        "nas_get_status" => block_on_command(nas_get_status(
            arg_string(args, &["ip"])? ,
            arg_optional_string(args, &["community"]),
            arg_optional_u16(args, &["port"])
        )),

        "pjlink_poll_many" => pjlink_poll_many(
            arg_vec_string(args, &["ips"])? ,
            arg_optional_string(args, &["password"])
        ),

        "pjlink_detect_models" => pjlink_detect_models(
            arg_vec_string(args, &["ips"])? ,
            arg_optional_string(args, &["password"])
        ),

        "pjlink_set_power" => Ok(serde_json::json!(pjlink_set_power(
            arg_string(args, &["ip"])? ,
            arg_bool(args, &["on"])? ,
            arg_optional_string(args, &["password"])
        )?)),

        "pjlink_set_shutter" => Ok(serde_json::json!(pjlink_set_shutter(
            arg_string(args, &["ip"])? ,
            arg_bool(args, &["muted"])? ,
            arg_optional_string(args, &["password"])
        )?)),

        "pixera_api_request" => block_on_command(pixera_api_request(
            arg_string(args, &["address"])? ,
            arg_value(args, &["params"]).and_then(|v| v.as_array()).map(|arr| arr.to_vec()),
            arg_optional_string(args, &["host"]),
            arg_optional_u16(args, &["port"]),
            arg_optional_u64(args, &["timeoutMs", "timeout_ms"])
        )),

        "get_next_cue" => block_on_command(get_next_cue()),
        "get_upcoming_cues" => block_on_command(get_upcoming_cues()),

        "minimize_window" => {
            minimize_window(app_handle_required()?);
            Ok(serde_json::json!(true))
        }
        "toggle_fullscreen" => {
            toggle_fullscreen(app_handle_required()?);
            Ok(serde_json::json!(true))
        }
        "hide_to_tray" => {
            hide_to_tray(app_handle_required()?);
            Ok(serde_json::json!(true))
        }
        "quit_app" => {
            quit_app(app_handle_required()?);
            Ok(serde_json::json!(true))
        }

        _ => Err(format!("Unbekannter Invoke-Command: {}", cmd)),
    }
}

fn header(name: &str, value: &str) -> Option<Header> {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).ok()
}

fn respond_json(request: tiny_http::Request, status: u16, payload: &serde_json::Value) {
    let body = payload.to_string();
    let mut response = Response::from_string(body).with_status_code(StatusCode(status));
    if let Some(h) = header("Content-Type", "application/json; charset=utf-8") {
        response = response.with_header(h);
    }
    if let Some(h) = header("Access-Control-Allow-Origin", "*") {
        response = response.with_header(h);
    }
    let _ = request.respond(response);
}

fn handle_lan_web_request(mut request: tiny_http::Request) {
    let method = request.method().clone();
    let path = request.url().split('?').next().unwrap_or("/").to_string();

    if method == Method::Get && (path == "/" || path == "/index.html") {
        let mut response = Response::from_string(current_webgui_index_html());
        if let Some(h) = header("Content-Type", "text/html; charset=utf-8") {
            response = response.with_header(h);
        }
        let _ = request.respond(response);
        return;
    }

    if method == Method::Get && path.starts_with("/js/") {
        if let Some((body, content_type)) = current_webgui_asset(&path[1..]) {
            let mut response = Response::from_string(body);
            if let Some(h) = header("Content-Type", content_type) {
                response = response.with_header(h);
            }
            let _ = request.respond(response);
            return;
        }
    }

    if method == Method::Get && (path == "/favicon.ico" || path == "/icon.ico") {
        let mut response = Response::from_data(current_webgui_favicon());
        if let Some(h) = header("Content-Type", "image/x-icon") {
            response = response.with_header(h);
        }
        let _ = request.respond(response);
        return;
    }

    if method == Method::Options && path == "/api/invoke" {
        let mut response = Response::empty(StatusCode(204));
        if let Some(h) = header("Access-Control-Allow-Origin", "*") {
            response = response.with_header(h);
        }
        if let Some(h) = header("Access-Control-Allow-Methods", "POST, OPTIONS") {
            response = response.with_header(h);
        }
        if let Some(h) = header("Access-Control-Allow-Headers", "Content-Type") {
            response = response.with_header(h);
        }
        let _ = request.respond(response);
        return;
    }

    if method == Method::Post && path == "/api/invoke" {
        const MAX_INVOKE_BODY_BYTES: usize = 1024 * 1024;
        let body_len = request.body_length();
        let reader = request.as_reader();
        let body: String;

        if let Some(len) = body_len {
            let len_usize = len as usize;
            if len_usize > MAX_INVOKE_BODY_BYTES {
                respond_json(request, 413, &serde_json::json!({"ok": false, "error": "Request-Body zu groß"}));
                return;
            }
            let mut buf = vec![0_u8; len_usize];
            if reader.read_exact(&mut buf).is_err() {
                respond_json(request, 400, &serde_json::json!({"ok": false, "error": "Ungültiger Request-Body"}));
                return;
            }
            body = match String::from_utf8(buf) {
                Ok(v) => v,
                Err(_) => {
                    respond_json(request, 400, &serde_json::json!({"ok": false, "error": "Request-Body muss UTF-8 sein"}));
                    return;
                }
            };
        } else {
            // Important: do not read unknown-length bodies here, it may block this single-threaded
            // LAN HTTP loop indefinitely on some clients/transfer modes.
            respond_json(request, 411, &serde_json::json!({"ok": false, "error": "Content-Length erforderlich"}));
            return;
        }

        let payload: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(_) => {
                respond_json(request, 400, &serde_json::json!({"ok": false, "error": "Ungültiges JSON"}));
                return;
            }
        };

        let cmd = payload
            .get("cmd")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if cmd.is_empty() {
            respond_json(request, 400, &serde_json::json!({"ok": false, "error": "Fehlender cmd-Parameter"}));
            return;
        }

        let args = payload
            .get("args")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));

        match remote_invoke_dispatch(&cmd, &args) {
            Ok(data) => respond_json(request, 200, &serde_json::json!({"ok": true, "data": data})),
            Err(err) => respond_json(request, 500, &serde_json::json!({"ok": false, "error": err})),
        }
        return;
    }

    if method == Method::Get && path == "/api/health" {
        respond_json(request, 200, &serde_json::json!({
            "ok": true,
            "mode": "lan-webgui",
            "port": ACTIVE_LAN_WEB_PORT.get().copied().unwrap_or(LAN_WEB_PORT_FALLBACK),
            "camera_port": CAMERA_MJPEG_PORT
        }));
        return;
    }

    let response = Response::from_string("Not Found").with_status_code(StatusCode(404));
    let _ = request.respond(response);
}

fn start_lan_web_server() {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }

    thread::spawn(|| {
        // Try the short, portless URL first; fall back if 80 is already occupied
        // (very common on shared networks) or requires elevated rights on this OS.
        let server = match Server::http(format!("0.0.0.0:{}", LAN_WEB_PORT_PREFERRED)) {
            Ok(s) => {
                let _ = ACTIVE_LAN_WEB_PORT.set(LAN_WEB_PORT_PREFERRED);
                eprintln!("LAN web server listening on port {} (http://<ip>/)", LAN_WEB_PORT_PREFERRED);
                s
            }
            Err(primary_err) => {
                match Server::http(format!("0.0.0.0:{}", LAN_WEB_PORT_FALLBACK)) {
                    Ok(s) => {
                        let _ = ACTIVE_LAN_WEB_PORT.set(LAN_WEB_PORT_FALLBACK);
                        eprintln!(
                            "LAN web server: port {} unavailable ({}), using fallback port {} instead",
                            LAN_WEB_PORT_PREFERRED, primary_err, LAN_WEB_PORT_FALLBACK
                        );
                        s
                    }
                    Err(fallback_err) => {
                        eprintln!(
                            "LAN web server bind error on both port {} ({}) and fallback port {} ({})",
                            LAN_WEB_PORT_PREFERRED, primary_err, LAN_WEB_PORT_FALLBACK, fallback_err
                        );
                        return;
                    }
                }
            }
        };

        for request in server.incoming_requests() {
            thread::spawn(move || {
                handle_lan_web_request(request);
            });
        }
    });
}

fn start_emergency_listener() {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }

    thread::spawn(|| {
        let socket = match UdpSocket::bind(OSC_LISTENER_ADDR) {
            Ok(s) => {
                let msg = format!("[OSC] Listener started on {}", OSC_LISTENER_ADDR);
                eprintln!("{}", msg);
                if let Some(app) = APP_HANDLE.get() {
                    let _ = write_app_log("info", &msg, now_timestamp_ms(), Some(&app));
                }
                s
            }
            Err(e) => {
                let msg = format!("[OSC ERROR] Failed to bind {}: {}", OSC_LISTENER_ADDR, e);
                eprintln!("{}", msg);
                if let Some(app) = APP_HANDLE.get() {
                    let _ = write_app_log("error", &msg, now_timestamp_ms(), Some(&app));
                }
                return;
            }
        };

        let mut buffer = [0u8; OSC_BUFFER_SIZE];
        
        loop {
            match socket.recv_from(&mut buffer) {
                Ok((size, addr)) => {
                    if size == 0 {
                        continue;
                    }
                    
                    let data = &buffer[..size];
                    let log_msg = format!("[OSC] Received {} bytes from {}", size, addr);
                    eprintln!("{}", log_msg);
                    if let Some(app) = APP_HANDLE.get() {
                        let _ = write_app_log("debug", &log_msg, now_timestamp_ms(), Some(&app));
                    }
                    
                    // Check for /emergency_pressed command
                    if size >= OSC_EMERGENCY_CMD_LEN && &data[0..OSC_EMERGENCY_CMD_LEN] == OSC_EMERGENCY_CMD {
                        handle_emergency_osc_command();
                    } else if size > 0 && data[0] == b'/' as u8 {
                        // Log unknown OSC commands for debugging
                        let max_len = std::cmp::min(50, size);
                        if let Ok(cmd) = std::str::from_utf8(&data[..max_len]) {
                            let debug_msg = format!("[OSC] Unknown command: {}", cmd);
                            eprintln!("{}", debug_msg);
                            if let Some(app) = APP_HANDLE.get() {
                                let _ = write_app_log("debug", &debug_msg, now_timestamp_ms(), Some(&app));
                            }
                        }
                    }
                }
                Err(e) => {
                    let msg = format!("[OSC ERROR] recv_from failed: {}", e);
                    eprintln!("{}", msg);
                    if let Some(app) = APP_HANDLE.get() {
                        let _ = write_app_log("error", &msg, now_timestamp_ms(), Some(&app));
                    }
                    // Continue listening despite errors
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
    });
}

fn handle_emergency_osc_command() {
    let msg = "[OSC] /emergency_pressed detected - emitting event";
    eprintln!("{}", msg);
    
    if let Some(app) = APP_HANDLE.get() {
        let _ = write_app_log("info", msg, now_timestamp_ms(), Some(&app));
        
        match app.emit("emergency-pressed-remote", ()) {
            Ok(_) => {
                let success = "[OSC] Event emitted successfully";
                eprintln!("{}", success);
                let _ = write_app_log("info", success, now_timestamp_ms(), Some(&app));
            }
            Err(e) => {
                let err = format!("[OSC ERROR] Failed to emit event: {}", e);
                eprintln!("{}", err);
                let _ = write_app_log("error", &err, now_timestamp_ms(), Some(&app));
            }
        }
    } else {
        eprintln!("[OSC ERROR] APP_HANDLE not available");
    }
}

fn main() {
    // Set Windows console to UTF-8 so umlauts (ä/ö/ü) don't crash stderr
    #[cfg(target_os = "windows")]
    unsafe { windows_sys::Win32::System::Console::SetConsoleOutputCP(65001); }

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            let app_handle = app.handle().clone();
            let _ = APP_HANDLE.set(app_handle.clone());
            let _ = resolve_log_dir(Some(&app_handle));
            install_panic_logging_hook();
            let _ = write_app_log("info", "Application startup", now_timestamp_ms(), Some(&app_handle));
            match ensure_ffmpeg_available(Some(&app_handle)) {
                Ok(path) => {
                    let _ = write_app_log("info", &format!("FFmpeg ready: {}", path), now_timestamp_ms(), Some(&app_handle));
                }
                Err(err) => {
                    let _ = write_app_log("error", &format!("FFmpeg setup failed: {}", err), now_timestamp_ms(), Some(&app_handle));
                }
            }
            start_camera_mjpeg_server();
            start_lan_web_server();
            start_emergency_listener();
            let sep       = tauri::menu::PredefinedMenuItem::separator(app)?;
            let show      = MenuItem::with_id(app, "show",      "PROJEKTIL öffnen", true, None::<&str>)?;
            let mute_all  = MenuItem::with_id(app, "mute_all",  "Alle Mute",         true, None::<&str>)?;
            let power_all = MenuItem::with_id(app, "power_all", "PowerAll",          true, None::<&str>)?;
            let emergency = MenuItem::with_id(app, "emergency", "Emergency Stop",    true, None::<&str>)?;
            let quit      = MenuItem::with_id(app, "quit",      "Beenden",           true, None::<&str>)?;
            let menu = Menu::with_items(app, &[
                &show, &sep, &mute_all, &power_all, &sep, &emergency, &sep, &quit
            ])?;
            let mut tray_builder = TrayIconBuilder::new()
                .menu(&menu)
                .tooltip("PROJEKTIL Control")
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "quit"      => app.exit(0),
                    "show"      => { if let Some(w) = app.get_webview_window("main") { let _ = w.show(); let _ = w.set_focus(); } }
                    "mute_all"  => { let _ = app.emit("tray-mute-all", ()); }
                    "power_all" => { let _ = app.emit("tray-power-all", ()); }
                    "emergency" => {
                        if let Some(w) = app.get_webview_window("main") { let _ = w.show(); let _ = w.set_focus(); }
                        let _ = app.emit("tray-emergency", ());
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } = event {
                        if let Some(w) = tray.app_handle().get_webview_window("main") {
                            let _ = w.show(); let _ = w.set_focus();
                        }
                    }
                });

            if let Some(icon) = app.default_window_icon() {
                tray_builder = tray_builder.icon(icon.clone());
            } else {
                let _ = write_app_log("error", "Tray icon missing: default_window_icon() returned None", now_timestamp_ms(), Some(&app_handle));
            }

            if let Err(e) = tray_builder.build(app) {
                let _ = write_app_log("error", &format!("Tray initialization failed: {}", e), now_timestamp_ms(), Some(&app_handle));
            }
            if let Some(w) = app.get_webview_window("main") { let _ = w.center(); }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            d40_command, d40_ping, d40_status, d40_set_gain, http_ping, icmp_ping, camera_ptz_command, camera_snapshot, camera_stream_frame, camera_prepare_stream, camera_restart_stream,
            system_get_battery_status,
            ups_get_status, ups_get_power_mode, ups_get_diagnostics, janitza_get_data, poe_switch_get_status, rutx50_get_status, nas_get_status,
            ups_get_status_managed, janitza_get_data_managed, nas_get_status_managed, poe_switch_get_status_managed, rutx50_get_status_managed,
            pjlink_poll_many, pjlink_detect_models, pjlink_set_power, pjlink_set_shutter,
            pixera_api_request, get_next_cue, get_upcoming_cues,
            send_emergency_notaus_osc, send_emergency_osc_to_switch, send_emergency_reset_osc,
            minimize_window, toggle_fullscreen,
            hide_to_tray, quit_app, open_external_url, companion_press_emergency_button, append_app_log, load_app_logs, get_config,
            save_site_metadata, save_hub_config, hub_post_json, save_telegram_config, save_ui_state, telegram_send_test, get_server_time_ms,
            get_device_health_status, get_offline_mode_enabled, set_offline_mode, clear_query_cache, reset_all_device_failures
        ])
        .run(tauri::generate_context!())
        .expect("Fehler beim Starten");
}
