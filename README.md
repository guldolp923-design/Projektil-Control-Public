# PROJEKTIL Control — Tauri App

## Projektstruktur

```
projektil-tauri/
├── frontend/
│   ├── index.html          ← Komplettes UI (HTML/CSS/JS)
│   └── js/
│       └── tauri-bridge.js ← Tauri API Bridge (optional, fuer Module)
├── src-tauri/
│   ├── src/
│   │   ├── main.rs         ← App-Einstieg, Tray, Window-Management
│   │   └── oca.rs          ← D40 AES70/OCA TCP Steuerung
│   ├── Cargo.toml          ← Rust Dependencies
│   └── tauri.conf.json     ← App-Konfiguration (Fenstergrösse etc.)
├── package.json
├── setup-and-build.bat     ← Windows Setup Script
└── README.md
```

---

## Voraussetzungen

### 1. Node.js installieren
https://nodejs.org (LTS Version)

### 2. Rust installieren
https://rustup.rs — einfach das Installer-Script ausführen

### 3. WebView2 (Windows 11 bereits vorinstalliert)
Falls Windows 10: https://developer.microsoft.com/microsoft-edge/webview2/

---

## Setup in 3 Schritten

```bash
# 1. Im Projektordner oeffnen
cd projektil-tauri

# 2. Dependencies installieren
npm install

# 3. Entwicklungsmodus starten (Hot-Reload)
npm run dev
```

Beim ersten Start dauert es 2-5 Minuten weil Rust alle Dependencies kompiliert.
Danach startet die App als natives Fenster.

---

## Fuer die Tour — Release Build

```bash
npm run build
```

Die fertige .exe liegt in:
```
src-tauri/target/release/projektil-control.exe
```

Die .exe ist standalone — keine zusätzliche Installation auf dem Laptop nötig.

---

## Autostart einrichten (Windows)

### Methode A: Setup-Script (empfohlen)
```
setup-and-build.bat als Administrator ausführen
```

### Methode B: Manuell
1. Win+R → `shell:startup`
2. Verknüpfung zur projektil-control.exe in den Startup-Ordner
3. Eigenschaften der Verknüpfung → "Minimiert starten" (startet im Tray)

---

## IP-Adressen konfigurieren

Die IP-Adressen können nun direkt in der Datei `config.json` im Hauptverzeichnis angepasst werden. 
Die App muss danach nicht neu gebaut werden, ein einfacher Neustart reicht aus. Falls die Datei fehlt, wird sie beim ersten Start automatisch mit Standardwerten erstellt.

---

## D40 OCA Commands

Verfügbare Commands für `d40_command`:
- `mute_A` / `unmute_A` — Kanal A
- `mute_B` / `unmute_B` — Kanal B
- `mute_C` / `unmute_C` — Kanal C
- `mute_D` / `unmute_D` — Kanal D
- `mute_all` / `unmute_all` — Alle 4 Kanäle
- `preset_1` / `preset_2` / `preset_3` — AmpPresets laden

---

## Tray-Icon Funktionen

Rechtsklick auf das PROJEKTIL Icon in der Taskleiste:
- PROJEKTIL öffnen
- Alle Projektoren Mute
- PowerAll (Warmup)
- ⚠ Emergency Stop
- Beenden

Doppelklick auf Tray-Icon: Fenster öffnen

---

## Entwicklung — Frontend ändern

Das HTML/CSS/JS in `frontend/index.html` kann direkt bearbeitet werden.
Im Dev-Modus (`npm run dev`) lädt die App bei Änderungen automatisch neu.

Für den Release-Build nach jeder Änderung: `npm run build`

---

## Troubleshooting

**App startet nicht:**
- Windows Defender kann .exe blockieren → "Trotzdem ausführen"
- Als Administrator starten beim ersten Start

**D40 nicht erreichbar:**
- IP-Adresse in get_config() prüfen
- Port 30013 TCP muss offen sein
- D40 muss im selben Netz sein

**Pixera-Verbindung schlägt fehl:**
- IP und Port 1338 prüfen
- Pixera muss laufen und Control-Modul aktiv sein
