import { createHash } from "node:crypto";
import { mkdir, readdir, readFile, stat, writeFile } from "node:fs/promises";
import { basename, join, relative, resolve } from "node:path";

const bundleRoot = resolve(process.argv[2] ?? "src-tauri/target/release/bundle");
const outputRoot = resolve(process.argv[3] ?? "release");

async function filesUnder(directory) {
  const entries = await readdir(directory, { withFileTypes: true });
  const files = [];
  for (const entry of entries) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) files.push(...(await filesUnder(path)));
    else if (entry.isFile()) files.push(path);
  }
  return files;
}

const files = (await filesUnder(bundleRoot)).sort();
if (files.length === 0) throw new Error(`no bundle files found under ${bundleRoot}`);

await mkdir(outputRoot, { recursive: true });
const checksums = [];
const artifacts = [];
for (const path of files) {
  const data = await readFile(path);
  const hash = createHash("sha256").update(data).digest("hex");
  const name = relative(bundleRoot, path);
  const info = await stat(path);
  checksums.push(`${hash}  ${name}`);
  artifacts.push({ path: name, bytes: info.size, sha256: hash });
}

await writeFile(join(outputRoot, "SHA256SUMS.txt"), `${checksums.join("\n")}\n`);
await writeFile(
  join(outputRoot, "release-manifest.json"),
  `${JSON.stringify({ application: "agent-send", artifacts }, null, 2)}\n`,
);
console.log(`manifest created for ${basename(bundleRoot)} (${files.length} files)`);
