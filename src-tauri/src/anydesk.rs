// ============================================================
// AnyDesk-ID Auto-Erkennung
// ============================================================
// Liest die lokale AnyDesk-ID aus, damit die AnyDesk-Adresse in den
// Einstellungen nicht mehr manuell eingetragen werden muss.
//
// Primaer:  %ProgramData%\AnyDesk*\system.conf, Zeile "ad.anynet.id=<ID>"
//           (Custom-Clients von my.anydesk.com landen in "AnyDesk-<prefix>"
//           statt im Standardordner "AnyDesk" - deshalb wird der ganze
//           %ProgramData%-Ordner nach "AnyDesk*" durchsucht.)
// Fallback: "<AnyDesk-Installationsordner>\AnyDesk.exe --get-id"
//           (Program Files (x86) bzw. Program Files, ebenfalls mit
//           Custom-Prefix-Unterstuetzung.)
// Wenn beides fehlschlaegt (AnyDesk nicht installiert o.ae.): Ok(None),
// niemals eine Panic - das Adressfeld bleibt dann einfach leer/manuell.

use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

/// Durchsucht ein Basisverzeichnis nach Unterordnern "AnyDesk" oder
/// "AnyDesk-<prefix>" (Custom-Client). "AnyDesk" ohne Suffix wird bevorzugt,
/// falls mehrere Kandidaten existieren.
fn find_anydesk_dirs(base: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let Ok(entries) = std::fs::read_dir(base) else {
        return dirs;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "AnyDesk" || name.starts_with("AnyDesk-") {
            dirs.push(path);
        }
    }
    // "AnyDesk" (Standardinstallation) zuerst probieren, danach Custom-Prefixe.
    dirs.sort_by_key(|p| {
        let is_plain = p.file_name().map(|n| n == "AnyDesk").unwrap_or(false);
        if is_plain { 0 } else { 1 }
    });
    dirs
}

fn parse_id_from_conf(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    for line in content.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("ad.anynet.id=") {
            let id = rest.trim();
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
    }
    None
}

/// Primaere Methode: system.conf unter %ProgramData%\AnyDesk*\ direkt lesen.
fn get_id_from_system_conf() -> Option<String> {
    let program_data = std::env::var("ProgramData").unwrap_or_else(|_| "C:\\ProgramData".to_string());
    for dir in find_anydesk_dirs(Path::new(&program_data)) {
        let conf = dir.join("system.conf");
        if conf.is_file() {
            if let Some(id) = parse_id_from_conf(&conf) {
                return Some(id);
            }
        }
    }
    None
}

fn find_anydesk_exe() -> Option<PathBuf> {
    let program_files_x86 =
        std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| "C:\\Program Files (x86)".to_string());
    let program_files = std::env::var("ProgramFiles").unwrap_or_else(|_| "C:\\Program Files".to_string());
    for base in [program_files_x86, program_files] {
        for dir in find_anydesk_dirs(Path::new(&base)) {
            let exe = dir.join("AnyDesk.exe");
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

/// Fallback-Methode: "AnyDesk.exe --get-id" aufrufen und stdout auswerten.
fn get_id_from_cli() -> Option<String> {
    let exe = find_anydesk_exe()?;
    let mut cmd = Command::new(&exe);
    cmd.arg("--get-id");
    #[cfg(target_os = "windows")]
    {
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(id)
}

/// Ermittelt die lokale AnyDesk-ID. Gibt Ok(None) zurueck (statt Err), wenn
/// AnyDesk nicht installiert ist oder die ID auf keinem der beiden Wege
/// gefunden werden konnte - das ist kein Fehlerfall, sondern ein normales,
/// erwartbares Ergebnis (z.B. auf einem PC ohne AnyDesk).
pub fn get_id() -> Result<Option<String>, String> {
    if let Some(id) = get_id_from_system_conf() {
        return Ok(Some(id));
    }
    if let Some(id) = get_id_from_cli() {
        return Ok(Some(id));
    }
    Ok(None)
}
