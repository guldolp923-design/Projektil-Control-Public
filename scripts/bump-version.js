const fs = require('fs');
const path = require('path');

const root = process.cwd();

const files = {
  packageJson: path.join(root, 'package.json'),
  cargoToml: path.join(root, 'src-tauri', 'Cargo.toml'),
  tauriConf: path.join(root, 'src-tauri', 'tauri.conf.json'),
  frontendIndex: path.join(root, 'frontend', 'index.html')
};

const args = process.argv.slice(2);
const dryRun = args.includes('--dry-run');
const channelArg = args.find((a) => a.startsWith('--channel='));
const channel = channelArg ? channelArg.split('=')[1] : 'preserve';

function parseVersion(version) {
  const match = String(version).trim().match(/^(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?$/);
  if (!match) {
    throw new Error(`Unsupported version format: ${version}`);
  }
  return {
    major: Number(match[1]),
    minor: Number(match[2]),
    patch: Number(match[3]),
    suffix: match[4] || ''
  };
}

function resolveSuffix(parsed, releaseChannel) {
  if (releaseChannel === 'stable') return '';
  if (releaseChannel === 'beta') return 'beta';
  if (releaseChannel === 'preserve') return parsed.suffix;
  throw new Error(`Unsupported channel '${releaseChannel}'. Use preserve|beta|stable.`);
}

function buildVersion(parsed, nextPatch, suffix) {
  return `${parsed.major}.${parsed.minor}.${nextPatch}${suffix ? `-${suffix}` : ''}`;
}

function updateText(content, regex, replacement, label) {
  if (!regex.test(content)) {
    throw new Error(`Could not update ${label}: pattern not found.`);
  }
  return content.replace(regex, replacement);
}

(function run() {
  const packageJson = JSON.parse(fs.readFileSync(files.packageJson, 'utf8'));
  const currentVersion = packageJson.version;
  const parsed = parseVersion(currentVersion);
  const nextPatch = parsed.patch + 1;
  const nextSuffix = resolveSuffix(parsed, channel);
  const nextVersion = buildVersion(parsed, nextPatch, nextSuffix);

  console.log(`Current version: ${currentVersion}`);
  console.log(`Next version:    ${nextVersion}`);

  if (dryRun) return;

  packageJson.version = nextVersion;
  fs.writeFileSync(files.packageJson, JSON.stringify(packageJson, null, 2) + '\n', 'utf8');

  const cargoToml = fs.readFileSync(files.cargoToml, 'utf8');
  const cargoUpdated = updateText(
    cargoToml,
    /^version\s*=\s*"[^"]+"/m,
    `version = "${nextVersion}"`,
    'Cargo.toml version'
  );
  fs.writeFileSync(files.cargoToml, cargoUpdated, 'utf8');

  const tauriConf = fs.readFileSync(files.tauriConf, 'utf8');
  const tauriUpdated = updateText(
    tauriConf,
    /"version"\s*:\s*"[^"]+"/,
    `"version": "${nextVersion}"`,
    'tauri.conf.json version'
  );
  fs.writeFileSync(files.tauriConf, tauriUpdated, 'utf8');

  const indexHtml = fs.readFileSync(files.frontendIndex, 'utf8');
  const indexUpdated = updateText(
    indexHtml,
    /const APP_VERSION_FALLBACK = '[^']+';/,
    `const APP_VERSION_FALLBACK = '${nextVersion}';`,
    'frontend fallback version'
  );
  fs.writeFileSync(files.frontendIndex, indexUpdated, 'utf8');

  console.log('Version updated in package.json, Cargo.toml, tauri.conf.json and frontend/index.html');
})();
