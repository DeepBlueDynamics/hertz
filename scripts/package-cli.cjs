'use strict';
// Pack hertz for npm: @deepbluedynamics/hertz (launchers, band plans, example
// config; no binary) and one @deepbluedynamics/hertz-<platform>-<arch> package
// per release target with that target's hertzd and hertz, which npm installs as
// an optional dependency only on a matching host. Ported from Lume.
//
//   node scripts/package-cli.cjs --linux-x64 <dir> --linux-arm64 <dir> \
//     --darwin-arm64 <dir> --darwin-x64 <dir> --win32-x64 <dir> [--output <dir>]
//
// Each <dir> holds that target's hertzd and hertz (.exe on Windows). Headers are
// checked so a binary built for the wrong platform or architecture is refused.
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const {execFileSync} = require('node:child_process');

const SCOPE = '@deepbluedynamics/hertz';
const TARGETS = ['linux-x64', 'linux-arm64', 'darwin-arm64', 'darwin-x64', 'win32-x64'];
const PROGRAMS = ['hertzd', 'hertz'];
const LABELS = {'linux-x64': 'Linux x64', 'linux-arm64': 'Linux arm64', 'darwin-arm64': 'macOS Apple Silicon',
  'darwin-x64': 'macOS Intel', 'win32-x64': 'Windows x64'};

const sha256 = file => crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');

function parseArgs(args) {
  const options = {output: path.resolve(__dirname, '../.package-cli')};
  if (args.includes('--help')) return {help: true};
  for (let i = 0; i < args.length; i += 2) {
    const key = args[i]?.replace(/^--/, '');
    if (!(TARGETS.includes(key) || key === 'output') || !args[i + 1] || args[i + 1].startsWith('--')) {
      throw new Error('expected ' + TARGETS.map(t => '--' + t + ' <dir>').join(' ') + ' [--output <dir>]');
    }
    options[key] = path.resolve(args[i + 1]);
  }
  const missing = TARGETS.filter(t => !options[t]);
  if (missing.length) throw new Error('missing binaries for ' + missing.join(', '));
  return options;
}

// ELF, 64-bit Mach-O and PE32+ headers: enough to refuse a wrong-platform binary.
function checkBinary(file, target) {
  const header = Buffer.alloc(4096);
  const fd = fs.openSync(file, 'r');
  let read;
  try { read = fs.readSync(fd, header, 0, header.length, 0); } finally { fs.closeSync(fd); }
  if (target.startsWith('linux-')) {
    if (read < 20 || header.toString('latin1', 0, 4) !== '\x7fELF' || header[4] !== 2) {
      throw new Error(file + ': expected a 64-bit ELF executable');
    }
    const machine = {'linux-x64': 0x3e, 'linux-arm64': 0xb7}[target];
    if (header.readUInt16LE(18) !== machine) throw new Error(file + ': ELF architecture does not match ' + target);
    return;
  }
  if (target.startsWith('darwin-')) {
    if (read < 8 || header.readUInt32LE(0) !== 0xfeedfacf) throw new Error(file + ': expected a 64-bit Mach-O executable');
    const cpu = {'darwin-arm64': 0x0100000c, 'darwin-x64': 0x01000007}[target];
    if (header.readUInt32LE(4) !== cpu) throw new Error(file + ': Mach-O architecture does not match ' + target);
    return;
  }
  const pe = read >= 64 && header.toString('latin1', 0, 2) === 'MZ' ? header.readUInt32LE(60) : -1;
  if (pe < 0 || pe + 6 > read || header.toString('latin1', pe, pe + 4) !== 'PE\0\0') {
    throw new Error(file + ': expected a PE executable');
  }
  if (header.readUInt16LE(pe + 4) !== 0x8664) throw new Error(file + ': PE architecture is not x64');
}

function binaryReadme(name, target, version) {
  const label = LABELS[target];
  return [
    '# ' + name, '',
    'The ' + label + ' `hertzd` and `hertz` programs for **[' + SCOPE + '](https://www.npmjs.com/package/' + SCOPE + ')**, ' +
      'a scanning radio receiver for RTL-SDR dongles with on-device transcription.', '',
    '**Install `' + SCOPE + '`, not this package.** npm adds this one by itself on a ' + label + ' machine:', '',
    '```sh', 'npm install -g ' + SCOPE, 'hertzd --version', '```', '',
    'Hertz ' + version + '. Source: [DeepBlueDynamics/hertz](https://github.com/DeepBlueDynamics/hertz).', '',
  ].join('\n');
}

function pack(stage, output, required) {
  const result = JSON.parse(execFileSync('npm', ['pack', '--json', '--offline', '--pack-destination', output], {
    cwd: stage, encoding: 'utf8', shell: process.platform === 'win32',
    env: {...process.env, npm_config_cache: path.join(output, '.npm-cache')},
  }))[0];
  const names = new Set(result.files.map(f => f.path));
  for (const name of required) if (!names.has(name)) throw new Error('npm pack omitted ' + name + ' from ' + result.name);
  const tarball = path.join(output, result.filename);
  return {package: result.name, version: result.version, tarball, bytes: fs.statSync(tarball).size,
    unpacked_bytes: result.unpackedSize, sha256: sha256(tarball)};
}

function gitHead(root) {
  try { return execFileSync('git', ['rev-parse', 'HEAD'], {cwd: root, encoding: 'utf8'}).trim(); } catch (_) { return undefined; }
}

function assemble(options) {
  const root = path.resolve(__dirname, '..');
  const source = path.join(root, 'npm/hertz');
  const output = path.resolve(options.output);
  const license = path.join(root, 'LICENSE');
  if (!fs.existsSync(license)) throw new Error('no LICENSE file at the repo root; npm packages need one');
  const exe = (target, program) => program + (target.startsWith('win32') ? '.exe' : '');
  // Check every binary before creating output or copying anything.
  for (const target of TARGETS) for (const p of PROGRAMS) checkBinary(path.join(options[target], exe(target, p)), target);
  const cli = JSON.parse(fs.readFileSync(path.join(source, 'package.json'), 'utf8'));
  for (const target of TARGETS) {
    if (cli.optionalDependencies?.[SCOPE + '-' + target] !== cli.version) {
      throw new Error(SCOPE + '-' + target + ' must be an optional dependency pinned to ' + cli.version);
    }
  }
  const head = gitHead(root);
  fs.mkdirSync(output, {recursive: true});
  const stages = [];
  const stage = name => { const dir = fs.mkdtempSync(path.join(output, name + '-')); stages.push(dir); return dir; };
  const packages = {};
  try {
    for (const target of TARGETS) {
      const [platform, arch] = target.split('-');
      const name = SCOPE + '-' + target;
      const dir = stage('hertz-' + target);
      fs.mkdirSync(path.join(dir, 'bin'), {recursive: true});
      const manifest = {format: 1, target, programs: {}};
      for (const p of PROGRAMS) {
        const dest = path.join(dir, 'bin', exe(target, p));
        fs.copyFileSync(path.join(options[target], exe(target, p)), dest);
        fs.chmodSync(dest, 0o755);
        manifest.programs[p] = {path: 'bin/' + exe(target, p), bytes: fs.statSync(dest).size, sha256: sha256(dest)};
      }
      fs.writeFileSync(path.join(dir, 'bin/manifest.json'), JSON.stringify(manifest, null, 2) + '\n');
      fs.writeFileSync(path.join(dir, 'package.json'), JSON.stringify({
        name, version: cli.version,
        description: 'The ' + target + ' hertzd and hertz binaries for ' + SCOPE + ', installed as its optional dependency',
        author: cli.author, homepage: cli.homepage, repository: cli.repository, bugs: cli.bugs, license: cli.license,
        files: ['bin/', 'README.md', 'LICENSE'], os: [platform], cpu: [arch], gitHead: head,
      }, null, 2) + '\n');
      fs.writeFileSync(path.join(dir, 'README.md'), binaryReadme(name, target, cli.version));
      fs.copyFileSync(license, path.join(dir, 'LICENSE'));
      packages[target] = pack(dir, output, [...PROGRAMS.map(p => 'bin/' + exe(target, p)), 'bin/manifest.json', 'LICENSE']);
    }
    const main = stage('hertz');
    for (const name of ['README.md', 'bin']) fs.cpSync(path.join(source, name), path.join(main, name), {recursive: true});
    fs.writeFileSync(path.join(main, 'package.json'), JSON.stringify({...cli, gitHead: head}, null, 2) + '\n');
    fs.cpSync(path.join(root, 'bandplans'), path.join(main, 'bandplans'), {recursive: true});
    fs.copyFileSync(path.join(root, 'hertz.example.toml'), path.join(main, 'hertz.example.toml'));
    fs.copyFileSync(license, path.join(main, 'LICENSE'));
    for (const p of PROGRAMS) fs.chmodSync(path.join(main, 'bin', p + '.js'), 0o755);
    const report = {...pack(main, output, ['bin/hertzd.js', 'bin/hertz.js', 'bin/run.js', 'hertz.example.toml', 'LICENSE']), packages};
    fs.writeFileSync(path.join(output, 'cli-report.json'), JSON.stringify(report, null, 2) + '\n');
    return report;
  } finally { for (const dir of stages) fs.rmSync(dir, {recursive: true, force: true}); }
}

if (require.main === module) {
  try {
    const options = parseArgs(process.argv.slice(2));
    if (options.help) {
      console.log('Usage: node scripts/package-cli.cjs ' + TARGETS.map(t => '--' + t + ' <dir>').join(' ') + ' [--output <dir>]');
    } else {
      const report = assemble(options);
      for (const p of [report, ...Object.values(report.packages)]) console.error(path.basename(p.tarball) + ': ' + p.bytes + ' bytes');
      console.log(JSON.stringify(report, null, 2));
    }
  } catch (error) { console.error('Packaging failed: ' + error.message); process.exitCode = 1; }
}
module.exports = {TARGETS, PROGRAMS, parseArgs, checkBinary, assemble};
