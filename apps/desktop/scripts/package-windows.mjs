// Builds the Windows sidecar and UI, stages them as Electron extra resources,
// and produces the unsigned NSIS installer. Run from apps/desktop.
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const desktop = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const repo = path.resolve(desktop, "../..");
const resources = path.join(desktop, "resources");
const target = process.platform === "win32"
  ? "x86_64-pc-windows-msvc"
  : "x86_64-pc-windows-gnu";

function quoteWindowsArg(value) {
  if (!/[\s"]/u.test(value)) return value;
  const escaped = value
    .replace(/(\\*)"/g, (_match, slashes) => `${slashes}${slashes}\"`)
    .replace(/(\\+)$/g, (_match, slashes) => `${slashes}${slashes}`);
  return `"${escaped}"`;
}

function run(command, args, extraEnv = {}) {
  console.log(`> ${command} ${args.join(" ")}`);
  // A Windows batch file needs cmd.exe, and cmd.exe needs every path quoted:
  // ordinary checkouts can live under "C:\\Users\\Ada Lovelace\\neha".
  const batch = process.platform === "win32" && /\.(cmd|bat)$/i.test(command);
  const file = batch ? (process.env.ComSpec || "cmd.exe") : command;
  const argv = batch
    ? ["/D", "/S", "/C", [command, ...args].map(quoteWindowsArg).join(" ")]
    : args;
  const result = spawnSync(file, argv, {
    cwd: desktop,
    stdio: "inherit",
    env: { ...process.env, ...extraEnv },
  });
  if (result.status !== 0) process.exit(result.status ?? 1);
}

run("cargo", ["build", "--release", "--target", target, "-p", "neha-app"]);
run(process.platform === "win32" ? "pnpm.cmd" : "pnpm", ["--dir", path.join(repo, "ui"), "build"]);

const sidecar = path.join(repo, "target", target, "release", process.platform === "win32" ? "neha.exe" : "neha.exe");
if (!fs.existsSync(sidecar)) throw new Error(`missing sidecar: ${sidecar}`);
const ui = path.join(repo, "ui", "dist");
if (!fs.existsSync(path.join(ui, "index.html"))) throw new Error(`missing UI build: ${ui}`);

fs.rmSync(resources, { recursive: true, force: true });
fs.mkdirSync(resources, { recursive: true });
fs.copyFileSync(sidecar, path.join(resources, "neha.exe"));
fs.cpSync(ui, path.join(resources, "ui"), { recursive: true });

run(
  process.platform === "win32"
    ? path.join(desktop, "node_modules", ".bin", "electron-builder.cmd")
    : path.join(desktop, "node_modules", ".bin", "electron-builder"),
  ["--win", "nsis", "--x64", "--publish", "never"],
  { CSC_IDENTITY_AUTO_DISCOVERY: "false" },
);
