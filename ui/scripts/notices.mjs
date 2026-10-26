// Lists the npm packages bundled into dist/app.js and dist/app.css with their licenses.
//   node scripts/notices.mjs                 print the Markdown section
//   node scripts/notices.mjs --write FILE    replace (or append) the section in FILE
//   node scripts/notices.mjs --check FILE    exit 1 if FILE's section differs
// Run `npm run build` first: dist/packages.json is written by the Vite build.
import { createHash } from 'node:crypto'
import { existsSync, readdirSync, readFileSync, writeFileSync } from 'node:fs'
import { dirname, join, resolve, sep } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const HEADING = '## Status page (JavaScript bundled in builds with the ui feature)'

function packages() {
  const manifest = join(root, 'dist', 'packages.json')
  if (!existsSync(manifest)) {
    console.error('dist/packages.json not found: run `npm run build` first.')
    process.exit(2)
  }
  return JSON.parse(readFileSync(manifest, 'utf8'))
}

function packageDir(dir) {
  // The manifest comes from our own build, but never read outside the project's node_modules.
  const abs = resolve(root, dir)
  if (!dir.startsWith('node_modules/') || !abs.startsWith(join(root, 'node_modules') + sep)) throw new Error(`unexpected package path ${dir}`)
  if (!existsSync(join(abs, 'package.json'))) throw new Error(`${dir} is not installed: run npm ci`)
  return abs
}

function licenseOf(pkg) {
  if (typeof pkg.license === 'string') return pkg.license
  if (pkg.license && typeof pkg.license.type === 'string') return pkg.license.type
  if (Array.isArray(pkg.licenses)) return pkg.licenses.map((l) => l.type ?? String(l)).join(' OR ')
  return 'UNKNOWN'
}

const norm = (t) => t.replace(/\r\n?/g, '\n').replace(/[ \t]+$/gm, '').trim()

function collect() {
  const rows = []
  const texts = new Map() // normalized text -> ["name version / FILE"]
  const seen = new Set()
  for (const { name, version, dir: rel } of packages()) {
    if (seen.has(`${name}@${version}`)) continue // the same release bundled from two places
    seen.add(`${name}@${version}`)
    const dir = packageDir(rel)
    const pkg = JSON.parse(readFileSync(join(dir, 'package.json'), 'utf8'))
    const files = readdirSync(dir).filter((f) => /^(licen[sc]e|notice)([-._].*)?$/i.test(f)).sort()
    const ids = []
    for (const f of files) {
      let text
      try {
        text = norm(readFileSync(join(dir, f), 'utf8'))
      } catch {
        continue // a directory named LICENSE, for example
      }
      if (!text) continue
      if (!texts.has(text)) texts.set(text, [])
      texts.get(text).push(`${name} ${version} / ${f}`)
      ids.push(noticeId(text))
    }
    if (ids.length === 0) console.error(`warning: ${name}@${version} ships no license file (declared: ${licenseOf(pkg)})`)
    rows.push({ name, version, license: licenseOf(pkg), ids: [...new Set(ids)] })
  }
  return { rows, texts }
}

const noticeId = (text) => `notice-${createHash('sha256').update(text).digest('hex').slice(0, 16)}`

function fence(text) {
  const longest = Math.max(2, ...[...text.matchAll(/`+/g)].map((m) => m[0].length))
  return '`'.repeat(longest + 1)
}

export function section() {
  const { rows, texts } = collect()
  const out = [HEADING, '', 'Production dependencies of the status page that are compiled into `app.js`, or whose stylesheets and fonts are inlined into `app.css`. Build tools and type packages are not shipped.', '']
  out.push('| Package | Version | License | Notice |', '|---|---|---|---|')
  for (const r of rows) out.push(`| ${r.name} | ${r.version} | ${r.license.replaceAll('|', '/')} | ${r.ids.join(', ') || 'none shipped'} |`)
  for (const [text, applies] of [...texts].sort((a, b) => a[1][0].localeCompare(b[1][0]))) {
    const f = fence(text)
    out.push('', `### ${noticeId(text)}`, '', `Applies to: ${applies.join('; ')}`, '', `${f}text`, text, f)
  }
  return `${out.join('\n')}\n`
}

function split(file) {
  const body = existsSync(file) ? readFileSync(file, 'utf8') : ''
  const m = new RegExp(`^${HEADING.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}[ \\t]*$`, 'm').exec(body)
  return m ? { before: body.slice(0, m.index), current: body.slice(m.index) } : { before: body, current: null }
}

const [mode, target = '../THIRD_PARTY_NOTICES.md'] = process.argv.slice(2)
if (!mode) {
  process.stdout.write(section())
} else if (mode === '--write' || mode === '--check') {
  const file = resolve(process.cwd(), target)
  const { before, current } = split(file)
  const next = section()
  if (mode === '--check') {
    if (current === null || current.trimEnd() !== next.trimEnd()) {
      console.error(`${target}: the status page notices section is ${current === null ? 'missing' : 'out of date'}. Run: npm run notices -- --write ${target}`)
      process.exit(1)
    }
    console.log(`${target}: status page notices are current.`)
  } else {
    const head = before === '' || before.endsWith('\n\n') ? before : `${before.replace(/\n*$/, '')}\n\n`
    writeFileSync(file, head + next)
    console.log(`${target}: wrote the status page notices section.`)
  }
} else {
  console.error('usage: notices.mjs [--write [FILE] | --check [FILE]] (FILE defaults to ../THIRD_PARTY_NOTICES.md)')
  process.exit(2)
}
