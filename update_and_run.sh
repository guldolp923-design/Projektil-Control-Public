#!/bin/bash
# ============================================================
# PROJEKTIL — Update & Dev Runner
# Verwendung: ./update_and_run.sh
# Legt Downloads automatisch in den richtigen Ordner
# ============================================================

PROJECT_DIR="$HOME/Documents/UI/projektil-tauri"
SRC_DIR="$PROJECT_DIR/src-tauri/src"
FRONTEND_DIR="$PROJECT_DIR/frontend"
DOWNLOADS="$HOME/Downloads"

echo "🔄 PROJEKTIL Update & Dev Runner"
echo "================================="

# --- main.rs ---
if [ -f "$DOWNLOADS/main.rs" ]; then
    cp "$DOWNLOADS/main.rs" "$SRC_DIR/main.rs"
    echo "✅ main.rs kopiert → $SRC_DIR/main.rs"
    rm "$DOWNLOADS/main.rs"
else
    echo "⚠️  main.rs nicht in Downloads gefunden — übersprungen"
fi

# --- index.html ---
if [ -f "$DOWNLOADS/index.html" ]; then
    cp "$DOWNLOADS/index.html" "$FRONTEND_DIR/index.html"
    echo "✅ index.html kopiert → $FRONTEND_DIR/index.html"
    rm "$DOWNLOADS/index.html"
else
    echo "⚠️  index.html nicht in Downloads gefunden — übersprungen"
fi

# --- Beliebige .rs Dateien ---
for f in "$DOWNLOADS"/*.rs; do
    [ -f "$f" ] || continue
    fname=$(basename "$f")
    cp "$f" "$SRC_DIR/$fname"
    echo "✅ $fname kopiert → $SRC_DIR/$fname"
    rm "$f"
done

echo ""
echo "🚀 Starte tauri dev..."
echo "================================="
cd "$PROJECT_DIR" && npm run dev
