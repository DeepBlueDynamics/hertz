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

1. **LICENSE.** Add a `LICENSE` file at the repo root. The release and
   `package-cli.cjs` refuse to run without one. `rusb` statically links libusb
   (LGPL-2.1), so check the obligations for the binaries.
2. **First publish by hand.** npm only lets you add a trusted publisher to a
   package that exists. Run the release once, download the `npm-cli` artifact,
   then `npm login` (browser and security key, in a real terminal) and
   `npm publish ./<file>.tgz --access public` for all six tarballs, platform
   packages first.
3. **Trusted publishers.** On npmjs.com, for each of the six packages, under
   Settings → Trusted publishing → GitHub Actions, add owner `DeepBlueDynamics`,
   repo `hertz`, and both workflows: `bump-version.yml` and `release.yml`, with
   the environment left empty. A mismatch fails with E404, not 403.

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
