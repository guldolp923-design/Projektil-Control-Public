// tauri-bridge.js
// Verbindet das HTML-Frontend mit dem Rust-Backend via Tauri invoke()
// Wird in index.html eingebunden BEVOR projektil.js

import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { getCurrentWindow } from '@tauri-apps/api/window';

// ============================================================
// WINDOW CONTROLS (custom titlebar)
// ============================================================
export async function minimizeWindow() {
    await invoke('minimize_window');
}

export async function toggleFullscreen() {
    await invoke('toggle_fullscreen');
}

export async function hideToTray() {
    await invoke('hide_to_tray');
}

export async function quitApp() {
    await invoke('quit_app');
}

// ============================================================
// CONFIG
// ============================================================
let _config = null;

export async function getConfig() {
    if (!_config) {
        _config = await invoke('get_config');
    }
    return _config;
}

// ============================================================
// D40 OCA COMMANDS
// ============================================================

/**
 * OCA Command an einen D40 senden
 * @param {string} ip - z.B. "192.168.1.51"
 * @param {string} command - "mute_A", "unmute_B", "mute_all", "preset_1" etc.
 */
export async function d40Command(ip, command) {
    try {
        const result = await invoke('d40_command', { ip, command });
        console.log('[OCA]', result);
        return { ok: true, result };
    } catch (err) {
        console.error('[OCA] Fehler:', err);
        return { ok: false, error: err };
    }
}

/**
 * D40 Verbindung pruefen
 * @param {string} ip
 * @returns {Promise<boolean>}
 */
export async function d40Ping(ip) {
    try {
        return await invoke('d40_ping', { ip });
    } catch {
        return false;
    }
}

// ============================================================
// STATUS POLLING — alle Geraete regelmaessig pingen
// ============================================================
const STATUS = {
    d40_01: false,
    d40_02: false,
    pixera: false,
};

async function pollStatus() {
    const cfg = await getConfig();

    // D40 via OCA TCP Ping
    STATUS.d40_01 = await d40Ping(cfg.d40_01_ip);
    STATUS.d40_02 = await d40Ping(cfg.d40_02_ip);

    // Pixera via HTTP
    try {
        const r = await fetch(`http://${cfg.pixera_ip}:${cfg.pixera_port}/`, {
            signal: AbortSignal.timeout(1000)
        });
        STATUS.pixera = r.ok;
    } catch {
        STATUS.pixera = false;
    }

    // Status-Update ans UI schicken
    window.dispatchEvent(new CustomEvent('device-status', { detail: STATUS }));
}

// ============================================================
// TRAY EVENTS — vom Rust-Backend empfangen
// ============================================================
export async function initTrayListeners() {
    // Tray: Alle Projektoren Mute
    await listen('tray-mute-all', () => {
        window.dispatchEvent(new CustomEvent('action-mute-all-projectors'));
    });

    // Tray: PowerAll
    await listen('tray-power-all', () => {
        window.dispatchEvent(new CustomEvent('action-power-all'));
    });

    // Tray: Emergency
    await listen('tray-emergency', () => {
        window.dispatchEvent(new CustomEvent('action-emergency'));
        // Tab wechseln zu Emergency
        if (typeof switchTab === 'function') switchTab('emergency');
    });
}

// ============================================================
// INIT
// ============================================================
export async function initTauriApp() {
    // Config laden
    const cfg = await getConfig();
    console.log('[PROJEKTIL] Config geladen:', cfg);

    // Tray-Listeners starten
    await initTrayListeners();

    // Status-Polling starten (alle 5 Sekunden)
    await pollStatus();
    setInterval(pollStatus, 5000);

    // Custom Titlebar-Buttons verdrahten
    document.getElementById('btn-minimize')?.addEventListener('click', minimizeWindow);
    document.getElementById('btn-fullscreen')?.addEventListener('click', toggleFullscreen);
    document.getElementById('btn-close')?.addEventListener('click', hideToTray);
    document.getElementById('btn-quit')?.addEventListener('click', () => {
        if (confirm('PROJEKTIL Control wirklich beenden?')) quitApp();
    });

    console.log('[PROJEKTIL] Tauri App bereit');
    return cfg;
}

// Auto-Init wenn Seite geladen
window.addEventListener('DOMContentLoaded', () => {
    initTauriApp().catch(console.error);
});
