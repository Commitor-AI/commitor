#!/usr/bin/env node
"use strict";

const { spawnSync } = require("child_process");
const fs = require("fs");
const path = require("path");

const binName = process.platform === "win32" ? "commitor-bin.exe" : "commitor-bin";
const binPath = path.join(__dirname, binName);

if (!fs.existsSync(binPath)) {
  console.error(
    "[commitor] Prebuilt binary not found at " +
      binPath +
      "\nThe postinstall step may have been skipped (e.g. `npm install --ignore-scripts`)" +
      " or failed.\n\nReinstall without --ignore-scripts, or fall back to:\n" +
      "  cargo install commitor-cli\n" +
      "or download a prebuilt binary from https://github.com/Commitor-AI/commitor/releases"
  );
  process.exit(1);
}

const result = spawnSync(binPath, process.argv.slice(2), { stdio: "inherit" });

if (result.error) {
  console.error("[commitor] Failed to launch binary: " + result.error.message);
  process.exit(1);
}

process.exit(result.status === null ? 1 : result.status);
