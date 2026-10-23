// Heartbeat: periodisch den Status dieses Control-Rechners und seiner
// aktivierten Geraete per HTTPS an einen externen Status-Server melden.
// Reines Monitoring: keine eingehenden Verbindungen, kein neuer Listener,
// kein offener Port. Laeuft als eigener Hintergrund-Thread, der niemals
// die Geraete-Steuerung blockiert (kein spawn_blocking noetig, da dieser
// Thread selbst nie auf dem Tauri/async-Pfad liegt).
//
// Sicherheitskritisch: der API-Key verlaesst dieses Modul nie Richtung
// Frontend. HeartbeatStatus enthaelt nur `key_set: bool`.
//
// Der Server identifiziert diesen Laptop allein am API-Key (keine System-ID
// mehr im Payload) und ermittelt den Standort selbst (keine lat/lon mehr).
// venue/place kommen aus den bereits in Control vorhandenen Show-Daten
// (Hub-aufgeloester Standort) und werden - wie die Geraeteliste - vom
// Frontend per heartbeat_update_snapshot aktuell gehalten, da sie nur dort
// (nicht in der persistierten config.json) in getrennter Form vorliegen.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

const MAX_ERROR_BUFFER: usize = 50;
const MAX_ERRORS_PER_BEAT: usize = 20;
const MAX_ERROR_MSG_LEN: usize = 300;
const MAX_DEVICES: usize = 128;
const MAX_BODY_BYTES: usize = 32 * 1024;
const HTTP_CONNECT_TIMEOUT_MS: u64 = 3000;
const HTTP_TOTAL_TIMEOUT_MS: u64 = 5000;
pub const DEFAULT_INTERVAL_S: u64 = 30;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn now_iso8601_utc() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct DeviceStatusEntry {
    pub name: String,
    pub status: String, // "ok" | "warn" | "error" | "offline"
}

#[derive(Debug, Clone, Serialize)]
struct ErrorEntry {
    ts: String,
    msg: String,
}

/// Konfigurationswerte, gelesen aus der normalen config.json (gleicher
/// Speicherort/Mechanismus wie der Hub-API-Key). `api_key` wird NIE an
/// das Frontend zurueckgegeben (siehe get_config() Redaktion in main.rs).
#[derive(Debug, Clone, Default)]
pub struct HeartbeatConfig {
    pub enabled: bool,
    pub url: String,
    pub api_key: String,
}

impl HeartbeatConfig {
    pub fn from_json(cfg: &serde_json::Value) -> Self {
        HeartbeatConfig {
            enabled: cfg.get("heartbeat_enabled").and_then(|v| v.as_bool()).unwrap_or(false),
            url: cfg.get("heartbeat_url").and_then(|v| v.as_str()).unwrap_or("").trim().to_string(),
            api_key: cfg.get("heartbeat_api_key").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        }
    }
}

struct State {
    devices: Vec<DeviceStatusEntry>,
    venue: String,
    place: String,
    demo_mode: bool,
    errors: VecDeque<ErrorEntry>,
    last_success_ms: Option<u64>,
    consecutive_failures: u32,
    last_result: String,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();
static APP_START: OnceLock<Instant> = OnceLock::new();
static TASK_STARTED: OnceLock<()> = OnceLock::new();

/// Ein vergifteter Mutex (ein anderer Thread ist waehrend des Lock-Haltens
/// in Panik geraten) darf den Heartbeat-Hintergrund-Thread nie dauerhaft
/// lahmlegen - wir lesen den zuletzt konsistenten Zustand trotzdem aus,
/// statt zu paniken. Gleiches Muster wie device_health() anderswo im Projekt.
fn lock_state() -> std::sync::MutexGuard<'static, State> {
    state().lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn state() -> &'static Mutex<State> {
    STATE.get_or_init(|| {
        Mutex::new(State {
            devices: Vec::new(),
            venue: String::new(),
            place: String::new(),
            demo_mode: false,
            errors: VecDeque::new(),
            last_success_ms: None,
            consecutive_failures: 0,
            last_result: "none".to_string(),
        })
    })
}

/// Einmal moeglichst frueh in main() aufrufen, fuer einen korrekten uptime_s-Wert.
pub fn mark_app_start() {
    APP_START.get_or_init(Instant::now);
}

fn uptime_s() -> u64 {
    APP_START.get().map(|t| t.elapsed().as_secs()).unwrap_or(0)
}

/// Vom Frontend periodisch aufgerufen (kein neuer Geraete-Poll/keine neue
/// Standort-Abfrage - nur ein Abbild des bereits anderswo in Control
/// ermittelten Status/Standorts/Modus). Ein Aufruf aktualisiert devices,
/// venue, place und demo_mode atomar zusammen, damit kein inkonsistenter
/// Zwischenzustand (z.B. neue Geraete, aber alter Standort) gesendet wird.
pub fn update_snapshot(devices: Vec<DeviceStatusEntry>, venue: String, place: String, demo_mode: bool) {
    let mut s = lock_state();
    s.devices = devices.into_iter().take(MAX_DEVICES).collect();
    s.venue = venue.trim().to_string();
    s.place = place.trim().to_string();
    s.demo_mode = demo_mode;
}

/// Neuen Fehler in den Ringpuffer aufnehmen. Wird nur fuer tatsaechlich
/// neue (nicht unterdrueckte, nicht deduplizierte) Fehler aufgerufen -
/// siehe addError() im Frontend.
pub fn push_error(msg: &str) {
    let truncated: String = if msg.chars().count() > MAX_ERROR_MSG_LEN {
        msg.chars().take(MAX_ERROR_MSG_LEN).collect()
    } else {
        msg.to_string()
    };
    let mut s = lock_state();
    if s.errors.len() >= MAX_ERROR_BUFFER {
        s.errors.pop_front();
    }
    s.errors.push_back(ErrorEntry { ts: now_iso8601_utc(), msg: truncated });
}

#[derive(Debug, Clone, Serialize)]
pub struct HeartbeatStatus {
    pub enabled: bool,
    pub url: String,
    /// Ob ein API-Key gespeichert ist - der Key selbst erscheint hier nie.
    pub key_set: bool,
    pub last_success_ms: Option<u64>,
    pub consecutive_failures: u32,
    pub last_result: String,
    pub pending_errors: usize,
    pub device_count: usize,
    /// venue/place sind keine Geheimnisse - im Gegensatz zum API-Key hier
    /// bewusst sichtbar, damit in der UI direkt nachvollziehbar ist, was
    /// der naechste Beat tatsaechlich senden wuerde (ohne auf einen echten
    /// Serverkontakt warten zu muessen).
    pub venue: String,
    pub place: String,
    pub demo_mode: bool,
}

pub fn get_status(cfg: &serde_json::Value) -> HeartbeatStatus {
    let hc = HeartbeatConfig::from_json(cfg);
    let s = lock_state();
    HeartbeatStatus {
        enabled: hc.enabled,
        url: hc.url,
        key_set: !hc.api_key.is_empty(),
        last_success_ms: s.last_success_ms,
        consecutive_failures: s.consecutive_failures,
        last_result: s.last_result.clone(),
        pending_errors: s.errors.len(),
        device_count: s.devices.len(),
        venue: s.venue.clone(),
        place: s.place.clone(),
        demo_mode: s.demo_mode,
    }
}

/// Aus einer erfolgreichen 200-Antwort extrahierte Zusatzinfo, sofern der
/// Server das neue JSON-Format liefert. Aeltere Server, die nur den reinen
/// Text "ok" senden, liefern hier einfach kein system/geo (beides None) -
/// das ist ausdruecklich kein Fehler.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SuccessInfo {
    pub system: Option<String>,
    pub geo: Option<String>, // "exact" | "place" | "none"
}

fn parse_success_body(body: &str) -> SuccessInfo {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body.trim()) {
        return SuccessInfo {
            system: v.get("system").and_then(|x| x.as_str()).map(|s| s.to_string()),
            geo: v.get("geo").and_then(|x| x.as_str()).map(|s| s.to_string()),
        };
    }
    SuccessInfo::default()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
    Success(SuccessInfo),
    Unauthorized,
    TooLarge,
    BadRequest,
    Timeout,
    NotConfigured(String),
    Other(String),
}

impl SendOutcome {
    /// Kurzer, UI-tauglicher Text ohne jegliche Secret-Werte.
    pub fn label(&self) -> String {
        match self {
            SendOutcome::Success(_) => "ok".to_string(),
            SendOutcome::Unauthorized => "401".to_string(),
            SendOutcome::TooLarge => "413".to_string(),
            SendOutcome::BadRequest => "400".to_string(),
            SendOutcome::Timeout => "timeout".to_string(),
            SendOutcome::NotConfigured(reason) => format!("not_configured: {}", reason),
            SendOutcome::Other(reason) => format!("error: {}", reason),
        }
    }
}

pub fn build_payload(app_version: &str) -> (serde_json::Value, usize) {
    let s = lock_state();
    let errors_to_send: Vec<&ErrorEntry> = s.errors.iter().take(MAX_ERRORS_PER_BEAT).collect();
    let payload = serde_json::json!({
        "version": app_version,
        "uptime_s": uptime_s(),
        "venue": s.venue,
        "place": s.place,
        "demo_mode": s.demo_mode,
        "devices": s.devices,
        "errors": errors_to_send,
    });
    (payload, errors_to_send.len())
}

fn mark_failure(result: &str) {
    let mut s = lock_state();
    s.consecutive_failures += 1;
    s.last_result = result.to_string();
}

fn mark_success(sent_error_count: usize) {
    let mut s = lock_state();
    for _ in 0..sent_error_count {
        s.errors.pop_front();
    }
    s.last_success_ms = Some(now_ms());
    s.consecutive_failures = 0;
    s.last_result = "ok".to_string();
}

/// Testbarer Kern: sendet genau einen Beat. `require_https` ist in Produktion
/// immer true; Unit-Tests gegen einen lokalen Mock-Server setzen es auf false.
pub fn send_once(hc: &HeartbeatConfig, app_version: &str, require_https: bool) -> SendOutcome {
    if hc.url.is_empty() || hc.api_key.is_empty() {
        return SendOutcome::NotConfigured("url oder api_key fehlt".to_string());
    }
    if require_https && !hc.url.starts_with("https://") {
        return SendOutcome::NotConfigured("URL muss https:// verwenden".to_string());
    }

    let (payload, sent_error_count) = build_payload(app_version);
    let body = match serde_json::to_vec(&payload) {
        Ok(b) => b,
        Err(e) => {
            let msg = format!("serialize: {}", e);
            mark_failure(&msg);
            return SendOutcome::Other(msg);
        }
    };
    if body.len() > MAX_BODY_BYTES {
        let msg = format!("payload {} bytes > {} bytes limit", body.len(), MAX_BODY_BYTES);
        mark_failure(&msg);
        return SendOutcome::Other(msg);
    }

    let client = match reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_millis(HTTP_CONNECT_TIMEOUT_MS))
        .timeout(Duration::from_millis(HTTP_TOTAL_TIMEOUT_MS))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("client build: {}", e);
            mark_failure(&msg);
            return SendOutcome::Other(msg);
        }
    };

    let url = format!("{}/hb", hc.url.trim_end_matches('/'));
    let result = client
        .post(&url)
        .bearer_auth(&hc.api_key)
        .header("Content-Type", "application/json")
        .body(body)
        .send();

    match result {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() {
                let body_text = resp.text().unwrap_or_default();
                let info = parse_success_body(&body_text);
                mark_success(sent_error_count);
                SendOutcome::Success(info)
            } else {
                let code = status.as_u16();
                let outcome = match code {
                    401 => SendOutcome::Unauthorized,
                    413 => SendOutcome::TooLarge,
                    400 => SendOutcome::BadRequest,
                    _ => SendOutcome::Other(format!("http {}", code)),
                };
                mark_failure(&outcome.label());
                outcome
            }
        }
        Err(e) => {
            let outcome = if e.is_timeout() {
                SendOutcome::Timeout
            } else {
                // reqwest-Fehlertext kann die Ziel-URL enthalten, aber nie den
                // Authorization-Header/Key - der steckt nur im Request, nicht im Error.
                SendOutcome::Other(e.to_string())
            };
            mark_failure(&outcome.label());
            outcome
        }
    }
}

/// Ergebnis eines manuellen Testversands, fuer die UI aufbereitet. Enthaelt
/// nie den API-Key.
#[derive(Debug, Clone, Serialize)]
pub struct SendTestResult {
    /// "ok" | "401" | "413" | "400" | "timeout" | "not_configured" | "error"
    pub outcome: String,
    pub system: Option<String>,
    pub geo: Option<String>,
}

/// Einmaliger manueller Testversand ("Test-Heartbeat senden" Button).
/// Erzwingt https, wie der normale Hintergrund-Task.
pub fn send_test(cfg: &serde_json::Value, app_version: &str) -> SendTestResult {
    let hc = HeartbeatConfig::from_json(cfg);
    match send_once(&hc, app_version, true) {
        SendOutcome::Success(info) => SendTestResult { outcome: "ok".to_string(), system: info.system, geo: info.geo },
        SendOutcome::NotConfigured(_) => SendTestResult { outcome: "not_configured".to_string(), system: None, geo: None },
        other => SendTestResult { outcome: other.label(), system: None, geo: None },
    }
}

/// Startet den Hintergrund-Thread. Idempotent (nur der erste Aufruf startet
/// tatsaechlich einen Thread). `get_cfg` liest bei jedem Tick die aktuelle,
/// lebende Konfiguration (damit Einstellungsaenderungen ohne Neustart
/// greifen). Blockiert niemals die Geraete-Steuerung: eigener OS-Thread,
/// keine gemeinsame Verbindung/Lock mit Geraete-Pollern. Intervall ist fest
/// (DEFAULT_INTERVAL_S), nicht konfigurierbar.
pub fn start<F>(get_cfg: F, app_version: String)
where
    F: Fn() -> serde_json::Value + Send + 'static,
{
    if TASK_STARTED.set(()).is_err() {
        return;
    }
    thread::spawn(move || {
        // Erster Beat kurz nach App-Start, nicht sofort (Netzwerk/Config noch nicht bereit).
        thread::sleep(Duration::from_secs(5));
        loop {
            let cfg = get_cfg();
            let hc = HeartbeatConfig::from_json(&cfg);
            if hc.enabled {
                let outcome = send_once(&hc, &app_version, true);
                if !matches!(outcome, SendOutcome::Success(_) | SendOutcome::NotConfigured(_)) {
                    eprintln!("[Heartbeat] Beat fehlgeschlagen: {}", outcome.label());
                }
            }
            thread::sleep(Duration::from_secs(DEFAULT_INTERVAL_S));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // Alle Tests teilen sich den einen globalen STATE-Mutex (das Modul ist
    // bewusst als Prozess-weiter Singleton gebaut, wie device_health()
    // anderswo im Projekt). Ohne Serialisierung wuerden parallel laufende
    // Tests sich gegenseitig die Ringpuffer-Zaehlung verfaelschen. Daher
    // hier ein eigener, von STATE unabhaengiger Test-Lock: jeder Test haelt
    // ihn fuer seine gesamte Laufzeit (_guard lebt bis Testende).
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn lock_test() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn reset_state() {
        let mut s = lock_state();
        s.devices.clear();
        s.venue.clear();
        s.place.clear();
        s.demo_mode = false;
        s.errors.clear();
        s.last_success_ms = None;
        s.consecutive_failures = 0;
        s.last_result = "none".to_string();
    }

    fn sample_config(url: String) -> HeartbeatConfig {
        HeartbeatConfig {
            enabled: true,
            url,
            api_key: "test-token-not-real".to_string(),
        }
    }

    #[test]
    fn payload_serializes_expected_shape() {
        let _guard = lock_test();
        reset_state();
        update_snapshot(
            vec![DeviceStatusEntry { name: "Projektor PJ-1".to_string(), status: "ok".to_string() }],
            "Johanneskirche".to_string(),
            "Stuttgart".to_string(),
            true,
        );
        push_error("Pixera: Timeline nicht erreichbar");
        let (payload, sent) = build_payload("1.8.2");
        assert_eq!(sent, 1);
        assert_eq!(payload["version"], "1.8.2");
        assert_eq!(payload["venue"], "Johanneskirche");
        assert_eq!(payload["place"], "Stuttgart");
        assert_eq!(payload["demo_mode"], true);
        assert_eq!(payload["devices"][0]["name"], "Projektor PJ-1");
        assert_eq!(payload["devices"][0]["status"], "ok");
        assert_eq!(payload["errors"][0]["msg"], "Pixera: Timeline nicht erreichbar");
        assert!(payload["errors"][0]["ts"].as_str().unwrap().ends_with('Z'));
        // Kein system/lat/lon mehr im Payload, und kein API-Key unter irgendwelchen Umstaenden.
        assert!(payload.get("system").is_none());
        assert!(payload.get("lat").is_none());
        assert!(payload.get("lon").is_none());
        let serialized = payload.to_string();
        assert!(!serialized.contains("test-token-not-real"));
    }

    #[test]
    fn error_ring_buffer_caps_at_max_and_drops_oldest() {
        let _guard = lock_test();
        reset_state();
        for i in 0..60 {
            push_error(&format!("error #{}", i));
        }
        let s = lock_state();
        assert_eq!(s.errors.len(), MAX_ERROR_BUFFER);
        // Die aeltesten (0..9) muessen verdraengt worden sein, die neuesten bleiben.
        assert_eq!(s.errors.front().unwrap().msg, "error #10");
        assert_eq!(s.errors.back().unwrap().msg, "error #59");
    }

    #[test]
    fn error_message_gets_truncated() {
        let _guard = lock_test();
        reset_state();
        let long_msg = "x".repeat(500);
        push_error(&long_msg);
        let s = lock_state();
        assert_eq!(s.errors.back().unwrap().msg.chars().count(), MAX_ERROR_MSG_LEN);
    }

    #[test]
    fn errors_only_removed_from_buffer_on_successful_send() {
        let _guard = lock_test();
        reset_state();
        push_error("first");
        push_error("second");
        let hc = sample_config("http://127.0.0.1:1".to_string()); // nichts hoert dort - Verbindung schlaegt fehl
        let outcome = send_once(&hc, "1.0.0", false);
        assert!(!matches!(outcome, SendOutcome::Success(_)));
        let s = lock_state();
        assert_eq!(s.errors.len(), 2, "Fehler duerfen bei fehlgeschlagenem Versand nicht verloren gehen");
    }

    #[test]
    fn send_against_mock_server_success_with_legacy_plain_ok_body() {
        let _guard = lock_test();
        reset_state();
        push_error("will be cleared on success");
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr();
        let handle = thread::spawn(move || {
            if let Ok(Some(req)) = server.recv_timeout(Duration::from_secs(5)) {
                let _ = req.respond(tiny_http::Response::from_string("ok").with_status_code(200));
            }
        });
        let hc = sample_config(format!("http://{}", addr));
        let outcome = send_once(&hc, "1.0.0", false);
        handle.join().unwrap();
        assert_eq!(outcome, SendOutcome::Success(SuccessInfo::default()), "aelterer Server, der nur 'ok' sendet, muss trotzdem als Erfolg ohne system/geo gewertet werden");
        let s = lock_state();
        assert_eq!(s.errors.len(), 0, "erfolgreich gesendete Fehler muessen aus dem Puffer entfernt werden");
        assert!(s.last_success_ms.is_some());
        assert_eq!(s.consecutive_failures, 0);
    }

    #[test]
    fn send_against_mock_server_success_with_json_system_and_geo() {
        let _guard = lock_test();
        reset_state();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr();
        let handle = thread::spawn(move || {
            if let Ok(Some(req)) = server.recv_timeout(Duration::from_secs(5)) {
                let body = r#"{"ok":true,"system":"LAP03","geo":"exact"}"#;
                let _ = req.respond(tiny_http::Response::from_string(body).with_status_code(200));
            }
        });
        let hc = sample_config(format!("http://{}", addr));
        let outcome = send_once(&hc, "1.0.0", false);
        handle.join().unwrap();
        assert_eq!(
            outcome,
            SendOutcome::Success(SuccessInfo { system: Some("LAP03".to_string()), geo: Some("exact".to_string()) })
        );
    }

    #[test]
    fn send_test_reports_system_and_geo_for_ui() {
        let _guard = lock_test();
        reset_state();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr();
        let handle = thread::spawn(move || {
            if let Ok(Some(req)) = server.recv_timeout(Duration::from_secs(5)) {
                let body = r#"{"ok":true,"system":"LAP03","geo":"place"}"#;
                let _ = req.respond(tiny_http::Response::from_string(body).with_status_code(200));
            }
        });
        let cfg = serde_json::json!({
            "heartbeat_enabled": true,
            "heartbeat_url": format!("http://{}", addr),
            "heartbeat_api_key": "test-token-not-real",
        });
        let hc = HeartbeatConfig::from_json(&cfg);
        // send_test() selbst erzwingt https (require_https=true); fuer den Test gegen den
        // lokalen http-Mock-Server rufen wir daher send_once() mit require_https=false aut,
        // aber pruefen die identische SendTestResult-Ableitung wie send_test() sie macht.
        let outcome = send_once(&hc, "1.0.0", false);
        handle.join().unwrap();
        let result = match outcome {
            SendOutcome::Success(info) => SendTestResult { outcome: "ok".to_string(), system: info.system, geo: info.geo },
            other => SendTestResult { outcome: other.label(), system: None, geo: None },
        };
        assert_eq!(result.outcome, "ok");
        assert_eq!(result.system.as_deref(), Some("LAP03"));
        assert_eq!(result.geo.as_deref(), Some("place"));
    }

    #[test]
    fn send_against_mock_server_unauthorized() {
        let _guard = lock_test();
        reset_state();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr();
        let handle = thread::spawn(move || {
            if let Ok(Some(req)) = server.recv_timeout(Duration::from_secs(5)) {
                let _ = req.respond(tiny_http::Response::from_string("unauthorized").with_status_code(401));
            }
        });
        let hc = sample_config(format!("http://{}", addr));
        let outcome = send_once(&hc, "1.0.0", false);
        handle.join().unwrap();
        assert_eq!(outcome, SendOutcome::Unauthorized);
        let s = lock_state();
        assert_eq!(s.consecutive_failures, 1);
    }

    #[test]
    fn send_times_out_when_server_never_responds() {
        let _guard = lock_test();
        reset_state();
        // Server nimmt die Verbindung an, antwortet aber nie - erzwingt den
        // Total-Timeout statt eines sofortigen Connect-Fehlers.
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr();
        let handle = thread::spawn(move || {
            // Request bewusst am Leben halten (nicht respond()en, nicht droppen) -
            // tiny_http wuerde einen fallengelassenen Request sofort mit einem
            // automatischen 500 beantworten, was den Timeout-Test unterlaufen wuerde.
            if let Ok(Some(req)) = server.recv_timeout(Duration::from_secs(8)) {
                thread::sleep(Duration::from_secs(7));
                let _ = req.respond(tiny_http::Response::from_string("too late").with_status_code(200));
            }
        });
        let hc = sample_config(format!("http://{}", addr));
        let started = Instant::now();
        let outcome = send_once(&hc, "1.0.0", false);
        let elapsed = started.elapsed();
        handle.join().unwrap();
        assert_eq!(outcome, SendOutcome::Timeout);
        assert!(elapsed < Duration::from_secs(7), "Timeout muss am konfigurierten Client-Timeout greifen, nicht erst am Testserver-Limit");
    }

    #[test]
    fn status_never_contains_api_key() {
        let _guard = lock_test();
        reset_state();
        let cfg = serde_json::json!({
            "heartbeat_enabled": true,
            "heartbeat_url": "https://example.invalid",
            "heartbeat_api_key": "test-token-not-real",
        });
        let status = get_status(&cfg);
        assert!(status.key_set);
        let serialized = serde_json::to_string(&status).unwrap();
        assert!(!serialized.contains("test-token-not-real"), "API-Key darf nie im heartbeat_get_status Rueckgabewert auftauchen");
        assert!(!serialized.to_lowercase().contains("api_key"), "Status-Struktur darf kein api_key-Feld besitzen");
    }

    #[test]
    fn not_configured_when_missing_fields() {
        let _guard = lock_test();
        reset_state();
        let hc = HeartbeatConfig::default();
        let outcome = send_once(&hc, "1.0.0", true);
        assert!(matches!(outcome, SendOutcome::NotConfigured(_)));
    }

    #[test]
    fn not_configured_when_only_url_set_without_key() {
        let _guard = lock_test();
        reset_state();
        let hc = HeartbeatConfig { enabled: true, url: "https://example.invalid".to_string(), api_key: String::new() };
        let outcome = send_once(&hc, "1.0.0", true);
        assert!(matches!(outcome, SendOutcome::NotConfigured(_)), "ohne API-Key darf kein Versand erfolgen, auch wenn die URL gesetzt ist");
    }
}
