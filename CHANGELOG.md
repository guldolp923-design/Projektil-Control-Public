# Changelog

## v1.0.22 - 2026-08-10

### Added
- Optionales Monitoring für den Notaus-Schalter mit der IP 192.168.1.99
- Schalter in den Einstellungen, ob die Installation einen Notaus verwendet

### Changed
- Emergency sendet jetzt nur noch einen OSC-Befehl `/notaus` an Pixera
- Die Endstufen werden im Emergency-Pfad nicht mehr gemutet

### Release Assets
- Private Release: Windows EXE + Windows Installer (NSIS) + Sourcecode-Archiv
- Public Release: Windows EXE + Windows Installer (NSIS)

## v1.0.16-beta - 2026-05-21

### Added
- Systemlog-Einträge sind jetzt sprachabhängig und schalten bei der Sprachwahl live um
- Einstellungen wurden komplett i18n-fähig gemacht, inklusive Platzhalter und Buttons

### Changed
- Hotline wurde fest auf +41 44 492 51 69 gesetzt
- Rust/Tauri-Abhängigkeiten wurden aktualisiert und lokal geprüft
- Runtime-Texte, Statusmeldungen und Fehlertexte wurden weiter vereinheitlicht

### Release Assets
- Private Release: Windows EXE + Windows Installer (NSIS) + Sourcecode-Archiv
- Public Release: Windows EXE + Windows Installer (NSIS)

## v1.0.4-beta - 2026-04-20

### Added
- Neuer Startup-Screen mit Projektil-Branding, Fortschrittsanzeige und Versionsanzeige
- Persistente System- und Fehlerlogs über Neustarts hinweg (90 Tage Aufbewahrung)

### Changed
- Startprüfung läuft parallel und dadurch schneller
- Fehlergründe beim Startup sind klar klassifiziert: Timeout, Verbindung abgelehnt, Gerät nicht erreichbar
- Fehlerlog zeigt nur noch Fehler der aktuellen Sitzung, ältere Einträge bleiben im Systemlog
- Deutsche UI-Texte wurden auf korrekte Umlaute und Schreibweise vereinheitlicht

### Release Assets
- Private Release: Windows EXE + Sourcecode-Archiv
- Public Release: Windows EXE