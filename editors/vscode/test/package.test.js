'use strict'

// The manifest against the code, both ways.
//
// A `package.json` is the half of an extension the editor reads and
// nothing type checks. A command contributed with no handler is a menu
// entry that throws when it is clicked, a handler with no contribution
// is dead code nobody can reach, and a path that moved is a language
// with no colours and no message about why. All three are cheap to
// check and expensive to find by hand.

const test = require('node:test')
const assert = require('node:assert')
const fs = require('node:fs')
const path = require('node:path')
const { execFileSync } = require('node:child_process')

const root = path.join(__dirname, '..')
const manifest = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8'))
const extension = fs.readFileSync(path.join(root, 'src/extension.js'), 'utf8')

test('every file the manifest names is there', () => {
  const named = [
    manifest.main,
    ...manifest.contributes.languages.map((l) => l.configuration),
    ...manifest.contributes.grammars.map((g) => g.path),
  ]
  for (const file of named) {
    assert.ok(fs.existsSync(path.join(root, file)), `${file} is named and is not there`)
  }
})

test('a command is contributed and handled, or it is neither', () => {
  const contributed = manifest.contributes.commands.map((c) => c.command).sort()
  const handled = [...extension.matchAll(/registerCommand\('([^']+)'/g)].map((m) => m[1]).sort()
  assert.deepStrictEqual(handled, contributed)

  // And the one on the status bar is one of them, since a bar item
  // whose command does not exist is a click that does nothing.
  const bar = extension.match(/status\.command = '([^']+)'/)
  assert.ok(contributed.includes(bar[1]), `${bar[1]} is on the status bar and is not a command`)
})

test('every setting the manifest declares is a setting the code reads', () => {
  const declared = Object.keys(manifest.contributes.configuration.properties)
    .map((key) => key.replace(/^zu\./, ''))
    .sort()
  // `trace.server` is read by the language client rather than by this
  // file, by that name, which is the convention the client defines.
  const read = ['trace.server']
  for (const m of extension.matchAll(/config\.get\('([^']+)'/g)) read.push(m[1])
  for (const m of extension.matchAll(/affectsConfiguration\('zu\.([^']+)'\)/g)) read.push(m[1])
  assert.deepStrictEqual([...new Set(read)].sort(), declared)
})

test('the bundled grammar is the generated one, byte for byte', () => {
  // `cargo xtask grammar` writes both copies from the word list, and
  // the check in CI covers this too. It is here as well because the
  // copy is the one thing in this directory that is generated, and the
  // failure it guards against is a hand edit that colours a keyword in
  // the editor and not on the website.
  const mine = fs.readFileSync(path.join(root, 'syntaxes/gql.tmLanguage.json'))
  const site = fs.readFileSync(path.join(root, '../../grammar/shiki/gql.tmLanguage.json'))
  assert.ok(mine.equals(site), 'run `cargo xtask grammar` rather than editing either copy')
})

test('the file that needs an editor to load still parses', () => {
  // `extension.js` requires `vscode`, which only exists inside the
  // editor, so no test can load it. What every test can do is refuse a
  // syntax error, which is the failure that would otherwise be found
  // by an install.
  execFileSync(process.execPath, ['--check', path.join(root, 'src/extension.js')])
})
