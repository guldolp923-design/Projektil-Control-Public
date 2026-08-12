/**
 * PROJEKTIL Control - Shared Utilities
 * Centralized error handling, event management, and dev tools
 */

// ============================================================================
// 1. ENVIRONMENT CONFIG
// ============================================================================
const IS_DEV = !window.location.href.includes('file://') || localStorage.getItem('DEV_MODE') === 'true';
const IS_PRODUCTION = !IS_DEV;

// ============================================================================
// 2. LOGGING SYSTEM (Dev-only)
// ============================================================================
window.logIfDev = function(level, message, data) {
  if (IS_DEV) {
    const timestamp = new Date().toLocaleTimeString();
    const prefix = `[${timestamp}] [${level.toUpperCase()}]`;
    const isDev = IS_DEV ? ' [DEV]' : '';
    
    switch(level.toLowerCase()) {
      case 'info':
        console.info(`${prefix}${isDev} ${message}`, data ?? '');
        break;
      case 'warn':
        console.warn(`${prefix}${isDev} ${message}`, data ?? '');
        break;
      case 'error':
        console.error(`${prefix}${isDev} ${message}`, data ?? '');
        break;
      case 'debug':
        console.debug(`${prefix}${isDev} ${message}`, data ?? '');
        break;
      default:
        console.log(`${prefix}${isDev} ${message}`, data ?? '');
    }
  }
};

// ============================================================================
// 3. ERROR HANDLING & LOGGING
// ============================================================================
window.logError = function(context, error, additionalData = {}) {
  const errorData = {
    context,
    message: error?.message || String(error),
    stack: error?.stack,
    timestamp: new Date().toISOString(),
    ...additionalData
  };
  
  logIfDev('error', `[${context}] ${error?.message || error}`, errorData);
  
  // Optional: Send to backend for persistent logging
  if (typeof addError === 'function') {
    addError(`${context}: ${error?.message || error}`);
  }
  
  return errorData;
};

// ============================================================================
// 4. INVOKE WRAPPER - Centralized Tauri/Remote IPC
// ============================================================================
window.invokeWithErrorHandling = async function(command, args = {}, options = {}) {
  const {
    fallbackValue = null,
    throwOnError = false,
    suppressLogging = false,
    timeout = 30000,
    retries = 0
  } = options;
  
  let lastError;
  
  // Check offline mode first
  if (window.offlineMode?.isEnabled && !window.offlineMode.isOnline) {
    const queued = window.offlineMode.queue(command, args);
    if (queued?.queued) {
      return fallbackValue;
    }
  }
  
  const commandId = Math.random().toString(36).substr(2, 9);
  const trackingId = window.timeoutWarnings?.startTracking(command, commandId);
  
  for (let attempt = 0; attempt <= retries; attempt++) {
    try {
      logIfDev('debug', `[Invoke] ${command}`, { args, attempt: attempt + 1, trackingId });
      
      // Call the original invoke function
      const result = await invoke(command, args);
      
      if (result === null && !suppressLogging) {
        logIfDev('warn', `[Invoke] ${command} returned null`, { args });
      }
      
      if (trackingId) {
        window.timeoutWarnings?.endTracking(trackingId);
      }
      
      return result;
    } catch (error) {
      lastError = error;
      
      if (attempt < retries) {
        logIfDev('warn', `[Invoke] ${command} failed, retrying (${attempt + 1}/${retries})`, { error: error.message });
        await delay(Math.pow(2, attempt) * 100); // Exponential backoff
      } else {
        logError('Invoke', error, { command, args, attempts: attempt + 1 });
        
        if (trackingId) {
          window.timeoutWarnings?.endTracking(trackingId);
        }
        
        if (throwOnError) throw error;
      }
    }
  }
  
  return fallbackValue;
};

/**
 * Global wrapper for Tauri command invocation
 * Automatically uses Tauri v2 API when available
 */
window.invokeCommand = async function(command, args = {}) {
  try {
    // Use Tauri v2 internals if available
    if (window.__TAURI_INTERNALS__) {
      const core = window.__TAURI_INTERNALS__.invoke;
      return await core(command, args);
    }
    
    // Fallback for testing or direct invoke
    return await invokeWithErrorHandling(command, args, { throwOnError: true });
  } catch (error) {
    logError('invokeCommand', error, { command, args });
    throw error;
  }
};

// ============================================================================
// 5. EVENT LISTENER MANAGEMENT - Centralized cleanup
// ============================================================================
const managedListeners = new Map();

window.createManagedEventListener = function(target, eventType, handler, options = {}) {
  const key = `${target === window ? 'window' : target?.id || 'unknown'}-${eventType}-${Math.random()}`;
  
  const wrappedHandler = function(e) {
    try {
      handler(e);
    } catch (error) {
      logError('EventListener', error, { eventType, target: target?.id });
    }
  };
  
  target.addEventListener(eventType, wrappedHandler, options);
  
  managedListeners.set(key, {
    target,
    eventType,
    handler: wrappedHandler,
    options
  });
  
  logIfDev('debug', `[Listener] Registered: ${eventType}`, { target: target?.id });
  
  // Return cleanup function
  return () => {
    target.removeEventListener(eventType, wrappedHandler, options);
    managedListeners.delete(key);
    logIfDev('debug', `[Listener] Removed: ${eventType}`, { target: target?.id });
  };
};

// Clean up all listeners on page unload
window.addEventListener('beforeunload', () => {
  for (const [key, listener] of managedListeners.entries()) {
    listener.target.removeEventListener(listener.eventType, listener.handler, listener.options);
  }
  managedListeners.clear();
  logIfDev('debug', '[Cleanup] All event listeners removed');
});

// ============================================================================
// 6. STORAGE CACHE - localStorage with memory cache + auto-sync
// ============================================================================
class StorageCache {
  constructor(namespace = 'app') {
    this.namespace = namespace;
    this.cache = new Map();
    this.subscriptions = new Map();
    this.loadFromStorage();
  }
  
  key(k) {
    return `${this.namespace}:${k}`;
  }
  
  get(k, defaultValue = null) {
    if (this.cache.has(k)) {
      return this.cache.get(k);
    }
    
    try {
      const stored = localStorage.getItem(this.key(k));
      const value = stored ? JSON.parse(stored) : defaultValue;
      this.cache.set(k, value);
      return value;
    } catch (error) {
      logError('StorageCache.get', error, { key: k });
      return defaultValue;
    }
  }
  
  set(k, v) {
    this.cache.set(k, v);
    
    try {
      if (v === null) {
        localStorage.removeItem(this.key(k));
      } else {
        localStorage.setItem(this.key(k), JSON.stringify(v));
      }
      logIfDev('debug', `[Storage] Set: ${k}`, { value: v });
      
      // Notify subscribers
      if (this.subscriptions.has(k)) {
        this.subscriptions.get(k).forEach(cb => cb(v));
      }
    } catch (error) {
      logError('StorageCache.set', error, { key: k });
    }
  }
  
  subscribe(k, callback) {
    if (!this.subscriptions.has(k)) {
      this.subscriptions.set(k, new Set());
    }
    this.subscriptions.get(k).add(callback);
    
    return () => {
      this.subscriptions.get(k).delete(callback);
    };
  }
  
  loadFromStorage() {
    try {
      for (let i = 0; i < localStorage.length; i++) {
        const key = localStorage.key(i);
        if (key?.startsWith(this.namespace + ':')) {
          const cacheKey = key.substring(this.namespace.length + 1);
          const value = JSON.parse(localStorage.getItem(key));
          this.cache.set(cacheKey, value);
        }
      }
      logIfDev('debug', `[Storage] Loaded ${this.cache.size} items`);
    } catch (error) {
      logError('StorageCache.loadFromStorage', error);
    }
  }
}

window.appStorage = new StorageCache('projektil');

// ============================================================================
// 7. ERROR BOUNDARY - Global error handler
// ============================================================================
window.addEventListener('error', (event) => {
  logError('UncaughtError', event.error, {
    filename: event.filename,
    lineno: event.lineno,
    colno: event.colno
  });
});

window.addEventListener('unhandledrejection', (event) => {
  logError('UnhandledPromiseRejection', event.reason);
});

// ============================================================================
// 8. PERFORMANCE MONITORING (Dev only)
// ============================================================================
window.measurePerformance = function(name, fn) {
  if (!IS_DEV) return fn();
  
  const start = performance.now();
  const result = fn();
  
  if (result instanceof Promise) {
    return result.finally(() => {
      const duration = performance.now() - start;
      logIfDev('debug', `[Perf] ${name}: ${duration.toFixed(2)}ms`);
    });
  }
  
  const duration = performance.now() - start;
  logIfDev('debug', `[Perf] ${name}: ${duration.toFixed(2)}ms`);
  return result;
};

// ============================================================================
// 9. REQUEST DEDUPLICATION - Prevent duplicate network calls
// ============================================================================
const pendingRequests = new Map();

window.dedupRequest = async function(key, asyncFn) {
  if (pendingRequests.has(key)) {
    logIfDev('debug', `[Dedup] Using cached promise for: ${key}`);
    return pendingRequests.get(key);
  }
  
  const promise = asyncFn()
    .finally(() => pendingRequests.delete(key));
  
  pendingRequests.set(key, promise);
  return promise;
};

// ============================================================================
// 10. TAURI EVENT LISTENER WRAPPER - Better error handling for events
// ============================================================================
window.setupTauriEventListener = async function(eventName, handler, options = {}) {
  const {
    persistent = true,
    onError = null
  } = options;
  
  try {
    const core = window.__TAURI_INTERNALS__;
    if (!core?.invoke) {
      throw new Error('Tauri internals not available');
    }
    
    const wrappedHandler = (payload) => {
      try {
        logIfDev('debug', `[TauriEvent] Received: ${eventName}`, { payload });
        handler(payload);
      } catch (error) {
        logError('TauriEventHandler', error, { eventName });
        onError?.(error);
      }
    };
    
    const cbId = core.transformCallback(wrappedHandler, !persistent);
    await core.invoke('plugin:event|listen', {
      event: eventName,
      target: { kind: 'Any' },
      handler: cbId
    });
    
    logIfDev('debug', `[TauriEvent] Listener registered: ${eventName}`, { persistent });
    
    return () => {
      core.invoke('plugin:event|unlisten', { id: cbId }).catch(e => 
        logIfDev('warn', `Failed to unlisten: ${eventName}`, { error: e })
      );
    };
  } catch (error) {
    logError('setupTauriEventListener', error, { eventName });
    throw error;
  }
};

// ============================================================================
// 11. BATCH OPERATIONS - Group multiple async calls
// ============================================================================
window.batchInvoke = async function(commands) {
  logIfDev('debug', `[Batch] Starting ${commands.length} operations`);
  
  const results = await Promise.allSettled(
    commands.map(({ cmd, args }) => invoke(cmd, args))
  );
  
  const successful = results.filter(r => r.status === 'fulfilled').map(r => r.value);
  const failed = results.filter(r => r.status === 'rejected').map(r => r.reason);
  
  if (failed.length > 0) {
    logIfDev('warn', `[Batch] ${failed.length}/${commands.length} operations failed`, { failed });
  }
  
  return { successful, failed, total: commands.length };
};

// ============================================================================
// 12. UTILITY FUNCTIONS
// ============================================================================

/**
 * Safe JSON parse with fallback
 */
window.safeJsonParse = function(json, fallback = null) {
  try {
    return JSON.parse(json);
  } catch {
    return fallback;
  }
};

/**
 * Debounce function execution
 */
window.debounce = function(fn, delay) {
  let timeout;
  return function(...args) {
    clearTimeout(timeout);
    timeout = setTimeout(() => fn(...args), delay);
  };
};

/**
 * Throttle function execution
 */
window.throttle = function(fn, limit) {
  let inThrottle;
  return function(...args) {
    if (!inThrottle) {
      fn(...args);
      inThrottle = true;
      setTimeout(() => inThrottle = false, limit);
    }
  };
};

/**
 * Safe DOM query with null checks
 */
window.safeQuerySelector = function(selector, parent = document) {
  try {
    return parent.querySelector(selector) || null;
  } catch (e) {
    logError('safeQuerySelector', e, { selector });
    return null;
  }
};

/**
 * Delay helper
 */
window.delay = function(ms) {
  return new Promise(r => setTimeout(r, ms));
};

// ============================================================================
// 12. OFFLINE MODE - Queue actions when offline, sync when online
// ============================================================================
window.offlineMode = {
  isEnabled: false,
  isOnline: navigator.onLine,
  pendingActions: [],
  
  init() {
    // Check offline mode from backend
    if (window.__TAURI_INTERNALS__) {
      window.invokeCommand('get_offline_mode_enabled').then(enabled => {
        this.isEnabled = enabled;
        logIfDev('info', 'Offline mode', { enabled: this.isEnabled });
      }).catch(e => logIfDev('warn', 'Failed to get offline mode', { error: e }));
    }
    
    // Listen for online/offline events
    window.addEventListener('online', () => {
      this.isOnline = true;
      logIfDev('info', 'App is ONLINE', {});
      this.syncPendingActions();
    });
    
    window.addEventListener('offline', () => {
      this.isOnline = false;
      logIfDev('warn', 'App is OFFLINE', {});
    });
  },
  
  async queue(commandName, args) {
    if (!this.isOnline) {
      this.pendingActions.push({ commandName, args, timestamp: Date.now() });
      logIfDev('debug', 'Action queued for offline', { command: commandName, count: this.pendingActions.length });
      
      // Persist to localStorage
      try {
        const key = 'offline_pending_actions';
        const existing = JSON.parse(localStorage.getItem(key) || '[]');
        existing.push({ commandName, args, timestamp: Date.now() });
        localStorage.setItem(key, JSON.stringify(existing.slice(-100))); // Keep last 100
      } catch (e) {
        logIfDev('warn', 'Failed to persist offline action', { error: e });
      }
      
      return { queued: true, online: false };
    }
    return null;
  },
  
  async syncPendingActions() {
    if (!this.isOnline) return;
    logIfDev('info', 'Syncing pending actions', { count: this.pendingActions.length });
    
    const toSync = [...this.pendingActions];
    this.pendingActions = [];
    
    // Also check localStorage
    try {
      const key = 'offline_pending_actions';
      const stored = JSON.parse(localStorage.getItem(key) || '[]');
      toSync.push(...stored);
      localStorage.removeItem(key);
    } catch (e) {
      logIfDev('warn', 'Failed to load offline actions from storage', { error: e });
    }
    
    for (const action of toSync) {
      try {
        const result = await window.invokeCommand(action.commandName, action.args);
        logIfDev('debug', 'Synced action', { command: action.commandName, success: true });
      } catch (e) {
        logIfDev('warn', 'Failed to sync action', { command: action.commandName, error: e });
      }
    }
  }
};

// ============================================================================
// 13. TIMEOUT WARNINGS - Show warning when command takes >30s
// ============================================================================
window.timeoutWarnings = {
  activeCommands: new Map(),
  timeoutMs: 30000, // 30 seconds
  
  startTracking(commandName, commandId) {
    const key = `${commandName}:${commandId}`;
    
    const timeoutId = setTimeout(() => {
      logIfDev('warn', 'Command timeout warning', { command: commandName, duration: '30s' });
      
      // Show warning dialog to user
      const warning = document.createElement('div');
      warning.className = 'timeout-warning';
      warning.innerHTML = `
        <div style="background: #ff6b6b; color: white; padding: 12px; border-radius: 4px; margin: 8px 0; font-size: 12px;">
          ⏱️ <strong>${commandName}</strong> läuft seit 30+ Sekunden...
          <br/><small>Das Gerät könnte nicht erreichbar sein. Bitte warten oder abbrechen.</small>
        </div>
      `;
      
      const container = document.getElementById('device-monitor') || document.body;
      container.appendChild(warning);
      
      // Auto-remove after 10 seconds
      setTimeout(() => warning.remove(), 10000);
    }, this.timeoutMs);
    
    this.activeCommands.set(key, { timeoutId, startTime: Date.now() });
    return key;
  },
  
  endTracking(commandId) {
    if (this.activeCommands.has(commandId)) {
      const { timeoutId, startTime } = this.activeCommands.get(commandId);
      clearTimeout(timeoutId);
      const duration = Date.now() - startTime;
      
      if (duration > 5000) {
        logIfDev('debug', 'Slow command', { commandId, duration: `${(duration/1000).toFixed(1)}s` });
      }
      
      this.activeCommands.delete(commandId);
    }
  }
};

// ============================================================================
// 14. DEVICE HEALTH MONITOR - Check device online status
// ============================================================================
window.deviceHealthMonitor = {
  health: new Map(),
  updateInterval: 60000, // 60 seconds
  
  async init() {
    this.updateStatus();
    setInterval(() => this.updateStatus(), this.updateInterval);
  },
  
  async updateStatus() {
    if (!window.__TAURI_INTERNALS__) return;
    
    try {
      const health = await window.invokeCommand('get_device_health_status');
      health.forEach(device => {
        this.health.set(device.device_id, device);
        logIfDev('debug', 'Device health', { device: device.device_id, online: device.is_online, failures: device.consecutive_failures });
      });
    } catch (e) {
      logIfDev('warn', 'Failed to get device health', { error: e });
    }
  },
  
  isOnline(deviceId) {
    const status = this.health.get(deviceId);
    return status?.is_online ?? true; // Assume online if unknown
  },
  
  getHealth(deviceId) {
    return this.health.get(deviceId) || null;
  }
};

// ============================================================================
// INITIALIZATION
// ============================================================================
logIfDev('info', 'Utils.js loaded', { 
  environment: IS_DEV ? 'DEV' : 'PRODUCTION',
  timestamp: new Date().toISOString()
});

// Initialize offline mode and device health monitor
if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', () => {
    window.offlineMode.init();
    window.deviceHealthMonitor.init();
  });
} else {
  window.offlineMode.init();
  window.deviceHealthMonitor.init();
}
