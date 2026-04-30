#!/usr/bin/env node

import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const packageRoot = path.resolve(__dirname, "..");
const binaryName = process.platform === "win32" ? "muagent.exe" : "muagent";

const candidates = [
  path.join(packageRoot, "target", "release", binaryName),
  path.join(packageRoot, "target", "debug", binaryName),
];

const binaryPath = candidates.find((candidate) => existsSync(candidate));

if (!binaryPath) {
  console.error(
    [
      "muagent native binary was not found.",
      "",
      "If this package was installed from local source, rebuild it with:",
      "  npm rebuild -g muagent",
      "",
      "Or build the Rust CLI directly:",
      "  cargo build --release --bin muagent",
    ].join("\n"),
  );
  process.exit(1);
}

const child = spawn(binaryPath, process.argv.slice(2), {
  stdio: "inherit",
  env: process.env,
});

child.on("error", (err) => {
  console.error(err);
  process.exit(1);
});

const forwardSignal = (signal) => {
  if (!child.killed) {
    child.kill(signal);
  }
};

for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
  process.on(signal, () => forwardSignal(signal));
}

child.on("exit", (code, signal) => {
  if (signal) {
    const signalExitCodes = {
      SIGHUP: 129,
      SIGINT: 130,
      SIGTERM: 143,
    };
    process.exit(signalExitCodes[signal] ?? 1);
    return;
  }
  process.exit(code ?? 1);
});
