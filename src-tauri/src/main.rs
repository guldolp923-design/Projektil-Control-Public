#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
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
const CAMERA_MJPEG_PORT: u16 = 41777;
const CAMERA_STREAM_IDLE_TIMEOUT_SECS: u64 = 90;
const CAMERA_STREAM_STALE_TIMEOUT_SECS: u64 = 60;
const CAMERA_STREAM_FIRST_FRAME_TIMEOUT_SECS: u64 = 20;
const CAMERA_STREAM_WRITE_TIMEOUT_MS: u64 = 25000;
const FFMPEG_RUNTIME_DOWNLOAD_URL: &str = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip";
const LOG_RETENTION_DAYS: u64 = 90;
const LOG_RETENTION_MS: u64 = LOG_RETENTION_DAYS * 24 * 60 * 60 * 1000;
const LOG_PRUNE_INTERVAL_MS: u64 = 12 * 60 * 60 * 1000;

static APP_LOG_DIR: OnceLock<PathBuf> = OnceLock::new();
static LAST_LOG_PRUNE_MS: OnceLock<Mutex<u64>> = OnceLock::new();

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
    }
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
    let download = Command::new("powershell")
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
    let extract = Command::new("powershell")
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
        let listener = match TcpListener::bind(("127.0.0.1", CAMERA_MJPEG_PORT)) {
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

    let load_pct = output_load / 10;
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
        Ok(_) => Ok("OK".to_string()),
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
// APC UPS — SNMPv1 UDP Port 161, Community: "Projektil"
// ============================================================
#[tauri::command]
async fn ups_get_status(ip: String) -> Result<serde_json::Value, String> {
    let community = "Projektil";
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    socket.set_read_timeout(Some(Duration::from_millis(2000))).ok();
    socket.connect(format!("{}:161", ip)).map_err(|e| e.to_string())?;

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
        ("output_load",   vec![1,3,6,1,4,1,318,1,1,1,3,3,4,0]),
        ("output_status", vec![1,3,6,1,4,1,318,1,1,1,4,1,1,0]),
        ("output_online", vec![1,3,6,1,4,1,318,1,1,1,4,1,2,0]),
    ];

    let mut result = serde_json::Map::new();
    for (key, oid) in &queries {
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

    Ok(serde_json::Value::Object(result))
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

fn snmp_query_raw(socket: &UdpSocket, community: &str, oid: &[u32]) -> Option<Vec<u8>> {
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

    let mut communities = vec![community.clone()];
    for fallback in ["public", "private", "projektil"] {
        let fb = fallback.to_string();
        if !communities.contains(&fb) {
            communities.push(fb);
        }
    }

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
            return Ok(serde_json::Value::Object(result));
        }
    }

    Err("SNMP keine Antwort vom PoE-Switch".to_string())
}

#[tauri::command]
async fn rutx50_get_status(ip: String, community: Option<String>, port: Option<u16>) -> Result<serde_json::Value, String> {
    let community = community.unwrap_or_else(|| "public".to_string());
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
    oca::ping(&ip).await.map_err(|e| e.to_string())
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
        Command::new("cmd")
            .args(["/C", "start", "", &url])
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
fn append_app_log(app: AppHandle, level: String, message: String, timestamp_ms: Option<u64>) -> Result<bool, String> {
    write_app_log(&level, &message, timestamp_ms.unwrap_or_else(now_timestamp_ms), Some(&app))?;
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
fn get_config() -> serde_json::Value {
    let config_path = "config.json";

    // Versuche die Datei zu lesen
    if let Ok(content) = fs::read_to_string(config_path) {
        if let Ok(json) = serde_json::from_str(&content) {
            return json;
        }
    }

    // Fallback: Standardwerte, falls Datei nicht existiert oder fehlerhaft ist
    let default_config = serde_json::json!({
        "pixera_ip": "192.168.1.31", "pixera_port": 1338,
        "pixera_octo1_ip": "192.168.1.32", "pixera_octo2_ip": "192.168.1.33",
        "d40_01_ip": "192.168.1.51", "d40_02_ip": "192.168.1.52", "d40_oca_port": 50014,
        "nas_ip": "192.168.1.21", "nas_port": 5000,
        "nas_snmp_port": 161, "nas_snmp_community": "projektil",
        "poe_switch_ip": "192.168.1.11", "poe_switch_name": "", "poe_switch_ping_port": 443,
        "poe_switch_snmp_port": 161, "poe_switch_snmp_community": "projektil",
        "rutx50_ip": "192.168.1.1", "rutx50_ping_port": 443,
        "rutx50_snmp_port": 161, "rutx50_snmp_community": "public",
        "ups_ip": "192.168.1.6", "power_disp_ip": "192.168.1.5",
        "cam_01_ip": "192.168.1.22", "cam_02_ip": "192.168.1.23",
        "projector_start": 101, "projector_count": 16,
        "hotline": "+41 XX XXX XX XX",
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
    });

    // Datei mit Standardwerten neu anlegen, falls sie fehlte
    let _ = fs::write(config_path, serde_json::to_string_pretty(&default_config).unwrap_or_default());
    
    default_config
}

fn main() {
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
            let sep       = tauri::menu::PredefinedMenuItem::separator(app)?;
            let show      = MenuItem::with_id(app, "show",      "PROJEKTIL öffnen", true, None::<&str>)?;
            let mute_all  = MenuItem::with_id(app, "mute_all",  "Alle Mute",         true, None::<&str>)?;
            let power_all = MenuItem::with_id(app, "power_all", "PowerAll",          true, None::<&str>)?;
            let emergency = MenuItem::with_id(app, "emergency", "Emergency Stop",    true, None::<&str>)?;
            let quit      = MenuItem::with_id(app, "quit",      "Beenden",           true, None::<&str>)?;
            let menu = Menu::with_items(app, &[
                &show, &sep, &mute_all, &power_all, &sep, &emergency, &sep, &quit
            ])?;
            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
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
                })
                .build(app)?;
            if let Some(w) = app.get_webview_window("main") { let _ = w.center(); }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            d40_command, d40_ping, d40_status, d40_set_gain, http_ping, camera_ptz_command, camera_snapshot, camera_stream_frame, camera_prepare_stream, camera_restart_stream,
            ups_get_status, janitza_get_data, poe_switch_get_status, rutx50_get_status, nas_get_status,
            pjlink_poll_many, pjlink_detect_models, pjlink_set_power, pjlink_set_shutter,
            minimize_window, toggle_fullscreen,
            hide_to_tray, quit_app, open_external_url, append_app_log, load_app_logs, get_config
        ])
        .run(tauri::generate_context!())
        .expect("Fehler beim Starten");
}
