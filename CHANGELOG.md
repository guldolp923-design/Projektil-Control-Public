# Changelog

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