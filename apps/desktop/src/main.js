// Local-first Neha desktop shell.
//
// The shell owns a bundled Space process, signs the first local Admin in
// without showing the invitation form, and loads the existing browser client.
// The Space remains the only server; this process is its supervisor.
const { app, BrowserWindow, dialog, safeStorage, session, shell } = require("electron");
const { spawn, spawnSync } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const readline = require("node:readline");

const {
  cookiePair,
  defaultDisplayName,
  generatePassword,
  parseStartupLine,
} = require("./startup");

const REPO_ROOT = path.resolve(__dirname, "../../..");
const START_TIMEOUT_MS = 20_000;
const READY_GRACE_MS = 300;

let server;
let localComputer;
let mainWindow;
let stopping = false;
let allowUnsandboxedWindowsComputer = false;

function log(...args) {
  console.error("[neha-desktop]", ...args);
}

function userDataDir() {
  return app.getPath("userData");
}

function accountFile() {
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

function unsandboxedConsentFile() {
  return path.join(computerHome(), "unsandboxed-consent.json");
}

async function confirmUnsandboxedMode() {
  if (process.platform !== "win32") return true;
  if (process.env.NEHA_DESKTOP_DECLINE_UNSANDBOXED === "1") return false;
  try {
    fs.readFileSync(unsandboxedConsentFile(), "utf8");
    return true;
  } catch {}
  const answer = await dialog.showMessageBox({
    type: "warning",
    title: "Windows Local Computer",
    message: "Windows Local Computer runs without an operating-system sandbox.",
    detail: "Commands run with your Windows account's rights and can read or change anything that account can, including Neha's saved credentials. Only use Bots you trust. Chat still works if you choose Not Now.",
    buttons: ["Use Local Computer", "Not Now"],
    defaultId: 1,
    cancelId: 1,
    noLink: true,
  });
  if (answer.response !== 0) {
    log("the Windows Local Computer was not enabled");
    return false;
  }
  fs.mkdirSync(computerHome(), { recursive: true });
  fs.writeFileSync(
    unsandboxedConsentFile(),
    JSON.stringify({ accepted: true, at: new Date().toISOString() }, null, 2),
    { mode: 0o600 },
  );
  return true;
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

function spaceEnv() {
  const env = { ...process.env, NEHA_HOME: spaceHome() };
  if (process.platform === "win32" && allowUnsandboxedWindowsComputer) {
    // The bundled Windows Local Computer has no OS sandbox. Enable the opt-in
    // only after the owner accepted the warning; otherwise the Space starts
    // without it and refuses the daemon.
    env.NEHA_ALLOW_UNSANDBOXED_COMPUTERS = "1";
  }
  return env;
}

function computerEnv() {
  return { ...process.env, NEHA_COMPUTER_HOME: computerHome() };
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

async function saveAccount(displayName, password) {
  const directory = userDataDir();
  fs.mkdirSync(directory, { recursive: true });
  let scheme = "safeStorage";
  let value;
  if (await safeStorage.isAsyncEncryptionAvailable()) {
    value = (await safeStorage.encryptStringAsync(password)).toString("base64");
  } else {
    // Windows uses DPAPI through Electron safeStorage. Keep a clearly marked
    // fallback for development hosts without a secret service; the packaged
    // release must not rely on this path.
    scheme = "plain";
    value = Buffer.from(password, "utf8").toString("base64");
    log("secure storage is unavailable; the development account password is stored locally in plain text");
  }
  fs.writeFileSync(
    accountFile(),
    JSON.stringify({ displayName, scheme, value }, null, 2),
    { mode: 0o600 },
  );
  try { fs.chmodSync(accountFile(), 0o600); } catch {}
}

async function loadAccount() {
  try {
    const body = JSON.parse(fs.readFileSync(accountFile(), "utf8"));
    if (!body || typeof body.displayName !== "string" || typeof body.value !== "string") return null;
    const bytes = Buffer.from(body.value, "base64");
    let password;
    if (body.scheme === "safeStorage") {
      password = (await safeStorage.decryptStringAsync(bytes)).result;
    } else {
      password = bytes.toString("utf8");
    }
    return { displayName: body.displayName, password };
  } catch (error) {
    if (error?.code !== "ENOENT") log("cannot read the saved account:", error.message);
    return null;
  }
}

async function postJson(url, body) {
  // This is a trusted main-process request, not a browser page, so it sends no
  // Origin. Keeping the URL on loopback avoids the system resolver for
  // arbitrary *.localhost names on Windows; the cookie is installed for the
  // browser Space separately.
  const response = await fetch(url, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!response.ok) {
    const text = await response.text().catch(() => "");
    throw new Error(`${url} failed (${response.status})${text ? `: ${text}` : ""}`);
  }
  return response;
}

function sessionCookieName(origin) {
  const match = /^neha-([^.]+)\.localhost$/.exec(new URL(origin).hostname);
  if (!match) throw new Error(`${origin} is not a browser Space origin`);
  return `neha_session_${match[1]}`;
}

async function installCookie(response, origin) {
  const values = typeof response.headers.getSetCookie === "function"
    ? response.headers.getSetCookie()
    : [response.headers.get("set-cookie")];
  const pair = values.map(cookiePair).find(Boolean);
  let name = pair?.name;
  let value = pair?.value;
  if (!value) {
    // A request with no Origin is the CLI shape: the Space returns the same
    // session token in the body instead of a Set-Cookie header.
    const body = await response.clone().json().catch(() => null);
    if (typeof body?.token !== "string") {
      throw new Error("the Space did not return a session token");
    }
    name = sessionCookieName(origin);
    value = body.token;
  }
  await session.defaultSession.cookies.set({
    url: origin,
    name,
    value,
    path: "/",
    httpOnly: true,
    sameSite: "strict",
  });
}

async function discoverOrigin(loopback) {
  const response = await fetch(`${loopback}/`, { redirect: "manual" });
  const location = response.headers.get("location");
  return location ? new URL(location).origin : new URL(loopback).origin;
}

async function joinFirstAdmin(invite, browserUrl) {
  const inviteUrl = new URL(invite);
  const loopback = inviteUrl.origin;
  const origin = browserUrl ? new URL(browserUrl).origin : loopback;
  const secret = inviteUrl.hash.replace(/^#/, "");
  if (!secret) throw new Error("the Space did not print a usable invitation");
  const displayName = defaultDisplayName(os.userInfo().username);
  const password = generatePassword();
  // Save before joining: a crash between the Space creating the account and
  // the shell writing its credentials would leave an account no later launch
  // can sign in to.
  await saveAccount(displayName, password);
  // Windows does not resolve arbitrary *.localhost names through the system
  // resolver. Send the request to loopback as a CLI-style request (no Origin),
  // then store its returned token as the browser Space's session cookie.
  const response = await postJson(
    `${loopback}/api/join`,
    { secret, display_name: displayName, password },
  );
  await installCookie(response, origin);
  log(`created the first local Admin as ${displayName}`);
  return origin;
}

async function loginSavedAccount(origin, loopback) {
  const account = await loadAccount();
  if (!account) {
    throw new Error("this Space already has an account, but no local desktop account was found");
  }
  const response = await postJson(
    `${loopback}/api/login`,
    { display_name: account.displayName, password: account.password },
  );
  await installCookie(response, origin);
  log(`signed in as ${account.displayName}`);
  return origin;
}

/**
 * Launches `neha serve` and resolves once it prints its loopback URL.
 *
 * The invitation and browser URL are captured while the process starts. A
 * Space with an existing account prints neither, so its browser origin is
 * discovered from the loopback redirect.
 */
function startSpace() {
  return new Promise((resolve, reject) => {
    const binary = binaryPath();
    if (!fs.existsSync(binary)) {
      reject(new Error(`the Space binary was not found at ${binary}`));
      return;
    }
    const ui = uiDir();
    if (!fs.existsSync(path.join(ui, "index.html"))) {
      reject(new Error(`the browser client was not found at ${ui}`));
      return;
    }

    let listening;
    let invite;
    let browser;
    let settled = false;
    const child = spawn(binary, ["serve", "--port", "0", "--ui", ui], {
      env: spaceEnv(),
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
      child.kill("SIGTERM");
      reject(error);
    };
    const timer = setTimeout(() => fail(new Error("the Space did not start in time")), START_TIMEOUT_MS);

    readline.createInterface({ input: child.stdout }).on("line", (line) => {
      log(line);
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

function waitForExit(child, timeoutMs = 2_000) {
  return new Promise((resolve) => {
    if (child.exitCode !== null || child.signalCode !== null) {
      resolve();
      return;
    }
    let settled = false;
    const finish = () => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      resolve();
    };
    const timer = setTimeout(() => {
      child.kill("SIGKILL");
      finish();
    }, timeoutMs);
    child.once("exit", finish);
    child.once("error", finish);
  });
}

async function restartSpace() {
  const old = server;
  server = undefined;
  if (old) {
    killTree(old);
    await waitForExit(old);
  }
  return startSpace();
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
    "--workspace",
    computerWorkspace(),
  ];
  if (ticketFile && fs.existsSync(ticketFile)) {
    args.push("--ticket-file", ticketFile);
  }
  const child = spawn(binaryPath(), args, {
    env: computerEnv(),
    stdio: ["ignore", "pipe", "pipe"],
    windowsHide: true,
  });
  child.stdout.on("data", (chunk) => process.stdout.write(`[pair] ${chunk}`));
  child.stderr.on("data", (chunk) => process.stderr.write(`[pair:err] ${chunk}`));
  await waitForExit(child, 15_000);
  return child.exitCode === 0;
}

async function startLocalComputer(spaceUrl, origin) {
  if (process.platform === "darwin") {
    log("the Local Computer is not available on macOS yet");
    return;
  }
  if (process.platform === "win32" && !allowUnsandboxedWindowsComputer) {
    log("the Windows Local Computer is disabled until its warning is accepted");
    return;
  }
  let paired = fs.existsSync(computerConfigFile());
  if (paired) {
    // The Space picks a fresh loopback port on every launch. The saved token
    // and Computer id stay valid; only the address has to move with it.
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
    paired = await runPairing(spaceUrl, ticketFileFor(origin));
    if (!paired) {
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
}

async function openLocalSpace() {
  if (process.platform === "win32") {
    allowUnsandboxedWindowsComputer = await confirmUnsandboxedMode();
  }
  const started = await startSpace();
  let origin;
  let spaceUrl;
  if (started.invite) {
    origin = await joinFirstAdmin(started.invite, started.browser);
    // The Space writes its local-computer ticket when it starts with an
    // existing Admin, not at the moment the invitation is accepted. Restart
    // once, then sign in to the new port.
    const restarted = await restartSpace();
    spaceUrl = restarted.listening;
    origin = await discoverOrigin(spaceUrl);
    await loginSavedAccount(origin, spaceUrl);
  } else {
    spaceUrl = started.listening;
    origin = await discoverOrigin(spaceUrl);
    await loginSavedAccount(origin, spaceUrl);
  }
  await startLocalComputer(spaceUrl, origin);

  mainWindow = new BrowserWindow({
    width: 1200,
    height: 800,
    minWidth: 720,
    minHeight: 520,
    backgroundColor: "#f4f3ee",
    show: false,
    title: "Neha",
    webPreferences: {
      contextIsolation: true,
      sandbox: true,
      nodeIntegration: false,
    },
  });

  session.defaultSession.setPermissionRequestHandler((_contents, _permission, callback) => {
    callback(false);
  });

  const allowedOrigin = new URL(origin).origin;
  mainWindow.webContents.on("will-navigate", (event, url) => {
    if (new URL(url).origin !== allowedOrigin) event.preventDefault();
  });
  mainWindow.webContents.setWindowOpenHandler(({ url }) => {
    if (new URL(url).protocol === "https:") void shell.openExternal(url);
    return { action: "deny" };
  });
  mainWindow.webContents.on("did-fail-load", (_event, code, description) => {
    log(`the client failed to load (${code} ${description})`);
  });
  mainWindow.once("ready-to-show", () => mainWindow?.show());
  await mainWindow.loadURL(`${allowedOrigin}/`);

  const capture = process.env.NEHA_DESKTOP_CAPTURE;
  if (capture) {
    setTimeout(async () => {
      try {
        const image = await mainWindow.webContents.capturePage();
        fs.writeFileSync(capture, image.toPNG());
        log(`wrote ${capture}`);
      } catch (error) {
        log("cannot capture the client:", error.message);
      } finally {
        app.quit();
      }
    }, 1_500);
  }
}

function killTree(child) {
  if (!child || child.killed || !child.pid) return;
  if (process.platform === "win32") {
    // child.kill() reaches only the daemon. taskkill /T also ends the
    // PowerShell/cmd command it started, so closing Neha cannot orphan a Bot.
    spawnSync("taskkill", ["/PID", String(child.pid), "/T", "/F"], {
      stdio: "ignore",
      windowsHide: true,
    });
    return;
  }
  child.kill("SIGTERM");
  setTimeout(() => child.kill("SIGKILL"), 500).unref();
}

function stopServer() {
  if (stopping) return;
  stopping = true;
  if (localComputer) {
    const child = localComputer;
    localComputer = undefined;
    killTree(child);
  }
  if (server) {
    const child = server;
    server = undefined;
    killTree(child);
  }
}

function fail(error) {
  log(error?.stack || String(error));
  stopServer();
  // A failed startup has no window to attach a native dialog to, and blocking
  // on one under a headless launch can prevent the process from exiting.
  app.exit(1);
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
  app.on("before-quit", stopServer);

  if (process.env.NEHA_DESKTOP_HOME) {
    fs.mkdirSync(process.env.NEHA_DESKTOP_HOME, { recursive: true });
    app.setPath("userData", path.resolve(process.env.NEHA_DESKTOP_HOME));
  }
  app.whenReady().then(openLocalSpace).catch(fail);
}
