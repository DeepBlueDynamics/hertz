# Releasing hertz to npm

`npm install -g @deepbluedynamics/hertz` installs `hertzd` and `hertz` prebuilt for
Linux arm64 (Raspberry Pi), Linux x64, macOS arm64 and x64, and Windows x64.
The setup is Lume's, ported.

## Packages

- `@deepbluedynamics/hertz` (`npm/hertz/`) contains the `hertzd` and `hertz`
  launchers (`bin/run.js`), the band plans and `hertz.example.toml`. The launcher
  runs the binary for this machine and sets `HERTZ_BANDPLANS` to the shipped band
  plans unless it's already set. There is no install script.
- `@deepbluedynamics/hertz-<platform>-<arch>` are built by
  `scripts/package-cli.cjs`, each holding that target's `hertzd`, `hertz` and a
  `bin/manifest.json` with sizes and sha256. The wrapper pins them as
  `optionalDependencies` at the same version, so npm installs only the matching
  one.

## Release

Actions → **Bump version and release** → Run (patch, minor, major or `X.Y.Z`),
or `gh workflow run bump-version.yml -f bump=patch`. Either one:

1. sets the version in `[workspace.package]` in `Cargo.toml`, in `Cargo.lock` and
   in `npm/hertz/package.json` (`scripts/bump-version.cjs`), commits
   `release: vX.Y.Z`, tags and pushes;
2. runs `release.yml`, which checks that the tag and versions agree, builds on
   native runners (Linux on 22.04 for glibc ≤ 2.39), smoke-tests `--version`,
   makes the GitHub Release with archives and `SHA256SUMS`, packs the npm tarballs
   and publishes them, platform packages first and the wrapper last.

Pushing a `v*` tag by hand runs `release.yml` alone.

## One-time setup (a person must do this)

1. **LICENSE.** `LICENSE` (BSD 3-Clause) at the repo root ships in every
   package; the release and `package-cli.cjs` refuse to run without it. `rusb`
   statically links libusb (LGPL-2.1); the README's License section says so.
2. **First publish by hand.** npm only lets you add a trusted publisher to a
   package that exists. Run the release once, then publish it with
   `scripts/npm-publish.ps1` (below). Done for 0.2.2.
3. **Trusted publishers.** The first CI publish fails with E404 until this is
   done. For each of the six packages, trust both workflows. npm checks the
   calling workflow, which is `bump-version.yml` when it starts the release.
   `pwsh scripts/npm-publish.ps1 -Trust` does this, or by hand:

   ```sh
   for pkg in hertz hertz-linux-x64 hertz-linux-arm64 hertz-darwin-arm64 hertz-darwin-x64 hertz-win32-x64; do
     for file in bump-version.yml release.yml; do
       npx npm@12 trust github "@deepbluedynamics/$pkg" --file "$file" --repo DeepBlueDynamics/hertz --allow-publish -y
     done
     npx npm@12 trust list "@deepbluedynamics/$pkg"
   done
   ```

   Or use npmjs.com → package → Settings → Trusted publishing, with owner
   `DeepBlueDynamics`, repo `hertz`, and the environment left empty. Check the
   file names character by character: a mismatch fails with E404, not 403. Lume
   lost a release to `bump-verion.yml`.

## Publishing by hand

If a release's **Publish to npm** job fails (no trusted publisher yet, an npm
outage), publish that release from your own npm account. You need `gh` logged in
to GitHub, Node, and npm access to the `@deepbluedynamics` scope. In PowerShell
(`pwsh`, Windows, macOS or Linux), from the repo root:

```powershell
pwsh scripts/npm-publish.ps1                    # the latest GitHub Release
pwsh scripts/npm-publish.ps1 -Version 0.2.3     # a specific version
pwsh scripts/npm-publish.ps1 -Trust             # also set up trusted publishing
```

The script downloads the `npm-cli` artifact (the six tarballs) from the release
run that built that version, runs `npm login` if you're logged out, and publishes
the platform packages and then the wrapper. For each `npm login` and
`npm publish`, press Enter and approve in the browser with your security key.
Packages already on npm are skipped, so if it stops partway, run it again. Run
artifacts expire after 90 days; after that, rerun the release.

Check the result with `npm install -g @deepbluedynamics/hertz` then
`hertz --version`. Without `-g`, npm installs into the nearest `package.json`
and puts nothing on your PATH.

## Running hertzd as a service

The `hertzd` command on your PATH is a Node launcher. It forwards SIGTERM and
SIGHUP to the real binary, but a service should start the binary directly, so
nothing sits between systemd and the process holding the dongle:

```sh
node -p "require('path').join(require('path').dirname(require.resolve('@deepbluedynamics/hertz-linux-arm64/package.json', {paths: [require('child_process').execSync('npm root -g').toString().trim()]})), 'bin/hertzd')"
```

Use that path in `ExecStart=`, and set `Environment=HERTZ_BANDPLANS=` to the
wrapper's `bandplans/` directory.

## On the Pi

```sh
npm install -g @deepbluedynamics/hertz
hertzd --version
```

Then do the USB setup in `npm/hertz/README.md`: blacklist `dvb_usb_rtl28xxu`
and install `docs/60-hertz-rtlsdr.rules`. Run `hertzd` once with internet so
Whistle downloads its engine and weights.

## Test packaging locally

```sh
node scripts/package-cli.cjs --linux-x64 <dir> --linux-arm64 <dir> \
  --darwin-arm64 <dir> --darwin-x64 <dir> --win32-x64 <dir> --output .package-cli
```

Each `<dir>` holds that target's `hertzd` and `hertz`; the script checks their
ELF, Mach-O or PE headers.
