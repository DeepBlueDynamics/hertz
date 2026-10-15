# Publish a hertz release to npm by hand, from your own npm login.
#
#   pwsh scripts/npm-publish.ps1                 # the latest GitHub Release
#   pwsh scripts/npm-publish.ps1 -Version 0.2.3
#   pwsh scripts/npm-publish.ps1 -Trust          # also (re)set up trusted publishing
#
# CI normally publishes on its own (docs/NPM.md). Use this when its "Publish to npm"
# job failed: it downloads the npm-cli artifact of that release run with gh, logs
# in to npm if needed, and publishes the six packages, platform packages first and
# the wrapper last. Packages already on npm are skipped, so rerun it if it stops.
# npm asks you to press Enter and approve in the browser for each publish.
param(
  [string]$Version,
  [switch]$Trust
)
$ErrorActionPreference = 'Stop'
$repo = 'DeepBlueDynamics/hertz'
$packages = 'hertz-linux-x64', 'hertz-linux-arm64', 'hertz-darwin-arm64', 'hertz-darwin-x64', 'hertz-win32-x64', 'hertz'
if (-not $Version) { $Version = (gh release view --repo $repo --json tagName --jq .tagName) -replace '^v' }
if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw "expected a version like 0.2.3, got '$Version'" }
$wrapper = "deepbluedynamics-hertz-$Version.tgz"
$dir = Join-Path ([IO.Path]::GetTempPath()) "hertz-npm-$Version"

function Run([scriptblock]$command) {
  & $command
  if ($LASTEXITCODE) { throw "failed: $command" }
}

# Find the release run that built this version: the npm-cli artifact of a
# bump-version.yml run (it calls release.yml) or of a tag-push release.yml run.
if (-not (Test-Path (Join-Path $dir $wrapper))) {
  $found = $false
  foreach ($workflow in 'bump-version.yml', 'release.yml') {
    $runs = gh run list --repo $repo --workflow $workflow --limit 20 --json databaseId --jq '.[].databaseId'
    foreach ($id in $runs) {
      $try = Join-Path ([IO.Path]::GetTempPath()) "hertz-npm-run-$id"
      if (-not (Test-Path $try)) {
        gh run download $id --repo $repo --name npm-cli --dir $try 2>$null
        if ($LASTEXITCODE) { continue }
      }
      if (Test-Path (Join-Path $try $wrapper)) {
        if (Test-Path $dir) { Remove-Item $dir -Recurse -Force }
        Move-Item $try $dir
        Write-Host "Using the tarballs from run $id" -ForegroundColor Green
        $found = $true
        break
      }
    }
    if ($found) { break }
  }
  if (-not $found) { throw "no release run has npm tarballs for $Version (artifacts expire after 90 days); rerun the release" }
}

$ErrorActionPreference = 'Continue'
npm whoami *> $null
if ($LASTEXITCODE) {
  Write-Host 'Logging in to npm: press Enter, approve in the browser.' -ForegroundColor Cyan
  Run { npm login --auth-type=web }
}
Write-Host ('Logged in to npm as ' + (npm whoami)) -ForegroundColor Green

foreach ($p in $packages) {
  $name = "@deepbluedynamics/$p"
  if ((npm view "$name@$Version" version 2>$null) -eq $Version) { Write-Host "$name@$Version is already published"; continue }
  Write-Host "Publishing ${name}@${Version}: press Enter, approve with your security key." -ForegroundColor Cyan
  npm publish (Join-Path $dir "deepbluedynamics-$p-$Version.tgz") --access public --auth-type=web
  if ($LASTEXITCODE) { Write-Host "Stopped at $name. Rerun this script to continue." -ForegroundColor Red; exit 1 }
}
Write-Host "hertz $Version is on npm: npm install -g @deepbluedynamics/hertz" -ForegroundColor Green

# Trusted publishing lets CI publish without a token. npm checks the calling
# workflow, which is bump-version.yml when it starts the release, so trust both.
if ($Trust) {
  foreach ($p in $packages) {
    foreach ($file in 'bump-version.yml', 'release.yml') {
      Write-Host "Trusting $file for @deepbluedynamics/$p" -ForegroundColor Cyan
      npx -y npm@12 trust github "@deepbluedynamics/$p" --file $file --repo $repo --allow-publish -y
    }
    npx -y npm@12 trust list "@deepbluedynamics/$p"
  }
}
