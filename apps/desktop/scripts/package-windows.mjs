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

function run(command, args, extraEnv = {}) {
  console.log(`> ${command} ${args.join(" ")}`);
  const result = spawnSync(command, args, {
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
