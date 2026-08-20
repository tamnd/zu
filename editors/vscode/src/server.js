'use strict'

// Where the language server is and how it should be started.
//
// This file is the part of the extension that is a decision rather than
// a call into the editor, and it is separate for that reason: it takes
// the settings, the platform and a way to ask whether a file exists,
// and it answers with a command line. Nothing here requires `vscode`,
// so the tests beside it run under plain node and check the answers a
// user would otherwise find out about by the server failing to start.

const path = require('node:path')

/** What the binary is called here. Windows is the only one that differs. */
function binaryName(platform) {
  return platform === 'win32' ? 'zu.exe' : 'zu'
}

/**
 * A path as the user wrote it, made absolute.
 *
 * A leading `~` is expanded because a settings file is a text file and
 * people write `~` in text files. A relative path is from the workspace
 * folder rather than from wherever the editor happened to be launched,
 * since the setting is stored beside the project it is about.
 */
function absolute(raw, where) {
  let text = raw.trim()
  if (!text) return ''
  if (text === '~' || text.startsWith('~/') || text.startsWith('~\\')) {
    if (!where.home) return text
    text = path.join(where.home, text.slice(1))
  }
  if (path.isAbsolute(text)) return text
  if (!where.workspace) return text
  return path.join(where.workspace, text)
}

/**
 * The binary to run, and where it was found.
 *
 * Three places, in the order a user would expect to win. A path they
 * set beats everything, because somebody who names a binary has a
 * reason and it is usually a build they are testing. Then the one
 * bundled in the extension, which is what an install from the
 * marketplace has and is the case where nothing was configured and
 * everything works. Then the bare name, which the editor looks up on
 * `PATH`, which is what a `cargo install` or a package manager leaves.
 */
function binary(where) {
  const named = absolute(where.setting || '', where)
  if (named) return { path: named, source: 'setting' }
  const bundled = path.join(where.extensionPath, 'bin', binaryName(where.platform))
  if (where.exists(bundled)) return { path: bundled, source: 'bundled' }
  return { path: binaryName(where.platform), source: 'path' }
}

/**
 * The database to answer about, or nothing.
 *
 * Nothing is a supported answer and not a failure. Without a database
 * the server checks syntax and offers keywords, which is everything
 * that can be said about text with no catalog behind it, and a file
 * that has been opened before its database exists is the normal way a
 * project starts.
 */
function database(where) {
  return absolute(where.database || '', where)
}

/**
 * The whole command line, ready to spawn.
 *
 * @param {{
 *   setting?: string,
 *   database?: string,
 *   extensionPath: string,
 *   platform: string,
 *   workspace?: string,
 *   home?: string,
 *   exists: (path: string) => boolean,
 * }} where
 */
function resolve(where) {
  const found = binary(where)
  const args = ['lsp', '--stdio']
  const db = database(where)
  if (db) args.push('--db', db)
  return { command: found.path, args, source: found.source, database: db }
}

/**
 * What the status bar says.
 *
 * The database is the one fact about this server a user cannot see from
 * anywhere else in the window, and it is the one that decides whether
 * completion knows any table names, so it is the one on the bar.
 */
function label(db) {
  return db ? `zu: ${path.basename(db)}` : 'zu: no database'
}

/** The tooltip under that, which is the same fact said in full. */
function tooltip(resolved) {
  const where = {
    setting: 'the path in zu.server.path',
    bundled: 'the binary bundled with this extension',
    path: 'the binary on PATH',
  }[resolved.source]
  const db = resolved.database
    ? `Answering about ${resolved.database}.`
    : 'No database, so names in the file are not checked and not offered.'
  return `${db}\nServer: ${resolved.command}, ${where}.\nClick to choose a database.`
}

module.exports = { resolve, label, tooltip, binaryName, absolute }
