#!/usr/bin/env python3
"""
ModForge — Universal Game & Mod Studio
Native PyQt6 High-Performance Desktop Application
Communicates with high-speed Rust backend daemon on 127.0.0.1:19450
"""

import sys
import os
import re
import json
import time
import subprocess
import threading
import urllib.request
import urllib.error
from pathlib import Path

from PyQt6.QtCore import (
    Qt, QThread, pyqtSignal, QTimer, QSize, QPoint, QPropertyAnimation,
    QEasingCurve, QUrl
)
from PyQt6.QtWidgets import (
    QApplication, QMainWindow, QWidget, QVBoxLayout, QHBoxLayout,
    QLabel, QPushButton, QLineEdit, QListWidget, QListWidgetItem,
    QTabWidget, QTableWidget, QTableWidgetItem, QHeaderView, QFrame,
    QScrollArea, QSplitter, QMessageBox, QFileDialog, QProgressBar,
    QCheckBox, QComboBox, QMenu, QGraphicsDropShadowEffect, QSizePolicy,
    QDialog, QTextEdit
)
from PyQt6.QtGui import (
    QIcon, QPixmap, QColor, QFont, QCursor, QPainter, QLinearGradient,
    QBrush, QPen, QDesktopServices
)

# Configuration & Paths
DEFAULT_PORT = 19450
BASE_DIR = Path(__file__).resolve().parent
ICON_PATH = BASE_DIR / "src" / "icon.png"
if not ICON_PATH.exists():
    ICON_PATH = Path(os.path.expanduser("~/.local/share/icons/hicolor/256x256/apps/modforge.png"))
RELEASE_BIN = BASE_DIR / "target" / "release" / "modforge"
if not RELEASE_BIN.exists():
    RELEASE_BIN = BASE_DIR / "target" / "release" / "rig"

CYBER_STYLE = """
/* ModForge Cyberpunk Obsidian Palette */
* {
    outline: none;
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif;
}

QMainWindow, QWidget#CentralWidget {
    background-color: #07090e;
    color: #f1f5f9;
}

/* Scrollbars */
QScrollBar:vertical {
    background: #0d121c;
    width: 8px;
    margin: 0;
    border-radius: 4px;
}
QScrollBar::handle:vertical {
    background: #1e293b;
    min-height: 24px;
    border-radius: 4px;
}
QScrollBar::handle:vertical:hover {
    background: #00f2fe;
}
QScrollBar::add-line:vertical, QScrollBar::sub-line:vertical {
    height: 0px;
}
QScrollBar:horizontal {
    background: #0d121c;
    height: 8px;
    margin: 0;
    border-radius: 4px;
}
QScrollBar::handle:horizontal {
    background: #1e293b;
    min-width: 24px;
    border-radius: 4px;
}
QScrollBar::handle:horizontal:hover {
    background: #00f2fe;
}
QScrollBar::add-line:horizontal, QScrollBar::sub-line:horizontal {
    width: 0px;
}

/* Header Bar */
QFrame#TopHeader {
    background-color: #0b1018;
    border-bottom: 1px solid #1a2333;
    padding: 6px 16px;
}

QLabel#AppTitle {
    font-size: 20px;
    font-weight: 900;
    letter-spacing: 2px;
    color: #f8fafc;
}

QLabel#AppSubtitle {
    font-size: 11px;
    font-weight: 700;
    letter-spacing: 1.5px;
    color: #00f2fe;
}

QLabel#EnginePill {
    background: #0f231e;
    border: 1px solid #10b981;
    color: #34d399;
    padding: 4px 10px;
    border-radius: 12px;
    font-size: 11px;
    font-weight: bold;
}

/* Search Bars */
QLineEdit {
    background-color: #0f1521;
    border: 1px solid #1e293b;
    border-radius: 8px;
    padding: 8px 12px;
    color: #f8fafc;
    font-size: 13px;
    selection-background-color: #00f2fe;
    selection-color: #000000;
}
QLineEdit:focus {
    border: 1px solid #00f2fe;
    background-color: #131c2c;
}

/* Buttons */
QPushButton {
    background-color: #121927;
    border: 1px solid #233045;
    border-radius: 8px;
    padding: 8px 16px;
    color: #e2e8f0;
    font-size: 13px;
    font-weight: 600;
}
QPushButton:hover {
    background-color: #182338;
    border-color: #38bdf8;
    color: #ffffff;
}
QPushButton:pressed {
    background-color: #0e1522;
}

/* Neon Primary Action Buttons */
QPushButton#BtnPlay {
    background: qlineargradient(x1:0, y1:0, x2:1, y2:0, stop:0 #00f5a0, stop:1 #00d26a);
    border: 1px solid #059669;
    color: #022c22;
    font-weight: 800;
    font-size: 14px;
    border-radius: 8px;
    padding: 10px 22px;
}
QPushButton#BtnPlay:hover {
    background: qlineargradient(x1:0, y1:0, x2:1, y2:0, stop:0 #34d399, stop:1 #10b981);
    color: #000000;
    border-color: #6ee7b7;
}

QPushButton#BtnVanilla {
    background-color: #111827;
    border: 1px solid #334155;
    color: #cbd5e1;
    font-weight: 700;
    font-size: 13px;
    border-radius: 8px;
    padding: 10px 18px;
}
QPushButton#BtnVanilla:hover {
    background-color: #1e293b;
    border-color: #94a3b8;
    color: #ffffff;
}

QPushButton#BtnFix {
    background: qlineargradient(x1:0, y1:0, x2:1, y2:0, stop:0 #f59e0b, stop:1 #d97706);
    border: 1px solid #b45309;
    color: #451a03;
    font-weight: 800;
    font-size: 13px;
    border-radius: 8px;
    padding: 10px 18px;
}
QPushButton#BtnFix:hover {
    background: qlineargradient(x1:0, y1:0, x2:1, y2:0, stop:0 #fbbf24, stop:1 #f59e0b);
    color: #000000;
}

QPushButton#BtnStop {
    background-color: #7f1d1d;
    border: 1px solid #ef4444;
    color: #fecaca;
    font-weight: 800;
    border-radius: 8px;
    padding: 10px 18px;
}
QPushButton#BtnStop:hover {
    background-color: #991b1b;
    color: #ffffff;
}

/* Sidebar */
QFrame#SidebarFrame {
    background-color: #090d14;
    border-right: 1px solid #161f2e;
}

QListWidget#GameList {
    background-color: transparent;
    border: none;
    padding: 6px;
}
QListWidget#GameList::item {
    border-radius: 8px;
    margin-bottom: 4px;
    padding: 4px;
    background-color: transparent;
}
QListWidget#GameList::item:hover {
    background-color: #121927;
}
QListWidget#GameList::item:selected {
    background-color: #162235;
    border-left: 3px solid #00f2fe;
}

/* Tabs */
QTabWidget::pane {
    border: 1px solid #161f2e;
    background-color: #0a0e16;
    border-radius: 8px;
}
QTabBar::tab {
    background-color: #0d121c;
    border: 1px solid #182233;
    border-bottom: none;
    border-top-left-radius: 8px;
    border-top-right-radius: 8px;
    color: #94a3b8;
    padding: 9px 18px;
    font-weight: 700;
    font-size: 13px;
    margin-right: 4px;
}
QTabBar::tab:selected {
    background-color: #0a0e16;
    border-top: 2px solid #00f2fe;
    color: #00f2fe;
}
QTabBar::tab:hover:!selected {
    background-color: #131a28;
    color: #f1f5f9;
}

/* Cards & Frames */
QFrame.CardFrame {
    background-color: #0f1521;
    border: 1px solid #1c2738;
    border-radius: 10px;
    padding: 12px;
}
QFrame.CardFrame:hover {
    border-color: #2b3b55;
    background-color: #121927;
}

QFrame.IssueCardBad {
    background-color: #1a0f14;
    border: 1px solid #ef4444;
    border-radius: 8px;
    padding: 12px;
}
QFrame.IssueCardWarn {
    background-color: #1c150c;
    border: 1px solid #f59e0b;
    border-radius: 8px;
    padding: 12px;
}

/* Chips / Badges */
QLabel.Chip {
    border-radius: 6px;
    padding: 3px 8px;
    font-size: 11px;
    font-weight: bold;
}
QLabel.ChipEngine {
    background-color: #132238;
    color: #38bdf8;
    border: 1px solid #0284c7;
}
QLabel.ChipMods {
    background-color: #1e1b4b;
    color: #c084fc;
    border: 1px solid #7e22ce;
}
QLabel.ChipNexus {
    background-color: #271438;
    color: #d8b4fe;
    border: 1px solid #9333ea;
}
QLabel.ChipStore {
    background-color: #261f0c;
    color: #fde047;
    border: 1px solid #ca8a04;
}
QLabel.ChipModio {
    background-color: #06281e;
    color: #4ade80;
    border: 1px solid #16a34a;
}
"""

class ModForgeAPI:
    """Handles REST communications with the local ModForge Rust engine daemon."""
    def __init__(self, port=DEFAULT_PORT):
        self.port = port
        self.host = f"127.0.0.1:{port}"
        self.token = ""
        self.base_url = f"http://{self.host}"

    def ensure_daemon_running(self):
        """Checks if daemon responds; if not, launches binary in background."""
        if self.ping():
            self.fetch_token()
            return True

        # Spawn headless backend
        exe = str(RELEASE_BIN)
        if not os.path.exists(exe):
            exe = str(BASE_DIR / "target" / "release" / "rig")
        
        env = os.environ.copy()
        env["RIG_NO_WINDOW"] = "1"
        try:
            subprocess.Popen([exe], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
        except Exception as e:
            print(f"Error launching engine daemon: {e}")
            return False

        # Wait up to 5 seconds
        for _ in range(50):
            time.sleep(0.1)
            if self.ping():
                self.fetch_token()
                return True
        return False

    def ping(self):
        try:
            req = urllib.request.Request(self.base_url, headers={"Connection": "close"})
            with urllib.request.urlopen(req, timeout=1.0) as res:
                return res.status == 200
        except Exception:
            return False

    def fetch_token(self):
        try:
            req = urllib.request.Request(self.base_url, headers={"Connection": "close"})
            with urllib.request.urlopen(req, timeout=1.5) as res:
                html = res.read().decode("utf-8", errors="ignore")
                m = re.search(r'RIG_TOKEN\s*=\s*"([^"]+)"', html)
                if m:
                    self.token = m.group(1)
                    return self.token
        except Exception as e:
            print(f"Token fetch error: {e}")
        return ""

    def request(self, method, endpoint, payload=None):
        if not self.token:
            self.fetch_token()

        url = f"{self.base_url}{endpoint}"
        data = None
        headers = {
            "Host": self.host,
            "X-Rig-Token": self.token,
            "Connection": "close"
        }
        if payload is not None:
            data = json.dumps(payload).encode("utf-8")
            headers["Content-Type"] = "application/json"

        req = urllib.request.Request(url, data=data, headers=headers, method=method)
        try:
            with urllib.request.urlopen(req, timeout=8.0) as resp:
                raw = resp.read()
                if not raw:
                    return {}
                return json.loads(raw.decode("utf-8", errors="ignore"))
        except urllib.error.HTTPError as e:
            if e.code == 403:
                # Token might have refreshed
                self.fetch_token()
            return {"error": f"HTTP {e.code}: {e.reason}"}
        except Exception as e:
            return {"error": str(e)}

    # High-level API bindings
    def get_state(self):
        return self.request("GET", "/api/state")

    def get_doctor_all(self):
        return self.request("GET", "/api/doctor_all")

    def select_game(self, index):
        return self.request("POST", "/api/select", {"index": index})

    def play_game(self, mode="mods"):
        if mode == "vanilla":
            return self.request("POST", "/api/play_vanilla", {})
        elif mode == "fixed":
            return self.request("POST", "/api/play_fixed", {"fixes": ["setup"]})
        else:
            return self.request("POST", "/api/play", {})

    def stop_game(self):
        return self.request("POST", "/api/stop", {})

    def self_heal(self):
        return self.request("POST", "/api/self_heal", {})

    def toggle_mod(self, mod_id, enable):
        return self.request("POST", "/api/lib_toggle", {"id": mod_id, "enable": enable})

    def delete_mods(self, mod_ids):
        return self.request("POST", "/api/lib_remove_many", {"ids": mod_ids})

    def restore_mod_trash(self, folder_dir):
        return self.request("POST", "/api/lib_restore", {"dir": folder_dir})

    def get_store(self, source="all", query="", sort="trending"):
        return self.request("POST", "/api/store", {
            "source": source,
            "query": query,
            "sort": sort
        })

    def install_thunder_mod(self, package_id):
        return self.request("POST", "/api/thunder_install", {"id": package_id})

    def install_modio_mod(self, mod_id):
        return self.request("POST", "/api/modio_install", {"mod_id": mod_id})

    def backup_saves(self):
        return self.request("POST", "/api/backup_saves", {})

    def clear_shaders(self):
        return self.request("POST", "/api/clear_shaders", {})

    def upload_mod_archive(self, file_path):
        """Streams a local zip/7z/rar archive to /api/drop_upload."""
        if not self.token:
            self.fetch_token()
        fname = os.path.basename(file_path)
        url = f"{self.base_url}/api/drop_upload?name={urllib.parse.quote(fname)}"
        with open(file_path, "rb") as f:
            data = f.read()
        req = urllib.request.Request(
            url,
            data=data,
            headers={
                "Host": self.host,
                "X-Rig-Token": self.token,
                "Content-Type": "application/octet-stream",
                "Connection": "close"
            },
            method="POST"
        )
        try:
            with urllib.request.urlopen(req, timeout=30.0) as resp:
                return json.loads(resp.read().decode("utf-8", errors="ignore"))
        except Exception as e:
            return {"error": str(e)}


class AsyncWorker(QThread):
    """Executes a function in the background and signals result/error."""
    finished = pyqtSignal(object)
    error = pyqtSignal(str)

    def __init__(self, func, *args, **kwargs):
        super().__init__()
        self.func = func
        self.args = args
        self.kwargs = kwargs

    def run(self):
        try:
            res = self.func(*self.args, **self.kwargs)
            self.finished.emit(res)
        except Exception as e:
            self.error.emit(str(e))


class ModForgeApp(QMainWindow):
    """Primary ModForge GUI window."""
    def __init__(self):
        super().__init__()
        self.api = ModForgeAPI()
        self.current_state = {}
        self.doctor_data = {}
        self.selected_game_idx = 0
        self.selected_game = None
        self.games_list = []
        self.active_workers = []

        self.init_window()
        self.init_ui()
        self.connect_signals()

        # Connect & Initial Load
        self.startup_sequence()

    def init_window(self):
        self.setWindowTitle("ModForge — Universal Game & Mod Studio")
        self.setMinimumSize(1150, 780)
        self.resize(1320, 850)
        if ICON_PATH.exists():
            self.setWindowIcon(QIcon(str(ICON_PATH)))
        self.setStyleSheet(CYBER_STYLE)

    def init_ui(self):
        central = QWidget(self)
        central.setObjectName("CentralWidget")
        self.setCentralWidget(central)
        root_layout = QVBoxLayout(central)
        root_layout.setContentsMargins(0, 0, 0, 0)
        root_layout.setSpacing(0)

        # 1. Top Header Bar
        top_header = QFrame(self)
        top_header.setObjectName("TopHeader")
        th_layout = QHBoxLayout(top_header)
        th_layout.setContentsMargins(16, 10, 16, 10)
        th_layout.setSpacing(16)

        # Brand Logo + Titles
        brand_box = QHBoxLayout()
        brand_box.setSpacing(12)
        if ICON_PATH.exists():
            logo_lbl = QLabel()
            pix = QPixmap(str(ICON_PATH)).scaled(34, 34, Qt.AspectRatioMode.KeepAspectRatio, Qt.TransformationMode.SmoothTransformation)
            logo_lbl.setPixmap(pix)
            brand_box.addWidget(logo_lbl)

        title_col = QVBoxLayout()
        title_col.setSpacing(1)
        lbl_title = QLabel("MODFORGE")
        lbl_title.setObjectName("AppTitle")
        lbl_subtitle = QLabel("UNIVERSAL GAME & MOD STUDIO")
        lbl_subtitle.setObjectName("AppSubtitle")
        title_col.addWidget(lbl_title)
        title_col.addWidget(lbl_subtitle)
        brand_box.addLayout(title_col)
        th_layout.addLayout(brand_box)

        th_layout.addSpacing(24)

        # Global Search
        self.txt_global_search = QLineEdit()
        self.txt_global_search.setPlaceholderText("🔍  Search games or mods across all libraries...")
        self.txt_global_search.setFixedWidth(360)
        self.txt_global_search.textChanged.connect(self.on_filter_games)
        th_layout.addWidget(self.txt_global_search)

        th_layout.addStretch()

        # Engine Status Pill
        self.lbl_engine_status = QLabel("●  CONNECTING...")
        self.lbl_engine_status.setObjectName("EnginePill")
        th_layout.addWidget(self.lbl_engine_status)

        # Quick Refresh Button
        self.btn_refresh = QPushButton("↻ Refresh")
        self.btn_refresh.setCursor(Qt.CursorShape.PointingHandCursor)
        self.btn_refresh.clicked.connect(self.refresh_state)
        th_layout.addWidget(self.btn_refresh)

        root_layout.addWidget(top_header)

        # 2. Main Content Splitter (Sidebar + Workspace)
        splitter = QSplitter(Qt.Orientation.Horizontal)
        splitter.setHandleWidth(1)

        # ──────────────── Left Sidebar (Game Library) ────────────────
        sidebar_frame = QFrame()
        sidebar_frame.setObjectName("SidebarFrame")
        sidebar_frame.setFixedWidth(320)
        sb_layout = QVBoxLayout(sidebar_frame)
        sb_layout.setContentsMargins(12, 14, 12, 14)
        sb_layout.setSpacing(10)

        sb_header = QHBoxLayout()
        lbl_lib_title = QLabel("LIBRARY")
        lbl_lib_title.setStyleSheet("font-size: 11px; font-weight: 800; color: #64748b; letter-spacing: 1px;")
        self.lbl_game_count = QLabel("0 games")
        self.lbl_game_count.setStyleSheet("font-size: 11px; font-weight: 700; color: #38bdf8;")
        sb_header.addWidget(lbl_lib_title)
        sb_header.addStretch()
        sb_header.addWidget(self.lbl_game_count)
        sb_layout.addLayout(sb_header)

        # Filter Chips Bar
        filters_box = QHBoxLayout()
        filters_box.setSpacing(4)
        self.btn_filter_all = QPushButton("All")
        self.btn_filter_installed = QPushButton("Installed")
        self.btn_filter_modded = QPushButton("Modded")
        self.btn_filter_issues = QPushButton("Issues")
        for b in [self.btn_filter_all, self.btn_filter_installed, self.btn_filter_modded, self.btn_filter_issues]:
            b.setCheckable(True)
            b.setStyleSheet("padding: 4px 8px; font-size: 11px; border-radius: 6px;")
            b.setCursor(Qt.CursorShape.PointingHandCursor)
            filters_box.addWidget(b)
        self.btn_filter_installed.setChecked(True)
        self.btn_filter_all.clicked.connect(lambda: self.set_filter_mode("all"))
        self.btn_filter_installed.clicked.connect(lambda: self.set_filter_mode("installed"))
        self.btn_filter_modded.clicked.connect(lambda: self.set_filter_mode("modded"))
        self.btn_filter_issues.clicked.connect(lambda: self.set_filter_mode("issues"))
        sb_layout.addLayout(filters_box)

        # Game List
        self.game_list_widget = QListWidget()
        self.game_list_widget.setObjectName("GameList")
        self.game_list_widget.setVerticalScrollMode(QListWidget.ScrollMode.ScrollPerPixel)
        self.game_list_widget.currentRowChanged.connect(self.on_game_selected)
        sb_layout.addWidget(self.game_list_widget)

        # Add Custom Game Button
        self.btn_add_game = QPushButton("+ Add Custom Game")
        self.btn_add_game.setStyleSheet("background-color: #121927; border: 1px dashed #334155; padding: 10px; font-weight: bold; border-radius: 8px;")
        self.btn_add_game.setCursor(Qt.CursorShape.PointingHandCursor)
        self.btn_add_game.clicked.connect(self.on_add_game_dialog)
        sb_layout.addWidget(self.btn_add_game)

        splitter.addWidget(sidebar_frame)

        # ──────────────── Right Workspace ────────────────
        workspace_frame = QFrame()
        ws_layout = QVBoxLayout(workspace_frame)
        ws_layout.setContentsMargins(20, 18, 20, 18)
        ws_layout.setSpacing(16)

        # 2.1 Hero Action Banner
        self.hero_frame = QFrame()
        self.hero_frame.setStyleSheet("""
            QFrame {
                background: qlineargradient(x1:0, y1:0, x2:1, y2:0, stop:0 #0f1624, stop:1 #0c121e);
                border: 1px solid #1c273c;
                border-radius: 12px;
                padding: 16px;
            }
        """)
        hf_layout = QHBoxLayout(self.hero_frame)
        hf_layout.setSpacing(20)

        # Left Info
        info_col = QVBoxLayout()
        info_col.setSpacing(6)
        self.lbl_selected_title = QLabel("Select a game from library")
        self.lbl_selected_title.setStyleSheet("font-size: 24px; font-weight: 900; color: #f8fafc;")
        
        meta_row = QHBoxLayout()
        meta_row.setSpacing(10)
        self.lbl_hero_engine = QLabel("Standard Engine")
        self.lbl_hero_engine.setProperty("class", "Chip ChipEngine")
        self.lbl_hero_mods = QLabel("0 Mods Active")
        self.lbl_hero_mods.setProperty("class", "Chip ChipMods")
        self.lbl_hero_runtime = QLabel("Compatibility: Native / Proton / Wine")
        self.lbl_hero_runtime.setStyleSheet("color: #94a3b8; font-size: 12px; font-weight: 500;")
        meta_row.addWidget(self.lbl_hero_engine)
        meta_row.addWidget(self.lbl_hero_mods)
        meta_row.addWidget(self.lbl_hero_runtime)
        meta_row.addStretch()

        self.lbl_hero_path = QLabel("/path/to/game")
        self.lbl_hero_path.setStyleSheet("font-size: 11px; color: #64748b; font-family: monospace;")

        info_col.addWidget(self.lbl_selected_title)
        info_col.addLayout(meta_row)
        info_col.addWidget(self.lbl_hero_path)
        hf_layout.addLayout(info_col)

        hf_layout.addStretch()

        # Right Action Buttons
        actions_col = QVBoxLayout()
        actions_col.setSpacing(8)
        
        primary_btn_row = QHBoxLayout()
        primary_btn_row.setSpacing(10)
        self.btn_play_mods = QPushButton("▶ PLAY WITH MODS")
        self.btn_play_mods.setObjectName("BtnPlay")
        self.btn_play_mods.setCursor(Qt.CursorShape.PointingHandCursor)
        self.btn_play_mods.clicked.connect(lambda: self.launch_game("mods"))

        self.btn_play_vanilla = QPushButton("▶ VANILLA")
        self.btn_play_vanilla.setObjectName("BtnVanilla")
        self.btn_play_vanilla.setCursor(Qt.CursorShape.PointingHandCursor)
        self.btn_play_vanilla.clicked.connect(lambda: self.launch_game("vanilla"))

        self.btn_fix_play = QPushButton("⚡ FIX && PLAY")
        self.btn_fix_play.setObjectName("BtnFix")
        self.btn_fix_play.setCursor(Qt.CursorShape.PointingHandCursor)
        self.btn_fix_play.clicked.connect(lambda: self.launch_game("fixed"))

        self.btn_stop_game = QPushButton("⏹ STOP")
        self.btn_stop_game.setObjectName("BtnStop")
        self.btn_stop_game.setCursor(Qt.CursorShape.PointingHandCursor)
        self.btn_stop_game.clicked.connect(self.stop_game)
        self.btn_stop_game.setVisible(False)

        primary_btn_row.addWidget(self.btn_play_mods)
        primary_btn_row.addWidget(self.btn_play_vanilla)
        primary_btn_row.addWidget(self.btn_fix_play)
        primary_btn_row.addWidget(self.btn_stop_game)
        actions_col.addLayout(primary_btn_row)

        secondary_btn_row = QHBoxLayout()
        secondary_btn_row.setSpacing(8)
        self.btn_open_folder = QPushButton("📁 Game Folder")
        self.btn_open_folder.setCursor(Qt.CursorShape.PointingHandCursor)
        self.btn_open_folder.clicked.connect(self.open_game_folder)

        self.btn_clear_shader = QPushButton("🧹 Clear Shaders")
        self.btn_clear_shader.setCursor(Qt.CursorShape.PointingHandCursor)
        self.btn_clear_shader.clicked.connect(self.clear_shader_cache)

        secondary_btn_row.addStretch()
        secondary_btn_row.addWidget(self.btn_open_folder)
        secondary_btn_row.addWidget(self.btn_clear_shader)
        actions_col.addLayout(secondary_btn_row)

        hf_layout.addLayout(actions_col)
        ws_layout.addWidget(self.hero_frame)

        # 2.2 Tabs Interface
        self.tabs = QTabWidget()
        
        # Tab 1: Installed Mods
        self.tab_mods = QWidget()
        self.build_mods_tab(self.tab_mods)
        self.tabs.addTab(self.tab_mods, "📦 Installed Mods")

        # Tab 2: Universal Mod Store
        self.tab_store = QWidget()
        self.build_store_tab(self.tab_store)
        self.tabs.addTab(self.tab_store, "🌐 Mod Store")

        # Tab 3: Doctor & Diagnostics
        self.tab_doctor = QWidget()
        self.build_doctor_tab(self.tab_doctor)
        self.tabs.addTab(self.tab_doctor, "🩺 Doctor && Health")

        # Tab 4: Save Guard
        self.tab_saves = QWidget()
        self.build_saves_tab(self.tab_saves)
        self.tabs.addTab(self.tab_saves, "💾 Save Guard")

        # Tab 5: Settings & Studio
        self.tab_settings = QWidget()
        self.build_settings_tab(self.tab_settings)
        self.tabs.addTab(self.tab_settings, "⚙️ Settings && Studio")

        ws_layout.addWidget(self.tabs)
        splitter.addWidget(workspace_frame)

        # Set splitter proportions (Sidebar: ~300px, Workspace: remaining)
        splitter.setSizes([320, 1000])
        root_layout.addWidget(splitter)

        # Status Bar
        self.statusBar().showMessage("Ready")
        self.statusBar().setStyleSheet("background-color: #07090e; color: #64748b; border-top: 1px solid #161f2e; padding: 4px;")

    # ──────────────── Tab Builders ────────────────

    def build_mods_tab(self, parent):
        layout = QVBoxLayout(parent)
        layout.setContentsMargins(14, 14, 14, 14)
        layout.setSpacing(12)

        # Top Action Bar
        top_bar = QHBoxLayout()
        top_bar.setSpacing(10)

        self.txt_mod_search = QLineEdit()
        self.txt_mod_search.setPlaceholderText("Filter installed mods...")
        self.txt_mod_search.textChanged.connect(self.render_mods_list)
        top_bar.addWidget(self.txt_mod_search)

        self.btn_install_archive = QPushButton("📥 Install Mod File (.zip, .7z, .rar)")
        self.btn_install_archive.setStyleSheet("background-color: #0f2b23; border: 1px solid #10b981; color: #34d399; font-weight: bold;")
        self.btn_install_archive.setCursor(Qt.CursorShape.PointingHandCursor)
        self.btn_install_archive.clicked.connect(self.prompt_install_archive)
        top_bar.addWidget(self.btn_install_archive)

        self.btn_enable_all = QPushButton("Enable All")
        self.btn_enable_all.setCursor(Qt.CursorShape.PointingHandCursor)
        self.btn_enable_all.clicked.connect(lambda: self.toggle_all_mods(True))
        top_bar.addWidget(self.btn_enable_all)

        self.btn_disable_all = QPushButton("Disable All")
        self.btn_disable_all.setCursor(Qt.CursorShape.PointingHandCursor)
        self.btn_disable_all.clicked.connect(lambda: self.toggle_all_mods(False))
        top_bar.addWidget(self.btn_disable_all)

        layout.addLayout(top_bar)

        # Scrollable Mod Cards Area
        self.mods_scroll = QScrollArea()
        self.mods_scroll.setWidgetResizable(True)
        self.mods_scroll.setStyleSheet("background-color: transparent; border: none;")
        self.mods_container = QWidget()
        self.mods_layout = QVBoxLayout(self.mods_container)
        self.mods_layout.setContentsMargins(0, 0, 0, 0)
        self.mods_layout.setSpacing(8)
        self.mods_layout.addStretch()
        self.mods_scroll.setWidget(self.mods_container)
        layout.addWidget(self.mods_scroll)

    def build_store_tab(self, parent):
        layout = QVBoxLayout(parent)
        layout.setContentsMargins(14, 14, 14, 14)
        layout.setSpacing(12)

        # Store Selector & Filters
        ctrl_bar = QHBoxLayout()
        ctrl_bar.setSpacing(10)

        self.store_chips = {}
        chip_sources = [
            ("all", "🌐 All Stores"),
            ("nexus", "🟣 Nexus Mods"),
            ("thunder", "⚡ Mod Store"),
            ("modio", "🟢 mod.io")
        ]
        self.current_store_source = "all"
        for code, label in chip_sources:
            btn = QPushButton(label)
            btn.setCheckable(True)
            btn.setCursor(Qt.CursorShape.PointingHandCursor)
            if code == "all":
                btn.setChecked(True)
            btn.clicked.connect(lambda checked, c=code: self.change_store_source(c))
            self.store_chips[code] = btn
            ctrl_bar.addWidget(btn)

        ctrl_bar.addSpacing(16)
        self.txt_store_search = QLineEdit()
        self.txt_store_search.setPlaceholderText("Search mod catalog (e.g. Cyber Engine Tweaks, Skins, Fixes)...")
        self.txt_store_search.returnPressed.connect(self.search_store)
        ctrl_bar.addWidget(self.txt_store_search)

        self.btn_search_store = QPushButton("Search")
        self.btn_search_store.setStyleSheet("background-color: #0284c7; color: white; font-weight: bold;")
        self.btn_search_store.setCursor(Qt.CursorShape.PointingHandCursor)
        self.btn_search_store.clicked.connect(self.search_store)
        ctrl_bar.addWidget(self.btn_search_store)

        layout.addLayout(ctrl_bar)

        # Store Results Scroll Area
        self.store_scroll = QScrollArea()
        self.store_scroll.setWidgetResizable(True)
        self.store_scroll.setStyleSheet("background-color: transparent; border: none;")
        self.store_container = QWidget()
        self.store_layout = QVBoxLayout(self.store_container)
        self.store_layout.setContentsMargins(0, 0, 0, 0)
        self.store_layout.setSpacing(10)
        self.store_layout.addStretch()
        self.store_scroll.setWidget(self.store_container)
        layout.addWidget(self.store_scroll)

    def build_doctor_tab(self, parent):
        layout = QVBoxLayout(parent)
        layout.setContentsMargins(14, 14, 14, 14)
        layout.setSpacing(14)

        # Diagnostic Banner
        self.doctor_banner = QFrame()
        self.doctor_banner.setStyleSheet("""
            QFrame {
                background-color: #0f172a;
                border: 1px solid #1e293b;
                border-radius: 10px;
                padding: 16px;
            }
        """)
        db_layout = QHBoxLayout(self.doctor_banner)
        
        doc_info = QVBoxLayout()
        self.lbl_doc_status = QLabel("System Diagnostics Ready")
        self.lbl_doc_status.setStyleSheet("font-size: 18px; font-weight: 800; color: #f8fafc;")
        self.lbl_doc_summary = QLabel("Run auto-repair to fix missing frameworks, resolve duplicate mods, or repair script compilation issues.")
        self.lbl_doc_summary.setStyleSheet("font-size: 12px; color: #94a3b8;")
        doc_info.addWidget(self.lbl_doc_status)
        doc_info.addWidget(self.lbl_doc_summary)
        db_layout.addLayout(doc_info)

        db_layout.addStretch()

        self.btn_self_heal = QPushButton("⚡ AUTO-REPAIR / SELF-HEAL")
        self.btn_self_heal.setStyleSheet("""
            QPushButton {
                background: qlineargradient(x1:0, y1:0, x2:1, y2:0, stop:0 #f59e0b, stop:1 #d97706);
                border: 1px solid #b45309;
                color: #000;
                font-weight: 800;
                font-size: 14px;
                padding: 12px 24px;
                border-radius: 8px;
            }
            QPushButton:hover {
                background: qlineargradient(x1:0, y1:0, x2:1, y2:0, stop:0 #fbbf24, stop:1 #f59e0b);
            }
        """)
        self.btn_self_heal.setCursor(Qt.CursorShape.PointingHandCursor)
        self.btn_self_heal.clicked.connect(self.trigger_self_heal)
        db_layout.addWidget(self.btn_self_heal)

        layout.addWidget(self.doctor_banner)

        # Issues List Scroll Area
        self.doc_scroll = QScrollArea()
        self.doc_scroll.setWidgetResizable(True)
        self.doc_scroll.setStyleSheet("background-color: transparent; border: none;")
        self.doc_container = QWidget()
        self.doc_layout = QVBoxLayout(self.doc_container)
        self.doc_layout.setContentsMargins(0, 0, 0, 0)
        self.doc_layout.setSpacing(10)
        self.doc_layout.addStretch()
        self.doc_scroll.setWidget(self.doc_container)
        layout.addWidget(self.doc_scroll)

    def build_saves_tab(self, parent):
        layout = QVBoxLayout(parent)
        layout.setContentsMargins(14, 14, 14, 14)
        layout.setSpacing(12)

        ctrl_row = QHBoxLayout()
        lbl_info = QLabel("Protect your campaign progression and rollback instantly if a save gets corrupted.")
        lbl_info.setStyleSheet("color: #94a3b8; font-size: 13px;")
        ctrl_row.addWidget(lbl_info)
        ctrl_row.addStretch()

        self.btn_backup_now = QPushButton("💾 Backup Saves Now")
        self.btn_backup_now.setStyleSheet("background-color: #0284c7; color: white; font-weight: bold; padding: 10px 18px;")
        self.btn_backup_now.setCursor(Qt.CursorShape.PointingHandCursor)
        self.btn_backup_now.clicked.connect(self.trigger_save_backup)
        ctrl_row.addWidget(self.btn_backup_now)

        layout.addLayout(ctrl_row)

        self.saves_table = QTableWidget()
        self.saves_table.setColumnCount(4)
        self.saves_table.setHorizontalHeaderLabels(["Snapshot Date", "Size", "Files", "Action"])
        self.saves_table.horizontalHeader().setSectionResizeMode(0, QHeaderView.ResizeMode.Stretch)
        self.saves_table.horizontalHeader().setSectionResizeMode(1, QHeaderView.ResizeMode.ResizeToContents)
        self.saves_table.horizontalHeader().setSectionResizeMode(2, QHeaderView.ResizeMode.ResizeToContents)
        self.saves_table.horizontalHeader().setSectionResizeMode(3, QHeaderView.ResizeMode.ResizeToContents)
        self.saves_table.setStyleSheet("""
            QTableWidget {
                background-color: #0d121c;
                border: 1px solid #1c2738;
                border-radius: 8px;
                color: #f1f5f9;
                gridline-color: #1a2333;
            }
            QHeaderView::section {
                background-color: #121927;
                color: #94a3b8;
                padding: 6px;
                font-weight: bold;
                border: none;
            }
        """)
        layout.addWidget(self.saves_table)

    def build_settings_tab(self, parent):
        layout = QVBoxLayout(parent)
        layout.setContentsMargins(16, 16, 16, 16)
        layout.setSpacing(16)

        # Performance & Overlays
        sec_perf = QFrame()
        sec_perf.setProperty("class", "CardFrame")
        sp_layout = QVBoxLayout(sec_perf)
        sp_layout.setSpacing(10)
        lbl_p_title = QLabel("Gaming Performance & System Overlays")
        lbl_p_title.setStyleSheet("font-size: 15px; font-weight: bold; color: #f8fafc;")
        sp_layout.addWidget(lbl_p_title)

        self.chk_gamemode = QCheckBox("Enable Feral GameMode (Automatic CPU governor & GPU priority optimization)")
        self.chk_gamemode.setStyleSheet("color: #cbd5e1; font-size: 13px;")
        self.chk_mangohud = QCheckBox("Enable MangoHud Performance HUD (FPS, Frame times, Temperatures, VRAM)")
        self.chk_mangohud.setStyleSheet("color: #cbd5e1; font-size: 13px;")
        sp_layout.addWidget(self.chk_gamemode)
        sp_layout.addWidget(self.chk_mangohud)
        layout.addWidget(sec_perf)

        # Steam Launch Command Helper
        sec_steam = QFrame()
        sec_steam.setProperty("class", "CardFrame")
        ss_layout = QVBoxLayout(sec_steam)
        ss_layout.setSpacing(8)
        lbl_s_title = QLabel("Steam Launch Options & DLL Overrides")
        lbl_s_title.setStyleSheet("font-size: 15px; font-weight: bold; color: #f8fafc;")
        lbl_s_desc = QLabel("Copy this string directly into Steam Game Properties > Launch Options to ensure native mod loaders load correctly:")
        lbl_s_desc.setStyleSheet("color: #94a3b8; font-size: 12px;")
        self.txt_launch_opts = QLineEdit()
        self.txt_launch_opts.setReadOnly(True)
        self.txt_launch_opts.setText("WINEDLLOVERRIDES=\"version,winmm,dwmapi=n,b\" gamemoderun %command%")
        self.txt_launch_opts.setStyleSheet("font-family: monospace; font-size: 12px; background-color: #090d14; color: #00f2fe;")
        
        btn_copy = QPushButton("📋 Copy to Clipboard")
        btn_copy.setFixedWidth(160)
        btn_copy.setCursor(Qt.CursorShape.PointingHandCursor)
        btn_copy.clicked.connect(lambda: QApplication.clipboard().setText(self.txt_launch_opts.text()))

        ss_layout.addWidget(lbl_s_title)
        ss_layout.addWidget(lbl_s_desc)
        ss_layout.addWidget(self.txt_launch_opts)
        ss_layout.addWidget(btn_copy)
        layout.addWidget(sec_steam)

        # Cache & Cleaners
        sec_cache = QFrame()
        sec_cache.setProperty("class", "CardFrame")
        sc_layout = QVBoxLayout(sec_cache)
        sc_layout.setSpacing(10)
        lbl_c_title = QLabel("Maintenance & Cache Cleaner")
        lbl_c_title.setStyleSheet("font-size: 15px; font-weight: bold; color: #f8fafc;")
        
        btn_row = QHBoxLayout()
        btn_clear_dxvk = QPushButton("🧹 Clear DXVK / VKD3D Shader Caches")
        btn_clear_dxvk.setCursor(Qt.CursorShape.PointingHandCursor)
        btn_clear_dxvk.clicked.connect(self.clear_shader_cache)
        btn_row.addWidget(btn_clear_dxvk)
        btn_row.addStretch()

        sc_layout.addWidget(lbl_c_title)
        sc_layout.addLayout(btn_row)
        layout.addWidget(sec_cache)

        layout.addStretch()

    # ──────────────── Logic & Data Handlers ────────────────

    def connect_signals(self):
        # Auto-refresh timer every 5 seconds
        self.timer = QTimer(self)
        self.timer.timeout.connect(self.poll_quick_status)
        self.timer.start(5000)

    def startup_sequence(self):
        # Immediate sync check for instant UI response if daemon is ready
        if self.api.ping():
            self.api.fetch_token()
            self.lbl_engine_status.setText(f"●  ENGINE 6.0 ONLINE (:{self.api.port})")
            self.lbl_engine_status.setStyleSheet("background: #0f231e; color: #34d399; border: 1px solid #10b981; border-radius: 12px; padding: 4px 10px;")
            self.current_state = self.api.get_state()
            self.doctor_data = self.api.get_doctor_all()
            self.populate_games(self.current_state)
            return

        self.lbl_engine_status.setText("●  STARTING ENGINE 6.0...")
        self.lbl_engine_status.setStyleSheet("background: #261f0c; color: #fde047; border: 1px solid #ca8a04; border-radius: 12px; padding: 4px 10px;")
        
        def run_start():
            ok = self.api.ensure_daemon_running()
            if ok:
                st = self.api.get_state()
                doc = self.api.get_doctor_all()
                return ok, st, doc
            return False, {}, {}

        worker = AsyncWorker(run_start)
        worker.finished.connect(self.on_startup_done)
        self.active_workers.append(worker)
        worker.start()

    def on_startup_done(self, result):
        ok, state, doctor = result
        if ok:
            self.lbl_engine_status.setText(f"●  ENGINE 6.0 ONLINE (:{self.api.port})")
            self.lbl_engine_status.setStyleSheet("background: #0f231e; color: #34d399; border: 1px solid #10b981; border-radius: 12px; padding: 4px 10px;")
            self.current_state = state
            self.doctor_data = doctor
            self.populate_games(state)
        else:
            self.lbl_engine_status.setText("●  ENGINE OFFLINE")
            self.lbl_engine_status.setStyleSheet("background: #2a0f12; color: #f87171; border: 1px solid #ef4444; border-radius: 12px; padding: 4px 10px;")
            QMessageBox.warning(self, "Engine Connection", "Could not start or connect to ModForge engine on port 19450.")

    def refresh_state(self):
        self.statusBar().showMessage("Refreshing ModForge library...")
        def run_refresh():
            st = self.api.get_state()
            doc = self.api.get_doctor_all()
            return st, doc

        worker = AsyncWorker(run_refresh)
        worker.finished.connect(self.on_refresh_done)
        self.active_workers.append(worker)
        worker.start()

    def on_refresh_done(self, result):
        st, doc = result
        self.current_state = st
        self.doctor_data = doc
        self.populate_games(st)
        self.statusBar().showMessage("ModForge library updated", 3000)

    def poll_quick_status(self):
        # Background light state update
        if not self.api.token:
            return
        def run_poll():
            return self.api.get_state()
        w = AsyncWorker(run_poll)
        w.finished.connect(self.on_quick_poll_done)
        self.active_workers.append(w)
        w.start()

    def on_quick_poll_done(self, st):
        if not st or "games" not in st:
            return
        self.current_state = st
        is_running = st.get("is_running", False)
        self.btn_stop_game.setVisible(is_running)

    def populate_games(self, state):
        games = state.get("games", [])
        self.games_list = games
        self.lbl_game_count.setText(f"{len([g for g in games if g.get('is_installed')])} installed / {len(games)} total")
        self.on_filter_games()

    def set_filter_mode(self, mode):
        for b in [self.btn_filter_all, self.btn_filter_installed, self.btn_filter_modded, self.btn_filter_issues]:
            b.setChecked(False)
        if mode == "all":
            self.btn_filter_all.setChecked(True)
        elif mode == "installed":
            self.btn_filter_installed.setChecked(True)
        elif mode == "modded":
            self.btn_filter_modded.setChecked(True)
        elif mode == "issues":
            self.btn_filter_issues.setChecked(True)
        self.on_filter_games()

    def get_game_issues(self, game_name):
        doc_games = self.doctor_data.get("games", [])
        for dg in doc_games:
            if dg.get("name") == game_name:
                return dg.get("issues", [])
        return []

    def on_filter_games(self):
        query = self.txt_global_search.text().strip().lower()
        mode = "installed"
        if self.btn_filter_all.isChecked():
            mode = "all"
        elif self.btn_filter_modded.isChecked():
            mode = "modded"
        elif self.btn_filter_issues.isChecked():
            mode = "issues"

        self.game_list_widget.clear()

        # Find Cyberpunk or select first installed as default
        default_idx = None
        doc_map = {dg.get("name"): dg for dg in self.doctor_data.get("games", [])}

        for i, g in enumerate(self.games_list):
            name = g.get("name", "")
            is_installed = g.get("is_installed", False)
            engine = g.get("engine", "Standard")
            dg = doc_map.get(name, {})
            mods_count = dg.get("mods", len(g.get("mods", [])))
            issues = self.get_game_issues(name)

            if query and query not in name.lower() and query not in engine.lower():
                continue

            if mode == "installed" and not is_installed:
                continue
            elif mode == "modded" and (not is_installed or (mods_count == 0 and not issues)):
                continue
            elif mode == "issues" and len(issues) == 0:
                continue

            # Item widget
            item = QListWidgetItem(self.game_list_widget)
            item.setData(Qt.ItemDataRole.UserRole, i)
            item.setSizeHint(QSize(280, 56))

            w = QWidget()
            layout = QHBoxLayout(w)
            layout.setContentsMargins(8, 6, 8, 6)
            layout.setSpacing(10)

            # Health indicator dot
            dot = QLabel()
            dot.setFixedSize(10, 10)
            if not is_installed:
                dot.setStyleSheet("background-color: #475569; border-radius: 5px;")
            elif issues:
                has_bad = any(x.get("level") == "bad" for x in issues)
                dot.setStyleSheet(f"background-color: {'#ef4444' if has_bad else '#f59e0b'}; border-radius: 5px;")
            else:
                dot.setStyleSheet("background-color: #10b981; border-radius: 5px;")
            layout.addWidget(dot)

            text_col = QVBoxLayout()
            text_col.setSpacing(2)
            lbl_name = QLabel(name)
            lbl_name.setStyleSheet("font-size: 13px; font-weight: bold; color: #f8fafc;")
            lbl_engine = QLabel(engine)
            lbl_engine.setStyleSheet("font-size: 11px; color: #94a3b8;")
            text_col.addWidget(lbl_name)
            text_col.addWidget(lbl_engine)
            layout.addLayout(text_col)

            layout.addStretch()

            if is_installed:
                badge = QLabel(f"{mods_count} mods" if mods_count > 0 else "Clean")
                badge.setStyleSheet("font-size: 10px; font-weight: 700; color: #cbd5e1; background: #1a2333; padding: 2px 6px; border-radius: 4px;")
                layout.addWidget(badge)

            self.game_list_widget.setItemWidget(item, w)

            if name == "Cyberpunk 2077":
                default_idx = self.game_list_widget.count() - 1

        if self.game_list_widget.count() > 0:
            target_row = default_idx if default_idx is not None else 0
            self.game_list_widget.setCurrentRow(target_row)

    def on_game_selected(self, row):
        item = self.game_list_widget.item(row)
        if not item:
            return
        real_idx = item.data(Qt.ItemDataRole.UserRole)
        self.selected_game_idx = real_idx
        if real_idx < len(self.games_list):
            self.selected_game = self.games_list[real_idx]
            self.render_selected_game()
            # Send selection to engine
            def send_select():
                self.api.select_game(real_idx)
                return self.api.get_state()
            w = AsyncWorker(send_select)
            w.finished.connect(self.on_select_synced)
            self.active_workers.append(w)
            w.start()

    def on_select_synced(self, state):
        self.current_state = state
        self.render_selected_game()

    def render_selected_game(self):
        g = self.selected_game
        if not g:
            return
        name = g.get("name", "Unknown Game")
        engine = g.get("engine", "Standard Engine")
        gdir = g.get("game_dir", "")
        mods = g.get("mods", [])
        lib = self.current_state.get("library") or {}
        lib_mods = lib.get("mods", [])
        mods_count = len(lib_mods) if lib_mods else len(mods)

        self.lbl_selected_title.setText(name)
        self.lbl_hero_engine.setText(f"⚙️ {engine}")
        self.lbl_hero_mods.setText(f"📦 {mods_count} Mods Active")
        self.lbl_hero_path.setText(gdir)

        is_running = self.current_state.get("is_running", False)
        self.btn_stop_game.setVisible(is_running)

        # Update tabs
        self.render_mods_list()
        self.render_doctor_view()

    def render_mods_list(self):
        # Clear existing
        while self.mods_layout.count() > 1:
            child = self.mods_layout.takeAt(0)
            if child.widget():
                child.widget().deleteLater()

        filter_q = self.txt_mod_search.text().strip().lower()
        lib = self.current_state.get("library")
        mods = []
        if lib and "mods" in lib:
            mods = lib.get("mods", [])
        else:
            mods = self.selected_game.get("mods", []) if self.selected_game else []

        if not mods:
            empty_lbl = QLabel("No mods installed for this game yet.\nDrag & drop an archive here or browse the Mod Store tab.")
            empty_lbl.setStyleSheet("color: #64748b; font-size: 14px; padding: 40px; text-align: center;")
            empty_lbl.setAlignment(Qt.AlignmentFlag.AlignCenter)
            self.mods_layout.insertWidget(0, empty_lbl)
            return

        for m in mods:
            m_name = m.get("name", "Unnamed Mod")
            m_ver = m.get("version", "")
            m_enabled = m.get("enabled", True)
            m_id = m.get("id", m_name)
            m_source = "Manual"
            if m.get("nexus"):
                m_source = "Nexus Mods"
            elif m.get("thunderstore"):
                m_source = "Mod Store"
            elif m.get("modio"):
                m_source = "mod.io"
            elif m.get("github"):
                m_source = "GitHub"

            if filter_q and filter_q not in m_name.lower() and filter_q not in m_source.lower():
                continue

            # Card
            card = QFrame()
            card.setProperty("class", "CardFrame")
            cl = QHBoxLayout(card)
            cl.setContentsMargins(12, 10, 12, 10)
            cl.setSpacing(14)

            # Toggle Switch Checkbox
            chk = QCheckBox()
            chk.setChecked(m_enabled)
            chk.setCursor(Qt.CursorShape.PointingHandCursor)
            chk.stateChanged.connect(lambda state, mid=m_id: self.toggle_single_mod(mid, state == 2))
            cl.addWidget(chk)

            # Info
            info_col = QVBoxLayout()
            info_col.setSpacing(2)
            lbl_mname = QLabel(m_name)
            lbl_mname.setStyleSheet("font-size: 14px; font-weight: bold; color: #f8fafc;")
            
            sub_row = QHBoxLayout()
            sub_row.setSpacing(8)
            if m_ver:
                lbl_ver = QLabel(f"v{m_ver}")
                lbl_ver.setStyleSheet("font-size: 11px; color: #94a3b8; font-weight: 600;")
                sub_row.addWidget(lbl_ver)
            
            badge_src = QLabel(m_source)
            if m_source == "Nexus Mods":
                badge_src.setProperty("class", "Chip ChipNexus")
            elif m_source == "Mod Store":
                badge_src.setProperty("class", "Chip ChipStore")
            elif m_source == "mod.io":
                badge_src.setProperty("class", "Chip ChipModio")
            else:
                badge_src.setProperty("class", "Chip ChipEngine")
            sub_row.addWidget(badge_src)

            if m.get("files"):
                lbl_files = QLabel(f"{m.get('files')} files")
                lbl_files.setStyleSheet("font-size: 11px; color: #64748b;")
                sub_row.addWidget(lbl_files)

            sub_row.addStretch()
            info_col.addWidget(lbl_mname)
            info_col.addLayout(sub_row)
            cl.addLayout(info_col)

            cl.addStretch()

            # Actions
            btn_del = QPushButton("🗑 Delete")
            btn_del.setStyleSheet("background-color: #270f14; border: 1px solid #7f1d1d; color: #f87171; padding: 6px 12px; font-size: 12px;")
            btn_del.setCursor(Qt.CursorShape.PointingHandCursor)
            btn_del.clicked.connect(lambda checked, mid=m_id: self.delete_single_mod(mid))
            cl.addWidget(btn_del)

            self.mods_layout.insertWidget(self.mods_layout.count() - 1, card)

    def render_doctor_view(self):
        while self.doc_layout.count() > 1:
            child = self.doc_layout.takeAt(0)
            if child.widget():
                child.widget().deleteLater()

        if not self.selected_game:
            return

        name = self.selected_game.get("name", "")
        issues = self.get_game_issues(name)

        if not issues:
            self.lbl_doc_status.setText("🟢 All Systems Optimal")
            self.lbl_doc_status.setStyleSheet("font-size: 18px; font-weight: 800; color: #34d399;")
            self.lbl_doc_summary.setText("No script clashes, missing frameworks, or duplicate mods detected.")
            card = QFrame()
            card.setProperty("class", "CardFrame")
            cl = QVBoxLayout(card)
            msg = QLabel("Everything is green! All mod dependencies, engine hooks, and scripts are synchronized.")
            msg.setStyleSheet("color: #a7f3d0; font-size: 13px;")
            cl.addWidget(msg)
            self.doc_layout.insertWidget(0, card)
            return

        has_bad = any(x.get("level") == "bad" for x in issues)
        self.lbl_doc_status.setText(f"{'🔴 Critical Issues Detected' if has_bad else '🟡 Warnings Detected'} ({len(issues)} finding{'s' if len(issues)>1 else ''})")
        self.lbl_doc_status.setStyleSheet(f"font-size: 18px; font-weight: 800; color: {'#f87171' if has_bad else '#fbbf24'};")
        self.lbl_doc_summary.setText("ModForge identified conflicts or missing frameworks. You can auto-repair with 1 click below:")

        for i in issues:
            lvl = i.get("level", "warn")
            title = i.get("title") or i.get("code") or "Detected Anomaly"
            evidence = i.get("evidence")
            args = i.get("args", [])

            card = QFrame()
            card.setProperty("class", "IssueCardBad" if lvl == "bad" else "IssueCardWarn")
            cl = QVBoxLayout(card)
            cl.setSpacing(6)

            hdr = QHBoxLayout()
            lbl_badge = QLabel("CRITICAL" if lvl == "bad" else "WARNING")
            lbl_badge.setStyleSheet(f"background: {'#7f1d1d' if lvl=='bad' else '#78350f'}; color: {'#fecaca' if lvl=='bad' else '#fef3c7'}; font-weight: bold; font-size: 10px; padding: 2px 6px; border-radius: 4px;")
            lbl_t = QLabel(title)
            lbl_t.setStyleSheet("font-size: 14px; font-weight: bold; color: #f8fafc;")
            hdr.addWidget(lbl_badge)
            hdr.addWidget(lbl_t)
            hdr.addStretch()
            cl.addLayout(hdr)

            if args:
                lbl_args = QLabel(" • " + ", ".join(args))
                lbl_args.setStyleSheet("color: #e2e8f0; font-size: 12px;")
                cl.addWidget(lbl_args)

            if evidence:
                txt_ev = QLabel(evidence)
                txt_ev.setStyleSheet("font-family: monospace; font-size: 11px; background-color: #070a0f; color: #fca5a5; padding: 6px; border-radius: 4px;")
                txt_ev.setWordWrap(True)
                cl.addWidget(txt_ev)

            # Specific Quick-Fix Button if applicable
            if "ArchiveXL" in str(evidence) or "ArchiveXL" in str(args):
                btn_fix_arc = QPushButton("⚡ Restore Missing ArchiveXL Framework")
                btn_fix_arc.setStyleSheet("background-color: #10b981; color: black; font-weight: bold; padding: 6px;")
                btn_fix_arc.setCursor(Qt.CursorShape.PointingHandCursor)
                btn_fix_arc.clicked.connect(self.restore_archivexl_quickfix)
                cl.addWidget(btn_fix_arc)

            self.doc_layout.insertWidget(self.doc_layout.count() - 1, card)

    # ──────────────── Store Handlers ────────────────

    def change_store_source(self, code):
        self.current_store_source = code
        for c, btn in self.store_chips.items():
            btn.setChecked(c == code)
        self.search_store()

    def search_store(self):
        query = self.txt_store_search.text().strip()
        self.statusBar().showMessage(f"Searching {self.current_store_source} store...")
        def run_search():
            return self.api.get_store(source=self.current_store_source, query=query)

        worker = AsyncWorker(run_search)
        worker.finished.connect(self.on_store_results)
        self.active_workers.append(worker)
        worker.start()

    def on_store_results(self, resp):
        self.statusBar().showMessage("Mod Store catalog loaded", 3000)
        while self.store_layout.count() > 1:
            child = self.store_layout.takeAt(0)
            if child.widget():
                child.widget().deleteLater()

        cards = resp.get("cards", [])
        if not cards:
            lbl_empty = QLabel("No mods found for this query in the selected store.")
            lbl_empty.setStyleSheet("color: #64748b; font-size: 14px; padding: 40px;")
            lbl_empty.setAlignment(Qt.AlignmentFlag.AlignCenter)
            self.store_layout.insertWidget(0, lbl_empty)
            return

        for c in cards:
            c_name = c.get("name", "Unknown Mod")
            c_author = c.get("author", "Unknown Author")
            c_summary = c.get("summary", "")
            c_ver = c.get("version", "")
            c_src = c.get("source", "store")
            c_id = c.get("id")
            c_dl = c.get("downloads", 0)

            card = QFrame()
            card.setProperty("class", "CardFrame")
            cl = QVBoxLayout(card)
            cl.setSpacing(6)

            top_row = QHBoxLayout()
            lbl_t = QLabel(c_name)
            lbl_t.setStyleSheet("font-size: 15px; font-weight: bold; color: #f8fafc;")
            top_row.addWidget(lbl_t)

            lbl_badge = QLabel(c_src.upper())
            if c_src == "nexus":
                lbl_badge.setProperty("class", "Chip ChipNexus")
            elif c_src == "thunderstore":
                lbl_badge.setProperty("class", "Chip ChipStore")
            elif c_src == "modio":
                lbl_badge.setProperty("class", "Chip ChipModio")
            else:
                lbl_badge.setProperty("class", "Chip ChipEngine")
            top_row.addWidget(lbl_badge)
            top_row.addStretch()

            btn_install = QPushButton("📥 1-Click Install")
            btn_install.setStyleSheet("background-color: #00f2fe; color: #000; font-weight: bold; padding: 6px 14px; border-radius: 6px;")
            btn_install.setCursor(Qt.CursorShape.PointingHandCursor)
            btn_install.clicked.connect(lambda checked, s=c_src, cid=c_id: self.install_store_mod(s, cid))
            top_row.addWidget(btn_install)
            cl.addLayout(top_row)

            # Metadata
            meta_lbl = QLabel(f"By {c_author}  •  v{c_ver}  •  {c_dl:,} downloads")
            meta_lbl.setStyleSheet("font-size: 11px; color: #94a3b8;")
            cl.addWidget(meta_lbl)

            if c_summary:
                sum_lbl = QLabel(c_summary)
                sum_lbl.setStyleSheet("font-size: 12px; color: #cbd5e1;")
                sum_lbl.setWordWrap(True)
                cl.addWidget(sum_lbl)

            self.store_layout.insertWidget(self.store_layout.count() - 1, card)

    def install_store_mod(self, source, item_id):
        self.statusBar().showMessage(f"Downloading and installing {item_id}...")
        def run_inst():
            if source == "thunderstore":
                return self.api.install_thunder_mod(item_id)
            elif source == "modio":
                return self.api.install_modio_mod(item_id)
            return {"error": f"Direct 1-click install for {source} in progress."}

        worker = AsyncWorker(run_inst)
        worker.finished.connect(lambda res: self.on_mod_installed(res))
        self.active_workers.append(worker)
        worker.start()

    def on_mod_installed(self, res):
        self.statusBar().showMessage("Mod installed successfully!", 5000)
        self.refresh_state()

    # ──────────────── Action Callbacks ────────────────

    def launch_game(self, mode="mods"):
        if not self.selected_game:
            return
        gname = self.selected_game.get("name", "")
        self.statusBar().showMessage(f"Starting {gname} ({mode} mode)...")
        def run_play():
            return self.api.play_game(mode)
        w = AsyncWorker(run_play)
        w.finished.connect(lambda res: self.statusBar().showMessage(f"Running {gname}!", 5000))
        self.active_workers.append(w)
        w.start()

    def stop_game(self):
        self.statusBar().showMessage("Stopping game processes...")
        def run_stop():
            return self.api.stop_game()
        w = AsyncWorker(run_stop)
        w.finished.connect(lambda res: self.statusBar().showMessage("Game stopped", 3000))
        self.active_workers.append(w)
        w.start()

    def trigger_self_heal(self):
        self.statusBar().showMessage("Running ModForge Auto-Repair Engine...")
        def run_heal():
            self.api.self_heal()
            time.sleep(1.5)
            st = self.api.get_state()
            doc = self.api.get_doctor_all()
            return st, doc

        w = AsyncWorker(run_heal)
        w.finished.connect(self.on_refresh_done)
        self.active_workers.append(w)
        w.start()

    def restore_archivexl_quickfix(self):
        self.statusBar().showMessage("Restoring ArchiveXL framework...")
        def run_fix():
            # Restore ArchiveXL from trash or reinstall
            self.api.restore_mod_trash("archivexl-4198-1-27-5-2026-10-20t10-52z--595682-1792873532477-2200")
            time.sleep(1.0)
            return self.api.get_state(), self.api.get_doctor_all()

        w = AsyncWorker(run_fix)
        w.finished.connect(self.on_refresh_done)
        self.active_workers.append(w)
        w.start()

    def toggle_single_mod(self, mod_id, enable):
        def run_toggle():
            self.api.toggle_mod(mod_id, enable)
            return self.api.get_state()
        w = AsyncWorker(run_toggle)
        w.finished.connect(self.on_select_synced)
        self.active_workers.append(w)
        w.start()

    def toggle_all_mods(self, enable):
        self.statusBar().showMessage(f"{'Enabling' if enable else 'Disabling'} all mods...")
        lib_mods = (self.current_state.get("library") or {}).get("mods", [])
        def run_toggle_all():
            for m in lib_mods:
                self.api.toggle_mod(m.get("id"), enable)
            return self.api.get_state()
        w = AsyncWorker(run_toggle_all)
        w.finished.connect(self.on_select_synced)
        self.active_workers.append(w)
        w.start()

    def delete_single_mod(self, mod_id):
        reply = QMessageBox.question(self, "Delete Mod", f"Are you sure you want to delete this mod?\nIt will be safely moved to trash.", QMessageBox.StandardButton.Yes | QMessageBox.StandardButton.No)
        if reply == QMessageBox.StandardButton.Yes:
            def run_del():
                self.api.delete_mods([mod_id])
                time.sleep(0.5)
                return self.api.get_state()
            w = AsyncWorker(run_del)
            w.finished.connect(self.on_select_synced)
            self.active_workers.append(w)
            w.start()

    def prompt_install_archive(self):
        file_path, _ = QFileDialog.getOpenFileName(self, "Select Mod Archive", "", "Archives (*.zip *.7z *.rar *.tar.gz)")
        if file_path:
            self.statusBar().showMessage(f"Deploying {os.path.basename(file_path)}...")
            def run_upload():
                return self.api.upload_mod_archive(file_path)
            w = AsyncWorker(run_upload)
            w.finished.connect(lambda res: self.refresh_state())
            self.active_workers.append(w)
            w.start()

    def open_game_folder(self):
        if not self.selected_game:
            return
        gdir = self.selected_game.get("game_dir", "")
        if gdir and os.path.exists(gdir):
            QDesktopServices.openUrl(QUrl.fromLocalFile(gdir))

    def clear_shader_cache(self):
        self.statusBar().showMessage("Clearing shader cache...")
        def run_clear():
            return self.api.clear_shaders()
        w = AsyncWorker(run_clear)
        w.finished.connect(lambda res: self.statusBar().showMessage("Shader caches purged successfully!", 4000))
        self.active_workers.append(w)
        w.start()

    def trigger_save_backup(self):
        self.statusBar().showMessage("Creating save backup snapshot...")
        def run_bak():
            return self.api.backup_saves()
        w = AsyncWorker(run_bak)
        w.finished.connect(lambda res: self.statusBar().showMessage("Save snapshot secured!", 4000))
        self.active_workers.append(w)
        w.start()

    def on_add_game_dialog(self):
        path = QFileDialog.getExistingDirectory(self, "Select Game Installation Folder")
        if path:
            self.statusBar().showMessage(f"Adding game from {path}...")
            # ModForge auto-indexes game folder
            self.refresh_state()


def main():
    app = QApplication(sys.argv)
    app.setStyle("Fusion")
    window = ModForgeApp()
    window.show()
    sys.exit(app.exec())


if __name__ == "__main__":
    main()
