#!/usr/bin/env node
// Set one version everywhere a release reads it: [workspace.package] in
// Cargo.toml, every hertz-* crate in Cargo.lock, and npm/hertz/package.json with
// its per-platform optional dependencies. Ported from Lume.
//
//   node scripts/bump-version.cjs patch|minor|major
//   node scripts/bump-version.cjs 1.2.3
//
// Prints the new version on stdout.
'use strict';
const fs = require('fs');
const path = require('path');

const root = path.resolve(__dirname, '..');
const files = {
  cargo: path.join(root, 'Cargo.toml'),
  lock: path.join(root, 'Cargo.lock'),
  cli: path.join(root, 'npm/hertz/package.json'),
};
const SEMVER = /^(\d+)\.(\d+)\.(\d+)$/;
const WORKSPACE_VERSION = /^(\[workspace\.package\]\r?\nversion = ")([^"]+)"/m;
const LOCK_ENTRY = /^(name = "hertz-[a-z]+"\r?\nversion = ")([^"]+)"/gm;

function nextVersion(current, request) {
  if (SEMVER.test(request)) return request;
  const parts = current.match(SEMVER);
  if (!parts) throw new Error(`current version ${current} is not MAJOR.MINOR.PATCH`);
  let [major, minor, patch] = parts.slice(1).map(Number);
  if (request === 'major') [major, minor, patch] = [major + 1, 0, 0];
  else if (request === 'minor') [minor, patch] = [minor + 1, 0];
  else if (request === 'patch') patch += 1;
  else throw new Error(`expected patch, minor, major or MAJOR.MINOR.PATCH, got "${request}"`);
  return `${major}.${minor}.${patch}`;
}

function main() {
  const request = process.argv[2];
  if (!request) throw new Error('usage: bump-version.cjs patch|minor|major|MAJOR.MINOR.PATCH');
  const cargo = fs.readFileSync(files.cargo, 'utf8');
  const match = cargo.match(WORKSPACE_VERSION);
  if (!match) throw new Error('could not find [workspace.package] version in Cargo.toml');
  const current = match[2];
  const next = nextVersion(current, request);
  if (next === current) throw new Error(`version is already ${current}`);
  fs.writeFileSync(files.cargo, cargo.replace(WORKSPACE_VERSION, (_, head) => `${head}${next}"`));

  let count = 0;
  const lock = fs.readFileSync(files.lock, 'utf8').replace(LOCK_ENTRY, (_, head) => { count += 1; return `${head}${next}"`; });
  if (count === 0) throw new Error('no hertz-* entries in Cargo.lock');
  fs.writeFileSync(files.lock, lock);

  const pkg = JSON.parse(fs.readFileSync(files.cli, 'utf8'));
  pkg.version = next;
  for (const name of Object.keys(pkg.optionalDependencies || {})) pkg.optionalDependencies[name] = next;
  fs.writeFileSync(files.cli, JSON.stringify(pkg, null, 2) + '\n');

  process.stdout.write(next + '\n');
}

try {
  main();
} catch (error) {
  process.stderr.write(`bump-version: ${error.message}\n`);
  process.exit(1);
}
