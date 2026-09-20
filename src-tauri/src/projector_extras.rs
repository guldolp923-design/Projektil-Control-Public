// ============================================================
// Projektor-Zusatzfunktionen (Testbild, Quelle waehlen) fuer die beiden bei
// Projektil eingesetzten Hersteller. Marken werden innerhalb einer Show nie
// gemischt - genau EIN globaler Wert (cfg["projector_brand"]) entscheidet,
// welche Implementierung fuer ALLE PJ 1..N verwendet wird.
//
// Hinweis zur urspruenglichen Spezifikation: dort war `async fn` im Trait
// vorgesehen. Traits mit async fn sind ohne die async-trait-Crate nicht
// dyn-kompatibel (kein Box<dyn ProjectorExtras>), und eine blockierende
// TCP-Verbindung bzw. reqwest::blocking innerhalb einer async fn kann exakt
// den Tokio-Runtime-Panic ausloesen, der in dieser Codebase schon einmal
// aufgetreten ist (siehe pixera_hub_command). Die Schnittstelle ist deshalb
// bewusst synchron/blockierend - wie der Rest dieser Codebase es fuer
// Tauri-Commands durchgaengig macht (z.B. hub_post_json, icmp_ping) - und
// wird ueber ein dyn Trait Object ausgewaehlt.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

pub trait ProjectorExtras: Send {
    fn set_test_pattern(&self, ip: &str, on: bool) -> Result<(), String>;
    fn get_test_pattern(&self, ip: &str) -> Result<bool, String>;
    fn switch_to_hdmi1(&self, ip: &str) -> Result<(), String>;
}

/// Waehlt EINMAL anhand des globalen Show-Werts die passende Implementierung.
/// Karten-UI und globale Aktionen im Frontend kennen nur die generischen
/// Tauri-Commands weiter unten, nie den Hersteller direkt.
pub fn projector_extras_for_brand(brand: &str, epson_password: Option<String>) -> Box<dyn ProjectorExtras> {
    if brand.eq_ignore_ascii_case("epson") {
        Box::new(EpsonExtras { password: epson_password })
    } else {
        Box::new(PanasonicExtras)
    }
}

// ============================================================
// PANASONIC PT-RQ18K / PT-RQ25K Series (LAN, Port 1024)
// Verifiziert gegen die offizielle Control Command List (RQ25K Series,
// docs.connect.panasonic.com, Stand 2023-06):
//   Testbild [OTS]: OTS:00 Aus, OTS:01 Weiss, OTS:02 Schwarz, OTS:05 Window,
//                   OTS:06 Reversed Window, OTS:07 Cross Hatch
//   Query:          QTS (Antwortformat nicht 100% verifiziert, vermutlich
//                   Echo wie "OTS:01" - siehe get_test_pattern unten)
//   Quelle:         IIS:HD1 (HDMI1), IIS:HD2 (HDMI2), IIS:DP1 (DisplayPort)
//   Power:          PON/POF, Query QPW -> "001"/"000"
// Auth-Handshake (Protect Mode, Standard-LAN-Protokoll der Panasonic
// Large-Venue-Serie, verifiziert ueber die baugleiche Sektion der
// Schwesterserie PT-RZ12K - die RQ25K-eigene Anleitung war beim Abruf
// gesperrt, dieser Abschnitt ist bei Panasonic aber seriesuebergreifend
// wortgleich):
//   1. TCP-Verbindung zu <IP>:1024
//   2. Begruessung "NTCONTROL 1 <random>" (Mode 1 = Protect) oder
//      "NTCONTROL 0" (kein Passwort noetig)
//   3. Hash = MD5("admin1:panasonic:<random>")  (Username:Passwort:Random)
//   4. Hash wird JEDEM Befehl direkt vorangestellt, kein Trennzeichen: CR am
//      Ende: "<32-Zeichen-Hash>OTS:01\r"
// ============================================================

const PANASONIC_PORT: u16 = 1024;
const PANASONIC_USER: &str = "admin1";
const PANASONIC_PASSWORD: &str = "panasonic";
const PANASONIC_TIMEOUT: Duration = Duration::from_millis(4000);

fn panasonic_send(ip: &str, cmd: &str) -> Result<String, String> {
    let addr = format!("{}:{}", ip, PANASONIC_PORT);
    let socket_addr = addr
        .parse::<std::net::SocketAddr>()
        .map_err(|e| format!("Panasonic: ungueltige IP {}: {}", ip, e))?;
    let stream = TcpStream::connect_timeout(&socket_addr, PANASONIC_TIMEOUT)
        .map_err(|e| format!("Panasonic {} nicht erreichbar: {}", ip, e))?;
    stream.set_read_timeout(Some(PANASONIC_TIMEOUT)).ok();
    stream.set_write_timeout(Some(PANASONIC_TIMEOUT)).ok();

    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut writer = stream;

    let mut greeting = String::new();
    reader
        .read_line(&mut greeting)
        .map_err(|e| format!("Panasonic {}: Begruessung fehlgeschlagen: {}", ip, e))?;
    let greeting = greeting.trim();

    let prefix = if let Some(rest) = greeting.strip_prefix("NTCONTROL 1 ") {
        let random = rest.trim();
        let material = format!("{}:{}:{}", PANASONIC_USER, PANASONIC_PASSWORD, random);
        format!("{:x}", md5::compute(material.as_bytes()))
    } else {
        // "NTCONTROL 0" oder unerwartetes Format -> ohne Hash-Praefix versuchen.
        String::new()
    };

    let payload = format!("{}{}\r", prefix, cmd);
    writer
        .write_all(payload.as_bytes())
        .map_err(|e| format!("Panasonic {}: Senden fehlgeschlagen: {}", ip, e))?;

    let mut response = String::new();
    reader
        .read_line(&mut response)
        .map_err(|e| format!("Panasonic {}: Antwort fehlgeschlagen: {}", ip, e))?;
    let response = response.trim().to_string();

    if response.starts_with("ERR") {
        return Err(format!("Panasonic {} lehnte Befehl '{}' ab: {}", ip, cmd, response));
    }
    Ok(response)
}

pub struct PanasonicExtras;

impl ProjectorExtras for PanasonicExtras {
    fn set_test_pattern(&self, ip: &str, on: bool) -> Result<(), String> {
        panasonic_send(ip, if on { "OTS:01" } else { "OTS:00" })?;
        Ok(())
    }

    fn get_test_pattern(&self, ip: &str) -> Result<bool, String> {
        let response = panasonic_send(ip, "QTS")?;
        // Antwortformat nicht 100% verifiziert (RQ25K-eigene Anleitung mit
        // Beispiel-Responses war beim Abruf gesperrt) - vermutlich ein Echo
        // des aktuellen Musters wie "OTS:01". Defensiv auf Sub-String pruefen
        // statt exaktem Vergleich, damit kleinere Formatabweichungen nicht zu
        // einem Fehlschlag fuehren.
        Ok(response.contains("01"))
    }

    fn switch_to_hdmi1(&self, ip: &str) -> Result<(), String> {
        panasonic_send(ip, "IIS:HD1")?;
        Ok(())
    }
}

// ============================================================
// EPSON EB-PU2216B (Web API, HTTP) - vollstaendig verifiziert
//   Basis: http://<IP>/api/v01/control/escvp21?cmd=...
//   Testbild an (Weiss): cmd=TESTPATTERN+01+06
//   Testbild aus:        cmd=TESTPATTERN+00
//   Quelle HDMI1:        cmd=SOURCE+30
//   Query:                cmd=TESTPATTERN? -> Textantwort "TESTPATTERN=01,06"
// Auth (nur falls "API Authentication" im Projektor-Menue aktiviert ist):
//   HTTP Digest, Username "EPSONWEB", Passwort = hinterlegtes
//   Web-Control-Passwort. Voraussetzung: "Web API" muss im Projektor-Menue
//   [Network Settings] eingeschaltet sein, sonst HTTP 403.
// ============================================================

const EPSON_TIMEOUT_SECS: u64 = 6;
const EPSON_USER: &str = "EPSONWEB";

fn parse_digest_params(header_value: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for part in header_value.split(',') {
        let part = part.trim();
        if let Some(eq_idx) = part.find('=') {
            let key = part[..eq_idx].trim().to_string();
            let mut value = part[eq_idx + 1..].trim().to_string();
            if value.starts_with('"') && value.ends_with('"') && value.len() >= 2 {
                value = value[1..value.len() - 1].to_string();
            }
            map.insert(key, value);
        }
    }
    map
}

fn epson_digest_auth_header(
    password: &str,
    challenge: &HashMap<String, String>,
    method: &str,
    uri: &str,
    nc: &str,
    cnonce: &str,
) -> Option<String> {
    let realm = challenge.get("realm")?;
    let nonce = challenge.get("nonce")?;
    let qop = challenge.get("qop").cloned().unwrap_or_default();
    let ha1 = format!("{:x}", md5::compute(format!("{}:{}:{}", EPSON_USER, realm, password).as_bytes()));
    let ha2 = format!("{:x}", md5::compute(format!("{}:{}", method, uri).as_bytes()));
    let response = if qop.contains("auth") {
        format!(
            "{:x}",
            md5::compute(format!("{}:{}:{}:{}:{}:{}", ha1, nonce, nc, cnonce, "auth", ha2).as_bytes())
        )
    } else {
        format!("{:x}", md5::compute(format!("{}:{}:{}", ha1, nonce, ha2).as_bytes()))
    };
    let mut header = format!(
        "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{}\"",
        EPSON_USER, realm, nonce, uri, response
    );
    if qop.contains("auth") {
        header.push_str(&format!(", qop=auth, nc={}, cnonce=\"{}\"", nc, cnonce));
    }
    if let Some(opaque) = challenge.get("opaque") {
        header.push_str(&format!(", opaque=\"{}\"", opaque));
    }
    Some(header)
}

fn epson_request(ip: &str, password: Option<&str>, cmd: &str) -> Result<String, String> {
    let uri = format!("/api/v01/control/escvp21?cmd={}", cmd);
    let url = format!("http://{}{}", ip, uri);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(EPSON_TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("Epson: Client-Aufbau fehlgeschlagen: {}", e))?;

    let first = client
        .get(&url)
        .send()
        .map_err(|e| format!("Epson {} nicht erreichbar: {}", ip, e))?;

    if first.status() == reqwest::StatusCode::FORBIDDEN {
        return Err(format!(
            "Epson {}: Web API ist im Projektor-Menue [Network Settings] deaktiviert (HTTP 403)",
            ip
        ));
    }

    if first.status() == reqwest::StatusCode::UNAUTHORIZED {
        let password = password
            .filter(|p| !p.is_empty())
            .ok_or_else(|| format!("Epson {}: API Authentication ist aktiv, aber kein Web-Control-Passwort hinterlegt", ip))?;
        let www_auth = first
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| format!("Epson {}: Digest-Challenge fehlt in der 401-Antwort", ip))?;
        let challenge = parse_digest_params(www_auth.strip_prefix("Digest ").unwrap_or(www_auth));
        let nc = "00000001";
        let cnonce_full = format!("{:x}", md5::compute(format!("{}:{}", now_timestamp_ms_local(), ip).as_bytes()));
        let cnonce = &cnonce_full[..8];
        let auth_header = epson_digest_auth_header(password, &challenge, "GET", &uri, nc, cnonce)
            .ok_or_else(|| format!("Epson {}: Digest-Challenge unvollstaendig (realm/nonce fehlt)", ip))?;

        let response = client
            .get(&url)
            .header(reqwest::header::AUTHORIZATION, auth_header)
            .send()
            .map_err(|e| format!("Epson {}: Anfrage fehlgeschlagen: {}", ip, e))?;
        let status = response.status();
        let text = response.text().unwrap_or_default();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(format!("Epson {}: falsches Web-Control-Passwort", ip));
        }
        if status == reqwest::StatusCode::FORBIDDEN {
            return Err(format!(
                "Epson {}: Web API ist im Projektor-Menue [Network Settings] deaktiviert (HTTP 403)",
                ip
            ));
        }
        if !status.is_success() {
            return Err(format!("Epson {} HTTP {}: {}", ip, status.as_u16(), text));
        }
        return Ok(text.trim().to_string());
    }

    let status = first.status();
    let text = first.text().unwrap_or_default();
    if !status.is_success() {
        return Err(format!("Epson {} HTTP {}: {}", ip, status.as_u16(), text));
    }
    Ok(text.trim().to_string())
}

fn now_timestamp_ms_local() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

pub struct EpsonExtras {
    pub password: Option<String>,
}

impl ProjectorExtras for EpsonExtras {
    fn set_test_pattern(&self, ip: &str, on: bool) -> Result<(), String> {
        let cmd = if on { "TESTPATTERN+01+06" } else { "TESTPATTERN+00" };
        epson_request(ip, self.password.as_deref(), cmd)?;
        Ok(())
    }

    fn get_test_pattern(&self, ip: &str) -> Result<bool, String> {
        let response = epson_request(ip, self.password.as_deref(), "TESTPATTERN?")?;
        // z.B. "TESTPATTERN=01,06" (an, Muster 06) oder "TESTPATTERN=00" (aus)
        Ok(response.to_uppercase().contains("=01"))
    }

    fn switch_to_hdmi1(&self, ip: &str) -> Result<(), String> {
        epson_request(ip, self.password.as_deref(), "SOURCE+30")?;
        Ok(())
    }
}
