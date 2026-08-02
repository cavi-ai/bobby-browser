import { createHash } from "node:crypto";
import { deflateRawSync } from "node:zlib";
import { lstat, mkdir, readFile, readdir, realpath, writeFile } from "node:fs/promises";
import path from "node:path";

import { createReleaseEnvelope } from "./create-release-envelope.mjs";
import { resolveReleaseIdentity } from "./lib.mjs";

const BLOCK = 512;

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function assertSafePath(relativePath) {
  const portable = relativePath.split(path.sep).join("/");
  if (!portable || portable.startsWith("/") || portable.split("/").includes("..")) {
    throw new Error(`unsafe documentation archive path: ${relativePath}`);
  }
  return portable;
}

async function collectFiles(root, archiveRoot) {
  const resolvedRoot = await realpath(root);
  if (!(await lstat(resolvedRoot)).isDirectory()) throw new Error("docs root must be a directory");
  const safeArchiveRoot = assertSafePath(archiveRoot);
  const files = [];
  async function visit(directory, relativeDirectory = "") {
    const entries = await readdir(directory, { withFileTypes: true });
    entries.sort((left, right) => left.name.localeCompare(right.name));
    for (const entry of entries) {
      const relativePath = assertSafePath(relativeDirectory ? `${relativeDirectory}/${entry.name}` : entry.name);
      const absolute = path.join(directory, entry.name);
      const info = await lstat(absolute);
      if (info.isSymbolicLink()) throw new Error(`documentation archive rejects symlink: ${relativePath}`);
      if (info.isDirectory()) await visit(absolute, relativePath);
      else if (info.isFile()) files.push({ name: `${safeArchiveRoot}/${relativePath}`, contents: await readFile(absolute) });
      else throw new Error(`documentation archive rejects non-file: ${relativePath}`);
    }
  }
  await visit(resolvedRoot);
  if (!files.length) throw new Error("documentation archive requires files");
  return files;
}

function writeString(header, offset, length, value) {
  const bytes = Buffer.from(value);
  if (bytes.length > length) throw new Error(`tar field too long: ${value}`);
  bytes.copy(header, offset);
}

function writeOctal(header, offset, length, value) {
  const encoded = value.toString(8).padStart(length - 1, "0");
  if (encoded.length >= length) throw new Error("tar number too large");
  writeString(header, offset, length - 1, encoded);
}

function splitName(value) {
  if (Buffer.byteLength(value) <= 100) return { name: value, prefix: "" };
  for (let index = value.lastIndexOf("/"); index > 0; index = value.lastIndexOf("/", index - 1)) {
    const prefix = value.slice(0, index);
    const name = value.slice(index + 1);
    if (Buffer.byteLength(prefix) <= 155 && Buffer.byteLength(name) <= 100) return { name, prefix };
  }
  throw new Error(`archive path is too long: ${value}`);
}

function tarEntry({ name, contents, epoch }) {
  const header = Buffer.alloc(BLOCK);
  const split = splitName(name);
  writeString(header, 0, 100, split.name);
  writeOctal(header, 100, 8, 0o644);
  writeOctal(header, 108, 8, 0);
  writeOctal(header, 116, 8, 0);
  writeOctal(header, 124, 12, contents.length);
  writeOctal(header, 136, 12, epoch);
  header.fill(0x20, 148, 156);
  header[156] = 48;
  writeString(header, 257, 6, "ustar\0");
  writeString(header, 263, 2, "00");
  writeString(header, 345, 155, split.prefix);
  const checksum = header.reduce((total, byte) => total + byte, 0).toString(8).padStart(6, "0");
  writeString(header, 148, 6, checksum);
  header[154] = 0;
  header[155] = 0x20;
  return Buffer.concat([header, contents, Buffer.alloc((BLOCK - (contents.length % BLOCK)) % BLOCK)]);
}

function crc32(bytes) {
  let crc = 0xffffffff;
  for (const byte of bytes) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit += 1) crc = (crc >>> 1) ^ (crc & 1 ? 0xedb88320 : 0);
  }
  return (crc ^ 0xffffffff) >>> 0;
}

function gzip(tar, epoch) {
  const header = Buffer.from([0x1f, 0x8b, 0x08, 0, 0, 0, 0, 0, 0, 255]);
  header.writeUInt32LE(epoch >>> 0, 4);
  const trailer = Buffer.alloc(8);
  trailer.writeUInt32LE(crc32(tar), 0);
  trailer.writeUInt32LE(tar.length >>> 0, 4);
  return Buffer.concat([header, deflateRawSync(tar, { level: 9 }), trailer]);
}

async function writeIdenticalOrAbsent(target, bytes) {
  try {
    const existing = await readFile(target);
    if (!existing.equals(bytes)) throw new Error(`refusing to overwrite non-identical release artifact: ${target}`);
  } catch (error) {
    if (error?.code !== "ENOENT") throw error;
    await writeFile(target, bytes, { flag: "wx" });
  }
}

export async function createProductDocsReleaseArtifact(input) {
  const release = resolveReleaseIdentity(input);
  if (input.repository !== "cavi-ai/bobby-browser") throw new Error("invalid release repository");
  const manifest = {
    schemaVersion: 1,
    slug: "bobby-browser",
    kind: "product-docs",
    version: release.version,
    tag: release.tag,
    repository: input.repository,
    commit: release.commit,
  };
  const files = await collectFiles(input.docsRoot, `docs/bobby-browser/v${release.version}`);
  const releaseBytes = Buffer.from(`${JSON.stringify(manifest, null, 2)}\n`);
  const entries = [{ name: "cavi-release.json", contents: releaseBytes }, ...files]
    .sort((left, right) => left.name.localeCompare(right.name));
  const tar = Buffer.concat([...entries.map((entry) => tarEntry({ ...entry, epoch: release.sourceDateEpoch })), Buffer.alloc(BLOCK * 2)]);
  const archive = gzip(tar, release.sourceDateEpoch);
  const artifactSha256 = sha256(archive);
  const artifactName = `bobby-browser-docs-${release.tag}.tar.gz`;
  await mkdir(input.outputDirectory, { recursive: true });
  const artifactPath = path.join(input.outputDirectory, artifactName);
  const checksumPath = `${artifactPath}.sha256`;
  await writeIdenticalOrAbsent(artifactPath, archive);
  await writeIdenticalOrAbsent(checksumPath, Buffer.from(`${artifactSha256}  ${artifactName}\n`));
  return {
    artifactName,
    artifactPath,
    checksumPath,
    artifactSha256,
    manifest,
    envelope: createReleaseEnvelope({ ...release, repository: input.repository, artifactSha256 }),
  };
}

if (process.argv[1] && import.meta.filename === process.argv[1]) {
  const values = Object.fromEntries(process.argv.slice(2).reduce((entries, option, index, args) => {
    if (option.startsWith("--") && args[index + 1] && !args[index + 1].startsWith("--")) entries.push([option.slice(2), args[index + 1]]);
    return entries;
  }, []));
  const releaseInput = {
    ...(values.version !== undefined ? { version: values.version } : {}),
    ...(values.tag !== undefined ? { tag: values.tag } : {}),
    ...(values.commit !== undefined ? { commit: values.commit } : {}),
    ...(values["source-date-epoch"] !== undefined
      ? { sourceDateEpoch: Number(values["source-date-epoch"]) }
      : {}),
  };
  createProductDocsReleaseArtifact({
    docsRoot: path.resolve(values["docs-root"]),
    outputDirectory: path.resolve(values.output),
    repository: values.repository,
    ...releaseInput,
  }).then((result) => {
    process.stdout.write(`${JSON.stringify(result.envelope)}\n`);
  }).catch((error) => {
    process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
    process.exitCode = 1;
  });
}
