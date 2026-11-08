// Builds web-server and copies it to src-tauri/binaries with the target-triple
// suffix required by Tauri's bundle.externalBin. Tauri's build script then
// copies this file next to the app binary in both dev and release builds.
import { execFileSync } from 'node:child_process'
import { chmodSync, copyFileSync, existsSync, mkdirSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const webUiDir = resolve(here, '..')
const repoDir = resolve(webUiDir, '..')
const release = process.argv.includes('--release')
const profile = release ? 'release' : 'debug'

function resolveBin(name) {
  const cargoHome = process.env.CARGO_HOME || join(process.env.HOME || '', '.cargo')
  const candidate = join(cargoHome, 'bin', name)
  if (existsSync(candidate)) return candidate
  return name
}

const cargo = process.env.CARGO || resolveBin('cargo')
const rustc = process.env.RUSTC || resolveBin('rustc')

const args = ['build', '-p', 'web-server']
if (release) args.push('--release')
console.log(`[sidecar] ${cargo} ${args.join(' ')}`)
execFileSync(cargo, args, { cwd: repoDir, stdio: 'inherit' })

const rustcInfo = execFileSync(rustc, ['-vV'], { encoding: 'utf8' })
const hostLine = rustcInfo.split('\n').find((line) => line.startsWith('host:'))
if (!hostLine) throw new Error(`Unable to determine host target triple:\n${rustcInfo}`)
const targetTriple = hostLine.slice('host:'.length).trim()

const targetDir = process.env.CARGO_TARGET_DIR || join(repoDir, 'target')
const exeSuffix = process.platform === 'win32' ? '.exe' : ''
const source = join(targetDir, profile, `web-server${exeSuffix}`)
if (!existsSync(source)) throw new Error(`Built sidecar not found: ${source}`)

const binariesDir = join(webUiDir, 'src-tauri', 'binaries')
mkdirSync(binariesDir, { recursive: true })
const destination = join(binariesDir, `web-server-${targetTriple}${exeSuffix}`)
copyFileSync(source, destination)
chmodSync(destination, 0o755)
console.log(`[sidecar] ${source} -> ${destination}`)
