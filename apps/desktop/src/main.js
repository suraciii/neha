// Local-first Neha desktop shell.
//
// The shell owns a bundled Space process, signs the first local Admin in
// without showing the invitation form, and loads the existing browser client.
// The Space remains the only server; this process is its supervisor.
const { app, BrowserWindow, safeStorage, session, shell } = require("electron");
const { spawn } = require("node:child_process");
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
let mainWindow;
let stopping = false;

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

async function postJson(url, body, origin) {
  const response = await fetch(url, {
    method: "POST",
    headers: { "content-type": "application/json", origin },
    body: JSON.stringify(body),
  });
  if (!response.ok) {
    const text = await response.text().catch(() => "");
    throw new Error(`${url} failed (${response.status})${text ? `: ${text}` : ""}`);
  }
  return response;
}

async function installCookie(response, origin) {
  const values = typeof response.headers.getSetCookie === "function"
    ? response.headers.getSetCookie()
    : [response.headers.get("set-cookie")];
  const pair = values.map(cookiePair).find(Boolean);
  if (!pair) throw new Error("the Space did not return a session cookie");
  await session.defaultSession.cookies.set({
    url: origin,
    name: pair.name,
    value: pair.value,
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
  const origin = browserUrl ? new URL(browserUrl).origin : inviteUrl.origin;
  const secret = inviteUrl.hash.replace(/^#/, "");
  if (!secret) throw new Error("the Space did not print a usable invitation");
  const displayName = defaultDisplayName(os.userInfo().username);
  const password = generatePassword();
  // Save before joining: a crash between the Space creating the account and
  // the shell writing its credentials would leave an account no later launch
  // can sign in to.
  await saveAccount(displayName, password);
  const response = await postJson(
    `${origin}/api/join`,
    { secret, display_name: displayName, password },
    origin,
  );
  await installCookie(response, origin);
  log(`created the first local Admin as ${displayName}`);
  return origin;
}

async function loginSavedAccount(origin) {
  const account = await loadAccount();
  if (!account) {
    throw new Error("this Space already has an account, but no local desktop account was found");
  }
  const response = await postJson(
    `${origin}/api/login`,
    { display_name: account.displayName, password: account.password },
    origin,
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
      env: { ...process.env, NEHA_HOME: spaceHome() },
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
      server = undefined;
      fail(new Error(`the Space exited before it was ready (${code ?? signal})`));
    });
  });
}

async function openLocalSpace() {
  const started = await startSpace();
  let origin;
  if (started.invite) {
    origin = await joinFirstAdmin(started.invite, started.browser);
  } else {
    origin = await discoverOrigin(started.listening);
    await loginSavedAccount(origin);
  }

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

function stopServer() {
  if (stopping) return;
  stopping = true;
  if (server && !server.killed) {
    const child = server;
    server = undefined;
    child.kill("SIGTERM");
    setTimeout(() => child.kill("SIGKILL"), 500).unref();
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
