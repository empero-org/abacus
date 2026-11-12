"use strict";
var __create = Object.create;
var __defProp = Object.defineProperty;
var __getOwnPropDesc = Object.getOwnPropertyDescriptor;
var __getOwnPropNames = Object.getOwnPropertyNames;
var __getProtoOf = Object.getPrototypeOf;
var __hasOwnProp = Object.prototype.hasOwnProperty;
var __copyProps = (to, from, except, desc) => {
  if (from && typeof from === "object" || typeof from === "function") {
    for (let key2 of __getOwnPropNames(from))
      if (!__hasOwnProp.call(to, key2) && key2 !== except)
        __defProp(to, key2, { get: () => from[key2], enumerable: !(desc = __getOwnPropDesc(from, key2)) || desc.enumerable });
  }
  return to;
};
var __toESM = (mod, isNodeMode, target) => (target = mod != null ? __create(__getProtoOf(mod)) : {}, __copyProps(
  // If the importer is in node compatibility mode or this is not an ESM
  // file that has been converted to a CommonJS file using a Babel-
  // compatible transform (i.e. "__esModule" has not been set), then set
  // "default" to the CommonJS "module.exports" for node compatibility.
  isNodeMode || !mod || !mod.__esModule ? __defProp(target, "default", { value: mod, enumerable: true }) : target,
  mod
));

// src/main/index.ts
var import_electron7 = require("electron");
var import_node_fs9 = require("node:fs");
var import_node_os3 = require("node:os");
var import_node_path10 = require("node:path");

// src/main/appServer.ts
var import_node_child_process = require("node:child_process");
var import_node_events = require("node:events");
var import_node_readline = require("node:readline");
var IDLE_EXIT_SECONDS = 300;
var STDERR_TAIL = 200;
var AppServerError = class extends Error {
  constructor(message, code, data) {
    super(message);
    this.code = code;
    this.data = data;
    this.name = "AppServerError";
  }
};
var AppServer = class extends import_node_events.EventEmitter {
  constructor(bin, workspace, args = []) {
    super();
    this.bin = bin;
    this.workspace = workspace;
    this.args = args;
  }
  #child = null;
  #pending = /* @__PURE__ */ new Map();
  #nextId = 1;
  #stderr = [];
  get running() {
    return this.#child !== null && this.#child.exitCode === null;
  }
  start() {
    const child = (0, import_node_child_process.spawn)(this.bin, [this.workspace, "app-server", ...this.args], {
      cwd: this.workspace,
      env: {
        ...process.env,
        NO_COLOR: "1",
        // One set where the app was started wins: 0 keeps servers running.
        ABACUS_IDLE_EXIT_SECS: process.env.ABACUS_IDLE_EXIT_SECS ?? String(IDLE_EXIT_SECONDS)
      },
      stdio: ["pipe", "pipe", "pipe"],
      windowsHide: true
    });
    this.#child = child;
    (0, import_node_readline.createInterface)({ input: child.stdout }).on("line", (line) => this.handleLine(line));
    (0, import_node_readline.createInterface)({ input: child.stderr }).on("line", (line) => {
      this.#stderr.push(line);
      if (this.#stderr.length > STDERR_TAIL) this.#stderr.shift();
      this.emit("log", line);
    });
    child.on("exit", (code) => this.#closed(code));
    child.on("error", (error) => {
      this.#stderr.push(error.message);
      this.#closed(-1);
    });
  }
  /** Parse one stdout line. Public so the framing can be tested without a process. */
  handleLine(line) {
    if (!line.trim()) return;
    let frame;
    try {
      frame = JSON.parse(line);
    } catch {
      this.emit("log", `unparsed stdout: ${line}`);
      return;
    }
    this.#dispatch(frame);
  }
  #dispatch(frame) {
    const hasId = frame.id !== void 0 && frame.id !== null;
    if (hasId && !frame.method) {
      const pending = this.#pending.get(frame.id);
      if (!pending) return;
      this.#pending.delete(frame.id);
      if (frame.error) {
        pending.reject(
          new AppServerError(
            frame.error.message ?? "request failed",
            frame.error.code,
            frame.error.data
          )
        );
      } else {
        pending.resolve(frame.result);
      }
      return;
    }
    if (hasId && frame.method) {
      this.emit("frame", {
        kind: "request",
        id: frame.id,
        method: frame.method,
        params: frame.params ?? {}
      });
      return;
    }
    if (frame.method) {
      this.emit("frame", {
        kind: "notification",
        method: frame.method,
        params: frame.params ?? {}
      });
    }
  }
  #write(frame) {
    if (!this.running || !this.#child)
      throw new AppServerError("the abacus process is not running");
    this.#child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", ...frame })}
`);
  }
  request(method, params = {}) {
    const id = this.#nextId++;
    return new Promise((resolve3, reject) => {
      this.#pending.set(id, { resolve: resolve3, reject });
      try {
        this.#write({ id, method, params });
      } catch (error) {
        this.#pending.delete(id);
        reject(error);
      }
    });
  }
  /** Answer a request the agent is blocked on. */
  respond(id, result) {
    this.#write({ id, result });
  }
  /** Ask politely, then make sure: a hung child must never outlive its window. */
  stop() {
    const child = this.#child;
    if (!child) return;
    try {
      this.#write({ method: "shutdown" });
    } catch {
    }
    setTimeout(() => {
      if (child.exitCode === null) child.kill("SIGTERM");
    }, 1500).unref();
  }
  #closed(code) {
    if (!this.#child) return;
    this.#child = null;
    for (const { reject } of this.#pending.values())
      reject(new AppServerError("the abacus process exited"));
    this.#pending.clear();
    this.emit("exit", code, this.#stderr.slice(-20).join("\n"));
  }
};

// src/main/sessions.ts
var Sessions = class {
  constructor(events) {
    this.events = events;
  }
  #next = 1;
  #windows = /* @__PURE__ */ new Map();
  #window(contents) {
    let window = this.#windows.get(contents.id);
    if (!window) {
      window = { active: null, servers: /* @__PURE__ */ new Map() };
      this.#windows.set(contents.id, window);
    }
    return window;
  }
  /**
   * Start a server in `workspace` and make it the window's. The one it had
   * keeps running when `keep` (its thread is still working), and stops
   * otherwise.
   */
  start(contents, bin, workspace, keep, args = []) {
    const window = this.#window(contents);
    if (!keep) this.stopActive(contents);
    const id = this.#next++;
    const server = new AppServer(bin, workspace, args);
    server.on("frame", (frame) => this.events.frame(contents, id, server, frame));
    server.on("exit", (code, tail) => {
      window.servers.delete(id);
      if (window.active === id) window.active = null;
      this.events.exit(contents, id, code, tail);
    });
    server.on("log", (line) => this.events.log(line));
    server.start();
    window.servers.set(id, server);
    window.active = id;
    return id;
  }
  /** Show a parked server's thread again; the one shown until now is parked when `keep`. */
  activate(contents, id, keep) {
    const window = this.#window(contents);
    const server = window.servers.get(id);
    if (!server?.running) throw new Error("that thread's abacus has stopped; open it again");
    if (window.active !== id && !keep) this.stopActive(contents);
    window.active = id;
    return server;
  }
  /** A server of this window: `id`'s, or the one it shows. */
  get(contents, id) {
    const window = this.#windows.get(contents.id);
    const server = window ? window.servers.get(id ?? window.active ?? -1) : void 0;
    if (!server?.running) throw new Error("no workspace is open in this window");
    return server;
  }
  /** The workspace of the thread the window shows. */
  activeWorkspace(contents) {
    const window = this.#windows.get(contents.id);
    return window?.servers.get(window.active ?? -1)?.workspace ?? null;
  }
  /** Let a server go: a parked one whose thread finished, or the window's own. */
  stop(contents, id) {
    const window = this.#windows.get(contents.id);
    const server = window?.servers.get(id);
    if (!window || !server) return;
    window.servers.delete(id);
    if (window.active === id) window.active = null;
    server.removeAllListeners();
    server.stop();
  }
  stopActive(contents) {
    const active = this.#windows.get(contents.id)?.active;
    if (active !== null && active !== void 0) this.stop(contents, active);
  }
  /** Every server of a window that closed. */
  closeWindow(windowId) {
    const window = this.#windows.get(windowId);
    if (!window) return;
    this.#windows.delete(windowId);
    for (const server of window.servers.values()) {
      server.removeAllListeners();
      server.stop();
    }
  }
  stopAll() {
    for (const id of this.#windows.keys()) this.closeWindow(id);
  }
};

// src/main/argv.ts
var import_node_fs = require("node:fs");
var import_node_path = require("node:path");
function workspaceFromArgv(argv, cwd, packaged) {
  for (const arg of argv.slice(packaged ? 1 : 2)) {
    if (arg.startsWith("-")) continue;
    const path = (0, import_node_path.resolve)(cwd, arg);
    if ((0, import_node_fs.existsSync)(path) && (0, import_node_fs.statSync)(path).isDirectory()) return path;
  }
  return void 0;
}

// src/main/cli.ts
var import_node_child_process2 = require("node:child_process");
var import_node_fs2 = require("node:fs");
var import_node_os = require("node:os");
var import_node_path2 = require("node:path");
var MINIMUM_CLI_VERSION = "0.7.1";
var EXE = process.platform === "win32" ? ".exe" : "";
function candidates(env = process.env, home = (0, import_node_os.homedir)()) {
  const list = [];
  if (env.ABACUS_BIN) list.push(env.ABACUS_BIN);
  list.push(
    (0, import_node_path2.join)(home, ".local", "bin", `abacus${EXE}`),
    (0, import_node_path2.join)(home, ".cargo", "bin", `abacus${EXE}`)
  );
  if (process.platform !== "win32") {
    list.push("/usr/local/bin/abacus", "/opt/homebrew/bin/abacus", "/usr/bin/abacus");
  }
  for (const directory of (env.PATH ?? "").split(import_node_path2.delimiter)) {
    if (directory) list.push((0, import_node_path2.join)(directory, `abacus${EXE}`));
  }
  return list;
}
function resolveCli(env = process.env) {
  return candidates(env).find((candidate) => (0, import_node_fs2.existsSync)(candidate)) ?? null;
}
function parseVersion(output) {
  return output.match(/(\d+\.\d+\.\d+)/)?.[1] ?? null;
}
function compareVersions(left, right) {
  const a = left.split(".").map(Number);
  const b = right.split(".").map(Number);
  for (let index = 0; index < 3; index += 1) {
    const delta = (a[index] ?? 0) - (b[index] ?? 0);
    if (delta !== 0) return delta;
  }
  return 0;
}
function cliSays(args, timeout) {
  const bin = resolveCli();
  if (!bin) return Promise.resolve("The abacus CLI is not installed.");
  return new Promise((resolve3) => {
    (0, import_node_child_process2.execFile)(
      bin,
      args,
      { timeout, windowsHide: true, env: { ...process.env, NO_COLOR: "1" } },
      (error, stdout, stderr) => {
        resolve3((stdout || stderr || (error ? error.message : "")).trim());
      }
    );
  });
}
function cliVersion(bin) {
  return new Promise((resolve3) => {
    (0, import_node_child_process2.execFile)(bin, ["--version"], { timeout: 1e4, windowsHide: true }, (error, stdout) => {
      resolve3(error ? null : parseVersion(String(stdout)));
    });
  });
}
async function detectCli(env = process.env) {
  const path = resolveCli(env);
  if (!path) return { state: "missing" };
  const version = await cliVersion(path);
  if (!version) return { state: "missing" };
  if (compareVersions(version, MINIMUM_CLI_VERSION) < 0) {
    return { state: "outdated", path, version, minimum: MINIMUM_CLI_VERSION };
  }
  return { state: "ready", path, version };
}

// src/main/projects.ts
var import_node_child_process3 = require("node:child_process");
var import_node_path3 = require("node:path");
function listSessions(bin) {
  return new Promise((resolve3) => {
    (0, import_node_child_process3.execFile)(
      bin,
      ["sessions", "--all", "--json"],
      {
        timeout: 2e4,
        maxBuffer: 64 * 1024 * 1024,
        windowsHide: true,
        env: { ...process.env, NO_COLOR: "1" }
      },
      (error, stdout) => {
        if (error) return resolve3(null);
        try {
          const parsed = JSON.parse(stdout);
          resolve3(Array.isArray(parsed.workspaces) ? parsed : null);
        } catch {
          resolve3(null);
        }
      }
    );
  });
}
function mergeProjects(listing, recent2, exists) {
  const projects = /* @__PURE__ */ new Map();
  for (const entry of listing?.workspaces ?? []) {
    projects.set(entry.workspace, {
      path: entry.workspace,
      name: (0, import_node_path3.basename)(entry.workspace) || entry.workspace,
      exists: entry.exists,
      sessions: entry.sessions,
      activeAt: entry.sessions[0]?.updatedAt ?? (/* @__PURE__ */ new Date(0)).toISOString()
    });
  }
  for (const entry of recent2) {
    const known = projects.get(entry.path);
    if (known) {
      if (Date.parse(entry.openedAt) > Date.parse(known.activeAt)) known.activeAt = entry.openedAt;
      continue;
    }
    projects.set(entry.path, {
      path: entry.path,
      name: entry.name,
      exists: exists(entry.path),
      sessions: [],
      activeAt: entry.openedAt
    });
  }
  return [...projects.values()].sort(
    (left, right) => Date.parse(right.activeAt) - Date.parse(left.activeAt)
  );
}

// src/main/browserWindows.ts
var import_electron3 = require("electron");

// src/main/browser.ts
var import_electron2 = require("electron");

// src/main/editMenu.ts
var import_electron = require("electron");
function editMenuItems(params, actions) {
  const items = [];
  const separate = () => {
    if (items.length > 0 && items.at(-1)?.type !== "separator") items.push({ type: "separator" });
  };
  if (params.isEditable && params.misspelledWord) {
    const suggestions = params.dictionarySuggestions.slice(0, 5);
    for (const suggestion of suggestions) {
      items.push({ label: suggestion, click: () => actions.replaceMisspelling(suggestion) });
    }
    if (suggestions.length === 0) items.push({ label: "No suggestions", enabled: false });
    items.push({
      label: `Add \u201C${params.misspelledWord}\u201D to the dictionary`,
      click: () => actions.learn(params.misspelledWord)
    });
    separate();
  }
  if (params.linkURL) {
    items.push({ label: "Copy link", click: () => actions.copyLink(params.linkURL) });
    separate();
  }
  if (params.isEditable) {
    items.push(
      { role: "cut", enabled: params.editFlags.canCut },
      { role: "copy", enabled: params.editFlags.canCopy },
      { role: "paste", enabled: params.editFlags.canPaste },
      { type: "separator" },
      { role: "selectAll", enabled: params.editFlags.canSelectAll }
    );
  } else if (params.selectionText.trim()) {
    items.push({ role: "copy" });
  }
  while (items.at(-1)?.type === "separator") items.pop();
  return items;
}
function editWithRightClick(contents) {
  contents.on("context-menu", (_event, params) => {
    const items = editMenuItems(params, {
      replaceMisspelling: (word) => contents.replaceMisspelling(word),
      learn: (word) => contents.session.addWordToSpellCheckerDictionary(word),
      copyLink: (url) => import_electron.clipboard.writeText(url)
    });
    if (items.length > 0) import_electron.Menu.buildFromTemplate(items).popup();
  });
}

// src/main/browser.ts
var PARTITION = "persist:abacus-browser";
var sessionReady = false;
function browserSession() {
  const browsing = import_electron2.session.fromPartition(PARTITION);
  if (!sessionReady) {
    sessionReady = true;
    browsing.setPermissionRequestHandler((_contents, _permission, decide) => decide(false));
    browsing.setPermissionCheckHandler(() => false);
  }
  return browsing;
}
function toUrl(input) {
  const text = input.trim();
  if (!text) return "about:blank";
  if (/^[a-z][a-z0-9+.-]*:/i.test(text) && !/^localhost:\d/i.test(text)) return text;
  const local = /^(localhost|127\.0\.0\.1|\[::1\]|0\.0\.0\.0)(:\d+)?(\/|$)/i.test(text);
  if (local) return `http://${text}`;
  if (!/\s/.test(text) && /^[\w-]+(\.[\w-]+)+(:\d+)?(\/.*)?$/.test(text)) return `https://${text}`;
  return `https://duckduckgo.com/?q=${encodeURIComponent(text)}`;
}
var Browser = class {
  /** The window the pages are shown in: the app's, or a window of their own (popped out). */
  #window;
  #poppedOut = false;
  #emit;
  #tabs = /* @__PURE__ */ new Map();
  #order = [];
  #active = null;
  #bounds = null;
  #shown = null;
  #agent = false;
  #next = 1;
  constructor(window, emit) {
    this.#window = window;
    this.#emit = emit;
  }
  state() {
    return {
      tabs: this.#order.map((id) => {
        const contents = this.#tabs.get(id).webContents;
        return {
          id,
          url: contents.getURL(),
          title: contents.getTitle(),
          loading: contents.isLoading(),
          canGoBack: contents.navigationHistory.canGoBack(),
          canGoForward: contents.navigationHistory.canGoForward()
        };
      }),
      active: this.#active,
      agent: this.#agent,
      poppedOut: this.#poppedOut
    };
  }
  /**
   * Show the pages in `window` from now on: a window of their own when popped
   * out, the app's again when docked. They stay hidden until that window's
   * pane says where they go.
   */
  attach(window, poppedOut) {
    if (this.#shown) {
      if (!this.#window.isDestroyed()) this.#window.contentView.removeChildView(this.#shown);
      this.#shown = null;
    }
    this.#window = window;
    this.#poppedOut = poppedOut;
    this.#bounds = null;
    this.#emit(this.state());
  }
  /** Whether `contents` is the window the pages are shown in, whose pane places them. */
  hostedBy(contents) {
    return !this.#window.isDestroyed() && this.#window.webContents === contents;
  }
  open(input) {
    const view = new import_electron2.WebContentsView({
      webPreferences: {
        session: browserSession(),
        sandbox: true,
        contextIsolation: true,
        nodeIntegration: false
      }
    });
    view.setBackgroundColor("#ffffff");
    editWithRightClick(view.webContents);
    const id = this.#next++;
    this.#tabs.set(id, view);
    this.#order.push(id);
    const contents = view.webContents;
    const changed = () => this.#emit(this.state());
    for (const event of [
      "did-start-loading",
      "did-stop-loading",
      "did-navigate",
      "did-navigate-in-page",
      "page-title-updated"
    ]) {
      contents.on(event, changed);
    }
    contents.setWindowOpenHandler(({ url }) => {
      this.open(url);
      return { action: "deny" };
    });
    if (input) void contents.loadURL(toUrl(input)).catch(() => {
    });
    this.select(id);
    return id;
  }
  select(id) {
    if (!this.#tabs.has(id)) return;
    this.#active = id;
    this.#show();
    this.#emit(this.state());
  }
  close(id) {
    const view = this.#tabs.get(id);
    if (!view) return;
    if (this.#shown === view) {
      this.#window.contentView.removeChildView(view);
      this.#shown = null;
    }
    this.#tabs.delete(id);
    const index = this.#order.indexOf(id);
    this.#order = this.#order.filter((tab) => tab !== id);
    view.webContents.close();
    if (this.#active === id) {
      this.#active = this.#order[Math.min(index, this.#order.length - 1)] ?? null;
      this.#show();
    }
    this.#emit(this.state());
  }
  navigate(id, input) {
    void this.#tabs.get(id)?.webContents.loadURL(toUrl(input)).catch(() => {
    });
  }
  back(id) {
    const history = this.#tabs.get(id)?.webContents.navigationHistory;
    if (history?.canGoBack()) history.goBack();
  }
  forward(id) {
    const history = this.#tabs.get(id)?.webContents.navigationHistory;
    if (history?.canGoForward()) history.goForward();
  }
  reload(id) {
    this.#tabs.get(id)?.webContents.reload();
  }
  stop(id) {
    this.#tabs.get(id)?.webContents.stop();
  }
  /** Where the renderer's page area is; null while the pane is hidden or covered. */
  setBounds(bounds) {
    this.#bounds = bounds;
    this.#show();
  }
  /** The active tab's page, opening a blank tab when there is none (for the agent). */
  page() {
    if (this.#active === null) this.open();
    return this.#tabs.get(this.#active).webContents;
  }
  setAgent(active) {
    if (this.#agent === active) return;
    this.#agent = active;
    this.#emit(this.state());
  }
  destroy() {
    for (const id of this.#order) this.close(id);
  }
  #show() {
    const view = this.#active === null ? null : this.#tabs.get(this.#active) ?? null;
    if (this.#shown && (this.#shown !== view || !this.#bounds)) {
      this.#window.contentView.removeChildView(this.#shown);
      this.#shown = null;
    }
    if (!view || !this.#bounds) return;
    view.setBounds(this.#bounds);
    if (this.#shown !== view) {
      this.#window.contentView.addChildView(view);
      this.#shown = view;
    }
  }
};

// src/main/browserWindows.ts
var BrowserWindows = class {
  constructor(preload, load) {
    this.preload = preload;
    this.load = load;
  }
  /** Each app window's browser, by the app window's contents id. */
  #browsers = /* @__PURE__ */ new Map();
  /** Popped-out windows, by the app window they belong to. */
  #popouts = /* @__PURE__ */ new Map();
  /** The app window each popped-out window belongs to, by the popped-out window's contents id. */
  #owners = /* @__PURE__ */ new Map();
  #ownerOf(contents) {
    return this.#owners.get(contents.id) ?? contents.id;
  }
  /** The browser `contents` drives: its own, or, from a popped-out window, its app window's. */
  for(contents) {
    const owner = this.#ownerOf(contents);
    let browser = this.#browsers.get(owner);
    if (!browser) {
      const window = import_electron3.BrowserWindow.fromWebContents(contents);
      if (!window) throw new Error("no window for this browser");
      browser = new Browser(window, (state) => this.#tell(owner, contents, state));
      this.#browsers.set(owner, browser);
    }
    return browser;
  }
  /** Both windows show the browser's tabs: the app's pane, and the popped-out window. */
  #tell(owner, contents, state) {
    if (!contents.isDestroyed()) contents.send("browser:state", state);
    const popout = this.#popouts.get(owner);
    if (popout && !popout.isDestroyed()) popout.webContents.send("browser:state", state);
  }
  /** Move the browser of `contents`'s app window into a window of its own, or bring that window forward. */
  popOut(contents) {
    const owner = this.#ownerOf(contents);
    const existing = this.#popouts.get(owner);
    if (existing && !existing.isDestroyed()) {
      existing.focus();
      return;
    }
    const app2 = import_electron3.BrowserWindow.fromWebContents(contents);
    const browser = this.for(contents);
    const window = new import_electron3.BrowserWindow({
      width: 1100,
      height: 820,
      minWidth: 420,
      minHeight: 320,
      title: "Browser \u2014 Abacus",
      backgroundColor: "#0e0b14",
      autoHideMenuBar: true,
      webPreferences: {
        preload: this.preload,
        contextIsolation: true,
        sandbox: true,
        nodeIntegration: false,
        spellcheck: true
      }
    });
    editWithRightClick(window.webContents);
    const popoutId = window.webContents.id;
    this.#popouts.set(owner, window);
    this.#owners.set(popoutId, owner);
    browser.attach(window, true);
    window.on("closed", () => {
      this.#popouts.delete(owner);
      this.#owners.delete(popoutId);
      if (app2 && !app2.isDestroyed()) browser.attach(app2, false);
    });
    this.load(window);
  }
  /** Bring the browser back into its app window: its popped-out window closes. */
  dock(contents) {
    this.#popouts.get(this.#ownerOf(contents))?.close();
  }
  /** An app window closed: its browser, and any window it popped out into, go with it. */
  closeFor(owner) {
    this.#browsers.get(owner)?.destroy();
    this.#browsers.delete(owner);
    const popout = this.#popouts.get(owner);
    this.#popouts.delete(owner);
    if (popout && !popout.isDestroyed()) {
      this.#owners.delete(popout.webContents.id);
      popout.removeAllListeners("closed");
      popout.destroy();
    }
  }
};

// src/main/browserTools.ts
var LOAD_TIMEOUT_MS = 2e4;
var SCREENSHOT_WIDTH = 1280;
var element = {
  ref: { type: "integer", description: "The element's number from browser_read_page." },
  element: {
    type: "string",
    description: "What the element is, in a few words (shown to the person)."
  }
};
var BROWSER_TOOLS = [
  {
    name: "browser_navigate",
    description: "Open a URL in the browser in the Abacus desktop app, where the person can watch. Opens a tab when none is open. Returns the page title and URL; read it with browser_read_page.",
    needsApproval: false,
    parameters: {
      type: "object",
      properties: {
        url: {
          type: "string",
          description: "The address, e.g. https://example.com or localhost:5173."
        }
      },
      required: ["url"]
    }
  },
  {
    name: "browser_read_page",
    description: "Read the browser's current page: title, URL, scroll position, the interactive elements numbered for browser_click and browser_type, and the visible text.",
    needsApproval: false,
    parameters: { type: "object", properties: {} }
  },
  {
    name: "browser_click",
    description: "Click an element by its number from browser_read_page. Read the page again afterwards: numbers change.",
    needsApproval: true,
    parameters: { type: "object", properties: element, required: ["ref"] }
  },
  {
    name: "browser_type",
    description: "Type text into an input, textarea or editable element by its number, replacing what is there. submit=true presses Enter afterwards.",
    needsApproval: true,
    parameters: {
      type: "object",
      properties: { ...element, text: { type: "string" }, submit: { type: "boolean" } },
      required: ["ref", "text"]
    }
  },
  {
    name: "browser_press_key",
    description: "Press a key in the page: Enter, Tab, Escape, Backspace, Space, the arrows, PageUp, PageDown, Home or End.",
    needsApproval: true,
    parameters: {
      type: "object",
      properties: {
        key: {
          type: "string",
          enum: [
            "Enter",
            "Tab",
            "Escape",
            "Backspace",
            "Space",
            "ArrowUp",
            "ArrowDown",
            "ArrowLeft",
            "ArrowRight",
            "PageUp",
            "PageDown",
            "Home",
            "End"
          ]
        }
      },
      required: ["key"]
    }
  },
  {
    name: "browser_scroll",
    description: "Scroll the page up or down by a number of screens (default one).",
    needsApproval: false,
    parameters: {
      type: "object",
      properties: {
        direction: { type: "string", enum: ["up", "down"] },
        amount: { type: "number" }
      },
      required: ["direction"]
    }
  },
  {
    name: "browser_back",
    description: "Go back to the previous page.",
    needsApproval: false,
    parameters: { type: "object", properties: {} }
  },
  {
    name: "browser_screenshot",
    description: "Take a screenshot of the visible page. It is shown to you as an image, for models that accept images.",
    needsApproval: false,
    parameters: { type: "object", properties: {} }
  }
];
function isBrowserTool(name) {
  return typeof name === "string" && BROWSER_TOOLS.some((tool) => tool.name === name);
}
async function runBrowserTool(browser, name, args) {
  browser.setAgent(true);
  try {
    return await run(browser, name, args);
  } catch (error) {
    return { content: `Error: ${error instanceof Error ? error.message : String(error)}` };
  } finally {
    browser.setAgent(false);
  }
}
async function run(browser, name, args) {
  switch (name) {
    case "browser_navigate": {
      const url = String(args.url ?? "");
      if (!url) throw new Error("url is required");
      const page = browser.page();
      const id = browser.state().active;
      browser.navigate(id, url);
      await settled(page);
      return { content: where(page) };
    }
    case "browser_read_page":
      return { content: format(await browser.page().executeJavaScript(READ_PAGE)) };
    case "browser_click": {
      const page = browser.page();
      const target = await locate(page, args.ref);
      click(page, target.x, target.y);
      await settled(page, 600);
      return { content: `Clicked [${String(args.ref)}] ${target.label}. ${where(page)}` };
    }
    case "browser_type": {
      const page = browser.page();
      const target = await locate(page, args.ref);
      click(page, target.x, target.y);
      await page.executeJavaScript(`(() => {
        const el = document.querySelector('[data-abacus-ref="${Number(args.ref)}"]');
        if (el && "select" in el) el.select();
        else if (el?.isContentEditable) document.execCommand("selectAll");
      })()`);
      await page.insertText(String(args.text ?? ""));
      if (args.submit) {
        key(page, "Enter");
        await settled(page, 600);
      }
      return {
        content: `Typed into [${String(args.ref)}] ${target.label}${args.submit ? " and pressed Enter" : ""}. ${where(page)}`
      };
    }
    case "browser_press_key": {
      const page = browser.page();
      key(page, String(args.key ?? ""));
      await settled(page, 300);
      return { content: `Pressed ${String(args.key)}. ${where(page)}` };
    }
    case "browser_scroll": {
      const sign = args.direction === "up" ? -1 : 1;
      const screens = Math.min(10, Math.max(0.1, Number(args.amount ?? 1) || 1));
      const position = await browser.page().executeJavaScript(
        `(() => { scrollBy(0, ${sign * screens} * innerHeight * 0.85); return { y: Math.round(scrollY), height: document.documentElement.scrollHeight, viewport: innerHeight }; })()`
      );
      return {
        content: `Scrolled ${args.direction}: at ${position.y}px of ${position.height}px (viewport ${position.viewport}px).`
      };
    }
    case "browser_back": {
      const page = browser.page();
      if (!page.navigationHistory.canGoBack())
        return { content: `There is no page to go back to. ${where(page)}` };
      page.navigationHistory.goBack();
      await settled(page);
      return { content: where(page) };
    }
    case "browser_screenshot": {
      const page = browser.page();
      let image = await page.capturePage();
      const { width } = image.getSize();
      if (width > SCREENSHOT_WIDTH) image = image.resize({ width: SCREENSHOT_WIDTH });
      const size = image.getSize();
      return {
        content: `Screenshot of ${where(page)} (${size.width}x${size.height})`,
        images: [`data:image/jpeg;base64,${image.toJPEG(82).toString("base64")}`]
      };
    }
    default:
      throw new Error(`unknown browser tool ${name}`);
  }
}
function where(page) {
  const title = page.getTitle();
  return `Now at ${title ? `"${title}" ` : ""}(${page.getURL() || "about:blank"}).`;
}
function settled(page, quiet = 150) {
  return new Promise((resolve3) => {
    let done = false;
    const finish = () => {
      if (done) return;
      done = true;
      clearTimeout(limit);
      page.removeListener("did-stop-loading", finish);
      resolve3();
    };
    const limit = setTimeout(finish, LOAD_TIMEOUT_MS);
    page.on("did-stop-loading", finish);
    setTimeout(() => {
      if (!page.isLoading()) finish();
    }, quiet);
  });
}
async function locate(page, ref) {
  const number = Number(ref);
  if (!Number.isInteger(number))
    throw new Error("ref must be an element number from browser_read_page");
  const target = await page.executeJavaScript(`(() => {
    const el = document.querySelector('[data-abacus-ref="${number}"]');
    if (!el) return null;
    el.scrollIntoView({ block: "center", inline: "center" });
    const r = el.getBoundingClientRect();
    const label = (el.getAttribute("aria-label") || el.innerText || el.value || el.placeholder || el.tagName).trim().replace(/\\s+/g, " ").slice(0, 60);
    return { x: Math.round(r.left + r.width / 2), y: Math.round(r.top + r.height / 2), label: JSON.stringify(label) };
  })()`);
  if (!target)
    throw new Error(`no element [${number}] on this page; read the page again for current numbers`);
  return target;
}
function click(page, x, y) {
  page.sendInputEvent({ type: "mouseMove", x, y });
  page.sendInputEvent({ type: "mouseDown", x, y, button: "left", clickCount: 1 });
  page.sendInputEvent({ type: "mouseUp", x, y, button: "left", clickCount: 1 });
}
var KEY_CODES = {
  ArrowUp: "Up",
  ArrowDown: "Down",
  ArrowLeft: "Left",
  ArrowRight: "Right"
};
function key(page, name) {
  const keyCode = KEY_CODES[name] ?? name;
  page.sendInputEvent({ type: "keyDown", keyCode });
  if (name === "Enter") page.sendInputEvent({ type: "char", keyCode: "\r" });
  if (name === "Space") page.sendInputEvent({ type: "char", keyCode: " " });
  page.sendInputEvent({ type: "keyUp", keyCode });
}
function format(page) {
  const lines = [
    `Title: ${page.title || "(none)"}`,
    `URL: ${page.url}`,
    `Scrolled to ${page.scroll.y}px of ${page.scroll.height}px (viewport ${page.scroll.viewport}px)`,
    "",
    page.elements.length ? "Interactive elements (their numbers work with browser_click and browser_type):" : "No interactive elements in view.",
    ...page.elements,
    "",
    "Text:",
    page.text || "(no text)"
  ];
  if (page.truncated) lines.push("\u2026(text cut short; scroll and read again for more)");
  return lines.join("\n");
}
var READ_PAGE = `(() => {
  const MAX_TEXT = 12000, MAX_ELEMENTS = 200;
  for (const el of document.querySelectorAll("[data-abacus-ref]")) el.removeAttribute("data-abacus-ref");
  const visible = (el) => {
    const r = el.getBoundingClientRect();
    const s = getComputedStyle(el);
    return r.width > 0 && r.height > 0 && s.visibility !== "hidden" && s.display !== "none";
  };
  const selector = "a[href], button, input:not([type=hidden]), textarea, select, summary, [role=button], [role=link], [role=tab], [role=menuitem], [role=checkbox], [role=radio], [role=switch], [contenteditable=true]";
  const elements = [];
  let n = 0;
  for (const el of document.querySelectorAll(selector)) {
    if (elements.length >= MAX_ELEMENTS) break;
    if (!visible(el) || el.disabled) continue;
    n += 1;
    el.setAttribute("data-abacus-ref", String(n));
    const tag = el.tagName.toLowerCase();
    const kind = el.getAttribute("role") || (tag === "a" ? "link" : tag === "input" ? (el.type || "text") + " input" : tag);
    const label = (el.getAttribute("aria-label") || el.innerText || el.value || el.placeholder || el.title || el.name || el.alt || "")
      .trim().replace(/\\s+/g, " ").slice(0, 80);
    const link = tag === "a" ? " -> " + (el.getAttribute("href") || "").slice(0, 100) : "";
    elements.push("[" + n + "] " + kind + ' "' + label + '"' + link);
  }
  const text = (document.body ? document.body.innerText : "").replace(/\\n{3,}/g, "\\n\\n").trim();
  return {
    title: document.title,
    url: location.href,
    text: text.slice(0, MAX_TEXT),
    truncated: text.length > MAX_TEXT,
    elements,
    scroll: { y: Math.round(scrollY), height: document.documentElement.scrollHeight, viewport: innerHeight },
  };
})()`;

// src/main/files.ts
var import_node_fs3 = require("node:fs");
var import_promises = require("node:fs/promises");
var import_node_path4 = require("node:path");
var import_node_stream = require("node:stream");
var import_node_url = require("node:url");
var import_electron4 = require("electron");
var FILE_SCHEME = "abacus-file";
var MAX_FILES = 20;
var PREVIEW_BYTES = 256 * 1024;
var PRESENT_TOOL = {
  name: "present_files",
  description: "Show files to the person in the Abacus desktop app, as cards under your message that open, preview and play them: documents (PDF, text, Markdown, JSON, CSV), images, audio and video. Use it for files the person should look at \u2014 a report you wrote, a chart, a recording \u2014 rather than only naming their paths. Paths are relative to the workspace, or absolute.",
  needsApproval: false,
  parameters: {
    type: "object",
    properties: {
      paths: {
        type: "array",
        items: { type: "string" },
        minItems: 1,
        maxItems: MAX_FILES,
        description: "The files to show, in the order to show them."
      }
    },
    required: ["paths"]
  }
};
var shown = /* @__PURE__ */ new Set();
var SHOWN = "Shown to the person as file cards:";
async function presentFiles(workspace, args) {
  const paths = Array.isArray(args.paths) ? args.paths.filter((path) => typeof path === "string" && path.trim() !== "") : [];
  if (paths.length === 0) return { content: "Error: give the paths of the files to show" };
  const lines = [];
  const problems = [];
  for (const raw of paths.slice(0, MAX_FILES)) {
    const path = (0, import_node_path4.isAbsolute)(raw) ? raw : (0, import_node_path4.resolve)(workspace, raw);
    try {
      if (!(await (0, import_promises.stat)(path)).isFile()) {
        problems.push(`Could not show ${raw}: it is not a file`);
        continue;
      }
      shown.add(path);
      lines.push(`- ${path}`);
    } catch {
      problems.push(`Could not show ${raw}: no such file`);
    }
  }
  if (lines.length === 0) return { content: `Error: ${problems.join("\n")}` };
  return { content: [SHOWN, ...lines, ...problems].join("\n") };
}
async function fileInfo(paths) {
  if (!Array.isArray(paths)) return [];
  return Promise.all(
    paths.filter((path) => typeof path === "string" && (0, import_node_path4.isAbsolute)(path)).map(async (path) => {
      shown.add(path);
      const size = await (0, import_promises.stat)(path).then(
        (info) => info.isFile() ? info.size : null,
        () => null
      );
      return { path, name: (0, import_node_path4.basename)(path), size };
    })
  );
}
function isShown(path) {
  return typeof path === "string" && shown.has(path);
}
async function readText(path) {
  if (!isShown(path)) return null;
  const file = await (0, import_promises.open)(path, "r");
  try {
    const { size } = await file.stat();
    const buffer = Buffer.alloc(Math.min(size, PREVIEW_BYTES));
    const { bytesRead } = await file.read(buffer, 0, buffer.length, 0);
    const bytes = buffer.subarray(0, bytesRead);
    if (bytes.subarray(0, 8192).includes(0)) return null;
    return { text: bytes.toString("utf8"), truncated: size > bytesRead };
  } finally {
    await file.close();
  }
}
var MIME = {
  ".pdf": "application/pdf",
  ".png": "image/png",
  ".jpg": "image/jpeg",
  ".jpeg": "image/jpeg",
  ".gif": "image/gif",
  ".webp": "image/webp",
  ".avif": "image/avif",
  ".bmp": "image/bmp",
  ".svg": "image/svg+xml",
  ".ico": "image/x-icon",
  ".mp3": "audio/mpeg",
  ".wav": "audio/wav",
  ".ogg": "audio/ogg",
  ".oga": "audio/ogg",
  ".opus": "audio/ogg",
  ".flac": "audio/flac",
  ".m4a": "audio/mp4",
  ".aac": "audio/aac",
  ".mp4": "video/mp4",
  ".m4v": "video/mp4",
  ".webm": "video/webm",
  ".mov": "video/quicktime",
  ".ogv": "video/ogg"
};
function answerFileRequests(browserFor2) {
  import_electron4.ipcMain.handle("files:info", (_event, paths) => fileInfo(paths));
  import_electron4.ipcMain.handle("files:text", (_event, path) => readText(path));
  import_electron4.ipcMain.handle("files:open", async (_event, path) => {
    if (isShown(path)) await import_electron4.shell.openPath(path);
  });
  import_electron4.ipcMain.handle("files:browse", (event, path) => {
    if (!isShown(path)) return;
    browserFor2(event.sender).open((0, import_node_url.pathToFileURL)(path).href);
    event.sender.send("browser:show");
  });
}
function registerFileScheme() {
  import_electron4.protocol.registerSchemesAsPrivileged([
    { scheme: FILE_SCHEME, privileges: { standard: true, secure: true, stream: true } }
  ]);
}
function serveFiles(session2) {
  session2.protocol.handle(FILE_SCHEME, async (request) => {
    const path = decodeURIComponent(new URL(request.url).pathname.slice(1));
    if (!isShown(path)) return new Response("Not shown", { status: 403 });
    const size = await (0, import_promises.stat)(path).then(
      (info) => info.isFile() ? info.size : null,
      () => null
    );
    if (size === null) return new Response("Not found", { status: 404 });
    const headers = {
      "Content-Type": MIME[(0, import_node_path4.extname)(path).toLowerCase()] ?? "application/octet-stream",
      "Accept-Ranges": "bytes"
    };
    const range = byteRange(request.headers.get("range"), size);
    if (range === "unsatisfiable") {
      return new Response(null, { status: 416, headers: { "Content-Range": `bytes */${size}` } });
    }
    const [start, end] = range ?? [0, size - 1];
    headers["Content-Length"] = String(Math.max(0, end - start + 1));
    if (range) headers["Content-Range"] = `bytes ${start}-${end}/${size}`;
    const body = size === 0 ? null : import_node_stream.Readable.toWeb((0, import_node_fs3.createReadStream)(path, { start, end }));
    return new Response(body, { status: range ? 206 : 200, headers });
  });
}
function byteRange(header, size) {
  const match = /^bytes=(\d*)-(\d*)$/.exec(header?.trim() ?? "");
  if (!match || match[1] === "" && match[2] === "") return null;
  const [, from, to] = match;
  if (from === "") {
    const last = Math.min(Number(to), size);
    return last === 0 ? "unsatisfiable" : [size - last, size - 1];
  }
  const start = Number(from);
  if (start >= size) return "unsatisfiable";
  return [start, to === "" ? size - 1 : Math.min(Number(to), size - 1)];
}

// src/main/groups.ts
var import_node_fs4 = require("node:fs");
var import_node_path5 = require("node:path");
var SessionGroups = class _SessionGroups {
  constructor(file) {
    this.file = file;
  }
  static in(directory) {
    return new _SessionGroups((0, import_node_path5.join)(directory, "session-groups.json"));
  }
  list() {
    try {
      return sanitizeGroups(JSON.parse((0, import_node_fs4.readFileSync)(this.file, "utf8")));
    } catch {
      return [];
    }
  }
  /** Replace every group with what the window sent, cleaned up; returns what was kept. */
  save(groups2) {
    const clean = sanitizeGroups(groups2);
    (0, import_node_fs4.mkdirSync)((0, import_node_path5.dirname)(this.file), { recursive: true });
    const temporary = `${this.file}.${process.pid}.tmp`;
    (0, import_node_fs4.writeFileSync)(temporary, JSON.stringify(clean, null, 2));
    (0, import_node_fs4.renameSync)(temporary, this.file);
    return clean;
  }
};
function sanitizeGroups(value) {
  if (!Array.isArray(value)) return [];
  const groupIds = /* @__PURE__ */ new Set();
  const placed = /* @__PURE__ */ new Set();
  const groups2 = [];
  for (const entry of value) {
    if (typeof entry !== "object" || entry === null) continue;
    const { id, name, sessions: sessions2, collapsed } = entry;
    if (typeof id !== "string" || !id || groupIds.has(id)) continue;
    if (typeof name !== "string") continue;
    groupIds.add(id);
    const members = (Array.isArray(sessions2) ? sessions2 : []).filter(
      (session2) => typeof session2 === "string" && !placed.has(session2)
    );
    for (const session2 of members) placed.add(session2);
    groups2.push({
      id,
      name: name.trim() || "Untitled group",
      sessions: members,
      ...collapsed === true ? { collapsed: true } : {}
    });
  }
  return groups2;
}

// src/main/pins.ts
var import_node_fs5 = require("node:fs");
var import_node_path6 = require("node:path");
var SidebarPins = class _SidebarPins {
  constructor(file) {
    this.file = file;
  }
  static in(directory) {
    return new _SidebarPins((0, import_node_path6.join)(directory, "sidebar-pins.json"));
  }
  list() {
    try {
      return sanitizePins(JSON.parse((0, import_node_fs5.readFileSync)(this.file, "utf8")));
    } catch {
      return sanitizePins(null);
    }
  }
  /** Replace the pins with what the window sent, cleaned up; returns what was kept. */
  save(pins2) {
    const clean = sanitizePins(pins2);
    (0, import_node_fs5.mkdirSync)((0, import_node_path6.dirname)(this.file), { recursive: true });
    const temporary = `${this.file}.${process.pid}.tmp`;
    (0, import_node_fs5.writeFileSync)(temporary, JSON.stringify(clean, null, 2));
    (0, import_node_fs5.renameSync)(temporary, this.file);
    return clean;
  }
};
function sanitizePins(value) {
  const record = typeof value === "object" && value !== null ? value : {};
  const strings = (list) => [
    ...new Set(
      (Array.isArray(list) ? list : []).filter(
        (entry) => typeof entry === "string" && entry.length > 0
      )
    )
  ];
  return { sessions: strings(record.sessions), projects: strings(record.projects) };
}

// src/main/recent.ts
var import_node_fs6 = require("node:fs");
var import_node_path7 = require("node:path");
var LIMIT = 20;
var RecentWorkspaces = class _RecentWorkspaces {
  constructor(file) {
    this.file = file;
  }
  static in(directory) {
    return new _RecentWorkspaces((0, import_node_path7.join)(directory, "recent-workspaces.json"));
  }
  list() {
    try {
      const parsed = JSON.parse((0, import_node_fs6.readFileSync)(this.file, "utf8"));
      if (!Array.isArray(parsed)) return [];
      return parsed.filter(
        (entry) => typeof entry === "object" && entry !== null && typeof entry.path === "string"
      );
    } catch {
      return [];
    }
  }
  /** Folders that still exist: a deleted project should not sit in the list as a dead link. */
  existing() {
    return this.list().filter((entry) => (0, import_node_fs6.existsSync)(entry.path));
  }
  touch(path, now = /* @__PURE__ */ new Date()) {
    const entry = {
      path,
      name: (0, import_node_path7.basename)(path) || path,
      openedAt: now.toISOString()
    };
    const next = [entry, ...this.list().filter((existing) => existing.path !== path)].slice(
      0,
      LIMIT
    );
    this.#write(next);
    return next;
  }
  forget(path) {
    const next = this.list().filter((existing) => existing.path !== path);
    this.#write(next);
    return next;
  }
  #write(entries) {
    (0, import_node_fs6.mkdirSync)((0, import_node_path7.dirname)(this.file), { recursive: true });
    const temporary = `${this.file}.${process.pid}.tmp`;
    (0, import_node_fs6.writeFileSync)(temporary, JSON.stringify(entries, null, 2));
    (0, import_node_fs6.renameSync)(temporary, this.file);
  }
};

// src/main/terminal.ts
var import_node_events2 = require("node:events");
var import_node_fs7 = require("node:fs");
var import_node_os2 = require("node:os");
var import_node_path8 = require("node:path");
var import_electron5 = require("electron");
var FLUSH_MS = 8;
var FLUSH_BYTES = 64 * 1024;
function defaultShell(platform = process.platform, env = process.env) {
  if (platform === "win32") {
    return { file: env.ABACUS_SHELL || "powershell.exe", args: ["-NoLogo"] };
  }
  const file = env.ABACUS_SHELL || env.SHELL || ((0, import_node_fs7.existsSync)("/bin/zsh") ? "/bin/zsh" : "/bin/bash");
  return { file, args: platform === "darwin" ? ["-l"] : [] };
}
function atPrompt(foreground, shell4, platform = process.platform) {
  if (platform === "win32") return false;
  const name = (file) => (0, import_node_path8.basename)(file.trim()).replace(/^-/, "");
  return name(foreground) !== "" && name(foreground) === name(shell4);
}
var Terminals = class extends import_node_events2.EventEmitter {
  constructor(spawn2) {
    super();
    this.spawn = spawn2;
  }
  #sessions = /* @__PURE__ */ new Map();
  #nextId = 1;
  /** A shell, or `command` in its place (`abacus update`, which may ask for a password). */
  open(owner, cwd, cols, rows, command = defaultShell()) {
    const { file, args } = command;
    const pty = this.spawn(file, args, {
      name: "xterm-256color",
      cols: clamp(cols, 2, 1e3),
      rows: clamp(rows, 1, 500),
      cwd: (0, import_node_fs7.existsSync)(cwd) ? cwd : (0, import_node_os2.homedir)(),
      env: {
        ...process.env,
        TERM: "xterm-256color",
        COLORTERM: "truecolor",
        TERM_PROGRAM: "abacus"
      }
    });
    const id = this.#nextId++;
    const session2 = { owner, pty, file, pending: "", timer: null };
    this.#sessions.set(id, session2);
    pty.onData((data) => {
      session2.pending += data;
      if (session2.pending.length >= FLUSH_BYTES) this.#flush(id);
      else session2.timer ??= setTimeout(() => this.#flush(id), FLUSH_MS);
    });
    pty.onExit(({ exitCode }) => {
      this.#flush(id);
      this.#sessions.delete(id);
      this.emit("event", owner, { kind: "exit", id, code: exitCode });
    });
    return id;
  }
  write(owner, id, data) {
    this.#own(owner, id)?.pty.write(data);
  }
  /** Whether the shell in this terminal is at its prompt, free to take a command. */
  idle(owner, id) {
    const session2 = this.#own(owner, id);
    return session2 !== void 0 && atPrompt(session2.pty.process, session2.file);
  }
  resize(owner, id, cols, rows) {
    const session2 = this.#own(owner, id);
    if (!session2) return;
    try {
      session2.pty.resize(clamp(cols, 2, 1e3), clamp(rows, 1, 500));
    } catch {
    }
  }
  close(owner, id) {
    this.#own(owner, id)?.pty.kill();
  }
  /** Every terminal a window opened: it closed, or left its project. */
  closeAll(owner) {
    for (const [id, session2] of this.#sessions) {
      if (session2.owner === owner) {
        session2.pty.kill();
        this.#sessions.delete(id);
      }
    }
  }
  #own(owner, id) {
    const session2 = this.#sessions.get(id);
    return session2?.owner === owner ? session2 : void 0;
  }
  #flush(id) {
    const session2 = this.#sessions.get(id);
    if (!session2) return;
    if (session2.timer) clearTimeout(session2.timer);
    session2.timer = null;
    if (!session2.pending) return;
    const data = session2.pending;
    session2.pending = "";
    this.emit("event", session2.owner, { kind: "data", id, data });
  }
};
function clamp(value, min, max) {
  return Math.min(max, Math.max(min, Math.floor(Number.isFinite(value) ? value : min)));
}
function answerTerminalRequests(host) {
  const onTerminal = (handle) => (event, id, ...rest) => {
    if (typeof id !== "number") return;
    void host()?.then((terminals2) => handle(terminals2, event, id, ...rest)).catch(() => {
    });
  };
  import_electron5.ipcMain.on(
    "terminal:write",
    onTerminal((terminals2, event, id, data) => {
      if (typeof data === "string") terminals2.write(event.sender.id, id, data);
    })
  );
  import_electron5.ipcMain.on(
    "terminal:resize",
    onTerminal(
      (terminals2, event, id, cols, rows) => terminals2.resize(event.sender.id, id, Number(cols), Number(rows))
    )
  );
  import_electron5.ipcMain.on(
    "terminal:close",
    onTerminal((terminals2, event, id) => terminals2.close(event.sender.id, id))
  );
  import_electron5.ipcMain.handle(
    "terminal:idle",
    async (event, id) => typeof id === "number" ? Boolean((await host())?.idle(event.sender.id, id)) : false
  );
}

// src/main/updates.ts
var RELEASES = "https://github.com/empero-org";
function tagVersion(location) {
  return location?.match(/\/releases\/tag\/v?(\d+(?:\.\d+)*)\/?$/)?.[1] ?? null;
}
async function latestRelease(repo) {
  try {
    const response = await fetch(`${RELEASES}/${repo}/releases/latest`, {
      redirect: "manual",
      signal: AbortSignal.timeout(1e4)
    });
    return tagVersion(response.headers.get("location"));
  } catch {
    return null;
  }
}
async function checkUpdates(desktop, cli) {
  const [desktopLatest, cliLatest] = await Promise.all([
    latestRelease("abacus-desktop"),
    cli ? latestRelease("abacus") : null
  ]);
  const newer = (current, latest) => latest && compareVersions(latest, current) > 0 ? { current, latest } : null;
  return { desktop: newer(desktop, desktopLatest), cli: cli ? newer(cli, cliLatest) : null };
}

// src/main/zoom.ts
var import_node_fs8 = require("node:fs");
var STEP = 0.5;
var LEVELS = { min: -3, max: 5 };
function zoomStep(input) {
  if (input.type !== "keyDown" || input.alt || !(input.control || input.meta)) return null;
  switch (input.key) {
    case "+":
    case "=":
    case "Add":
      return 1;
    case "-":
    case "_":
    case "Subtract":
      return -1;
    case "0":
      return 0;
    default:
      return null;
  }
}
function clamp2(level) {
  return Math.min(LEVELS.max, Math.max(LEVELS.min, level));
}
function zoomed(level, step) {
  return step === 0 ? 0 : clamp2(level + step * STEP);
}
function savedLevel(file) {
  try {
    const { level } = JSON.parse((0, import_node_fs8.readFileSync)(file, "utf8"));
    return typeof level === "number" && Number.isFinite(level) ? clamp2(level) : 0;
  } catch {
    return 0;
  }
}
function zoomWithKeys(window, file) {
  const contents = window.webContents;
  contents.on("did-finish-load", () => contents.setZoomLevel(savedLevel(file)));
  contents.on("before-input-event", (event, input) => {
    const step = zoomStep(input);
    if (step === null) return;
    event.preventDefault();
    const level = zoomed(contents.getZoomLevel(), step);
    contents.setZoomLevel(level);
    try {
      (0, import_node_fs8.writeFileSync)(file, JSON.stringify({ level }));
    } catch {
    }
  });
}

// src/main/window.ts
var import_electron6 = require("electron");
var import_node_path9 = require("node:path");
var TITLE_BAR_HEIGHT = 44;
var BACKGROUND = "#0e0b14";
function createWindow(options) {
  const mac = process.platform === "darwin";
  const window = new import_electron6.BrowserWindow({
    width: 1360,
    height: 880,
    minWidth: 880,
    minHeight: 560,
    show: true,
    backgroundColor: BACKGROUND,
    title: "Abacus",
    titleBarStyle: mac ? "hiddenInset" : "hidden",
    trafficLightPosition: mac ? { x: 16, y: 15 } : void 0,
    titleBarOverlay: mac ? void 0 : { color: BACKGROUND, symbolColor: "#bcb5c9", height: TITLE_BAR_HEIGHT },
    webPreferences: {
      preload: options.preload,
      contextIsolation: true,
      sandbox: true,
      nodeIntegration: false,
      spellcheck: true
    }
  });
  window.once("ready-to-show", () => { window.show(); window.focus(); });
  window.webContents.once("dom-ready", () => { window.show(); window.focus(); });
  window.webContents.once("did-finish-load", () => { window.show(); window.focus(); });
  editWithRightClick(window.webContents);
  window.webContents.setWindowOpenHandler(({ url }) => {
    if (isExternalUrl(url)) void import_electron6.shell.openExternal(url);
    return { action: "deny" };
  });
  window.webContents.on("will-navigate", (event, url) => {
    if (url !== window.webContents.getURL()) {
      event.preventDefault();
      if (isExternalUrl(url)) void import_electron6.shell.openExternal(url);
    }
  });
  if (options.devServerUrl) void window.loadURL(options.devServerUrl);
  else void window.loadFile(options.rendererHtml);
  return window;
}
function isExternalUrl(url) {
  try {
    const { protocol: protocol2 } = new URL(url);
    return protocol2 === "https:" || protocol2 === "http:" || protocol2 === "mailto:";
  } catch {
    return false;
  }
}
function rendererPaths(outDir) {
  return {
    preload: (0, import_node_path9.join)(outDir, "preload", "index.cjs"),
    rendererHtml: (0, import_node_path9.join)(outDir, "renderer", "index.html")
  };
}

// src/main/index.ts
var OUT_DIR = (0, import_node_path10.join)(__dirname, "..");
var DEV_SERVER_URL = process.env.ABACUS_DESKTOP_DEV_URL;
var sessions = new Sessions({
  frame(contents, id, server, frame) {
    if (frame.kind === "request" && frame.method === "tool/call") {
      const { name, arguments: args } = frame.params ?? {};
      const browsing = isBrowserTool(name);
      if (browsing && !contents.isDestroyed()) contents.send("browser:show");
      const answer = browsing ? runBrowserTool(browserFor(contents), name, args ?? {}) : name === PRESENT_TOOL.name && presentFiles(server.workspace, args ?? {});
      if (answer) {
        void answer.then((answer2) => server.running && server.respond(frame.id, answer2));
        return;
      }
    }
    send(contents, { ...frame, server: id });
  },
  exit: (contents, id, code, tail) => send(contents, { kind: "exit", code, tail, server: id }),
  log(line) {
    if (DEV_SERVER_URL) console.error(`[abacus] ${line}`);
  }
});
var browsers = new BrowserWindows(rendererPaths(OUT_DIR).preload, (window) => {
  if (DEV_SERVER_URL) void window.loadURL(`${DEV_SERVER_URL}#browser`);
  else void window.loadFile(rendererPaths(OUT_DIR).rendererHtml, { hash: "browser" });
});
var browserFor = (contents) => browsers.for(contents);
function tellOtherWindows(sender, channel, value) {
  for (const window of import_electron7.BrowserWindow.getAllWindows()) {
    if (window.webContents !== sender && !window.webContents.isDestroyed()) {
      window.webContents.send(channel, value);
    }
  }
}
var DEV_PORTS = [5173, 3e3, 3001, 8080, 8e3, 4321, 4200, 5e3, 8888, 4173, 5174];
async function detectDevServer() {
  const answers = await Promise.all(
    DEV_PORTS.map(
      (port) => fetch(`http://localhost:${port}/`, { signal: AbortSignal.timeout(600) }).then(() => `http://localhost:${port}/`).catch(() => null)
    )
  );
  return answers.find(Boolean) ?? null;
}
var chatsDir = "";
var CHAT_CLI = "0.7.6";
var windowWorkspaces = /* @__PURE__ */ new Map();
var recent;
var groups;
var pins;
var terminals = null;
var updateTerminal = null;
function terminalHost() {
  terminals ??= import("@lydell/node-pty").then(({ spawn: spawn2 }) => {
    const host = new Terminals(spawn2);
    host.on("event", (owner, event) => {
      if (event.kind === "exit" && event.id === updateTerminal) updateTerminal = null;
      const contents = import_electron7.webContents.fromId(owner);
      if (contents && !contents.isDestroyed()) contents.send("terminal:event", event);
    });
    return host;
  });
  return terminals;
}
function closeTerminals(owner) {
  void terminals?.then((host) => host.closeAll(owner)).catch(() => {
  });
}
var mainWindows = /* @__PURE__ */ new Set();
var petOverlayWindow = null;
var petOverlayReady = false;
function getMainWindows() {
  return Array.from(mainWindows).filter((w) => !w.isDestroyed());
}
function getPrimaryMainWindow() {
  const list = getMainWindows();
  return list.find((w) => w.isFocused()) || list[0] || null;
}
function isMainWindowVisible() {
  return getMainWindows().some((w) => w.isVisible() && !w.isMinimized());
}
function restoreMainWindow() {
  const win = getPrimaryMainWindow() || openWindow();
  if (win.isMinimized()) win.restore();
  win.show();
  win.focus();
  if (typeof broadcastPetOverlayState === "function") broadcastPetOverlayState();
  return win;
}
function hideMainWindows() {
  for (const w of getMainWindows()) {
    w.hide();
  }
  if (typeof broadcastPetOverlayState === "function") broadcastPetOverlayState();
}
function openWindow(workspace) {
  const window = createWindow({ ...rendererPaths(OUT_DIR), devServerUrl: DEV_SERVER_URL });
  mainWindows.add(window);
  zoomWithKeys(window, (0, import_node_path10.join)(import_electron7.app.getPath("userData"), "zoom.json"));
  const id = window.webContents.id;
  if (workspace) {
    launchWorkspaces.set(id, workspace);
  }
  const syncVis = () => {
    if (typeof broadcastPetOverlayState === "function") broadcastPetOverlayState();
  };
  window.on("show", syncVis);
  window.on("hide", syncVis);
  window.on("minimize", syncVis);
  window.on("restore", syncVis);
  window.on("closed", () => {
    mainWindows.delete(window);
    sessions.closeWindow(id);
    closeTerminals(id);
    browsers.closeFor(id);
    windowWorkspaces.delete(id);
    launchWorkspaces.delete(id);
    if (getMainWindows().length === 0 && petOverlayWindow && !petOverlayWindow.isDestroyed()) {
      petOverlayWindow.destroy();
      petOverlayWindow = null;
    }
  });
  return window;
}
var launchWorkspaces = /* @__PURE__ */ new Map();
var PET_FORWARD_METHODS = /* @__PURE__ */ new Set([
  "turn/started",
  "item/reasoning/textDelta",
  "item/agentMessage/delta",
  "item/started",
  "tool/call",
  "item/commandExecution/requestApproval",
  "item/fileChange/requestApproval",
  "tool/requestUserInput",
  "turn/diff/updated",
  "checkpoints/updated",
  "turn/completed",
  "warning"
]);
function send(contents, message) {
  if (!contents.isDestroyed()) contents.send("abacus:message", message);
  if (message && PET_FORWARD_METHODS.has(message.method) && petOverlayWindow && !petOverlayWindow.isDestroyed() && petOverlayReady) {
    const ws = windowWorkspaces.get(contents.id);
    const workspaceName = ws ? nameOf(ws) : "";
    petOverlayWindow.webContents.send("pet:overlayEvent", {
      type: "agent",
      payload: { ...message, workspaceName }
    });
  }
}
function nameOf(workspace) {
  return workspace === chatsDir ? "Chats" : (0, import_node_path10.basename)(workspace) || workspace;
}
async function openWorkspace(event, path, keep) {
  const chat = path === chatsDir;
  if (chat) (0, import_node_fs9.mkdirSync)(chatsDir, { recursive: true });
  if (typeof path !== "string" || !(0, import_node_fs9.existsSync)(path) || !(0, import_node_fs9.statSync)(path).isDirectory()) {
    throw new Error("that folder no longer exists");
  }
  const bin = resolveCli();
  if (!bin) throw new Error("the abacus CLI is not installed");
  if (chat) {
    const cli = await detectCli();
    if (cli.state === "missing" || compareVersions(cli.version, CHAT_CLI) < 0)
      throw new Error(`Chats need abacus ${CHAT_CLI} or newer. Update it in Settings, About.`);
  }
  const contents = event.sender;
  if (windowWorkspaces.get(contents.id) !== path) closeTerminals(contents.id);
  windowWorkspaces.set(contents.id, path);
  const server = sessions.start(contents, bin, path, keep === true, chat ? ["--chat"] : []);
  if (!chat) recent.touch(path);
  return { workspace: path, name: nameOf(path), server };
}
function activateSession(event, id, keep) {
  if (typeof id !== "number") throw new Error("id must be a number");
  const contents = event.sender;
  const { workspace } = sessions.activate(contents, id, keep === true);
  if (windowWorkspaces.get(contents.id) !== workspace) closeTerminals(contents.id);
  windowWorkspaces.set(contents.id, workspace);
  return { workspace, name: nameOf(workspace), server: id };
}
function registerIpc() {
  import_electron7.ipcMain.handle("app:info", async () => ({
    version: import_electron7.app.getVersion(),
    platform: process.platform,
    cli: await detectCli(),
    chats: chatsDir
  }));
  import_electron7.ipcMain.handle("cli:detect", () => detectCli());
  import_electron7.ipcMain.handle("cli:checkUpdate", () => cliSays(["update", "--check"], 3e4));
  import_electron7.ipcMain.handle("updates:check", async () => {
    const cli = await detectCli();
    return checkUpdates(import_electron7.app.getVersion(), cli.state === "missing" ? null : cli.version);
  });
  import_electron7.ipcMain.handle("workspace:recent", () => recent.existing());
  import_electron7.ipcMain.handle("projects:list", async () => {
    const bin = resolveCli();
    const listing = bin ? await listSessions(bin) : null;
    if (!listing) return null;
    return mergeProjects(listing, recent.list(), (path) => (0, import_node_fs9.existsSync)(path));
  });
  import_electron7.ipcMain.handle("groups:list", () => groups.list());
  import_electron7.ipcMain.handle("groups:save", (event, value) => {
    const saved = groups.save(value);
    tellOtherWindows(event.sender, "groups:changed", saved);
    return saved;
  });
  import_electron7.ipcMain.handle("pins:list", () => pins.list());
  import_electron7.ipcMain.handle("pins:save", (event, value) => {
    const saved = pins.save(value);
    tellOtherWindows(event.sender, "pins:changed", saved);
    return saved;
  });
  import_electron7.ipcMain.handle("sync:status", () => cliSays(["sync", "status"], 2e4));
  import_electron7.ipcMain.handle("sync:pair", async () => {
    try {
      const out = await cliSays(["sync", "pair"], 2e4);
      const m = out.match(/https:\/\/abacus\.empero\.org\/pair#t=[a-zA-Z0-9_-]+/);
      return { ok: true, raw: out, url: m ? m[0] : null };
    } catch (err) {
      return { ok: false, error: err.message };
    }
  });
  import_electron7.ipcMain.handle(
    "workspace:forget",
    (_event, path) => typeof path === "string" ? recent.forget(path) : recent.list()
  );
  import_electron7.ipcMain.handle("workspace:pick", async (event) => {
    const window = import_electron7.BrowserWindow.fromWebContents(event.sender);
    const options = {
      title: "Open a project folder",
      properties: ["openDirectory", "createDirectory"]
    };
    const result = window ? await import_electron7.dialog.showOpenDialog(window, { ...options, properties: [...options.properties] }) : await import_electron7.dialog.showOpenDialog({ ...options, properties: [...options.properties] });
    return result.canceled ? null : result.filePaths[0] ?? null;
  });
  import_electron7.ipcMain.handle("workspace:launch", (event) => launchWorkspaces.get(event.sender.id) ?? null);
  import_electron7.ipcMain.handle("workspace:open", openWorkspace);
  import_electron7.ipcMain.handle("session:activate", activateSession);
  import_electron7.ipcMain.handle("session:stop", (event, id) => {
    if (typeof id === "number") sessions.stop(event.sender, id);
  });
  import_electron7.ipcMain.handle("workspace:close", (event) => {
    sessions.stopActive(event.sender);
    closeTerminals(event.sender.id);
    windowWorkspaces.delete(event.sender.id);
  });
  import_electron7.ipcMain.handle("session:delete", async (event, threadId) => {
    try {
      const session2 = sessions.get(event.sender);
      if (session2) {
        await session2.request("thread/delete", { threadId }).catch(() => {});
      }
    } catch {}
    try {
      const fs = require("fs");
      const path = require("path");
      const os = require("os");
      const sessionsDir = path.join(os.homedir(), ".abacus", "sessions");
      if (fs.existsSync(sessionsDir)) {
        for (const ws of fs.readdirSync(sessionsDir)) {
          const file = path.join(sessionsDir, ws, `${threadId}.json`);
          if (fs.existsSync(file)) {
            fs.unlinkSync(file);
          }
        }
      }
    } catch {}
    return { ok: true };
  });
  import_electron7.ipcMain.handle("window:new", (_event, path) => {
    openWindow(typeof path === "string" ? path : void 0);
  });
  import_electron7.ipcMain.handle("abacus:request", async (event, method, params, id) => {
    if (typeof method !== "string") throw new Error("method must be a string");
    const session2 = sessions.get(event.sender, typeof id === "number" ? id : void 0);
    const result = await session2.request(method, params ?? {});
    if (method === "initialize" && result?.capabilities?.clientTools) {
      await session2.request("tools/register", { tools: [...BROWSER_TOOLS, PRESENT_TOOL] }).catch(() => {
      });
    }
    return result;
  });
  import_electron7.ipcMain.handle("browser:state", (event) => browserFor(event.sender).state());
  import_electron7.ipcMain.handle(
    "browser:open",
    (event, url) => browserFor(event.sender).open(typeof url === "string" ? url : void 0)
  );
  import_electron7.ipcMain.handle("browser:detect", () => detectDevServer());
  import_electron7.ipcMain.handle("browser:popout", (event) => browsers.popOut(event.sender));
  import_electron7.ipcMain.handle("browser:dock", (event) => browsers.dock(event.sender));
  const tabAction = (act) => (event, id, ...rest) => {
    if (typeof id === "number") act(browserFor(event.sender), id, ...rest);
  };
  import_electron7.ipcMain.on(
    "browser:navigate",
    tabAction((browser, id, url) => browser.navigate(id, String(url ?? "")))
  );
  import_electron7.ipcMain.on(
    "browser:back",
    tabAction((browser, id) => browser.back(id))
  );
  import_electron7.ipcMain.on(
    "browser:forward",
    tabAction((browser, id) => browser.forward(id))
  );
  import_electron7.ipcMain.on(
    "browser:reload",
    tabAction((browser, id) => browser.reload(id))
  );
  import_electron7.ipcMain.on(
    "browser:stop",
    tabAction((browser, id) => browser.stop(id))
  );
  import_electron7.ipcMain.on(
    "browser:select",
    tabAction((browser, id) => browser.select(id))
  );
  import_electron7.ipcMain.on(
    "browser:close",
    tabAction((browser, id) => browser.close(id))
  );
  import_electron7.ipcMain.on("browser:bounds", (event, bounds) => {
    const rect = bounds;
    const valid = rect && [rect.x, rect.y, rect.width, rect.height].every(
      (value) => typeof value === "number" && Number.isFinite(value)
    );
    const browser = browserFor(event.sender);
    if (!browser.hostedBy(event.sender)) return;
    browser.setBounds(
      valid ? {
        x: Math.round(rect.x),
        y: Math.round(rect.y),
        width: Math.max(0, Math.round(rect.width)),
        height: Math.max(0, Math.round(rect.height))
      } : null
    );
  });
  import_electron7.ipcMain.handle("abacus:respond", (event, id, result, server) => {
    if (typeof id !== "string" && typeof id !== "number")
      throw new Error("id must be a string or number");
    sessions.get(event.sender, typeof server === "number" ? server : void 0).respond(id, result);
  });
  import_electron7.ipcMain.handle("terminal:open", async (event, cols, rows, run2) => {
    const update = run2 === "update";
    const workspace = windowWorkspaces.get(event.sender.id);
    if (!workspace && !update) throw new Error("open a project first");
    const bin = update ? resolveCli() : null;
    if (update && !bin) throw new Error("the abacus CLI is not installed");
    if (update && updateTerminal !== null) throw new Error("an update is already running");
    let host;
    try {
      host = await terminalHost();
    } catch (error) {
      terminals = null;
      throw new Error(
        `the terminal is not available on this system (${error instanceof Error ? error.message : error})`
      );
    }
    if (!bin) return host.open(event.sender.id, workspace ?? (0, import_node_os3.homedir)(), Number(cols), Number(rows));
    if (updateTerminal !== null) throw new Error("an update is already running");
    updateTerminal = host.open(
      event.sender.id,
      workspace ?? (0, import_node_os3.homedir)(),
      Number(cols),
      Number(rows),
      {
        file: bin,
        args: ["update"]
      }
    );
    return updateTerminal;
  });
  answerTerminalRequests(() => terminals);
  import_electron7.ipcMain.handle("shell:openExternal", (_event, url) => {
    if (typeof url === "string" && isExternalUrl(url)) return import_electron7.shell.openExternal(url);
  });
  answerFileRequests(browserFor);
  import_electron7.ipcMain.handle("shell:reveal", async (_event, path) => {
    if (typeof path !== "string" || !(0, import_node_fs9.existsSync)(path)) return;
    if ((0, import_node_fs9.statSync)(path).isDirectory()) await import_electron7.shell.openPath(path);
    else import_electron7.shell.showItemInFolder(path);
  });

  const PET_UA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";
  const getAbacusPetsDir = () => {
    const dir = (0, import_node_path10.join)((0, import_node_os3.homedir)(), ".abacus", "pets");
    (0, import_node_fs9.mkdirSync)(dir, { recursive: true });
    return dir;
  };
  const getCodexPetsDir = () => (0, import_node_path10.join)((0, import_node_os3.homedir)(), ".codex", "pets");
  const sanitizePetId = (raw) => String(raw || "").trim().toLowerCase().replace(/[^a-z0-9_-]+/g, "-").replace(/^-+|-+$/g, "");

  const readInstalledPetDir = (dirPath, sourceLabel) => {
    try {
      const manifestFile = (0, import_node_path10.join)(dirPath, "pet.json");
      if (!(0, import_node_fs9.existsSync)(manifestFile)) return null;
      const manifest = JSON.parse((0, import_node_fs9.readFileSync)(manifestFile, "utf8"));
      const spriteRel = manifest.spritesheetPath || "spritesheet.webp";
      const spriteFull = (0, import_node_path10.join)(dirPath, spriteRel);
      if (!(0, import_node_fs9.existsSync)(spriteFull)) return null;
      shown.add(spriteFull);
      const posterFull = (0, import_node_path10.join)(dirPath, "poster.webp");
      const hasPoster = (0, import_node_fs9.existsSync)(posterFull);
      if (hasPoster) shown.add(posterFull);
      const id = sanitizePetId(manifest.id || (0, import_node_path10.basename)(dirPath));
      return {
        id,
        displayName: manifest.displayName || id,
        description: manifest.description || "",
        spriteVersionNumber: manifest.spriteVersionNumber === 2 ? 2 : 1,
        kind: manifest.kind || "creature",
        ownerName: manifest.ownerName || manifest.ownerHandle || sourceLabel,
        tags: Array.isArray(manifest.tags) ? manifest.tags : [],
        likeCount: manifest.likeCount || 0,
        installed: true,
        source: sourceLabel,
        localDir: dirPath,
        spritesheetUrl: `abacus-file://local/${encodeURIComponent(spriteFull)}`,
        posterUrl: hasPoster ? `abacus-file://local/${encodeURIComponent(posterFull)}` : null
      };
    } catch {
      return null;
    }
  };

  const PET_WIN_W = 340;
  const PET_WIN_H = 290;
  const DEFAULT_PET_CONFIG = {
    enabled: true,
    activePetId: "yuki-cream-rabbit",
    size: 96,
    roaming: true,
    cursorLook: true,
    speechBubbles: true,
    osNotifications: "always",
    soundEffects: true,
    manualState: "auto",
    winX: null,
    winY: null
  };

  const getPetConfigPath = () => (0, import_node_path10.join)(getAbacusPetsDir(), "config.json");
  const loadPetConfig = () => {
    try {
      const p = getPetConfigPath();
      if ((0, import_node_fs9.existsSync)(p)) {
        const parsed = JSON.parse((0, import_node_fs9.readFileSync)(p, "utf8"));
        return { ...DEFAULT_PET_CONFIG, ...parsed };
      }
    } catch {}
    return { ...DEFAULT_PET_CONFIG };
  };
  let petConfig = loadPetConfig();
  const savePetConfig = (next) => {
    petConfig = { ...petConfig, ...next };
    try {
      (0, import_node_fs9.writeFileSync)(getPetConfigPath(), JSON.stringify(petConfig, null, 2));
    } catch {}
    return petConfig;
  };

  const listAllInstalledPets = () => {
    const map = new Map();
    const scanRoot = (rootDir, label) => {
      if (!(0, import_node_fs9.existsSync)(rootDir)) return;
      try {
        for (const entry of (0, import_node_fs9.readdirSync)(rootDir, { withFileTypes: true })) {
          if (!entry.isDirectory()) continue;
          const pet = readInstalledPetDir((0, import_node_path10.join)(rootDir, entry.name), label);
          if (pet && !map.has(pet.id)) map.set(pet.id, pet);
        }
      } catch {}
    };
    scanRoot(getAbacusPetsDir(), "abacus");
    scanRoot(getCodexPetsDir(), "codex");
    return Array.from(map.values());
  };

  const clampPetWindowPos = (x, y) => {
    const displays = import_electron7.screen.getAllDisplays();
    let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
    for (const d of displays) {
      minX = Math.min(minX, d.bounds.x);
      minY = Math.min(minY, d.bounds.y);
      maxX = Math.max(maxX, d.bounds.x + d.bounds.width);
      maxY = Math.max(maxY, d.bounds.y + d.bounds.height);
    }
    if (!Number.isFinite(minX)) return { x: 800, y: 500 };
    return {
      x: Math.max(minX - 60, Math.min(maxX - PET_WIN_W + 60, Math.round(x))),
      y: Math.max(minY - 80, Math.min(maxY - PET_WIN_H + 40, Math.round(y)))
    };
  };

  const defaultPetWindowPos = () => {
    const wa = import_electron7.screen.getPrimaryDisplay().workArea;
    return {
      x: wa.x + wa.width - PET_WIN_W - 18,
      y: wa.y + wa.height - PET_WIN_H - 4
    };
  };

  globalThis.broadcastPetOverlayState = () => {
    const installed = listAllInstalledPets();
    const payload = {
      type: "state",
      config: petConfig,
      installed,
      mainVisible: isMainWindowVisible()
    };
    if (petOverlayWindow && !petOverlayWindow.isDestroyed() && petOverlayReady) {
      petOverlayWindow.webContents.send("pet:overlayEvent", payload);
    }
    for (const mw of getMainWindows()) {
      if (!mw.webContents.isDestroyed()) {
        mw.webContents.send("pet:overlayEvent", payload);
      }
    }
  };

  let petInteractiveRects = null;
  let petIgnoringMouse = true;
  let petCursorInside = false;
  let dragState = null;
  let dragLoopTimer = null;
  let petCursorTimer = null;
  let lastCursorPushAt = 0;

  const setPetMouseIgnore = (ignore) => {
    if (!petOverlayWindow || petOverlayWindow.isDestroyed()) return;
    if (ignore === petIgnoringMouse) return;
    petIgnoringMouse = ignore;
    if (ignore) petOverlayWindow.setIgnoreMouseEvents(true, { forward: true });
    else petOverlayWindow.setIgnoreMouseEvents(false);
  };

  const isCursorInPetRects = () => {
    if (!petOverlayWindow || petOverlayWindow.isDestroyed()) return false;
    const cur = import_electron7.screen.getCursorScreenPoint();
    const [wx, wy] = petOverlayWindow.getPosition();
    const rx = cur.x - wx;
    const ry = cur.y - wy;
    const rects = Array.isArray(petInteractiveRects) && petInteractiveRects.length > 0
      ? petInteractiveRects
      : computeDefaultActorHit();
    return rects.some(r => rx >= r.left && rx <= r.right && ry >= r.top && ry <= r.bottom);
  };

  const computeDefaultActorHit = () => {
    const sz = Math.max(48, Math.min(180, Number(petConfig.size) || 96));
    const ah = Math.round((sz * 208) / 192);
    const cx = Math.round(PET_WIN_W / 2);
    const bottom = PET_WIN_H - 34;
    return [{
      left: cx - Math.round(sz / 2) - 10,
      top: bottom - ah - 10,
      right: cx + Math.round(sz / 2) + 10,
      bottom: PET_WIN_H
    }];
  };

  const ensurePetCursorTracker = () => {
    if (petCursorTimer) return;
    petCursorTimer = setInterval(() => {
      if (!petConfig.enabled) return;
      if (!petOverlayWindow || petOverlayWindow.isDestroyed() || !petOverlayReady) return;
      try {
        const cur = import_electron7.screen.getCursorScreenPoint();
        const [wx, wy] = petOverlayWindow.getPosition();

        // Single authoritative OS-level hit test (prevents Chromium transparent-pixel mouseleave flicker)
        if (dragState) {
          setPetMouseIgnore(false);
          if (!petCursorInside) {
            petCursorInside = true;
            petOverlayWindow.webContents.send("pet:overlayEvent", { type: "hover", payload: true });
          }
        } else {
          const inside = isCursorInPetRects();
          setPetMouseIgnore(!inside);
          if (inside !== petCursorInside) {
            petCursorInside = inside;
            petOverlayWindow.webContents.send("pet:overlayEvent", { type: "hover", payload: inside });
          }
        }

        const now = Date.now();
        if (petConfig.cursorLook && now - lastCursorPushAt >= 90) {
          lastCursorPushAt = now;
          const cx = wx + Math.round(PET_WIN_W / 2);
          const cy = wy + PET_WIN_H - Math.round((petConfig.size || 96) / 2) - 24;
          petOverlayWindow.webContents.send("pet:overlayEvent", {
            type: "cursor",
            payload: { dx: cur.x - cx, dy: cur.y - cy }
          });
        }
      } catch {}
    }, 24);
  };

  const ensurePetOverlayWindow = () => {
    if (!petConfig.enabled) {
      if (petOverlayWindow && !petOverlayWindow.isDestroyed()) {
        petOverlayWindow.hide();
      }
      return null;
    }
    if (petOverlayWindow && !petOverlayWindow.isDestroyed()) {
      if (!petOverlayWindow.isVisible()) petOverlayWindow.showInactive();
      return petOverlayWindow;
    }
    const initPos = (typeof petConfig.winX === "number" && typeof petConfig.winY === "number")
      ? clampPetWindowPos(petConfig.winX, petConfig.winY)
      : defaultPetWindowPos();

    petOverlayReady = false;
    petOverlayWindow = new import_electron7.BrowserWindow({
      width: PET_WIN_W,
      height: PET_WIN_H,
      x: initPos.x,
      y: initPos.y,
      transparent: true,
      frame: false,
      alwaysOnTop: true,
      skipTaskbar: true,
      resizable: false,
      hasShadow: false,
      focusable: false,
      backgroundColor: "#00000000",
      webPreferences: {
        preload: rendererPaths(OUT_DIR).preload,
        contextIsolation: true,
        nodeIntegration: false,
        backgroundThrottling: false
      }
    });
    petOverlayWindow.setAlwaysOnTop(true, "screen-saver");
    petOverlayWindow.setVisibleOnAllWorkspaces(true, { visibleOnFullScreen: true });
    petIgnoringMouse = true;
    petOverlayWindow.setIgnoreMouseEvents(true, { forward: true });

    const petHtml = (0, import_node_path10.join)(OUT_DIR, "renderer", "pet.html");
    void petOverlayWindow.loadFile(petHtml);
    petOverlayWindow.webContents.on("did-finish-load", () => {
      petOverlayReady = true;
      globalThis.broadcastPetOverlayState();
      ensurePetCursorTracker();
    });
    petOverlayWindow.on("closed", () => {
      if (dragLoopTimer) {
        clearInterval(dragLoopTimer);
        dragLoopTimer = null;
      }
      dragState = null;
      petOverlayWindow = null;
      petOverlayReady = false;
    });
    return petOverlayWindow;
  };
  globalThis.ensurePetOverlayWindow = ensurePetOverlayWindow;

  import_electron7.ipcMain.handle("pets:listInstalled", async () => {
    return listAllInstalledPets();
  });

  import_electron7.ipcMain.handle("pets:getConfig", async () => petConfig);

  import_electron7.ipcMain.handle("pets:setConfig", async (_event, patch = {}) => {
    savePetConfig(patch || {});
    ensurePetOverlayWindow();
    globalThis.broadcastPetOverlayState();
    return petConfig;
  });

  import_electron7.ipcMain.handle("pets:getOverlayState", async () => ({
    config: petConfig,
    installed: listAllInstalledPets(),
    mainVisible: isMainWindowVisible()
  }));

  import_electron7.ipcMain.handle("pets:sendCommand", async (_event, cmd = {}) => {
    if (petOverlayWindow && !petOverlayWindow.isDestroyed() && petOverlayReady) {
      petOverlayWindow.webContents.send("pet:overlayEvent", { type: "command", payload: cmd });
    }
    return true;
  });

  import_electron7.ipcMain.handle("pets:resetPosition", async () => {
    const pos = defaultPetWindowPos();
    savePetConfig({ winX: pos.x, winY: pos.y });
    if (petOverlayWindow && !petOverlayWindow.isDestroyed()) {
      petOverlayWindow.setBounds({ x: pos.x, y: pos.y, width: PET_WIN_W, height: PET_WIN_H });
    }
    return pos;
  });

  import_electron7.ipcMain.on("pets:wakeMouse", () => {
    if (dragState || isCursorInPetRects()) {
      setPetMouseIgnore(false);
    }
  });

  import_electron7.ipcMain.on("pets:setInteractiveRects", (_event, rects) => {
    if (Array.isArray(rects)) {
      petInteractiveRects = rects;
    }
  });

  const performDragStep = () => {
    if (!dragState || !petOverlayWindow || petOverlayWindow.isDestroyed()) return;
    const cur = import_electron7.screen.getCursorScreenPoint();
    const dx = cur.x - dragState.curX;
    const dy = cur.y - dragState.curY;
    dragState.moved = Math.max(dragState.moved, Math.hypot(dx, dy));
    const clamped = clampPetWindowPos(dragState.winX + dx, dragState.winY + dy);
    petOverlayWindow.setPosition(clamped.x, clamped.y);
  };

  import_electron7.ipcMain.on("pets:dragStart", () => {
    if (!petOverlayWindow || petOverlayWindow.isDestroyed()) return;
    const cur = import_electron7.screen.getCursorScreenPoint();
    const [wx, wy] = petOverlayWindow.getPosition();
    dragState = { winX: wx, winY: wy, curX: cur.x, curY: cur.y, moved: 0 };
    setPetMouseIgnore(false);
    if (dragLoopTimer) clearInterval(dragLoopTimer);
    dragLoopTimer = setInterval(performDragStep, 16);
  });

  import_electron7.ipcMain.on("pets:dragStep", () => {
    performDragStep();
  });

  import_electron7.ipcMain.handle("pets:dragEnd", async () => {
    if (dragLoopTimer) {
      clearInterval(dragLoopTimer);
      dragLoopTimer = null;
    }
    if (!petOverlayWindow || petOverlayWindow.isDestroyed()) {
      dragState = null;
      return 0;
    }
    performDragStep();
    const moved = dragState ? dragState.moved : 0;
    dragState = null;
    const [wx, wy] = petOverlayWindow.getPosition();
    savePetConfig({ winX: wx, winY: wy });
    return moved;
  });

  import_electron7.ipcMain.handle("pets:roamStep", async (_event, dx) => {
    if (!petOverlayWindow || petOverlayWindow.isDestroyed() || dragState) return false;
    const [wx, wy] = petOverlayWindow.getPosition();
    const display = import_electron7.screen.getDisplayMatching({ x: wx, y: wy, width: PET_WIN_W, height: PET_WIN_H });
    const wa = display.workArea;
    const nextX = wx + Number(dx || 0);
    if ((dx < 0 && nextX < wa.x) || (dx > 0 && nextX > wa.x + wa.width - PET_WIN_W)) {
      return false;
    }
    petOverlayWindow.setPosition(Math.round(nextX), wy);
    petConfig.winX = Math.round(nextX);
    return true;
  });

  import_electron7.ipcMain.handle("pets:showMainWindow", async () => {
    restoreMainWindow();
    return true;
  });

  import_electron7.ipcMain.handle("pets:hideMainWindow", async () => {
    hideMainWindows();
    if (petOverlayWindow && !petOverlayWindow.isDestroyed() && petOverlayReady) {
      petOverlayWindow.webContents.send("pet:overlayEvent", {
        type: "command",
        payload: {
          state: "waving",
          text: "Abacus hidden! I'll keep notifying you here on your desktop.",
          duration: 4500,
          status: "normal",
          header: "Background Mode"
        }
      });
    }
    return true;
  });

  import_electron7.ipcMain.handle("pets:toggleMainWindow", async () => {
    if (isMainWindowVisible()) {
      hideMainWindows();
    } else {
      restoreMainWindow();
    }
    return isMainWindowVisible();
  });

  import_electron7.ipcMain.handle("pets:openStudio", async () => {
    const win = restoreMainWindow();
    if (win && !win.webContents.isDestroyed()) {
      win.webContents.send("pet:openStudio");
    }
    return true;
  });

  import_electron7.ipcMain.handle("pets:catalog", async (_event, params = {}) => {
    const qs = new URLSearchParams();
    qs.set("page", String(params.page || 1));
    qs.set("pageSize", String(params.pageSize || 48));
    const sort = String(params.sort || "popular");
    qs.set("sort", sort);
    if (sort === "random") qs.set("random", String(Math.random()));
    if (params.q && String(params.q).trim()) qs.set("q", String(params.q).trim());
    if (params.kind && params.kind !== "all") qs.set("kind", String(params.kind));
    if (params.version && params.version !== "all") qs.set("version", String(params.version));
    const res = await fetch(`https://codex-pets.net/api/pets?${qs.toString()}`, {
      headers: { "Accept": "application/json", "User-Agent": PET_UA }
    });
    if (!res.ok) throw new Error(`codex-pets.net returned HTTP ${res.status}`);
    return await res.json();
  });

  const fetchRemotePetMeta = async (rawIdOrUrl) => {
    let id = String(rawIdOrUrl || "").trim();
    id = id.replace(/^https?:\/\/codex-pets\.net\/(?:#\/)?(?:pets?\/)?/i, "").replace(/[/?#].*$/, "");
    id = sanitizePetId(id);
    if (!id) throw new Error("Please enter a valid codex-pets.net ID or URL");
    const res = await fetch(`https://codex-pets.net/api/pets/${encodeURIComponent(id)}`, {
      headers: { "Accept": "application/json", "User-Agent": PET_UA }
    });
    if (!res.ok) throw new Error(`Pet '${id}' not found on codex-pets.net (HTTP ${res.status})`);
    const data = await res.json();
    return data.pet || data;
  };

  import_electron7.ipcMain.handle("pets:getRemote", async (_event, rawIdOrUrl) => {
    return await fetchRemotePetMeta(rawIdOrUrl);
  });

  import_electron7.ipcMain.handle("pets:installRemote", async (_event, petOrId) => {
    const meta = typeof petOrId === "string" ? await fetchRemotePetMeta(petOrId) : petOrId;
    const id = sanitizePetId(meta.id || meta.displayName);
    if (!id) throw new Error("Invalid pet ID");
    let spriteUrl = meta.spritesheetUrl || "";
    if (spriteUrl.startsWith("/")) spriteUrl = `https://codex-pets.net${spriteUrl}`;
    if (!spriteUrl) {
      const fullMeta = await fetchRemotePetMeta(id);
      spriteUrl = fullMeta.spritesheetUrl?.startsWith("/") ? `https://codex-pets.net${fullMeta.spritesheetUrl}` : fullMeta.spritesheetUrl;
      Object.assign(meta, fullMeta);
    }
    const spriteRes = await fetch(spriteUrl, { headers: { "User-Agent": PET_UA } });
    if (!spriteRes.ok) throw new Error(`Failed to download spritesheet (HTTP ${spriteRes.status})`);
    const spriteBuf = Buffer.from(await spriteRes.arrayBuffer());
    const petDir = (0, import_node_path10.join)(getAbacusPetsDir(), id);
    (0, import_node_fs9.mkdirSync)(petDir, { recursive: true });
    const spriteFull = (0, import_node_path10.join)(petDir, "spritesheet.webp");
    (0, import_node_fs9.writeFileSync)(spriteFull, spriteBuf);

    let posterUrl = meta.posterUrl || "";
    if (posterUrl.startsWith("/")) posterUrl = `https://codex-pets.net${posterUrl}`;
    if (posterUrl) {
      try {
        const posterRes = await fetch(posterUrl, { headers: { "User-Agent": PET_UA } });
        if (posterRes.ok) {
          (0, import_node_fs9.writeFileSync)((0, import_node_path10.join)(petDir, "poster.webp"), Buffer.from(await posterRes.arrayBuffer()));
        }
      } catch {}
    }

    const vNum = meta.spriteVersionNumber === 2 || meta.validationReport?.spriteVersionNumber === 2 || meta.validationReport?.atlasSize === "1536x2288" ? 2 : 1;
    const manifest = {
      id,
      displayName: meta.displayName || id,
      description: meta.description || "",
      spritesheetPath: "spritesheet.webp",
      spriteVersionNumber: vNum,
      kind: meta.kind || "creature",
      ownerName: meta.ownerName || meta.ownerHandle || "codex-pets.net",
      tags: Array.isArray(meta.tags) ? meta.tags : [],
      likeCount: meta.likeCount || 0
    };
    (0, import_node_fs9.writeFileSync)((0, import_node_path10.join)(petDir, "pet.json"), JSON.stringify(manifest, null, 2));
    const item = readInstalledPetDir(petDir, "abacus");
    globalThis.broadcastPetOverlayState();
    return item;
  });

  import_electron7.ipcMain.handle("pets:saveCustom", async (_event, payload) => {
    const manifest = payload?.manifest || {};
    const id = sanitizePetId(manifest.id || manifest.displayName || `custom-${Date.now()}`);
    const petDir = (0, import_node_path10.join)(getAbacusPetsDir(), id);
    (0, import_node_fs9.mkdirSync)(petDir, { recursive: true });
    const spriteBuf = Buffer.from(String(payload.spritesheetBase64 || ""), "base64");
    const ext = payload.ext || ".webp";
    const spriteFile = `spritesheet${ext}`;
    (0, import_node_fs9.writeFileSync)((0, import_node_path10.join)(petDir, spriteFile), spriteBuf);
    const cleanManifest = {
      id,
      displayName: manifest.displayName || id,
      description: manifest.description || "Custom imported pet",
      spritesheetPath: spriteFile,
      spriteVersionNumber: manifest.spriteVersionNumber === 2 ? 2 : 1,
      kind: manifest.kind || "creature",
      ownerName: manifest.ownerName || "local"
    };
    (0, import_node_fs9.writeFileSync)((0, import_node_path10.join)(petDir, "pet.json"), JSON.stringify(cleanManifest, null, 2));
    const item = readInstalledPetDir(petDir, "abacus");
    globalThis.broadcastPetOverlayState();
    return item;
  });

  import_electron7.ipcMain.handle("pets:removeInstalled", async (_event, rawId) => {
    const id = sanitizePetId(rawId);
    if (!id) return false;
    const petDir = (0, import_node_path10.join)(getAbacusPetsDir(), id);
    if ((0, import_node_fs9.existsSync)(petDir)) {
      (0, import_node_fs9.rmSync)(petDir, { recursive: true, force: true });
    }
    globalThis.broadcastPetOverlayState();
    return true;
  });

  import_electron7.ipcMain.handle("pets:notify", async (_event, payload = {}) => {
    try {
      const mode = petConfig.osNotifications || "always";
      if (mode === "off") return false;
      const mw = getPrimaryMainWindow();
      const mainFocused = Boolean(mw && mw.isVisible() && !mw.isMinimized() && mw.isFocused());
      if (mode === "unfocused" && mainFocused) return false;

      if (import_electron7.Notification.isSupported()) {
        const notif = new import_electron7.Notification({
          title: String(payload.title || "Abacus Companion"),
          body: String(payload.body || ""),
          silent: Boolean(payload.silent)
        });
        notif.on("click", () => {
          restoreMainWindow();
        });
        notif.show();
        return true;
      }
    } catch {}
    return false;
  });
}
registerFileScheme();
if (!import_electron7.app.requestSingleInstanceLock()) {
  import_electron7.app.quit();
} else {
  import_electron7.app.on("second-instance", (_event, argv, workingDirectory) => {
    const workspace = workspaceFromArgv(argv, workingDirectory, import_electron7.app.isPackaged);
    const window = getPrimaryMainWindow();
    if (workspace) {
      openWindow(workspace);
    } else if (window) {
      restoreMainWindow();
    } else {
      openWindow();
    }
  });
  void import_electron7.app.whenReady().then(() => {
    recent = RecentWorkspaces.in(import_electron7.app.getPath("userData"));
    groups = SessionGroups.in(import_electron7.app.getPath("userData"));
    pins = SidebarPins.in(import_electron7.app.getPath("userData"));
    chatsDir = (0, import_node_path10.join)(import_electron7.app.getPath("userData"), "chats");
    serveFiles(import_electron7.session.defaultSession);
    registerIpc();
    openWindow(workspaceFromArgv(process.argv, process.cwd(), import_electron7.app.isPackaged));
    if (typeof globalThis.ensurePetOverlayWindow === "function") {
      globalThis.ensurePetOverlayWindow();
    }
    import_electron7.app.on("activate", () => {
      if (getMainWindows().length === 0) openWindow();
    });
  });
  import_electron7.app.on("window-all-closed", () => {
    if (process.platform !== "darwin") import_electron7.app.quit();
  });
  import_electron7.app.on("before-quit", () => {
    if (petOverlayWindow && !petOverlayWindow.isDestroyed()) {
      petOverlayWindow.destroy();
      petOverlayWindow = null;
    }
    sessions.stopAll();
  });
}
//# sourceMappingURL=index.cjs.map
