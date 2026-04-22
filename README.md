# PROJEKTIL Control

<p align="center">
	<a href="#schnellstart"><img alt="Quick Start" src="https://img.shields.io/badge/Start-3%20Schritte-0a7ea4"></a>
	<img alt="Platform" src="https://img.shields.io/badge/Platform-Windows-1f6feb">
	<img alt="Runtime" src="https://img.shields.io/badge/Tauri-Desktop-green">
	<img alt="Status" src="https://img.shields.io/badge/Status-Beta-orange">
</p>

Desktop-Steuerungsapp für Showbetrieb auf Basis von Tauri.

PROJEKTIL Control bündelt zentrale Aktionen für Projektoren, D40 und Pixera in einer schnellen, robusten Windows-App mit Tray-Integration, Startup-Checks und Logging.

## Warum dieses Projekt?

- Zuverlässiger Start mit klaren Verbindungsprüfungen und verständlichen Fehlermeldungen
- Direkte Steuerung von D40-Befehlen (Mute/Unmute, Presets)
- Schnelle Live-Aktionen über das Tray-Menü
- Persistente Logs über Neustarts hinweg
- Konfiguration per `config.json` ohne Neubuild

## Inhaltsverzeichnis

- [Schnellstart](#schnellstart)
- [Screenshots und Demo](#screenshots-und-demo)
- [Projektstruktur](#projektstruktur)
- [Konfiguration](#konfiguration)
- [D40-Befehle](#d40-befehle)
- [Tray-Funktionen](#tray-funktionen)
- [Autostart unter Windows](#autostart-unter-windows)
- [Troubleshooting](#troubleshooting)

## Schnellstart

### Voraussetzungen

1. Node.js (LTS): https://nodejs.org
2. Rust (Toolchain): https://rustup.rs
3. WebView2 (unter Windows 10): https://developer.microsoft.com/microsoft-edge/webview2/

### Entwicklung starten

```bash
cd projektil-control
npm install
npm run dev
```

Hinweis: Der erste Start kann 2 bis 5 Minuten dauern, da Rust alle Abhängigkeiten kompiliert.

### Release-Build erstellen

```bash
npm run build
```

Die ausführbare Datei liegt anschließend unter:

```text
src-tauri/target/release/projektil-control.exe
```

## Screenshots und Demo

Hier kannst du die GitHub-Seite visuell aufwerten, sobald Material vorliegt:

```text
docs/images/app-overview.png
docs/images/startup-screen.png
```

Beispiel-Einbindung:

```markdown
![App Overview](docs/images/app-overview.png)
![Startup Screen](docs/images/startup-screen.png)
```

## Projektstruktur

```text
projektil-control/
├── frontend/
│   ├── index.html          ← Komplettes UI (HTML/CSS/JS)
│   └── js/
│       └── tauri-bridge.js ← Tauri-API-Bridge (optional für Module)
├── src-tauri/
│   ├── src/
│   │   ├── main.rs         ← App-Einstieg, Tray, Fenster-Management
│   │   └── oca.rs          ← D40 AES70/OCA-TCP-Steuerung
│   ├── Cargo.toml          ← Rust-Abhängigkeiten
│   └── tauri.conf.json     ← App-Konfiguration (Fenstergröße etc.)
├── config.json             ← Laufzeitkonfiguration (IP/Ports)
├── package.json
├── setup-and-build.bat     ← Windows-Setup-Skript
└── README.md
```

## Konfiguration

Die IP-Adressen können direkt in `config.json` im Hauptverzeichnis angepasst werden.

- Kein Neubuild nötig
- Ein Neustart der App reicht aus
- Falls die Datei fehlt, wird sie beim ersten Start automatisch mit Standardwerten erzeugt

## D40-Befehle

Verfügbare Werte für `d40_command`:

- `mute_A` / `unmute_A` für Kanal A
- `mute_B` / `unmute_B` für Kanal B
- `mute_C` / `unmute_C` für Kanal C
- `mute_D` / `unmute_D` für Kanal D
- `mute_all` / `unmute_all` für alle vier Kanäle
- `preset_1` / `preset_2` / `preset_3` zum Laden von Amp-Presets

## Tray-Funktionen

Per Rechtsklick auf das PROJEKTIL-Icon in der Taskleiste:

- PROJEKTIL öffnen
- Alle Projektoren Mute
- PowerAll (Warmup)
- Emergency Stop
- Beenden

Per Doppelklick auf das Tray-Icon:

- Fenster öffnen

## Autostart unter Windows

### Methode A (empfohlen)

`setup-and-build.bat` als Administrator ausführen.

### Methode B (manuell)

1. `Win + R` und `shell:startup` öffnen
2. Verknüpfung zur `projektil-control.exe` in den Startup-Ordner legen
3. In den Eigenschaften der Verknüpfung `Minimiert starten` aktivieren

## Troubleshooting

### App startet nicht

- Windows Defender kann eine EXE blockieren: `Trotzdem ausführen`
- Beim ersten Start einmal als Administrator ausführen

### D40 nicht erreichbar

- IP-Adresse in der Konfiguration prüfen
- TCP-Port `30013` muss erreichbar sein
- D40 muss im selben Netz sein

### Pixera-Verbindung schlägt fehl

- IP und Port `1338` prüfen
- Pixera muss laufen
- Control-Modul muss aktiv sein
