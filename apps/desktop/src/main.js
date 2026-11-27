// Local-first Neha desktop shell.
//
// The shell asks where the Space lives, starts a bundled Space only for the
// local choice, and then lets the Space's own web client handle sign-in. The
// launcher is a small page from the same UI build, served by a private
// protocol; it is the only page with a desktop bridge.
const {
  app,
  BrowserWindow,
  ipcMain,
  Menu,
  net,
  protocol,
  session,
  shell,
} = require("electron");
const { spawn, spawnSync } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const readline = require("node:readline");
const { pathToFileURL } = require("node:url");

const { normalizeRemoteUrl, probeUrl } = require("./selection");
const { parseStartupLine } = require("./startup");

const REPO_ROOT = path.resolve(__dirname, "../../..");
const START_TIMEOUT_MS = 20_000;
const READY_GRACE_MS = 300;
const UI_SCHEME = "neha";
const LAUNCHER_URL = `${UI_SCHEME}://ui/desktop.html`;

protocol.registerSchemesAsPrivileged([
  {
    scheme: UI_SCHEME,
    privileges: { standard: true, secure: true, supportFetchAPI: true, corsEnabled: true },
  },
]);

let server;
let localComputer;
let loginWatcher;
let mainWindow;
let currentOrigin;
let quitting = false;
let cleanupDone = false;

function log(...args) {
  console.error("[neha-desktop]", ...args);
}

function userDataDir() {
  return app.getPath("userData");
}

function selectionFile() {
  return path.join(userDataDir(), "desktop-space.json");
}

function legacyAccountFile() {
  return path.join(userDataDir(), "account.json");
}

function spaceHome() {
  return process.env.NEHA_HOME || path.join(userDataDir(), "space");
}

function computerHome() {
  return path.join(userDataDir(), "computer");
}

function computerWorkspace() {
  return path.join(userDataDir(), "workspace");
}

function computerConfigFile() {
  return path.join(computerHome(), "computer.json");
}

function standardConfigDirectory() {
  if (process.platform === "win32") {
    const base = process.env.LOCALAPPDATA || process.env.APPDATA;
    return base ? path.join(base, "Neha") : null;
  }
  if (process.env.XDG_CONFIG_HOME && path.isAbsolute(process.env.XDG_CONFIG_HOME)) {
    return path.join(process.env.XDG_CONFIG_HOME, "neha");
  }
  return path.join(os.homedir(), ".config", "neha");
}

function ticketFileFor(origin) {
  const match = /^neha-([^.]+)\.localhost$/.exec(new URL(origin).hostname);
  const config = standardConfigDirectory();
  if (!match || !config) return null;
  return path.join(config, "spaces", match[1], "local-computer.ticket");
}

function binaryPath() {
  if (process.env.NEHA_BIN) return path.resolve(process.env.NEHA_BIN);
  const name = process.platform === "win32" ? "neha.exe" : "neha";
  if (app.isPackaged) return path.join(process.resourcesPath, name);
  return path.join(REPO_ROOT, "target", "debug", name);
}

function uiDir() {
  return process.env.NEHA_UI_DIR || (app.isPackaged
    ? path.join(process.resourcesPath, "ui")
    : path.join(REPO_ROOT, "ui", "dist"));
}

function computerEnv() {
  return { ...process.env, NEHA_COMPUTER_HOME: computerHome() };
}

function spaceEnv(useLocalComputer) {
  const env = { ...process.env, NEHA_HOME: spaceHome() };
  if (process.platform === "win32" && useLocalComputer) {
    // The explicit Windows Local Computer has no OS sandbox. The launcher
    // records the owner's choice and this is the only place that turns the
    // Space's opt-in on.
    env.NEHA_ALLOW_UNSANDBOXED_COMPUTERS = "1";
  }
  return env;
}

// ---------------------------------------------------------------- selection ---

function readSelection() {
  try {
    const body = JSON.parse(fs.readFileSync(selectionFile(), "utf8"));
    if (body?.version !== 1) return null;
    if (body.mode === "local") {
      return { mode: "local", localComputer: body.localComputer === true };
    }
    if (body.mode === "remote") {
      return { mode: "remote", spaceUrl: normalizeRemoteUrl(body.spaceUrl) };
    }
    return null;
  } catch {
    return null;
  }
}

function writeSelection(selection) {
  fs.mkdirSync(userDataDir(), { recursive: true });
  fs.writeFileSync(
    selectionFile(),
    JSON.stringify({ version: 1, ...selection }, null, 2),
    { mode: 0o600 },
  );
  try { fs.chmodSync(selectionFile(), 0o600); } catch {}
}

function clearSelection() {
  try { fs.rmSync(selectionFile(), { force: true }); } catch {}
}

// Old alpha builds created an Admin with a generated password and signed in
// automatically. The new flow uses the Space's normal account flow, so that
// account has no password its owner knows. Keep the old data in named backups
// and start clean rather than locking the owner out.
function archiveLegacyAutoAdmin() {
  if (!fs.existsSync(legacyAccountFile())) return;
  const stamp = new Date().toISOString().replace(/[:.]/g, "-");
  for (const name of ["space", "computer", "workspace"]) {
    const source = path.join(userDataDir(), name);
    if (!fs.existsSync(source)) continue;
    let target = path.join(userDataDir(), `${name}.pre-login-${stamp}`);
    for (let suffix = 1; fs.existsSync(target); suffix += 1) {
      target = path.join(userDataDir(), `${name}.pre-login-${stamp}-${suffix}`);
    }
    fs.renameSync(source, target);
    log(`archived the old ${name} folder at ${target}`);
  }
  fs.rmSync(legacyAccountFile(), { force: true });
  log("removed the old automatic account; the next local Space starts with a normal sign-in");
}

// ------------------------------------------------------------------- protocol ---

function installUiProtocol() {
  const root = path.resolve(uiDir());
  protocol.handle(UI_SCHEME, async (request) => {
    const url = new URL(request.url);
    if (url.hostname !== "ui") {
      return new Response("Not found", { status: 404 });
    }
    const relative = decodeURIComponent(url.pathname).replace(/^\/+/, "") || "index.html";
    const file = path.resolve(root, relative);
    if (file !== root && !file.startsWith(root + path.sep)) {
      return new Response("Not found", { status: 404 });
    }
    return net.fetch(pathToFileURL(file).toString());
  });
}

// --------------------------------------------------------------------- window ---

function createWindow() {
  mainWindow = new BrowserWindow({
    width: 1200,
    height: 800,
    minWidth: 720,
    minHeight: 520,
    backgroundColor: "#f2f3ee",
    show: false,
    title: "Neha",
    icon: app.isPackaged
      ? path.join(process.resourcesPath, "icon.png")
      : path.join(REPO_ROOT, "apps", "desktop", "build", "icon.png"),
    autoHideMenuBar: true,
    webPreferences: {
      contextIsolation: true,
      sandbox: true,
      nodeIntegration: false,
      preload: path.join(__dirname, "preload.js"),
    },
  });
  installMenu();

  mainWindow.webContents.on("will-navigate", (event, url) => {
    if (!navigationAllowed(url)) {
      event.preventDefault();
      log(`blocked navigation to ${url}`);
    }
  });
  mainWindow.webContents.setWindowOpenHandler(({ url }) => {
    if (new URL(url).protocol === "https:") void shell.openExternal(url);
    return { action: "deny" };
  });
  mainWindow.webContents.on("did-fail-load", (_event, code, description) => {
    log(`the client failed to load (${code} ${description})`);
  });
  mainWindow.once("ready-to-show", () => mainWindow?.show());
  session.defaultSession.setPermissionRequestHandler((_contents, _permission, callback) => {
    callback(false);
  });
}

function installMenu() {
  const template = [
    ...(process.platform === "darwin" ? [{ role: "appMenu" }] : []),
    {
      label: "Neha",
      submenu: [
        {
          label: "Change Space…",
          accelerator: "CommandOrControl+Shift+K",
          click: () => void resetToLauncher(),
        },
        { type: "separator" },
        { role: "quit" },
      ],
    },
    { role: "editMenu" },
  ];
  Menu.setApplicationMenu(Menu.buildFromTemplate(template));
}

function navigationAllowed(url) {
  try {
    const next = new URL(url);
    if (next.protocol === UI_SCHEME && next.hostname === "ui") return true;
    return currentOrigin !== null && next.origin === currentOrigin;
  } catch {
    return false;
  }
}

async function showUrl(url) {
  if (!mainWindow) throw new Error("the desktop window is not open");
  currentOrigin = new URL(url).origin;
  log(`loading ${url}`);
  await mainWindow.loadURL(url);
  log(`loaded ${url}`);
  maybeCapture();
}

async function showLauncher() {
  if (!mainWindow) throw new Error("the desktop window is not open");
  currentOrigin = null;
  log("loading the Space chooser");
  await mainWindow.loadURL(LAUNCHER_URL);
  log("loaded the Space chooser");
  maybeCapture();
}

function maybeCapture() {
  const target = process.env.NEHA_DESKTOP_CAPTURE;
  if (!target || !mainWindow) return;
  setTimeout(async () => {
    try {
      const image = await mainWindow.webContents.capturePage();
      fs.writeFileSync(target, image.toPNG());
      log(`wrote ${target}`);
    } catch (error) {
      log("cannot capture the client:", error.message);
    } finally {
      app.quit();
    }
  }, 1_500);
}

// ---------------------------------------------------------------------- space ---

function startSpace(useLocalComputer) {
  return new Promise((resolve, reject) => {
    const binary = binaryPath();
    if (!fs.existsSync(binary)) {
      reject(new Error(`the Space binary was not found at ${binary}`));
      return;
    }
    const ui = uiDir();
    for (const file of ["index.html", "desktop.html"]) {
      if (!fs.existsSync(path.join(ui, file))) {
        reject(new Error(`the client build is missing ${file} at ${ui}`));
        return;
      }
    }

    let listening;
    let invite;
    let browser;
    let settled = false;
    const child = spawn(binary, ["serve", "--port", "0", "--ui", ui], {
      env: spaceEnv(useLocalComputer),
      stdio: ["ignore", "pipe", "pipe"],
      windowsHide: true,
    });
    server = child;

    const finish = () => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      resolve({ child, listening, invite, browser });
    };
    const fail = (error) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      if (server === child) server = undefined;
      killTree(child);
      reject(error);
    };
    const timer = setTimeout(() => fail(new Error("the Space did not start in time")), START_TIMEOUT_MS);

    readline.createInterface({ input: child.stdout }).on("line", (line) => {
      process.stdout.write(`[space] ${line}\n`);
      const parsed = parseStartupLine(line);
      if (!parsed) return;
      if (parsed.listening) {
        listening = parsed.listening;
        setTimeout(finish, READY_GRACE_MS);
      }
      if (parsed.invite) invite = parsed.invite;
      if (parsed.browser) browser = parsed.browser;
    });
    child.stderr.on("data", (chunk) => process.stderr.write(`[space] ${chunk}`));
    child.once("error", (error) => fail(error));
    child.once("exit", (code, signal) => {
      if (server === child) server = undefined;
      fail(new Error(`the Space exited before it was ready (${code ?? signal})`));
    });
  });
}

async function discoverOrigin(loopback) {
  const response = await fetch(`${loopback}/`, { redirect: "manual" });
  if (!response.ok && !(response.status >= 300 && response.status < 400)) {
    throw new Error(`the Space did not answer at ${loopback} (${response.status})`);
  }
  const location = response.headers.get("location");
  if (location) return new URL(location, loopback).origin;
  return new URL(loopback).origin;
}

function killTree(child) {
  if (!child || child.killed) return;
  if (process.platform === "win32" && child.pid) {
    spawnSync("taskkill", ["/PID", String(child.pid), "/T", "/F"], {
      stdio: "ignore",
      windowsHide: true,
    });
    return;
  }
  child.kill("SIGTERM");
}

async function stopSpace() {
  const child = server;
  server = undefined;
  if (child) killTree(child);
}

// --------------------------------------------------------------- local computer ---

function stopLocalComputer() {
  if (loginWatcher) {
    loginWatcher.abort();
    loginWatcher = undefined;
  }
  if (localComputer) {
    const child = localComputer;
    localComputer = undefined;
    killTree(child);
  }
}

function sessionCookieName(origin) {
  const match = /^neha-([^.]+)\.localhost$/.exec(new URL(origin).hostname);
  if (!match) throw new Error(`${origin} is not a local Space origin`);
  return `neha_session_${match[1]}`;
}

function wait(ms, signal) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(resolve, ms);
    signal.addEventListener("abort", () => {
      clearTimeout(timer);
      reject(new Error("aborted"));
    }, { once: true });
  });
}

function watchForLocalLogin(origin, spaceUrl) {
  const cookieName = sessionCookieName(origin);
  const controller = new AbortController();
  loginWatcher = controller;
  void (async () => {
    try {
      while (!controller.signal.aborted) {
        const cookies = await session.defaultSession.cookies.get({ url: origin });
        const cookie = cookies.find((item) => item.name === cookieName && item.value);
        if (cookie) {
          await startLocalComputer(spaceUrl, origin, cookie);
          return;
        }
        await wait(500, controller.signal);
      }
    } catch (error) {
      if (!controller.signal.aborted) {
        log("the Local Computer could not start; Chat is still available:", error.message);
      }
    }
  })();
}

async function requestTicket(spaceUrl, cookie) {
  // The session cookie is scoped to the Space's browser host, while this
  // trusted main-process request goes to loopback. Send its token the way the
  // CLI does, so the ticket endpoint authenticates without a browser Host.
  const response = await fetch(`${spaceUrl}/api/computer/ticket`, {
    method: "POST",
    headers: { authorization: `Bearer ${cookie.value}` },
  });
  if (!response.ok) {
    throw new Error(`the Space did not issue a Local Computer ticket (${response.status})`);
  }
  const body = await response.json();
  if (typeof body?.ticket !== "string" || body.ticket.length === 0) {
    throw new Error("the Space returned no Local Computer ticket");
  }
  return body.ticket;
}

async function writeTicketFile(origin, spaceUrl, ticket) {
  const file = ticketFileFor(origin) || path.join(computerHome(), "local-computer.ticket");
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, JSON.stringify({ ticket, url: spaceUrl }), { mode: 0o600 });
  try { fs.chmodSync(file, 0o600); } catch {}
  return file;
}

async function runPairing(spaceUrl, ticketFile) {
  fs.mkdirSync(computerHome(), { recursive: true });
  fs.mkdirSync(computerWorkspace(), { recursive: true });
  const args = [
    "computer",
    "pair",
    "--yes",
    "--space",
    spaceUrl,
    "--ticket-file",
    ticketFile,
    "--workspace",
    computerWorkspace(),
  ];
  const child = spawn(binaryPath(), args, {
    env: computerEnv(),
    stdio: ["ignore", "pipe", "pipe"],
    windowsHide: true,
  });
  child.stdout.on("data", (chunk) => process.stdout.write(`[pair] ${chunk}`));
  child.stderr.on("data", (chunk) => process.stderr.write(`[pair:err] ${chunk}`));
  const code = await new Promise((resolve) => {
    child.once("error", () => resolve(1));
    child.once("exit", (value) => resolve(value ?? 1));
  });
  return code === 0;
}

async function waitForComputerConnected(child, timeoutMs = 10_000) {
  return new Promise((resolve) => {
    let settled = false;
    const finish = () => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      resolve();
    };
    const timer = setTimeout(finish, timeoutMs);
    child.stdout.on("data", (chunk) => {
      if (chunk.toString().includes("Connected to")) finish();
    });
    child.once("exit", finish);
    child.once("error", finish);
  });
}

async function startLocalComputer(spaceUrl, origin, cookie) {
  if (process.platform === "darwin") {
    log("the Local Computer is not available on macOS yet");
    return;
  }
  let paired = fs.existsSync(computerConfigFile());
  if (paired) {
    try {
      const config = JSON.parse(fs.readFileSync(computerConfigFile(), "utf8"));
      if (config.space_url !== spaceUrl) {
        config.space_url = spaceUrl;
        fs.writeFileSync(computerConfigFile(), JSON.stringify(config), { mode: 0o600 });
        log(`moved the saved Local Computer pairing to ${spaceUrl}`);
      }
    } catch (error) {
      log("the saved Local Computer pairing is unreadable; pairing again:", error.message);
      paired = false;
    }
  }
  if (!paired) {
    const ticket = await requestTicket(spaceUrl, cookie);
    const file = await writeTicketFile(origin, spaceUrl, ticket);
    if (!(await runPairing(spaceUrl, file))) {
      log("the Local Computer could not pair; Chat is still available");
      return;
    }
  }
  localComputer = spawn(binaryPath(), [
    "computer",
    "run",
    "--workspace",
    computerWorkspace(),
  ], {
    env: computerEnv(),
    stdio: ["ignore", "pipe", "pipe"],
    windowsHide: true,
  });
  localComputer.stdout.on("data", (chunk) => process.stdout.write(`[computer] ${chunk}`));
  localComputer.stderr.on("data", (chunk) => process.stderr.write(`[computer:err] ${chunk}`));
  localComputer.once("exit", (code, signal) => {
    log(`the Local Computer exited (${code ?? signal})`);
  });
  log(`started the Local Computer at ${computerWorkspace()}`);
  await waitForComputerConnected(localComputer);
}

// -------------------------------------------------------------------- choices ---

async function openLocalSpace(useLocalComputer) {
  stopLocalComputer();
  await stopSpace();
  log("starting the local Space");
  const started = await startSpace(useLocalComputer);
  log(`the Space is ready at ${started.listening}`);
  const origin = await discoverOrigin(started.listening);
  log(`the browser origin is ${origin}`);
  await showUrl(started.invite ? (started.browser || started.invite) : `${origin}/`);
  if (useLocalComputer) watchForLocalLogin(origin, started.listening);
  log(`opened the local Space at ${origin}`);
}

async function openRemoteSpace(input, persist = true) {
  const origin = normalizeRemoteUrl(input);
  // Do not follow a redirect here: the Space may redirect to its browser host
  // (`*.localhost`), which Node cannot resolve on Windows. Chromium resolves it
  // when the window loads the address.
  const response = await fetch(probeUrl(origin), {
    redirect: "manual",
    signal: AbortSignal.timeout(8_000),
  });
  if (!response.ok && !(response.status >= 300 && response.status < 400)) {
    throw new Error(`the Space did not answer (${response.status})`);
  }
  stopLocalComputer();
  await stopSpace();
  if (persist) writeSelection({ mode: "remote", spaceUrl: origin });
  await showUrl(`${origin}/`);
  log(`opened the remote Space at ${origin}`);
}

async function resetToLauncher() {
  stopLocalComputer();
  await stopSpace();
  clearSelection();
  await showLauncher();
}

// ----------------------------------------------------------------------- ipc ---

function registerIpc() {
  ipcMain.handle("desktop:choose-local", async (_event, useLocalComputer) => {
    const enabled = useLocalComputer === true;
    try {
      writeSelection({ mode: "local", localComputer: enabled });
      await openLocalSpace(enabled);
      return { ok: true };
    } catch (error) {
      log(error?.stack || String(error));
      clearSelection();
      await stopLocalComputer();
      await stopSpace();
      await showLauncher();
      return { ok: false, message: error?.message || String(error) };
    }
  });

  ipcMain.handle("desktop:connect-remote", async (_event, url) => {
    try {
      await openRemoteSpace(String(url));
      return { ok: true };
    } catch (error) {
      log(error?.stack || String(error));
      return { ok: false, message: error?.message || String(error) };
    }
  });
}

// ------------------------------------------------------------------ lifecycle ---

async function openSavedSpace() {
  const selection = readSelection();
  if (selection?.mode === "remote") {
    await openRemoteSpace(selection.spaceUrl, false);
    return;
  }
  if (selection?.mode === "local") {
    await openLocalSpace(selection.localComputer === true);
    return;
  }
  await showLauncher();
}

function fail(error) {
  log(error?.stack || String(error));
  stopLocalComputer();
  void stopSpace();
  app.exit(1);
}

async function cleanup() {
  if (cleanupDone) return;
  cleanupDone = true;
  stopLocalComputer();
  await stopSpace();
}

if (!app.requestSingleInstanceLock()) {
  app.quit();
} else {
  app.on("second-instance", () => {
    if (mainWindow) {
      if (mainWindow.isMinimized()) mainWindow.restore();
      mainWindow.show();
      mainWindow.focus();
    }
  });
  app.on("window-all-closed", () => app.quit());
  app.on("before-quit", (event) => {
    if (quitting) return;
    quitting = true;
    event.preventDefault();
    void cleanup().finally(() => app.quit());
  });

  if (process.env.NEHA_DESKTOP_HOME) {
    fs.mkdirSync(process.env.NEHA_DESKTOP_HOME, { recursive: true });
    app.setPath("userData", path.resolve(process.env.NEHA_DESKTOP_HOME));
  }

  app.whenReady().then(async () => {
    installUiProtocol();
    createWindow();
    archiveLegacyAutoAdmin();
    registerIpc();
    try {
      await openSavedSpace();
    } catch (error) {
      log(error?.stack || String(error));
      if (error?.cause) log("cause:", error.cause);
      clearSelection();
      await stopLocalComputer();
      await stopSpace();
      await showLauncher();
    }
  }).catch(fail);
}
