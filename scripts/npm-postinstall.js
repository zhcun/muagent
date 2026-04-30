#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const packageRoot = path.resolve(__dirname, "..");
const binaryName = process.platform === "win32" ? "muagent.exe" : "muagent";
const binaryPath = path.join(packageRoot, "target", "release", binaryName);

if (process.env.MUAGENT_NPM_SKIP_BUILD) {
  console.log("Skipping muagent native build because MUAGENT_NPM_SKIP_BUILD is set.");
  process.exit(0);
}

const cargo = process.platform === "win32" ? "cargo.cmd" : "cargo";
const args = ["build", "--release", "--locked", "--bin", "muagent"];

console.log("Building muagent native binary with Cargo...");
const result = spawnSync(cargo, args, {
  cwd: packageRoot,
  stdio: "inherit",
  env: process.env,
});

if (result.error) {
  if (result.error.code === "ENOENT") {
    console.error(
      [
        "Cargo was not found in PATH.",
        "Install Rust first, then rerun `npm install -g .` from this repository.",
        "Rust installer: https://rustup.rs/",
      ].join("\n"),
    );
  } else {
    console.error(result.error);
  }
  process.exit(1);
}

if (result.status !== 0) {
  process.exit(result.status ?? 1);
}

if (!existsSync(binaryPath)) {
  console.error(`Cargo finished, but ${binaryPath} was not created.`);
  process.exit(1);
}
