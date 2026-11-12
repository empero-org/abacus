// Abacus Desktop Extensions: VS Code Style Appearance & Theme Engine + Git Checkpoints & Rollback

(function() {
  const THEME_KEY = "abacus.desktop_theme";
  const DB_NAME = "abacus_customizations";
  const STORE_NAME = "wallpapers";

  // Built-in theme presets with complete token mapping
  const PRESETS = {
    vscode: {
      name: "VS Code Dark+",
      primary: "#007acc",
      bg: "#1e1e1e",
      sidebar: "#252526",
      card: "#2d2d2d",
      text: "#d4d4d4",
      swatches: ["#007acc", "#1e1e1e", "#d4d4d4"]
    },
    monokai: {
      name: "Monokai Pro",
      primary: "#ffd866",
      bg: "#2d2a2e",
      sidebar: "#221f22",
      card: "#3a353b",
      text: "#fcfcfa",
      swatches: ["#ffd866", "#2d2a2e", "#a9dc76"]
    },
    tokyo: {
      name: "Tokyo Night",
      primary: "#7aa2f7",
      bg: "#1a1b26",
      sidebar: "#16161e",
      card: "#24283b",
      text: "#c0caf5",
      swatches: ["#7aa2f7", "#1a1b26", "#bb9af7"]
    },
    abacus: {
      name: "Abacus Dark",
      primary: "#b66bff",
      bg: "#0e0b14",
      sidebar: "#09070d",
      card: "#151020",
      text: "#eae6f2",
      swatches: ["#b66bff", "#0e0b14", "#eae6f2"]
    },
    github: {
      name: "GitHub Dark",
      primary: "#58a6ff",
      bg: "#0d1117",
      sidebar: "#161b22",
      card: "#21262d",
      text: "#c9d1d9",
      swatches: ["#58a6ff", "#0d1117", "#3fb950"]
    },
    amoled: {
      name: "AMOLED Black",
      primary: "#a855f7",
      bg: "#000000",
      sidebar: "#070707",
      card: "#121212",
      text: "#ffffff",
      swatches: ["#a855f7", "#000000", "#ffffff"]
    }
  };

  const FONTS = {
    inter: "'Inter', -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif",
    mono: "'JetBrains Mono', 'Consolas', 'Courier New', monospace",
    consolas: "'Consolas', 'Courier New', monospace",
    fira: "'Fira Code', 'JetBrains Mono', monospace",
    system: "-apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif"
  };

  let themeState = {
    preset: "vscode",
    hasBg: false,
    dim: 35,
    blur: 0,
    fontFamily: "inter",
    fontSize: 14,
    primary: "#007acc",
    text: "#d4d4d4",
    bg: "#1e1e1e"
  };

  let checkpointsState = [];
  let filesState = [];
  let selectedFilePath = null;
  let selectedCheckpointId = null;
  let fileSearchQuery = "";
  let currentWallpaperUrl = null;

  // Load saved state
  try {
    const raw = localStorage.getItem(THEME_KEY);
    if (raw) Object.assign(themeState, JSON.parse(raw));
  } catch (e) {
    console.warn("Theme load error:", e);
  }

  // --- IndexedDB for persistent large wallpaper storage ---
  function openThemeDB() {
    return new Promise((resolve, reject) => {
      const req = indexedDB.open(DB_NAME, 1);
      req.onupgradeneeded = () => {
        if (!req.result.objectStoreNames.contains(STORE_NAME)) {
          req.result.createObjectStore(STORE_NAME);
        }
      };
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => reject(req.error);
    });
  }

  async function saveWallpaperBlob(blob) {
    try {
      const db = await openThemeDB();
      return new Promise((resolve, reject) => {
        const tx = db.transaction(STORE_NAME, "readwrite");
        tx.objectStore(STORE_NAME).put(blob, "custom-wallpaper");
        tx.oncomplete = () => resolve();
        tx.onerror = () => reject(tx.error);
      });
    } catch (e) {
      console.error("Failed to save wallpaper in IndexedDB:", e);
    }
  }

  async function loadWallpaperBlob() {
    try {
      const db = await openThemeDB();
      return new Promise((resolve, reject) => {
        const tx = db.transaction(STORE_NAME, "readonly");
        const req = tx.objectStore(STORE_NAME).get("custom-wallpaper");
        req.onsuccess = () => resolve(req.result);
        req.onerror = () => reject(req.error);
      });
    } catch (e) {
      return null;
    }
  }

  async function deleteWallpaperBlob() {
    try {
      const db = await openThemeDB();
      return new Promise((resolve, reject) => {
        const tx = db.transaction(STORE_NAME, "readwrite");
        tx.objectStore(STORE_NAME).delete("custom-wallpaper");
        tx.oncomplete = () => resolve();
        tx.onerror = () => reject(tx.error);
      });
    } catch (e) {}
  }

  function hexToRgba(hex, alpha) {
    let c = (hex || "#1e1e1e").replace("#", "");
    if (c.length === 3) c = c[0] + c[0] + c[1] + c[1] + c[2] + c[2];
    const num = parseInt(c, 16);
    const r = (num >> 16) & 255;
    const g = (num >> 8) & 255;
    const b = num & 255;
    return `rgba(${r}, ${g}, ${b}, ${alpha})`;
  }

  function applyTheme() {
    const p = PRESETS[themeState.preset] || PRESETS.vscode;
    const bg = themeState.bg || p.bg;
    const text = themeState.text || p.text;
    const primary = themeState.primary || p.primary;
    const sidebar = p.sidebar;
    const card = p.card;
    const fontDef = FONTS[themeState.fontFamily] || FONTS.inter;

    let styleTag = document.getElementById("abacus-custom-theme-style");
    if (!styleTag) {
      styleTag = document.createElement("style");
      styleTag.id = "abacus-custom-theme-style";
      document.head.appendChild(styleTag);
    }

    const hasBg = !!currentWallpaperUrl;

    styleTag.textContent = `
      :root, html, body {
        --font-sans: ${fontDef} !important;
        --font-mono: ${fontDef} !important;
        --default-font-family: ${fontDef} !important;
        --default-mono-font-family: ${fontDef} !important;
        --color-primary: ${primary} !important;
        --color-brand: ${primary} !important;
        --color-ring: ${primary} !important;
        --color-foreground: ${text} !important;
        --color-background: ${bg} !important;
        --color-sidebar: ${sidebar} !important;
        --color-card: ${card} !important;
        --color-panel: ${bg} !important;
        --color-input: ${card} !important;
        --color-input-focused: ${card} !important;
        --color-terminal-bg: ${sidebar} !important;
        --color-terminal-fg: ${text} !important;
        --ui-font-size: ${themeState.fontSize}px !important;
      }

      body, body *, #root, button, input, textarea, select {
        font-family: ${fontDef} !important;
      }

      ${hasBg ? `
        html, body, #root {
          background: transparent !important;
          background-color: transparent !important;
        }
        .bg-background, [class*="bg-background"] {
          background-color: ${hexToRgba(bg, 0.55)} !important;
        }
        .bg-sidebar, aside, [class*="bg-sidebar"] {
          background-color: ${hexToRgba(sidebar, 0.75)} !important;
          backdrop-filter: blur(14px) !important;
        }
        .bg-card, form, [class*="bg-card"] {
          background-color: ${hexToRgba(card, 0.70)} !important;
          backdrop-filter: blur(10px) !important;
        }
      ` : `
        html, body {
          background-color: ${bg} !important;
        }
        .bg-background, [class*="bg-background"] {
          background-color: ${bg} !important;
        }
        .bg-sidebar, aside, [class*="bg-sidebar"] {
          background-color: ${sidebar} !important;
        }
        .bg-card, form, [class*="bg-card"] {
          background-color: ${card} !important;
        }
      `}
    `;

    let backdrop = document.getElementById("abacus-custom-backdrop");
    let overlay = document.getElementById("abacus-custom-overlay");

    if (!backdrop) {
      backdrop = document.createElement("div");
      backdrop.id = "abacus-custom-backdrop";
      document.body.prepend(backdrop);
    }

    if (!overlay) {
      overlay = document.createElement("div");
      overlay.id = "abacus-custom-overlay";
      document.body.prepend(overlay);
    }

    if (hasBg) {
      document.body.classList.add("abacus-has-custom-bg");
      backdrop.style.backgroundImage = `url("${currentWallpaperUrl}")`;
      backdrop.style.filter = `blur(${themeState.blur}px)`;
      backdrop.style.opacity = "1";
      overlay.style.backgroundColor = `rgba(0, 0, 0, ${themeState.dim / 100})`;
    } else {
      document.body.classList.remove("abacus-has-custom-bg");
      backdrop.style.backgroundImage = "none";
      backdrop.style.opacity = "0";
      overlay.style.backgroundColor = "transparent";
    }

    try {
      localStorage.setItem(THEME_KEY, JSON.stringify({
        preset: themeState.preset,
        hasBg: hasBg,
        dim: themeState.dim,
        blur: themeState.blur,
        fontFamily: themeState.fontFamily,
        fontSize: themeState.fontSize,
        primary: themeState.primary,
        text: themeState.text,
        bg: themeState.bg
      }));
    } catch (e) {}
  }

  function showToast(msg) {
    let toast = document.getElementById("abacus-ext-toast");
    if (!toast) {
      toast = document.createElement("div");
      toast.id = "abacus-ext-toast";
      document.body.appendChild(toast);
    }
    toast.textContent = msg;
    toast.classList.add("show");
    setTimeout(() => toast.classList.remove("show"), 2500);
  }

  function escapeHtml(str) {
    return String(str || "").replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  }

  // --- File-Based Checkpoints, Timeline & Diff Functions ---
  function synthesizeFilesFromCheckpoints(checkpoints) {
    const map = new Map();
    for (const cp of checkpoints) {
      const created = cp.created_files || [];
      const touched = cp.touched_files || [];
      const all = [
        ...created.map(p => ({ path: p, wasCreated: true })),
        ...touched.filter(p => !created.includes(p)).map(p => ({ path: p, wasCreated: false }))
      ];
      for (const item of all) {
        if (!map.has(item.path)) {
          map.set(item.path, {
            file_path: item.path,
            status: item.wasCreated ? "created" : "modified",
            added_lines: 0,
            removed_lines: 0,
            revisions: []
          });
        }
        map.get(item.path).revisions.push({
          checkpoint_id: cp.id,
          turn_index: cp.turn_index,
          step_index: cp.step_index || 0,
          timestamp: cp.timestamp || 0,
          tool_name: cp.tool_name || "edit",
          prompt_summary: cp.prompt_preview || cp.prompt_summary || cp.id,
          was_created: item.wasCreated
        });
      }
    }
    return Array.from(map.values()).reverse();
  }

  function fileExtBadge(filePath) {
    const base = String(filePath || "").split("/").pop() || "";
    const parts = base.split(".");
    if (parts.length > 1) {
      return parts.pop().slice(0, 4).toUpperCase();
    }
    return "FILE";
  }

  function splitPathParts(filePath) {
    const norm = String(filePath || "").replace(/\\/g, "/");
    const idx = norm.lastIndexOf("/");
    if (idx === -1) return { name: norm, dir: "workspace root" };
    return { name: norm.slice(idx + 1), dir: norm.slice(0, idx) };
  }

  function statusLetter(status) {
    if (status === "created") return "A";
    if (status === "deleted") return "D";
    return "M";
  }

  function formatTimestamp(ts) {
    if (!ts) return "";
    const d = new Date(ts * 1000);
    if (isNaN(d.getTime())) return "";
    return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
  }

  async function fetchCheckpoints() {
    if (!window.abacus || typeof window.abacus.request !== "function") return;
    try {
      const res = await window.abacus.request("checkpoints/list", {});
      checkpointsState = (res && Array.isArray(res.checkpoints)) ? res.checkpoints : [];
      if (res && Array.isArray(res.files) && res.files.length > 0) {
        filesState = res.files;
      } else {
        filesState = synthesizeFilesFromCheckpoints(checkpointsState);
      }
      if (selectedFilePath && !filesState.some(f => f.file_path === selectedFilePath)) {
        selectedFilePath = null;
        selectedCheckpointId = null;
      }
      if (!selectedFilePath && filesState.length > 0) {
        selectedFilePath = filesState[0].file_path;
        const revs = filesState[0].revisions || [];
        selectedCheckpointId = revs.length > 0 ? revs[0].checkpoint_id : null;
      }
      renderCheckpointsStudio();
      updateCheckpointsBadge();
    } catch (e) {}
  }

  async function rollbackSingleFile(checkpointId, filePath) {
    if (!window.abacus || typeof window.abacus.request !== "function") return;
    if (!checkpointId || !filePath) return;
    try {
      const res = await window.abacus.request("checkpoints/rollback", {
        checkpointId,
        filePath
      });
      const restored = (res && (res.restoredFiles || res.restored_files)) || [filePath];
      showToast(`Restored ${restored.join(", ")}`);
      await fetchCheckpoints();
    } catch (e) {
      showToast("Restore failed: " + (e.message || e));
    }
  }

  async function rollbackAllInCheckpoint(checkpointId) {
    if (!window.abacus || typeof window.abacus.request !== "function") return;
    try {
      const res = await window.abacus.request("checkpoints/rollback", checkpointId ? { checkpointId } : {});
      const restored = (res && (res.restoredFiles || res.restored_files)) || [];
      showToast(`Restored ${restored.length} file(s)`);
      await fetchCheckpoints();
    } catch (e) {
      showToast("Restore failed: " + (e.message || e));
    }
  }

  function updateCheckpointsBadge() {
    const badge = document.getElementById("abacus-sidebar-cp-count");
    const topCount = document.getElementById("abacus-cp-files-count");
    const len = filesState.length;
    if (badge) {
      badge.textContent = String(len);
      badge.style.display = len > 0 ? "inline-block" : "none";
    }
    if (topCount) {
      topCount.textContent = `${len} file${len === 1 ? "" : "s"}`;
    }
  }

  function renderDiffLines(diffText) {
    const viewer = document.getElementById("abacus-cp-diff-viewer");
    if (!viewer) return;
    if (!diffText || !diffText.trim()) {
      viewer.innerHTML = `<div class="abacus-cp-empty">No differences between this snapshot and the current file on disk.</div>`;
      return;
    }
    const lines = String(diffText).split("\n");
    const html = lines.map(line => {
      let cls = "abacus-diff-ctx";
      if (line.startsWith("+++") || line.startsWith("---")) cls = "abacus-diff-meta";
      else if (line.startsWith("@@")) cls = "abacus-diff-hunk";
      else if (line.startsWith("+")) cls = "abacus-diff-add";
      else if (line.startsWith("-")) cls = "abacus-diff-del";
      return `<div class="abacus-diff-line ${cls}">${escapeHtml(line)}</div>`;
    }).join("");
    viewer.innerHTML = html;
  }

  async function loadSelectedFileDiff() {
    const viewer = document.getElementById("abacus-cp-diff-viewer");
    if (!viewer) return;
    if (!selectedFilePath || !selectedCheckpointId) {
      viewer.innerHTML = `<div class="abacus-cp-empty">Select a modified file on the left to inspect its version timeline and unified diff.</div>`;
      return;
    }
    viewer.innerHTML = `<div class="abacus-cp-empty">Loading diff...</div>`;
    try {
      const res = await window.abacus.request("checkpoints/diff", {
        checkpointId: selectedCheckpointId,
        filePath: selectedFilePath
      });
      renderDiffLines(res && res.diff_text ? res.diff_text : "");
    } catch (e) {
      viewer.innerHTML = `<div class="abacus-cp-empty">Could not load diff: ${escapeHtml(e.message || e)}</div>`;
    }
  }

  function renderCheckpointsStudio() {
    const filesListEl = document.getElementById("abacus-cp-files-list");
    const detailPathEl = document.getElementById("abacus-cp-detail-path");
    const restoreFileBtn = document.getElementById("abacus-cp-restore-file-btn");
    const timelineStripEl = document.getElementById("abacus-cp-timeline-strip");
    const diffViewerEl = document.getElementById("abacus-cp-diff-viewer");
    if (!filesListEl || !detailPathEl || !timelineStripEl || !diffViewerEl) return;

    updateCheckpointsBadge();

    const q = (fileSearchQuery || "").trim().toLowerCase();
    const filtered = q
      ? filesState.filter(f => f.file_path.toLowerCase().includes(q))
      : filesState;

    filesListEl.innerHTML = "";
    if (!filtered.length) {
      filesListEl.innerHTML = `<div class="abacus-cp-empty">${filesState.length ? "No matching files." : "No modified files recorded in this session yet."}</div>`;
      detailPathEl.textContent = "No file selected";
      if (restoreFileBtn) restoreFileBtn.style.display = "none";
      timelineStripEl.innerHTML = "";
      diffViewerEl.innerHTML = `<div class="abacus-cp-empty">Snapshots are captured automatically before Abacus edits, creates, or deletes any file.</div>`;
      return;
    }

    if (!selectedFilePath || !filtered.some(f => f.file_path === selectedFilePath)) {
      selectedFilePath = filtered[0].file_path;
      const revs = filtered[0].revisions || [];
      selectedCheckpointId = revs.length > 0 ? revs[0].checkpoint_id : null;
    }

    for (const fileObj of filtered) {
      const isSelected = fileObj.file_path === selectedFilePath;
      const row = document.createElement("div");
      row.className = `abacus-cp-file-row ${isSelected ? "active" : ""}`;
      const parts = splitPathParts(fileObj.file_path);
      const ext = fileExtBadge(fileObj.file_path);
      const sClass = fileObj.status || "modified";
      const sLetter = statusLetter(sClass);
      const addStr = fileObj.added_lines > 0 ? `<span class="abacus-cp-stat-add">+${fileObj.added_lines}</span>` : "";
      const delStr = fileObj.removed_lines > 0 ? `<span class="abacus-cp-stat-del">-${fileObj.removed_lines}</span>` : "";

      row.innerHTML = `
        <div class="abacus-cp-file-main">
          <span class="abacus-cp-ext-badge">${escapeHtml(ext)}</span>
          <div class="abacus-cp-file-text">
            <div class="abacus-cp-file-name" title="${escapeHtml(fileObj.file_path)}">${escapeHtml(parts.name)}</div>
            <div class="abacus-cp-file-dir">${escapeHtml(parts.dir)}</div>
          </div>
        </div>
        <div class="abacus-cp-file-right">
          <div class="abacus-cp-stats">${addStr}${delStr}</div>
          <span class="abacus-cp-status-badge ${escapeHtml(sClass)}">${sLetter}</span>
        </div>
      `;
      row.onclick = () => {
        selectedFilePath = fileObj.file_path;
        const revs = fileObj.revisions || [];
        selectedCheckpointId = revs.length > 0 ? revs[0].checkpoint_id : null;
        renderCheckpointsStudio();
      };
      filesListEl.appendChild(row);
    }

    const activeFile = filtered.find(f => f.file_path === selectedFilePath) || filtered[0];
    const revs = activeFile.revisions || [];
    if (!selectedCheckpointId || !revs.some(r => r.checkpoint_id === selectedCheckpointId)) {
      selectedCheckpointId = revs.length > 0 ? revs[0].checkpoint_id : null;
    }

    detailPathEl.textContent = activeFile.file_path;
    if (restoreFileBtn) {
      restoreFileBtn.style.display = selectedCheckpointId ? "inline-block" : "none";
      restoreFileBtn.onclick = () => rollbackSingleFile(selectedCheckpointId, activeFile.file_path);
    }

    timelineStripEl.innerHTML = "";
    revs.forEach((rev, idx) => {
      const pill = document.createElement("button");
      pill.type = "button";
      const isRevActive = rev.checkpoint_id === selectedCheckpointId;
      pill.className = `abacus-cp-rev-pill ${isRevActive ? "active" : ""}`;
      const timeLabel = formatTimestamp(rev.timestamp);
      const toolLabel = rev.tool_name || "edit";
      const vLabel = idx === 0 ? `v1 (Initial before ${toolLabel})` : `v${idx + 1} (${toolLabel})`;
      pill.textContent = `${vLabel} · Turn #${rev.turn_index}${timeLabel ? " · " + timeLabel : ""}`;
      pill.title = rev.prompt_summary || rev.checkpoint_id;
      pill.onclick = () => {
        selectedCheckpointId = rev.checkpoint_id;
        renderCheckpointsStudio();
      };
      timelineStripEl.appendChild(pill);
    });

    loadSelectedFileDiff();
  }

  function findSettingsDom() {
    const closeBtn = document.querySelector('button[aria-label="Close settings"]');
    if (!closeBtn) return null;
    const container = closeBtn.closest("div.max-w-2xl");
    if (!container) return null;
    const sections = Array.from(container.querySelectorAll(":scope > section"));
    const appearanceSection = sections.find(sec => {
      const h2 = sec.querySelector("h2");
      return h2 && /^Appearance(\s*&\s*Themes)?$/i.test(h2.textContent.trim());
    }) || null;
    return { container, appearanceSection };
  }

  function findSidebarSettingsButton() {
    return Array.from(document.querySelectorAll("button")).find(b => {
      if (
        b.closest("#abacus-sidebar-ext-row") ||
        b.closest("#abacus-settings-pets-section") ||
        b.closest("#abacus-settings-appearance-ext")
      ) {
        return false;
      }
      const t = (b.textContent || "").trim();
      return t === "Settings" || /^Settings(\b|$)/.test(t);
    });
  }

  function openSettingsToSection(sectionName) {
    const scrollTarget = () => {
      ensureSettingsExtensions();
      const target = sectionName === "pets"
        ? document.getElementById("abacus-settings-pets-section")
        : document.getElementById("abacus-settings-appearance-ext");
      if (target) {
        target.scrollIntoView({ behavior: "smooth", block: "start" });
        return true;
      }
      return false;
    };

    if (scrollTarget()) return;

    const settingsBtn = findSidebarSettingsButton();
    if (settingsBtn) {
      settingsBtn.click();
      let tries = 0;
      const timer = setInterval(() => {
        tries += 1;
        if (scrollTarget() || tries >= 15) {
          clearInterval(timer);
        }
      }, 45);
    }
  }

  function mountAppearanceIntoSettings(appearanceSection) {
    if (!appearanceSection) return;
    const h2 = appearanceSection.querySelector("h2");
    if (h2 && h2.textContent.trim() === "Appearance") {
      h2.textContent = "Appearance & Themes";
    }
    if (document.getElementById("abacus-settings-appearance-ext")) return;

    const card = document.createElement("div");
    card.id = "abacus-settings-appearance-ext";
    card.className = "flex flex-col divide-y rounded-xl border bg-card";
    card.innerHTML = `
      <div class="abacus-settings-ext-row">
        <div class="abacus-settings-ext-header">
          <div>
            <div class="abacus-settings-ext-title">Color Theme Presets</div>
            <div class="abacus-settings-ext-hint">Choose a VS Code-inspired palette for editor, sidebar, and surfaces.</div>
          </div>
        </div>
        <div class="abacus-presets-grid" id="abacus-presets-list"></div>
      </div>

      <div class="abacus-settings-ext-row">
        <div class="abacus-settings-ext-header">
          <div>
            <div class="abacus-settings-ext-title">Custom Background Image</div>
            <div class="abacus-settings-ext-hint">Apply a wallpaper behind the glass UI with adjustable dim and blur.</div>
          </div>
          <div style="display:flex;gap:8px;align-items:center;flex-wrap:wrap;">
            <input type="file" id="abacus-bg-file" accept="image/*" style="display:none;" />
            <button type="button" class="abacus-action-btn" id="abacus-btn-choose-bg">Upload Image File…</button>
            <button type="button" class="abacus-action-btn secondary" id="abacus-btn-clear-bg">Remove Background</button>
          </div>
        </div>
        <div class="abacus-slider-row" style="margin-top:4px;">
          <span class="abacus-slider-label">Dim Overlay</span>
          <input type="range" class="abacus-slider" id="abacus-dim-slider" min="0" max="90" value="${themeState.dim}" />
          <span class="abacus-slider-val" id="abacus-dim-val">${themeState.dim}%</span>
        </div>
        <div class="abacus-slider-row" style="margin-top:2px;">
          <span class="abacus-slider-label">Background Blur</span>
          <input type="range" class="abacus-slider" id="abacus-blur-slider" min="0" max="25" value="${themeState.blur}" />
          <span class="abacus-slider-val" id="abacus-blur-val">${themeState.blur}px</span>
        </div>
      </div>

      <div class="abacus-settings-ext-row">
        <div class="abacus-settings-ext-title">Font Family</div>
        <div class="abacus-segmented" id="abacus-font-list" style="flex-wrap:wrap;">
          <button type="button" class="abacus-segment-btn ${themeState.fontFamily === 'inter' ? 'active' : ''}" data-font="inter">Inter</button>
          <button type="button" class="abacus-segment-btn ${themeState.fontFamily === 'mono' ? 'active' : ''}" data-font="mono">JetBrains Mono</button>
          <button type="button" class="abacus-segment-btn ${themeState.fontFamily === 'consolas' ? 'active' : ''}" data-font="consolas">Consolas</button>
          <button type="button" class="abacus-segment-btn ${themeState.fontFamily === 'fira' ? 'active' : ''}" data-font="fira">Fira Code</button>
          <button type="button" class="abacus-segment-btn ${themeState.fontFamily === 'system' ? 'active' : ''}" data-font="system">System UI</button>
        </div>
      </div>

      <div class="abacus-settings-ext-row">
        <div class="abacus-settings-ext-title">Font Size</div>
        <div class="abacus-segmented" id="abacus-size-list" style="flex-wrap:wrap;">
          <button type="button" class="abacus-segment-btn ${themeState.fontSize === 12 ? 'active' : ''}" data-size="12">12px</button>
          <button type="button" class="abacus-segment-btn ${themeState.fontSize === 13 ? 'active' : ''}" data-size="13">13px</button>
          <button type="button" class="abacus-segment-btn ${themeState.fontSize === 14 ? 'active' : ''}" data-size="14">14px (Default)</button>
          <button type="button" class="abacus-segment-btn ${themeState.fontSize === 15 ? 'active' : ''}" data-size="15">15px</button>
          <button type="button" class="abacus-segment-btn ${themeState.fontSize === 16 ? 'active' : ''}" data-size="16">16px</button>
          <button type="button" class="abacus-segment-btn ${themeState.fontSize === 18 ? 'active' : ''}" data-size="18">18px</button>
        </div>
      </div>

      <div class="abacus-settings-ext-row">
        <div class="abacus-settings-ext-title">Custom Colors</div>
        <div class="abacus-color-row" style="margin-top:0;flex-wrap:wrap;">
          <div class="abacus-color-item">
            <span class="abacus-color-label">Primary Accent</span>
            <input type="color" class="abacus-color-picker" id="abacus-color-primary" value="${themeState.primary}" />
          </div>
          <div class="abacus-color-item">
            <span class="abacus-color-label">Foreground / Ink</span>
            <input type="color" class="abacus-color-picker" id="abacus-color-text" value="${themeState.text}" />
          </div>
          <div class="abacus-color-item">
            <span class="abacus-color-label">Background</span>
            <input type="color" class="abacus-color-picker" id="abacus-color-bg" value="${themeState.bg}" />
          </div>
        </div>
      </div>
    `;

    appearanceSection.appendChild(card);

    // Populate Presets Grid
    const presetsList = document.getElementById("abacus-presets-list");
    Object.keys(PRESETS).forEach(key => {
      const p = PRESETS[key];
      const item = document.createElement("div");
      item.className = `abacus-preset-item ${themeState.preset === key ? "active" : ""}`;
      item.innerHTML = `
        <div class="abacus-preset-name">${p.name}</div>
        <div class="abacus-preset-swatches">
          ${p.swatches.map(s => `<span class="abacus-preset-swatch" style="background:${s}"></span>`).join("")}
        </div>
      `;
      item.onclick = () => {
        themeState.preset = key;
        themeState.primary = p.primary;
        themeState.bg = p.bg;
        themeState.text = p.text;
        const cp = document.getElementById("abacus-color-primary");
        const cb = document.getElementById("abacus-color-bg");
        const ct = document.getElementById("abacus-color-text");
        if (cp) cp.value = p.primary;
        if (cb) cb.value = p.bg;
        if (ct) ct.value = p.text;
        document.querySelectorAll("#abacus-presets-list .abacus-preset-item").forEach(el => el.classList.remove("active"));
        item.classList.add("active");
        applyTheme();
        showToast(`Theme set to ${p.name}`);
      };
      presetsList.appendChild(item);
    });

    // Background image handlers
    const fileInput = document.getElementById("abacus-bg-file");
    document.getElementById("abacus-btn-choose-bg").onclick = () => fileInput.click();
    fileInput.onchange = async (e) => {
      const file = e.target.files && e.target.files[0];
      if (!file) return;
      try {
        await saveWallpaperBlob(file);
        if (currentWallpaperUrl) URL.revokeObjectURL(currentWallpaperUrl);
        currentWallpaperUrl = URL.createObjectURL(file);
        themeState.hasBg = true;
        applyTheme();
        showToast("Custom background image applied!");
      } catch (err) {
        showToast("Failed to save wallpaper: " + err.message);
      }
    };

    document.getElementById("abacus-btn-clear-bg").onclick = async () => {
      await deleteWallpaperBlob();
      if (currentWallpaperUrl) URL.revokeObjectURL(currentWallpaperUrl);
      currentWallpaperUrl = null;
      themeState.hasBg = false;
      fileInput.value = "";
      applyTheme();
      showToast("Background image removed");
    };

    const dimSlider = document.getElementById("abacus-dim-slider");
    const dimVal = document.getElementById("abacus-dim-val");
    dimSlider.oninput = (e) => {
      themeState.dim = parseInt(e.target.value, 10);
      dimVal.textContent = themeState.dim + "%";
      applyTheme();
    };

    const blurSlider = document.getElementById("abacus-blur-slider");
    const blurVal = document.getElementById("abacus-blur-val");
    blurSlider.oninput = (e) => {
      themeState.blur = parseInt(e.target.value, 10);
      blurVal.textContent = themeState.blur + "px";
      applyTheme();
    };

    document.querySelectorAll("#abacus-font-list .abacus-segment-btn").forEach(btn => {
      btn.onclick = () => {
        document.querySelectorAll("#abacus-font-list .abacus-segment-btn").forEach(b => b.classList.remove("active"));
        btn.classList.add("active");
        themeState.fontFamily = btn.dataset.font;
        applyTheme();
      };
    });

    document.querySelectorAll("#abacus-size-list .abacus-segment-btn").forEach(btn => {
      btn.onclick = () => {
        document.querySelectorAll("#abacus-size-list .abacus-segment-btn").forEach(b => b.classList.remove("active"));
        btn.classList.add("active");
        themeState.fontSize = parseInt(btn.dataset.size, 10);
        applyTheme();
      };
    });

    document.getElementById("abacus-color-primary").oninput = (e) => {
      themeState.primary = e.target.value;
      applyTheme();
    };
    document.getElementById("abacus-color-text").oninput = (e) => {
      themeState.text = e.target.value;
      applyTheme();
    };
    document.getElementById("abacus-color-bg").oninput = (e) => {
      themeState.bg = e.target.value;
      applyTheme();
    };
  }

  function ensureSettingsExtensions() {
    const dom = findSettingsDom();
    if (!dom) return false;
    mountAppearanceIntoSettings(dom.appearanceSection);
    mountPetsIntoSettings(dom.appearanceSection, dom.container);
    return true;
  }

  function setupUI() {
    // 1. Modals Container (Checkpoints Studio remains a workspace modal)
    let modalsWrap = document.getElementById("abacus-ext-modals");
    if (!modalsWrap) {
      modalsWrap = document.createElement("div");
      modalsWrap.id = "abacus-ext-modals";
      document.body.appendChild(modalsWrap);
    }

    modalsWrap.innerHTML = `
      <!-- Checkpoints Modal (VS Code 2-Column File History & Diff Studio, Zero Icons) -->
      <div id="abacus-modal-checkpoints" class="abacus-modal-overlay">
        <div class="abacus-cp-modal-content">
          <div class="abacus-cp-topbar">
            <div class="abacus-cp-topbar-left">
              <span class="abacus-cp-heading">File History & Checkpoints</span>
              <span class="abacus-cp-count-pill" id="abacus-cp-files-count">0 files</span>
            </div>
            <div class="abacus-cp-topbar-right">
              <input type="text" class="abacus-cp-search" id="abacus-cp-search-input" placeholder="Filter files..." />
              <button type="button" class="abacus-cp-btn" id="abacus-cp-refresh">Refresh</button>
              <button type="button" class="abacus-cp-btn danger-subtle" id="abacus-cp-rollback-latest">Restore All</button>
              <button type="button" class="abacus-cp-btn" data-close>Close</button>
            </div>
          </div>
          <div class="abacus-cp-split">
            <div class="abacus-cp-files-pane">
              <div class="abacus-cp-pane-label">Modified Files</div>
              <div class="abacus-cp-files-list" id="abacus-cp-files-list"></div>
            </div>
            <div class="abacus-cp-detail-pane">
              <div class="abacus-cp-detail-header">
                <div class="abacus-cp-detail-path" id="abacus-cp-detail-path">No file selected</div>
                <button type="button" class="abacus-cp-btn primary" id="abacus-cp-restore-file-btn" style="display:none;">Restore File to Selected Version</button>
              </div>
              <div class="abacus-cp-timeline-strip" id="abacus-cp-timeline-strip"></div>
              <div class="abacus-cp-diff-viewer" id="abacus-cp-diff-viewer"></div>
            </div>
          </div>
        </div>
      </div>
    `;

    // Wire Checkpoints Modal Close
    const modalCheckpoints = document.getElementById("abacus-modal-checkpoints");
    document.querySelectorAll("#abacus-modal-checkpoints [data-close], #abacus-modal-checkpoints").forEach(el => {
      el.addEventListener("click", (e) => {
        if (e.target === el) {
          modalCheckpoints.classList.remove("open");
        }
      });
    });

    // Checkpoints actions
    const searchInput = document.getElementById("abacus-cp-search-input");
    if (searchInput) {
      searchInput.oninput = (e) => {
        fileSearchQuery = e.target.value || "";
        renderCheckpointsStudio();
      };
    }
    document.getElementById("abacus-cp-refresh").onclick = fetchCheckpoints;
    document.getElementById("abacus-cp-rollback-latest").onclick = () => {
      if (checkpointsState.length > 0) {
        rollbackAllInCheckpoint(checkpointsState[0].id);
      } else {
        showToast("No checkpoints available to restore");
      }
    };

    // Listen to live app-server notifications
    if (window.abacus && typeof window.abacus.onMessage === "function") {
      window.abacus.onMessage((msg) => {
        if (!msg) return;
        if (msg.method === "checkpoints/updated" || msg.method === "turn/completed" || msg.method === "thread/started") {
          fetchCheckpoints();
        }
        handlePetAgentEvent(msg);
      });
    }

    // Keyboard shortcut: Ctrl+K Ctrl+T or Ctrl+Alt+T opens Settings -> Appearance & Themes
    let ctrlKPressed = false;
    window.addEventListener("keydown", (e) => {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "k") {
        ctrlKPressed = true;
        setTimeout(() => { ctrlKPressed = false; }, 1000);
      } else if (ctrlKPressed && (e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "t") {
        ctrlKPressed = false;
        e.preventDefault();
        openSettingsToSection("appearance");
      } else if ((e.ctrlKey || e.metaKey) && e.altKey && e.key.toLowerCase() === "t") {
        e.preventDefault();
        openSettingsToSection("appearance");
      } else if (e.key === "Escape") {
        modalCheckpoints.classList.remove("open");
        const modalPets = document.getElementById("abacus-modal-pets");
        if (modalPets) modalPets.classList.remove("open");
      }
    });

    // Observe React DOM so merged Settings sections appear immediately whenever Settings opens
    const rootEl = document.getElementById("root") || document.body;
    let settingsCheckPending = false;
    const obs = new MutationObserver(() => {
      if (settingsCheckPending) return;
      settingsCheckPending = true;
      requestAnimationFrame(() => {
        settingsCheckPending = false;
        ensureSettingsExtensions();
      });
    });
    obs.observe(rootEl, { childList: true, subtree: true });

    // Ensure Checkpoints Sidebar button above Settings (Pets & Appearance live inside Settings)
    ensureSidebarButton();
    setInterval(() => {
      ensureSidebarButton();
      ensureSettingsExtensions();
    }, 1500);
    fetchCheckpoints();
    initPetSystem();
  }

  function ensureSidebarButton() {
    const settingsBtn = findSidebarSettingsButton();
    if (!settingsBtn) return;
    const parentRow = settingsBtn.parentElement;
    if (!parentRow || !parentRow.parentElement) return;

    let customRow = document.getElementById("abacus-sidebar-ext-row");
    if (!customRow) {
      customRow = document.createElement("div");
      customRow.id = "abacus-sidebar-ext-row";
      customRow.className = "mb-1";
      customRow.innerHTML = `
        <button id="abacus-sidebar-checkpoints-btn" type="button" class="flex w-full items-center gap-2.5 rounded-lg px-2.5 py-1.5 text-ui-base text-foreground hover:bg-hover disabled:opacity-50">
          <span style="flex:1;text-align:left;">Checkpoints</span>
          <span id="abacus-sidebar-cp-count" style="display:none;background:rgba(182,107,255,0.2);color:#e2e8f0;font-size:10px;padding:1px 6px;border-radius:999px;font-weight:600;">0</span>
        </button>
      `;
      parentRow.parentElement.insertBefore(customRow, parentRow);
    }

    const cpBtn = document.getElementById("abacus-sidebar-checkpoints-btn");
    if (cpBtn) {
      cpBtn.onclick = () => {
        const m = document.getElementById("abacus-modal-checkpoints");
        if (m) {
          m.classList.add("open");
          fetchCheckpoints();
          setPetTemporaryState("review", "Reviewing file timeline & diffs", 5000, "running");
        }
      };
    }

    updateCheckpointsBadge();
  }

  /* ==========================================================================
     ABACUS PET SYSTEM & CODEX-PETS.NET COMPANION ENGINE
     Supports v1 (9-state 1536x1872) & v2 (11-state 1536x2288 + 16-angle 360° look)
     ========================================================================== */

  const PET_STORAGE_KEY = "abacus_pet_preferences_v1";
  const PET_CELL_W = 192;
  const PET_CELL_H = 208;
  const PET_COLS = 8;

  const PET_STATES_META = [
    { id: "idle", label: "Idle", row: 0, framesV1: 6, framesV2: 7, fps: 4 },
    { id: "running-right", label: "Run Right", row: 1, framesV1: 8, framesV2: 8, fps: 7 },
    { id: "running-left", label: "Run Left", row: 2, framesV1: 8, framesV2: 8, fps: 7 },
    { id: "waving", label: "Waving", row: 3, framesV1: 4, framesV2: 4, fps: 5 },
    { id: "jumping", label: "Jumping", row: 4, framesV1: 5, framesV2: 5, fps: 6 },
    { id: "failed", label: "Failed", row: 5, framesV1: 8, framesV2: 8, fps: 5 },
    { id: "waiting", label: "Waiting", row: 6, framesV1: 6, framesV2: 6, fps: 4 },
    { id: "running", label: "Running / Coding", row: 7, framesV1: 6, framesV2: 6, fps: 6 },
    { id: "review", label: "Review", row: 8, framesV1: 6, framesV2: 6, fps: 4 }
  ];

  let petPrefs = {
    enabled: true,
    activePetId: "yuki-cream-rabbit",
    size: 96,
    roaming: true,
    cursorLook: true,
    speechBubbles: true,
    osNotifications: "always",
    soundEffects: true,
    manualState: "auto"
  };

  function savePetPrefs(patch) {
    if (patch && typeof patch === "object") {
      Object.assign(petPrefs, patch);
    }
    if (window.abacus && window.abacus.pets && typeof window.abacus.pets.setConfig === "function") {
      window.abacus.pets.setConfig(petPrefs).catch(() => {});
    }
    updateSidebarPetBadge();
  }

  let installedPets = [];
  let catalogPets = [];
  let catalogTotal = 0;
  let catalogLoading = false;
  let catalogQuery = { q: "", sort: "popular", kind: "all", version: "all" };

  let activePetObj = null;
  let studioFrameIndex = 0;
  let studioAnimTimer = null;

  function updateSidebarPetBadge() {
    const badge = document.getElementById("abacus-sidebar-pet-badge");
    if (!badge) return;
    if (!petPrefs.enabled) {
      badge.textContent = "OFF";
      badge.style.background = "rgba(148,163,184,0.16)";
      badge.style.color = "#94a3b8";
    } else {
      const name = activePetObj ? activePetObj.displayName : "ON";
      badge.textContent = name.length > 10 ? name.slice(0, 10) + "…" : name;
      badge.style.background = "rgba(56,189,248,0.18)";
      badge.style.color = "#bae6fd";
    }
  }

  function setPetTemporaryState(stateId, bubbleText, durationMs = 3800, bubbleKind = "normal") {
    if (window.abacus && window.abacus.pets && typeof window.abacus.pets.sendCommand === "function") {
      window.abacus.pets.sendCommand({
        state: stateId,
        text: bubbleText || "",
        duration: durationMs,
        status: bubbleKind
      }).catch(() => {});
    }
  }

  function handlePetAgentEvent() {
    // Handled directly in Main -> Floating Desktop Pet Overlay Window so it works even when Abacus is hidden!
  }

  function renderStudioPreviewFrame() {
    const studioPreviewSprite = document.getElementById("abacus-pets-studio-preview-sprite");
    const studioPreviewMeta = document.getElementById("abacus-pets-studio-preview-meta");
    if (!studioPreviewSprite || !activePetObj) return;

    const effState = petPrefs.manualState && petPrefs.manualState !== "auto" ? petPrefs.manualState : "idle";
    const vNum = activePetObj.spriteVersionNumber === 2 ? 2 : 1;
    const totalRows = vNum === 2 ? 11 : 9;
    const meta = PET_STATES_META.find(s => s.id === effState) || PET_STATES_META[0];
    const totalFrames = vNum === 2 ? meta.framesV2 : meta.framesV1;
    const frame = studioFrameIndex % Math.max(1, totalFrames);

    const pw = 80;
    const ph = Math.round((pw * PET_CELL_H) / PET_CELL_W);
    studioPreviewSprite.style.width = `${pw}px`;
    studioPreviewSprite.style.height = `${ph}px`;
    studioPreviewSprite.style.backgroundImage = `url("${activePetObj.spritesheetUrl}")`;
    studioPreviewSprite.style.backgroundSize = `${pw * PET_COLS}px ${ph * totalRows}px`;
    studioPreviewSprite.style.backgroundPosition = `-${frame * pw}px -${meta.row * ph}px`;

    if (studioPreviewMeta) {
      studioPreviewMeta.textContent = `${activePetObj.displayName} · v${vNum} (${totalRows} rows) · Preview: ${effState.toUpperCase()}`;
    }
  }

  function restartSpriteLoop() {
    if (studioAnimTimer) clearInterval(studioAnimTimer);
    const effState = petPrefs.manualState && petPrefs.manualState !== "auto" ? petPrefs.manualState : "idle";
    const meta = PET_STATES_META.find(s => s.id === effState) || PET_STATES_META[0];
    const intervalMs = Math.round(1000 / Math.max(4, meta.fps || 6));
    renderStudioPreviewFrame();
    studioAnimTimer = setInterval(() => {
      studioFrameIndex = (studioFrameIndex + 1) % 64;
      renderStudioPreviewFrame();
    }, intervalMs);
  }

  // Pure-JS ZIP Extractor for codex-pets.net .zip packages
  async function extractCodexPetZip(file) {
    const buf = await file.arrayBuffer();
    const bytes = new Uint8Array(buf);
    const view = new DataView(buf);
    let offset = 0;
    let manifestJson = null;
    let spriteBytes = null;
    let spriteExt = ".webp";

    while (offset + 30 <= bytes.length) {
      const sig = view.getUint32(offset, true);
      if (sig !== 0x04034b50) break;
      const method = view.getUint16(offset + 8, true);
      const compSize = view.getUint32(offset + 18, true);
      const nameLen = view.getUint16(offset + 26, true);
      const extraLen = view.getUint16(offset + 28, true);
      const nameBytes = bytes.subarray(offset + 30, offset + 30 + nameLen);
      const fileName = new TextDecoder().decode(nameBytes);
      const dataStart = offset + 30 + nameLen + extraLen;
      const compData = bytes.subarray(dataStart, dataStart + compSize);
      offset = dataStart + compSize;

      if (fileName.endsWith("/")) continue;
      let rawData = null;
      if (method === 0) {
        rawData = compData;
      } else if (method === 8 && typeof DecompressionStream !== "undefined") {
        const ds = new DecompressionStream("deflate-raw");
        const writer = ds.writable.getWriter();
        writer.write(compData);
        writer.close();
        const outBuf = await new Response(ds.readable).arrayBuffer();
        rawData = new Uint8Array(outBuf);
      }
      if (!rawData) continue;

      const base = fileName.split("/").pop().toLowerCase();
      if (base === "pet.json") {
        manifestJson = JSON.parse(new TextDecoder().decode(rawData));
      } else if (base.endsWith(".webp") || base.endsWith(".png")) {
        if (!spriteBytes || base.includes("sprite")) {
          spriteBytes = rawData;
          spriteExt = base.endsWith(".png") ? ".png" : ".webp";
        }
      }
    }

    if (!spriteBytes) {
      throw new Error("ZIP archive does not contain spritesheet.webp or .png");
    }
    if (!manifestJson) {
      const fallbackId = file.name.replace(/(\.codex-pet)?\.zip$/i, "");
      manifestJson = { id: fallbackId, displayName: fallbackId, spriteVersionNumber: 1 };
    }

    let binary = "";
    const chunk = 0x8000;
    for (let i = 0; i < spriteBytes.length; i += chunk) {
      binary += String.fromCharCode.apply(null, spriteBytes.subarray(i, i + chunk));
    }
    const base64 = btoa(binary);
    return { manifest: manifestJson, spritesheetBase64: base64, ext: spriteExt };
  }

  async function refreshInstalledPets() {
    if (window.abacus && window.abacus.pets && typeof window.abacus.pets.getOverlayState === "function") {
      try {
        const state = await window.abacus.pets.getOverlayState();
        if (state && state.config) Object.assign(petPrefs, state.config);
        if (state && Array.isArray(state.installed)) installedPets = state.installed;
      } catch (e) {
        installedPets = [];
      }
    }
    if (installedPets.length > 0) {
      activePetObj = installedPets.find(p => p.id === petPrefs.activePetId) || installedPets[0];
      if (petPrefs.activePetId !== activePetObj.id) {
        savePetPrefs({ activePetId: activePetObj.id });
      }
    } else {
      activePetObj = null;
    }
    updateSidebarPetBadge();
    renderStudioPreviewFrame();
    renderInstalledPetsGrid();
  }

  async function loadCodexPetsCatalog() {
    const gridEl = document.getElementById("abacus-pets-catalog-grid");
    const statusEl = document.getElementById("abacus-pets-catalog-status");
    if (!gridEl) return;
    catalogLoading = true;
    if (statusEl) statusEl.textContent = "Loading community pets from codex-pets.net…";
    gridEl.innerHTML = `<div class="abacus-cp-empty" style="grid-column:1/-1;">Fetching live catalog from https://codex-pets.net …</div>`;

    try {
      const res = await window.abacus.pets.catalog({
        page: 1,
        pageSize: 48,
        sort: catalogQuery.sort,
        q: catalogQuery.q,
        kind: catalogQuery.kind,
        version: catalogQuery.version
      });
      catalogPets = Array.isArray(res.pets) ? res.pets : [];
      catalogTotal = Number(res.total) || catalogPets.length;
      if (statusEl) {
        statusEl.textContent = `${catalogTotal.toLocaleString()} pets on codex-pets.net · Showing ${catalogPets.length}`;
      }
      renderCatalogPetsGrid();
    } catch (err) {
      if (statusEl) statusEl.textContent = "Failed to load catalog";
      gridEl.innerHTML = `<div class="abacus-cp-empty" style="grid-column:1/-1;">Could not reach codex-pets.net: ${escapeHtml(err.message || String(err))}</div>`;
    } finally {
      catalogLoading = false;
    }
  }

  function renderInstalledPetsGrid() {
    const grid = document.getElementById("abacus-pets-installed-grid");
    if (!grid) return;
    if (!installedPets.length) {
      grid.innerHTML = `<div class="abacus-cp-empty" style="grid-column:1/-1;">No pets installed yet. Open the "Codex Pets Online Hub" tab to install one in 1 click!</div>`;
      return;
    }
    grid.innerHTML = "";
    installedPets.forEach(pet => {
      const isSelected = activePetObj && activePetObj.id === pet.id;
      const vNum = pet.spriteVersionNumber === 2 ? 2 : 1;
      const card = document.createElement("div");
      card.className = `abacus-pet-item-card ${isSelected ? "active" : ""}`;
      const thumbW = 68;
      const thumbH = Math.round((thumbW * PET_CELL_H) / PET_CELL_W);
      const totalRows = vNum === 2 ? 11 : 9;

      card.innerHTML = `
        <div class="abacus-pet-thumb-box">
          <span class="abacus-pet-badge ${vNum === 2 ? "v2" : ""}">v${vNum}${vNum === 2 ? " · 360°" : ""}</span>
          <div style="width:${thumbW}px;height:${thumbH}px;background-image:url('${pet.spritesheetUrl}');background-size:${thumbW * PET_COLS}px ${thumbH * totalRows}px;background-position:0 0;image-rendering:pixelated;"></div>
        </div>
        <div style="display:flex;align-items:center;justify-content:space-between;gap:6px;">
          <div style="font-weight:700;font-size:12.5px;color:#f8fafc;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;" title="${escapeHtml(pet.displayName)}">${escapeHtml(pet.displayName)}</div>
          <span style="font-size:10px;color:#94a3b8;text-transform:uppercase;">${escapeHtml(pet.kind || "pet")}</span>
        </div>
        <div style="font-size:11px;color:#94a3b8;height:30px;overflow:hidden;line-height:1.35;">${escapeHtml(pet.description || "Ready to accompany your coding session.")}</div>
        <div style="display:flex;gap:6px;margin-top:auto;">
          <button type="button" class="abacus-cp-btn ${isSelected ? "" : "primary"}" style="flex:1;" data-summon>${isSelected ? "Active Companion" : "Summon"}</button>
          ${installedPets.length > 1 ? `<button type="button" class="abacus-cp-btn" data-remove title="Remove pet">Remove</button>` : ""}
        </div>
      `;

      card.querySelector("[data-summon]").onclick = () => {
        activePetObj = pet;
        savePetPrefs({ activePetId: pet.id, enabled: true });
        setPetTemporaryState("waving", `Hi! I'm ${pet.displayName}!`, 3500, "success");
        renderInstalledPetsGrid();
        syncPetStudioControls();
      };

      const removeBtn = card.querySelector("[data-remove]");
      if (removeBtn) {
        removeBtn.onclick = async () => {
          await window.abacus.pets.removeInstalled(pet.id);
          await refreshInstalledPets();
          showToast(`Removed ${pet.displayName}`);
        };
      }
      grid.appendChild(card);
    });
  }

  function renderCatalogPetsGrid() {
    const grid = document.getElementById("abacus-pets-catalog-grid");
    if (!grid) return;
    if (!catalogPets.length) {
      grid.innerHTML = `<div class="abacus-cp-empty" style="grid-column:1/-1;">No matching pets found on codex-pets.net.</div>`;
      return;
    }
    grid.innerHTML = "";
    const installedIds = new Set(installedPets.map(p => p.id));

    catalogPets.forEach(pet => {
      const vNum = pet.spriteVersionNumber === 2 || pet.validationReport?.spriteVersionNumber === 2 ? 2 : 1;
      const isInstalled = installedIds.has(pet.id);
      const isCurrent = activePetObj && activePetObj.id === pet.id;
      const posterSrc = pet.posterUrl?.startsWith("/")
        ? `https://codex-pets.net${pet.posterUrl}`
        : (pet.posterUrl || `https://codex-pets.net${pet.spritesheetUrl}`);

      const card = document.createElement("div");
      card.className = `abacus-pet-item-card ${isCurrent ? "active" : ""}`;
      card.innerHTML = `
        <div class="abacus-pet-thumb-box">
          <span class="abacus-pet-badge ${vNum === 2 ? "v2" : ""}">v${vNum}${vNum === 2 ? " · 360°" : ""}</span>
          <img src="${escapeHtml(posterSrc)}" alt="${escapeHtml(pet.displayName)}" loading="lazy" style="height:78px;width:auto;object-fit:contain;image-rendering:pixelated;" />
        </div>
        <div style="display:flex;align-items:center;justify-content:space-between;gap:6px;">
          <div style="font-weight:700;font-size:12.5px;color:#f8fafc;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;" title="${escapeHtml(pet.displayName)}">${escapeHtml(pet.displayName)}</div>
          <span style="font-size:10.5px;color:#94a3b8;">Likes: ${Number(pet.likeCount || 0)}</span>
        </div>
        <div style="font-size:11px;color:#94a3b8;height:30px;overflow:hidden;line-height:1.35;" title="${escapeHtml(pet.description || "")}">${escapeHtml(pet.description || `By @${pet.ownerHandle || pet.ownerName || "community"}`)}</div>
        <div style="display:flex;gap:6px;margin-top:auto;">
          <button type="button" class="abacus-cp-btn primary" style="flex:1;" data-install>${isCurrent ? "Active" : isInstalled ? "Summon" : "Install & Summon"}</button>
        </div>
      `;

      const btn = card.querySelector("[data-install]");
      btn.onclick = async () => {
        if (isInstalled) {
          const existing = installedPets.find(p => p.id === pet.id);
          if (existing) {
            activePetObj = existing;
            savePetPrefs({ activePetId: existing.id, enabled: true });
            setPetTemporaryState("waving", `Hi! I'm ${existing.displayName}!`, 3500, "success");
            renderCatalogPetsGrid();
            renderInstalledPetsGrid();
            syncPetStudioControls();
            return;
          }
        }
        btn.disabled = true;
        btn.textContent = "Downloading…";
        try {
          const installed = await window.abacus.pets.installRemote(pet);
          await refreshInstalledPets();
          if (installed) {
            activePetObj = installedPets.find(p => p.id === installed.id) || installed;
            savePetPrefs({ activePetId: activePetObj.id, enabled: true });
            setPetTemporaryState("jumping", `${activePetObj.displayName} joined Abacus!`, 4200, "success");
            showToast(`Installed & summoned ${activePetObj.displayName}!`);
          }
          renderCatalogPetsGrid();
          syncPetStudioControls();
        } catch (err) {
          btn.disabled = false;
          btn.textContent = "Retry Install";
          showToast(`Install failed: ${err.message || err}`);
        }
      };

      grid.appendChild(card);
    });
  }

  function syncPetStudioControls() {
    const enabledChk = document.getElementById("abacus-pet-toggle-enabled");
    const settingsEnabledChk = document.getElementById("abacus-pet-settings-enabled");
    const summaryEl = document.getElementById("abacus-pet-settings-summary");
    const roamChk = document.getElementById("abacus-pet-toggle-roaming");
    const lookChk = document.getElementById("abacus-pet-toggle-look");
    const bubbleChk = document.getElementById("abacus-pet-toggle-bubble");
    const soundChk = document.getElementById("abacus-pet-toggle-sound");
    const notifSelect = document.getElementById("abacus-pet-select-notif");
    const sizeSlider = document.getElementById("abacus-pet-size-slider");
    const sizeVal = document.getElementById("abacus-pet-size-val");

    if (enabledChk) enabledChk.checked = Boolean(petPrefs.enabled);
    if (settingsEnabledChk) settingsEnabledChk.checked = Boolean(petPrefs.enabled);
    if (summaryEl) {
      const petName = activePetObj ? activePetObj.displayName : "None selected";
      const verLabel = activePetObj && activePetObj.spriteVersionNumber === 2 ? "v2" : "v1";
      const statusText = petPrefs.enabled ? "Active on desktop" : "Hidden";
      summaryEl.textContent = `${petName} (${verLabel}) · ${statusText} · ${installedPets.length} installed`;
    }
    if (roamChk) roamChk.checked = Boolean(petPrefs.roaming);
    if (lookChk) lookChk.checked = Boolean(petPrefs.cursorLook);
    if (bubbleChk) bubbleChk.checked = Boolean(petPrefs.speechBubbles);
    if (soundChk) soundChk.checked = Boolean(petPrefs.soundEffects);
    if (notifSelect) notifSelect.value = petPrefs.osNotifications || "always";
    if (sizeSlider) sizeSlider.value = String(petPrefs.size || 96);
    if (sizeVal) sizeVal.textContent = `${petPrefs.size || 96}px`;

    document.querySelectorAll("#abacus-pet-state-pills .abacus-pets-state-pill").forEach(btn => {
      btn.classList.toggle("active", btn.dataset.state === (petPrefs.manualState || "auto"));
    });
  }

  function openPetsStudioModal() {
    const modal = document.getElementById("abacus-modal-pets");
    if (!modal) return;
    modal.classList.add("open");
    syncPetStudioControls();
    refreshInstalledPets();
    if (catalogPets.length === 0 && !catalogLoading) {
      loadCodexPetsCatalog();
    }
  }

  function mountPetsIntoSettings(appearanceSection, settingsContainer) {
    if (!settingsContainer || document.getElementById("abacus-settings-pets-section")) return;

    const section = document.createElement("section");
    section.id = "abacus-settings-pets-section";
    section.className = "flex flex-col gap-3";
    section.innerHTML = `
      <div class="flex flex-col gap-1">
        <h2 class="abacus-label">Desktop Pets</h2>
        <p class="text-ui-sm text-foreground-subtlest">Floating transparent companion outside the Abacus window, live task alerts, and codex-pets.net hub.</p>
      </div>

      <div class="flex flex-col divide-y rounded-xl border bg-card">
        <div class="flex items-center justify-between gap-4 px-4 py-3.5">
          <div class="flex min-w-0 flex-1 flex-col gap-0.5">
            <span class="text-ui-base">Floating Desktop Companion</span>
            <span class="text-ui-sm text-foreground-subtlest">Show your draggable pet overlay anywhere on desktop even when Abacus is hidden.</span>
          </div>
          <div style="display:flex;align-items:center;gap:12px;flex-shrink:0;">
            <button type="button" class="abacus-cp-btn" id="abacus-pet-settings-hide-btn" title="Hide Abacus window — your floating pet keeps watching & notifying you">Hide Abacus Window</button>
            <input type="checkbox" id="abacus-pet-settings-enabled" style="width:16px;height:16px;cursor:pointer;" />
          </div>
        </div>

        <div class="flex items-center justify-between gap-4 px-4 py-3.5">
          <div class="flex min-w-0 flex-1 flex-col gap-0.5">
            <span class="text-ui-base">Pets Studio & Codex-Pets Hub</span>
            <span class="text-ui-sm text-foreground-subtlest" id="abacus-pet-settings-summary">Loading installed companions…</span>
          </div>
          <div style="flex-shrink:0;">
            <button type="button" class="abacus-cp-btn primary" id="abacus-pet-settings-open-btn">Open Pets Studio</button>
          </div>
        </div>
      </div>
    `;

    if (appearanceSection && appearanceSection.parentElement === settingsContainer) {
      appearanceSection.insertAdjacentElement("afterend", section);
    } else {
      settingsContainer.appendChild(section);
    }

    document.getElementById("abacus-pet-settings-enabled").onchange = (e) => {
      savePetPrefs({ enabled: e.target.checked });
      syncPetStudioControls();
    };
    document.getElementById("abacus-pet-settings-hide-btn").onclick = () => {
      if (window.abacus && window.abacus.pets && window.abacus.pets.hideMainWindow) {
        window.abacus.pets.hideMainWindow();
      }
    };
    document.getElementById("abacus-pet-settings-open-btn").onclick = () => {
      openPetsStudioModal();
    };

    syncPetStudioControls();
  }

  function initPetSystem() {
    // 1. Clean up any legacy embedded stage so only the free-floating Desktop Overlay Window is rendered
    const legacyStage = document.getElementById("abacus-pet-stage");
    if (legacyStage) legacyStage.remove();

    // 2. Append Spacious Pets Studio Modal (960x680) into #abacus-ext-modals
    const modalsWrap = document.getElementById("abacus-ext-modals");
    if (modalsWrap && !document.getElementById("abacus-modal-pets")) {
      const petModal = document.createElement("div");
      petModal.id = "abacus-modal-pets";
      petModal.className = "abacus-modal-overlay";
      petModal.innerHTML = `
        <div class="abacus-pets-modal-content">
          <div class="abacus-pets-topbar">
            <div style="display:flex;align-items:center;gap:14px;">
              <span style="font-size:15px;font-weight:700;color:#f8fafc;">Abacus Pets & Codex-Pets Hub</span>
              <div class="abacus-pets-tabs">
                <button type="button" class="abacus-pets-tab active" data-pet-tab="my-pets">My Pets & Settings</button>
                <button type="button" class="abacus-pets-tab" data-pet-tab="online-hub">Codex-Pets.net Online Hub (3,680+)</button>
              </div>
            </div>
            <div style="display:flex;gap:8px;align-items:center;">
              <button type="button" class="abacus-cp-btn primary" id="abacus-pets-hide-app-btn" title="Hide Abacus window — your floating pet keeps watching & notifying you">Hide Abacus Window</button>
              <button type="button" class="abacus-cp-btn" id="abacus-pets-close">Close</button>
            </div>
          </div>

          <div class="abacus-pets-body">
            <!-- TAB 1: MY PETS & SETTINGS -->
            <div class="abacus-pets-pane active" id="abacus-pets-pane-my-pets">
              <div class="abacus-pets-split">
                <!-- Left Column: Live Stage Preview & Behavior Controls -->
                <div class="abacus-pets-card">
                  <div class="abacus-pets-stage-preview">
                    <div id="abacus-pets-studio-preview-sprite" style="image-rendering:pixelated;"></div>
                    <div id="abacus-pets-studio-preview-meta" style="margin-top:8px;font-size:11px;color:#94a3b8;font-weight:600;text-align:center;">Loading pet…</div>
                  </div>

                  <div class="abacus-pets-toggle-row">
                    <span class="abacus-pets-toggle-label">Show Floating Desktop Companion</span>
                    <input type="checkbox" id="abacus-pet-toggle-enabled" checked />
                  </div>
                  <div class="abacus-pets-toggle-row">
                    <span class="abacus-pets-toggle-label">Free Screen-Wide Roaming</span>
                    <input type="checkbox" id="abacus-pet-toggle-roaming" checked />
                  </div>
                  <div class="abacus-pets-toggle-row">
                    <span class="abacus-pets-toggle-label">v2 360° Global Cursor Tracking</span>
                    <input type="checkbox" id="abacus-pet-toggle-look" checked />
                  </div>
                  <div class="abacus-pets-toggle-row">
                    <span class="abacus-pets-toggle-label">Live Agent Speech Bubbles</span>
                    <input type="checkbox" id="abacus-pet-toggle-bubble" checked />
                  </div>
                  <div class="abacus-pets-toggle-row">
                    <span class="abacus-pets-toggle-label">Synth Chime Sound Effects</span>
                    <input type="checkbox" id="abacus-pet-toggle-sound" checked />
                  </div>
                  <div class="abacus-pets-toggle-row" style="border-bottom:none;">
                    <span class="abacus-pets-toggle-label">Desktop OS Notifications</span>
                    <select id="abacus-pet-select-notif" class="abacus-pets-select" style="padding:4px 8px;">
                      <option value="always">Always Notify</option>
                      <option value="unfocused">When Unfocused</option>
                      <option value="off">Off</option>
                    </select>
                  </div>

                  <div class="abacus-slider-row" style="margin-top:10px;">
                    <span class="abacus-slider-label">Companion Size</span>
                    <input type="range" class="abacus-slider" id="abacus-pet-size-slider" min="56" max="156" value="${petPrefs.size || 96}" />
                    <span class="abacus-slider-val" id="abacus-pet-size-val">${petPrefs.size || 96}px</span>
                  </div>

                  <div style="margin-top:12px;">
                    <div style="font-size:11px;font-weight:700;text-transform:uppercase;color:#94a3b8;margin-bottom:4px;">Animation State Override</div>
                    <div class="abacus-pets-state-pills" id="abacus-pet-state-pills">
                      <button type="button" class="abacus-pets-state-pill active" data-state="auto">Auto (Live Agent Sync)</button>
                      <button type="button" class="abacus-pets-state-pill" data-state="idle">Idle</button>
                      <button type="button" class="abacus-pets-state-pill" data-state="running">Coding</button>
                      <button type="button" class="abacus-pets-state-pill" data-state="review">Review</button>
                      <button type="button" class="abacus-pets-state-pill" data-state="waving">Waving</button>
                      <button type="button" class="abacus-pets-state-pill" data-state="jumping">Jumping</button>
                      <button type="button" class="abacus-pets-state-pill" data-state="waiting">Waiting</button>
                      <button type="button" class="abacus-pets-state-pill" data-state="failed">Failed</button>
                      <button type="button" class="abacus-pets-state-pill" data-state="running-right">Run R</button>
                      <button type="button" class="abacus-pets-state-pill" data-state="running-left">Run L</button>
                    </div>
                  </div>

                  <div style="display:flex;gap:8px;margin-top:14px;">
                    <button type="button" class="abacus-cp-btn primary" id="abacus-pet-test-notif" style="flex:1;">Test Alert & Celebration</button>
                    <button type="button" class="abacus-cp-btn" id="abacus-pet-reset-pos">Reset Position</button>
                  </div>
                </div>

                <!-- Right Column: Installed Pets & Direct Importer -->
                <div style="display:flex;flex-direction:column;gap:14px;">
                  <div class="abacus-pets-card">
                    <div style="font-size:12px;font-weight:700;text-transform:uppercase;letter-spacing:0.05em;color:#94a3b8;margin-bottom:8px;">
                      Import Pet from codex-pets.net (URL, ID, or .zip Package)
                    </div>
                    <div style="display:flex;gap:8px;align-items:center;flex-wrap:wrap;">
                      <input type="text" id="abacus-pet-import-input" class="abacus-pets-input" style="flex:1;min-width:220px;" placeholder="Paste https://codex-pets.net/#/pets/... or pet ID (e.g. guga, dario, white-cat)" />
                      <button type="button" class="abacus-cp-btn primary" id="abacus-pet-import-url-btn">Fetch & Install</button>
                      <input type="file" id="abacus-pet-import-zip" accept=".zip" style="display:none;" />
                      <button type="button" class="abacus-cp-btn" id="abacus-pet-import-zip-btn">Import .zip File…</button>
                    </div>
                  </div>

                  <div class="abacus-pets-card" style="flex:1;">
                    <div style="display:flex;align-items:center;justify-content:space-between;margin-bottom:6px;">
                      <span style="font-size:12px;font-weight:700;text-transform:uppercase;letter-spacing:0.05em;color:#94a3b8;">Installed Companions (~/.abacus/pets & ~/.codex/pets)</span>
                      <button type="button" class="abacus-cp-btn" id="abacus-pet-browse-hub-cta">Browse Online Hub &rarr;</button>
                    </div>
                    <div class="abacus-pets-installed-grid" id="abacus-pets-installed-grid"></div>
                  </div>
                </div>
              </div>
            </div>

            <!-- TAB 2: CODEX-PETS.NET ONLINE HUB -->
            <div class="abacus-pets-pane" id="abacus-pets-pane-online-hub">
              <div class="abacus-pets-Controls-row">
                <input type="text" id="abacus-pets-hub-search" class="abacus-pets-input" style="flex:1;min-width:220px;" placeholder="Search 3,680+ pets on codex-pets.net by name, anime, cat, robot, author…" />
                <select id="abacus-pets-hub-sort" class="abacus-pets-select">
                  <option value="popular">Sort: Most Popular</option>
                  <option value="new">Sort: Newest Uploads</option>
                  <option value="random">Sort: Random Discovery</option>
                </select>
                <select id="abacus-pets-hub-kind" class="abacus-pets-select">
                  <option value="all">Kind: All Types</option>
                  <option value="animal">Animal</option>
                  <option value="person">Person / Character</option>
                  <option value="creature">Creature</option>
                  <option value="object">Object / Robot</option>
                </select>
                <select id="abacus-pets-hub-version" class="abacus-pets-select">
                  <option value="all">Atlas: v1 & v2</option>
                  <option value="2">v2 Only (11-State + 360° Look)</option>
                  <option value="1">v1 Only (9-State)</option>
                </select>
                <button type="button" class="abacus-cp-btn primary" id="abacus-pets-hub-refresh">Search / Refresh</button>
              </div>
              <div id="abacus-pets-catalog-status" style="font-size:11.5px;color:#94a3b8;margin-bottom:10px;">Ready to browse codex-pets.net</div>
              <div class="abacus-pets-catalog-grid" id="abacus-pets-catalog-grid"></div>
            </div>
          </div>
        </div>
      `;
      modalsWrap.appendChild(petModal);

      // Wire modal tabs & close
      petModal.addEventListener("click", (e) => {
        if (e.target === petModal) petModal.classList.remove("open");
      });
      document.getElementById("abacus-pets-close").onclick = () => petModal.classList.remove("open");
      document.getElementById("abacus-pets-hide-app-btn").onclick = () => {
        petModal.classList.remove("open");
        if (window.abacus && window.abacus.pets && window.abacus.pets.hideMainWindow) {
          window.abacus.pets.hideMainWindow();
        }
      };

      const switchTab = (tabName) => {
        petModal.querySelectorAll(".abacus-pets-tab").forEach(t => {
          t.classList.toggle("active", t.dataset.petTab === tabName);
        });
        document.getElementById("abacus-pets-pane-my-pets").classList.toggle("active", tabName === "my-pets");
        document.getElementById("abacus-pets-pane-online-hub").classList.toggle("active", tabName === "online-hub");
        if (tabName === "online-hub" && catalogPets.length === 0 && !catalogLoading) {
          loadCodexPetsCatalog();
        }
      };

      petModal.querySelectorAll(".abacus-pets-tab").forEach(btn => {
        btn.onclick = () => switchTab(btn.dataset.petTab);
      });
      document.getElementById("abacus-pet-browse-hub-cta").onclick = () => switchTab("online-hub");

      // Settings Toggles (all synced directly to Main -> Floating Overlay Window)
      document.getElementById("abacus-pet-toggle-enabled").onchange = (e) => {
        savePetPrefs({ enabled: e.target.checked });
        syncPetStudioControls();
      };
      document.getElementById("abacus-pet-toggle-roaming").onchange = (e) => {
        savePetPrefs({ roaming: e.target.checked });
      };
      document.getElementById("abacus-pet-toggle-look").onchange = (e) => {
        savePetPrefs({ cursorLook: e.target.checked });
      };
      document.getElementById("abacus-pet-toggle-bubble").onchange = (e) => {
        savePetPrefs({ speechBubbles: e.target.checked });
      };
      document.getElementById("abacus-pet-toggle-sound").onchange = (e) => {
        savePetPrefs({ soundEffects: e.target.checked });
      };
      document.getElementById("abacus-pet-select-notif").onchange = (e) => {
        savePetPrefs({ osNotifications: e.target.value });
      };
      document.getElementById("abacus-pet-size-slider").oninput = (e) => {
        const val = parseInt(e.target.value, 10) || 96;
        document.getElementById("abacus-pet-size-val").textContent = `${val}px`;
        savePetPrefs({ size: val });
      };

      document.querySelectorAll("#abacus-pet-state-pills .abacus-pets-state-pill").forEach(btn => {
        btn.onclick = () => {
          savePetPrefs({ manualState: btn.dataset.state });
          syncPetStudioControls();
          restartSpriteLoop();
        };
      });

      document.getElementById("abacus-pet-test-notif").onclick = async () => {
        const name = activePetObj ? activePetObj.displayName : "Abacus Pet";
        if (window.abacus && window.abacus.pets) {
          window.abacus.pets.sendCommand({
            state: "jumping",
            text: `${name}: Task completed! All checks passed!`,
            duration: 5000,
            status: "success",
            header: "Done",
            sound: "complete"
          });
          window.abacus.pets.notify({
            title: `${name} · Task Completed`,
            body: "Your coding task finished with 0 errors!",
            silent: true
          });
        }
      };

      document.getElementById("abacus-pet-reset-pos").onclick = async () => {
        if (window.abacus && window.abacus.pets && window.abacus.pets.resetPosition) {
          await window.abacus.pets.resetPosition();
        }
        showToast("Reset floating companion position to bottom-right");
      };

      // Import by URL or Pet ID
      const importInput = document.getElementById("abacus-pet-import-input");
      const importBtn = document.getElementById("abacus-pet-import-url-btn");
      importBtn.onclick = async () => {
        const val = (importInput.value || "").trim();
        if (!val) {
          showToast("Enter a codex-pets.net URL or pet ID first");
          return;
        }
        importBtn.disabled = true;
        importBtn.textContent = "Installing…";
        try {
          const installed = await window.abacus.pets.installRemote(val);
          await refreshInstalledPets();
          if (installed) {
            activePetObj = installedPets.find(p => p.id === installed.id) || installed;
            savePetPrefs({ activePetId: activePetObj.id, enabled: true });
            importInput.value = "";
            setPetTemporaryState("jumping", `${activePetObj.displayName} installed from codex-pets.net!`, 4500, "success");
            showToast(`Installed & summoned ${activePetObj.displayName}!`);
          }
        } catch (err) {
          showToast(`Import failed: ${err.message || err}`);
        } finally {
          importBtn.disabled = false;
          importBtn.textContent = "Fetch & Install";
        }
      };

      // Import by .zip file
      const zipInput = document.getElementById("abacus-pet-import-zip");
      const zipBtn = document.getElementById("abacus-pet-import-zip-btn");
      zipBtn.onclick = () => zipInput.click();
      zipInput.onchange = async (e) => {
        const file = e.target.files && e.target.files[0];
        if (!file) return;
        zipBtn.disabled = true;
        zipBtn.textContent = "Extracting…";
        try {
          const payload = await extractCodexPetZip(file);
          const installed = await window.abacus.pets.saveCustom(payload);
          await refreshInstalledPets();
          if (installed) {
            activePetObj = installedPets.find(p => p.id === installed.id) || installed;
            savePetPrefs({ activePetId: activePetObj.id, enabled: true });
            setPetTemporaryState("jumping", `${activePetObj.displayName} imported from ZIP!`, 4500, "success");
            showToast(`Imported ${activePetObj.displayName}!`);
          }
        } catch (err) {
          showToast(`ZIP import failed: ${err.message || err}`);
        } finally {
          zipBtn.disabled = false;
          zipBtn.textContent = "Import .zip File…";
          zipInput.value = "";
        }
      };

      // Hub Search & Filters
      const triggerHubSearch = () => {
        catalogQuery.q = document.getElementById("abacus-pets-hub-search").value || "";
        catalogQuery.sort = document.getElementById("abacus-pets-hub-sort").value || "popular";
        catalogQuery.kind = document.getElementById("abacus-pets-hub-kind").value || "all";
        catalogQuery.version = document.getElementById("abacus-pets-hub-version").value || "all";
        loadCodexPetsCatalog();
      };
      document.getElementById("abacus-pets-hub-refresh").onclick = triggerHubSearch;
      document.getElementById("abacus-pets-hub-sort").onchange = triggerHubSearch;
      document.getElementById("abacus-pets-hub-kind").onchange = triggerHubSearch;
      document.getElementById("abacus-pets-hub-version").onchange = triggerHubSearch;
      document.getElementById("abacus-pets-hub-search").addEventListener("keydown", (e) => {
        if (e.key === "Enter") triggerHubSearch();
      });
    }

    if (window.abacus && window.abacus.pets && window.abacus.pets.onOpenStudio) {
      window.abacus.pets.onOpenStudio(() => openPetsStudioModal());
    }

    if (window.abacus && window.abacus.pets && window.abacus.pets.onOverlayEvent) {
      window.abacus.pets.onOverlayEvent((ev) => {
        if (ev && ev.type === "state") {
          if (ev.config) Object.assign(petPrefs, ev.config);
          if (Array.isArray(ev.installed)) installedPets = ev.installed;
          activePetObj = installedPets.find(p => p.id === petPrefs.activePetId) || installedPets[0] || null;
          updateSidebarPetBadge();
          syncPetStudioControls();
          renderStudioPreviewFrame();
          renderInstalledPetsGrid();
        }
      });
    }

    ensureSettingsExtensions();
    // 3. Load installed pets & start studio preview loop
    refreshInstalledPets().then(() => {
      restartSpriteLoop();
    });
  }

  // Initialize
  async function init() {
    // Check if a saved wallpaper exists in IndexedDB
    try {
      const savedBlob = await loadWallpaperBlob();
      if (savedBlob) {
        currentWallpaperUrl = URL.createObjectURL(savedBlob);
        themeState.hasBg = true;
      }
    } catch (e) {}

    applyTheme();
    setupUI();
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }
})();
