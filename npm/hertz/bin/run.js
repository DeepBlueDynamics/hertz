'use strict';
// Runs a prebuilt hertz program from the optional dependency
// @deepbluedynamics/hertz-<platform>-<arch>, passing through arguments, stdio
// and the exit status. hertzd finds the band plans shipped with this package
// unless HERTZ_BANDPLANS is already set.
const path = require('node:path');
const {spawnSync} = require('node:child_process');

const SUPPORTED = ['linux-x64', 'linux-arm64', 'darwin-arm64', 'darwin-x64', 'win32-x64'];

module.exports = function run(program) {
  const target = process.platform + '-' + process.arch;
  let binary;
  try {
    const pkg = require.resolve('@deepbluedynamics/hertz-' + target + '/package.json');
    binary = path.join(path.dirname(pkg), 'bin', program + (process.platform === 'win32' ? '.exe' : ''));
  } catch (_) {
    console.error(program + ': no prebuilt binary for ' + target + '. Supported: ' + SUPPORTED.join(', ') + '.');
    if (SUPPORTED.includes(target)) {
      console.error(program + ': reinstall without --omit=optional (or --no-optional) so npm installs @deepbluedynamics/hertz-' + target + '.');
    }
    process.exit(1);
  }
  const env = {...process.env};
  if (!env.HERTZ_BANDPLANS) env.HERTZ_BANDPLANS = path.join(__dirname, '..', 'bandplans');
  const result = spawnSync(binary, process.argv.slice(2), {stdio: 'inherit', env});
  if (result.error) {
    console.error(program + ': could not run ' + binary + ': ' + result.error.message);
    process.exit(1);
  }
  if (result.signal) process.kill(process.pid, result.signal);
  process.exit(result.status ?? 1);
};
