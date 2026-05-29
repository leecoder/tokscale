#!/usr/bin/env node
import { execSync } from "node:child_process";
import { createWriteStream, chmodSync, mkdirSync, existsSync, readFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { get } from "node:https";
import { pipeline } from "node:stream/promises";

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = join(__dirname, "..");
const BIN_DIR = join(ROOT, "bin");

const REPO = "leecoder/tokscale";

function getTarget() {
  const arch = process.arch;
  const platform = process.platform;

  if (platform === "darwin") {
    if (arch === "arm64") return "aarch64-apple-darwin";
    if (arch === "x64") return "x86_64-apple-darwin";
  }

  if (platform === "linux") {
    const libc = detectLibc();
    if (arch === "arm64") {
      return libc === "musl"
        ? "aarch64-unknown-linux-musl"
        : "aarch64-unknown-linux-gnu";
    }
    if (arch === "x64") {
      return libc === "musl"
        ? "x86_64-unknown-linux-musl"
        : "x86_64-unknown-linux-gnu";
    }
  }

  if (platform === "win32") {
    if (arch === "arm64") return "aarch64-pc-windows-msvc";
    if (arch === "x64") return "x86_64-pc-windows-msvc";
  }

  return null;
}

function detectLibc() {
  try {
    const output = execSync("ldd --version 2>&1 || true", {
      encoding: "utf-8",
    }).toLowerCase();
    return output.includes("musl") ? "musl" : "gnu";
  } catch {
    return "gnu";
  }
}

function getVersion() {
  try {
    const content = readFileSync(join(ROOT, "package.json"), "utf-8");
    return JSON.parse(content).version;
  } catch {
    return null;
  }
}

function httpsGet(url) {
  return new Promise((resolve, reject) => {
    get(url, (res) => {
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        return httpsGet(res.headers.location).then(resolve, reject);
      }
      if (res.statusCode !== 200) {
        reject(new Error(`HTTP ${res.statusCode} for ${url}`));
        return;
      }
      resolve(res);
    }).on("error", reject);
  });
}

async function main() {
  const target = getTarget();
  if (!target) {
    console.error(
      `Unsupported platform: ${process.platform} ${process.arch}`
    );
    process.exit(1);
  }

  const version = getVersion();
  const isWindows = target.includes("windows");
  const binaryName = isWindows ? "tokscale.exe" : "tokscale";
  const assetName = isWindows
    ? `tokscale-${target}.exe`
    : `tokscale-${target}`;

  const tag = version ? `v${version}` : "latest";
  let url;
  if (tag === "latest") {
    url = `https://github.com/${REPO}/releases/latest/download/${assetName}`;
  } else {
    url = `https://github.com/${REPO}/releases/download/${tag}/${assetName}`;
  }

  console.log(`Downloading tokscale for ${target}...`);
  console.log(`  ${url}`);

  if (!existsSync(BIN_DIR)) {
    mkdirSync(BIN_DIR, { recursive: true });
  }

  const dest = join(BIN_DIR, binaryName);

  try {
    const res = await httpsGet(url);
    const ws = createWriteStream(dest);
    await pipeline(res, ws);

    if (!isWindows) {
      chmodSync(dest, 0o755);
    }

    console.log(`Installed tokscale to ${dest}`);
  } catch (err) {
    if (tag !== "latest") {
      const latestUrl = `https://github.com/${REPO}/releases/latest/download/${assetName}`;
      console.log(`  Version ${tag} not found, trying latest...`);
      try {
        const res = await httpsGet(latestUrl);
        const ws = createWriteStream(dest);
        await pipeline(res, ws);
        if (!isWindows) {
          chmodSync(dest, 0o755);
        }
        console.log(`Installed tokscale (latest) to ${dest}`);
        return;
      } catch {}
    }
    console.error(`Failed to download tokscale: ${err.message}`);
    console.error("You can build from source: cargo build --release -p tokscale-cli");
    process.exit(1);
  }
}

main();
