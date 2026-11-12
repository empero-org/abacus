"use strict";

// src/preload/index.ts
var import_electron = require("electron");
var bridge = {
  appInfo: () => import_electron.ipcRenderer.invoke("app:info"),
  detectCli: () => import_electron.ipcRenderer.invoke("cli:detect"),
  checkForUpdate: () => import_electron.ipcRenderer.invoke("cli:checkUpdate"),
  checkUpdates: () => import_electron.ipcRenderer.invoke("updates:check"),
  recentWorkspaces: () => import_electron.ipcRenderer.invoke("workspace:recent"),
  listProjects: () => import_electron.ipcRenderer.invoke("projects:list"),
  syncStatus: () => import_electron.ipcRenderer.invoke("sync:status"),
  pairPhone: () => import_electron.ipcRenderer.invoke("sync:pair"),
  forgetWorkspace: (path) => import_electron.ipcRenderer.invoke("workspace:forget", path),
  pickWorkspace: () => import_electron.ipcRenderer.invoke("workspace:pick"),
  openWorkspace: (path, keep) => import_electron.ipcRenderer.invoke("workspace:open", path, keep),
  activateSession: (server, keep) => import_electron.ipcRenderer.invoke("session:activate", server, keep),
  stopSession: (server) => import_electron.ipcRenderer.invoke("session:stop", server),
  deleteSession: (id) => import_electron.ipcRenderer.invoke("session:delete", id),
  closeWorkspace: () => import_electron.ipcRenderer.invoke("workspace:close"),
  newWindow: (path) => import_electron.ipcRenderer.invoke("window:new", path),
  launchWorkspace: () => import_electron.ipcRenderer.invoke("workspace:launch"),
  request: (method, params, server) => import_electron.ipcRenderer.invoke("abacus:request", method, params, server),
  respond: (id, result, server) => import_electron.ipcRenderer.invoke("abacus:respond", id, result, server),
  onMessage: (listener) => {
    const handler = (_event, message) => listener(message);
    import_electron.ipcRenderer.on("abacus:message", handler);
    return () => import_electron.ipcRenderer.removeListener("abacus:message", handler);
  },
  openTerminal: (cols, rows, run) => import_electron.ipcRenderer.invoke("terminal:open", cols, rows, run),
  writeTerminal: (id, data) => import_electron.ipcRenderer.send("terminal:write", id, data),
  resizeTerminal: (id, cols, rows) => import_electron.ipcRenderer.send("terminal:resize", id, cols, rows),
  closeTerminal: (id) => import_electron.ipcRenderer.send("terminal:close", id),
  terminalIdle: (id) => import_electron.ipcRenderer.invoke("terminal:idle", id),
  onTerminal: (listener) => {
    const handler = (_event, event) => listener(event);
    import_electron.ipcRenderer.on("terminal:event", handler);
    return () => import_electron.ipcRenderer.removeListener("terminal:event", handler);
  },
  browser: {
    state: () => import_electron.ipcRenderer.invoke("browser:state"),
    open: (url) => import_electron.ipcRenderer.invoke("browser:open", url),
    detectDevServer: () => import_electron.ipcRenderer.invoke("browser:detect"),
    navigate: (id, url) => import_electron.ipcRenderer.send("browser:navigate", id, url),
    back: (id) => import_electron.ipcRenderer.send("browser:back", id),
    forward: (id) => import_electron.ipcRenderer.send("browser:forward", id),
    reload: (id) => import_electron.ipcRenderer.send("browser:reload", id),
    stop: (id) => import_electron.ipcRenderer.send("browser:stop", id),
    select: (id) => import_electron.ipcRenderer.send("browser:select", id),
    close: (id) => import_electron.ipcRenderer.send("browser:close", id),
    setBounds: (bounds) => import_electron.ipcRenderer.send("browser:bounds", bounds),
    popOut: () => import_electron.ipcRenderer.invoke("browser:popout"),
    dock: () => import_electron.ipcRenderer.invoke("browser:dock"),
    onState: (listener) => {
      const handler = (_event, state) => listener(state);
      import_electron.ipcRenderer.on("browser:state", handler);
      return () => import_electron.ipcRenderer.removeListener("browser:state", handler);
    },
    onShow: (listener) => {
      const handler = () => listener();
      import_electron.ipcRenderer.on("browser:show", handler);
      return () => import_electron.ipcRenderer.removeListener("browser:show", handler);
    }
  },
  groups: {
    list: () => import_electron.ipcRenderer.invoke("groups:list"),
    save: (groups) => import_electron.ipcRenderer.invoke("groups:save", groups),
    onChange: (listener) => {
      const handler = (_event, groups) => listener(groups);
      import_electron.ipcRenderer.on("groups:changed", handler);
      return () => import_electron.ipcRenderer.removeListener("groups:changed", handler);
    }
  },
  pins: {
    list: () => import_electron.ipcRenderer.invoke("pins:list"),
    save: (pins) => import_electron.ipcRenderer.invoke("pins:save", pins),
    onChange: (listener) => {
      const handler = (_event, pins) => listener(pins);
      import_electron.ipcRenderer.on("pins:changed", handler);
      return () => import_electron.ipcRenderer.removeListener("pins:changed", handler);
    }
  },
  files: {
    info: (paths) => import_electron.ipcRenderer.invoke("files:info", paths),
    text: (path) => import_electron.ipcRenderer.invoke("files:text", path),
    open: (path) => import_electron.ipcRenderer.invoke("files:open", path),
    browse: (path) => import_electron.ipcRenderer.invoke("files:browse", path),
    url: (path) => `abacus-file://local/${encodeURIComponent(path)}`
  },
  // Electron no longer puts the path on File; only the preload can ask for it.
  pathForFile: (file) => import_electron.webUtils.getPathForFile(file) || null,
  openExternal: (url) => import_electron.ipcRenderer.invoke("shell:openExternal", url),
  revealPath: (path) => import_electron.ipcRenderer.invoke("shell:reveal", path),
  pets: {
    catalog: (params) => import_electron.ipcRenderer.invoke("pets:catalog", params),
    getRemote: (idOrUrl) => import_electron.ipcRenderer.invoke("pets:getRemote", idOrUrl),
    listInstalled: () => import_electron.ipcRenderer.invoke("pets:listInstalled"),
    installRemote: (petOrId) => import_electron.ipcRenderer.invoke("pets:installRemote", petOrId),
    saveCustom: (payload) => import_electron.ipcRenderer.invoke("pets:saveCustom", payload),
    removeInstalled: (id) => import_electron.ipcRenderer.invoke("pets:removeInstalled", id),
    notify: (payload) => import_electron.ipcRenderer.invoke("pets:notify", payload),
    getConfig: () => import_electron.ipcRenderer.invoke("pets:getConfig"),
    setConfig: (patch) => import_electron.ipcRenderer.invoke("pets:setConfig", patch),
    getOverlayState: () => import_electron.ipcRenderer.invoke("pets:getOverlayState"),
    sendCommand: (cmd) => import_electron.ipcRenderer.invoke("pets:sendCommand", cmd),
    resetPosition: () => import_electron.ipcRenderer.invoke("pets:resetPosition"),
    setInteractiveRects: (rects) => import_electron.ipcRenderer.send("pets:setInteractiveRects", rects),
    wakeMouse: () => import_electron.ipcRenderer.send("pets:wakeMouse"),
    dragStart: () => import_electron.ipcRenderer.send("pets:dragStart"),
    dragStep: () => import_electron.ipcRenderer.send("pets:dragStep"),
    dragEnd: () => import_electron.ipcRenderer.invoke("pets:dragEnd"),
    roamStep: (dx) => import_electron.ipcRenderer.invoke("pets:roamStep", dx),
    showMainWindow: () => import_electron.ipcRenderer.invoke("pets:showMainWindow"),
    hideMainWindow: () => import_electron.ipcRenderer.invoke("pets:hideMainWindow"),
    toggleMainWindow: () => import_electron.ipcRenderer.invoke("pets:toggleMainWindow"),
    openStudio: () => import_electron.ipcRenderer.invoke("pets:openStudio"),
    onOverlayEvent: (listener) => {
      const handler = (_event, payload) => listener(payload);
      import_electron.ipcRenderer.on("pet:overlayEvent", handler);
      return () => import_electron.ipcRenderer.removeListener("pet:overlayEvent", handler);
    },
    onOpenStudio: (listener) => {
      const handler = () => listener();
      import_electron.ipcRenderer.on("pet:openStudio", handler);
      return () => import_electron.ipcRenderer.removeListener("pet:openStudio", handler);
    }
  }
};
import_electron.contextBridge.exposeInMainWorld("abacus", bridge);
//# sourceMappingURL=index.cjs.map
