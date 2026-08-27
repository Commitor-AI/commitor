import { readFileSync } from "node:fs";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");

const cargo = readFileSync(resolve(root, "crates/cli/Cargo.toml"), "utf8");
const pkg = JSON.parse(
  readFileSync(resolve(root, "npm/package.json"), "utf8")
);

const m = cargo.match(/^version\s*=\s*"([^"]+)"/m);
const cargoVersion = m ? m[1] : null;

if (!cargoVersion) {
  console.error("Could not parse version from crates/cli/Cargo.toml");
  process.exit(1);
}

if (cargoVersion !== pkg.version) {
  console.error(
    "Version mismatch: crates/cli/Cargo.toml = " +
      cargoVersion +
      " but npm/package.json = " +
      pkg.version +
      "\nBump both to the same version in the release PR."
  );
  process.exit(1);
}

console.log("Versions in sync: " + cargoVersion);
