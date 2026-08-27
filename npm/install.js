"use strict";

const fs = require("fs");
const https = require("https");
const path = require("path");
const crypto = require("crypto");
const { URL } = require("url");

const pkg = require("./package.json");
const VERSION = pkg.version;
const REPO = "Commitor-AI/commitor";

// Map Node's platform/arch to the cargo target triple used in release asset
// names. Must stay in sync with .github/workflows/release.yml.
function targetTriple() {
  const p = process.platform;
  const a = process.arch;
  if (p === "darwin" && a === "arm64") return "aarch64-apple-darwin";
  if (p === "darwin" && a === "x64") return "x86_64-apple-darwin";
  if (p === "linux" && a === "x64") return "x86_64-unknown-linux-gnu";
  if (p === "win32" && a === "x64") return "x86_64-pc-windows-msvc";
  return null;
}

function fail(msg) {
  console.error("\n[commitor-cli] ERROR: " + msg + "\n");
  process.exit(1);
}

function download(urlStr) {
  return new Promise((resolve, reject) => {
    const opts = { headers: { "User-Agent": "commitor-cli-install" } };
    https
      .get(urlStr, opts, (res) => {
        if (res.statusCode === 301 || res.statusCode === 302) {
          const next = new URL(res.headers.location, urlStr).toString();
          res.resume();
          return resolve(download(next));
        }
        if (res.statusCode !== 200) {
          res.resume();
          return reject(new Error("HTTP " + res.statusCode));
        }
        const chunks = [];
        res.on("data", (c) => chunks.push(c));
        res.on("end", () => resolve(Buffer.concat(chunks)));
      })
      .on("error", reject);
  });
}

async function main() {
  const triple = targetTriple();
  if (!triple) {
    fail(
      "Unsupported platform/architecture: " +
        process.platform +
        "/" +
        process.arch +
        ".\ncommitor-cli ships prebuilt binaries for:\n" +
        "  - macOS arm64 / x64\n  - Linux x64\n  - Windows x64\n\n" +
        "Install from source instead: `cargo install commitor-cli`\n" +
        "or download a binary manually from https://github.com/" +
        REPO +
        "/releases"
    );
  }

  const isWin = process.platform === "win32";
  const asset = "commitor-" + triple + (isWin ? ".exe" : "");
  const base =
    "https://github.com/" + REPO + "/releases/download/v" + VERSION + "/";
  const url = base + asset;
  const sumUrl = base + asset + ".sha256";

  const binDest = path.join(
    __dirname,
    "bin",
    isWin ? "commitor-bin.exe" : "commitor-bin"
  );

  console.log(
    "[commitor-cli] Downloading prebuilt binary (" + triple + ", v" + VERSION + ")..."
  );

  let buf;
  try {
    buf = await download(url);
  } catch (e) {
    fail(
      "Failed to download commitor binary from:\n  " +
        url +
        "\n\nError: " +
        e.message +
        "\n\nInstall from source via `cargo install commitor-cli`\n" +
        "or download a prebuilt binary manually from https://github.com/" +
        REPO +
        "/releases"
    );
  }

  if (buf.length === 0) {
    fail("Downloaded binary is empty (0 bytes) from:\n  " + url);
  }

  // Verify checksum if the release publishes one.
  try {
    const sumBuf = await download(sumUrl);
    const expected = sumBuf.toString("utf8").trim().split(/\s+/)[0];
    const actual = crypto.createHash("sha256").update(buf).digest("hex");
    if (expected !== actual) {
      fail(
        "Checksum mismatch for " +
          asset +
          ".\n  expected: " +
          expected +
          "\n  actual:   " +
          actual
      );
    }
    console.log("[commitor-cli] Checksum verified.");
  } catch (e) {
    console.warn(
      "[commitor-cli] WARNING: could not verify checksum (" +
        e.message +
        "). Proceeding with size check only."
    );
  }

  fs.writeFileSync(binDest, buf);
  if (!isWin) fs.chmodSync(binDest, 0o755);

  console.log("[commitor-cli] Installed to " + binDest);
}

main().catch((e) => fail(e && e.stack ? e.stack : String(e)));
