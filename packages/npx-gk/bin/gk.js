#!/usr/bin/env node
// `npx gas-killer <args>`: the one-line install (install-gk.sh) on first use, then `gk`.
const { spawnSync } = require("child_process");
const { existsSync } = require("fs");
const path = require("path");
const os = require("os");

const home = process.env.GK_HOME || path.join(os.homedir(), ".gk");
const gk = path.join(home, "bin", "gk");
const installer = process.env.GK_INSTALLER_URL ||
  "https://gaskiller.xyz/bash";

if (!existsSync(gk)) {
  const r = spawnSync("sh", ["-c", `curl -fsSL "${installer}" | sh`], { stdio: "inherit" });
  if (r.status !== 0) process.exit(r.status ?? 1);
}
const run = spawnSync(gk, process.argv.slice(2), { stdio: "inherit" });
process.exit(run.status ?? 1);
