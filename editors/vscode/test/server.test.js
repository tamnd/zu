'use strict'

// What the extension decides before it talks to anything.
//
// The command line is where an extension goes wrong in a way the user
// cannot see: a setting read from the wrong scope, a relative path
// resolved against the wrong directory, a binary looked for under the
// wrong name on Windows. All of those are answers rather than actions,
// so they are checked here, under plain node, with no editor running.

const test = require('node:test')
const assert = require('node:assert')
const path = require('node:path')

const { resolve, label, tooltip, absolute } = require('../src/server')

const none = () => false

const base = {
  extensionPath: '/ext',
  platform: 'linux',
  workspace: '/work',
  home: '/home/someone',
  exists: none,
}

test('the server is always the same command, and the database is the only variable', () => {
  assert.deepStrictEqual(resolve(base).args, ['lsp', '--stdio'])
  assert.deepStrictEqual(resolve({ ...base, database: 'social.zu1' }).args, [
    'lsp',
    '--stdio',
    '--db',
    '/work/social.zu1',
  ])
})

test('a path the user set wins, then the bundled binary, then PATH', () => {
  // A path somebody set is a build they are testing, and it beats an
  // install every time.
  assert.deepStrictEqual(
    resolve({ ...base, setting: '/opt/zu/bin/zu', exists: () => true }),
    {
      command: '/opt/zu/bin/zu',
      args: ['lsp', '--stdio'],
      source: 'setting',
      database: '',
    },
  )

  // Then the one in the extension, which is what an install from the
  // marketplace has and is the case where nothing was configured.
  const bundled = resolve({ ...base, exists: (p) => p === '/ext/bin/zu' })
  assert.strictEqual(bundled.command, '/ext/bin/zu')
  assert.strictEqual(bundled.source, 'bundled')

  // Then the bare name, which the editor looks up, and which is what a
  // package manager or a `cargo install` leaves behind.
  const bare = resolve(base)
  assert.strictEqual(bare.command, 'zu')
  assert.strictEqual(bare.source, 'path')
})

test('windows looks for the name windows uses', () => {
  const found = resolve({ ...base, platform: 'win32' })
  assert.strictEqual(found.command, 'zu.exe')

  const bundled = resolve({
    ...base,
    platform: 'win32',
    exists: (p) => p === path.join('/ext', 'bin', 'zu.exe'),
  })
  assert.strictEqual(bundled.source, 'bundled')
})

test('a path is from the workspace and a tilde is from home', () => {
  assert.strictEqual(absolute('graphs/social.zu1', base), '/work/graphs/social.zu1')
  assert.strictEqual(absolute('/tmp/social.zu1', base), '/tmp/social.zu1')
  assert.strictEqual(absolute('~/social.zu1', base), '/home/someone/social.zu1')
  assert.strictEqual(absolute('  ', base), '')

  // With no folder open there is nothing to be relative to, so the text
  // is passed through and the server says what it makes of it. That is
  // better than joining it to whatever directory the editor was
  // launched from, which is a directory the user never chose.
  assert.strictEqual(absolute('social.zu1', { ...base, workspace: '' }), 'social.zu1')
})

test('the status bar says the one fact that is not visible anywhere else', () => {
  assert.strictEqual(label('/work/graphs/social.zu1'), 'zu: social.zu1')
  assert.strictEqual(label(''), 'zu: no database')

  // No database is a supported answer, so the tooltip explains what
  // that costs rather than reading as a failure.
  const quiet = tooltip(resolve(base))
  assert.match(quiet, /No database/)
  assert.match(quiet, /on PATH/)

  const loud = tooltip(resolve({ ...base, database: '/tmp/social.zu1' }))
  assert.match(loud, /Answering about \/tmp\/social\.zu1/)
})
