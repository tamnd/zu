'use strict'

// The extension itself, which is the thin half.
//
// Everything this knows about zuQL is in the server, which is `zu lsp
// --stdio` in the same binary as the engine, so this file has one job:
// start that binary, tell it which database to answer about, and put
// the answer to that question somewhere the user can see and change it.
// The decision worth writing down is what is not here. There is no
// second parser, no keyword list, no completion of any kind, and no
// copy of anything the engine already knows, because an extension that
// held any of those would be a thing that drifts from the engine in the
// gap between two releases.
//
// The one exception is the TextMate grammar in `syntaxes/`, and it is
// generated from `grammar/vocabulary.toml` by `cargo xtask grammar`
// along with the copy the documentation site uses. It exists because a
// file is coloured the moment it opens and the server takes a moment to
// start, and it is checked in CI against the same list the shell and
// the tree-sitter grammar are checked against, so it cannot drift.

const fs = require('node:fs')
const path = require('node:path')
const vscode = require('vscode')
const { LanguageClient } = require('vscode-languageclient/node')

const { resolve, label, tooltip } = require('./server')

/** The running server, or nothing while it is being restarted. */
let client = null
/** The bar item that says which database is attached. */
let status = null

async function activate(context) {
  status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 100)
  status.command = 'zu.selectDatabase'
  context.subscriptions.push(status)

  context.subscriptions.push(
    vscode.commands.registerCommand('zu.restartServer', () => start(context)),
    vscode.commands.registerCommand('zu.selectDatabase', () => choose(context)),
  )

  // A setting that changes where the server is or what it answers about
  // is a setting that needs the server started again. The trace level
  // is not: the client reads it per message.
  context.subscriptions.push(
    vscode.workspace.onDidChangeConfiguration((event) => {
      if (event.affectsConfiguration('zu.server.path') || event.affectsConfiguration('zu.database')) {
        start(context)
      }
    }),
  )

  await start(context)
}

/** The first workspace folder, which is what a relative path is from. */
function workspace() {
  const folders = vscode.workspace.workspaceFolders
  return folders && folders.length > 0 ? folders[0].uri.fsPath : ''
}

/** What the settings and the platform add up to right now. */
function wanted(context) {
  const config = vscode.workspace.getConfiguration('zu')
  return resolve({
    setting: config.get('server.path', ''),
    database: config.get('database', ''),
    extensionPath: context.extensionPath,
    platform: process.platform,
    workspace: workspace(),
    home: require('node:os').homedir(),
    exists: (p) => fs.existsSync(p),
  })
}

/**
 * Starts the server, stopping the one that is running first.
 *
 * A binary that is not there is a message and not a thrown error. The
 * usual reason is that zu is not installed yet, which is a thing the
 * user can fix in a minute and is not a thing an editor should report
 * as a crash.
 */
async function start(context) {
  await stop()
  const found = wanted(context)
  if (found.source === 'path' && !onPath(found.command)) {
    say(found)
    vscode.window
      .showWarningMessage(
        'The zu binary was not found. Install it, or set zu.server.path to where it is.',
        'Open Settings',
      )
      .then((answer) => {
        if (answer === 'Open Settings') {
          vscode.commands.executeCommand('workbench.action.openSettings', 'zu.server.path')
        }
      })
    return
  }

  const run = { command: found.command, args: found.args }
  client = new LanguageClient(
    'zu',
    'zu',
    { run, debug: run },
    {
      // Untitled documents count. A scratch buffer is where a
      // statement gets written before anybody decides where it lives,
      // and the server needs no file on disk to check one.
      documentSelector: [
        { scheme: 'file', language: 'gql' },
        { scheme: 'untitled', language: 'gql' },
      ],
      outputChannelName: 'zu',
    },
  )
  try {
    await client.start()
  } catch (e) {
    client = null
    vscode.window.showErrorMessage(`The zu language server did not start: ${e.message}`)
  }
  say(found)
}

/** Whether a bare name is findable, which is what `PATH` means here. */
function onPath(name) {
  const dirs = (process.env.PATH || '').split(path.delimiter).filter(Boolean)
  const exts = process.platform === 'win32' ? (process.env.PATHEXT || '.EXE').split(';') : ['']
  return dirs.some((dir) => exts.some((ext) => fs.existsSync(path.join(dir, name + ext))))
}

async function stop() {
  const running = client
  client = null
  if (running) await running.stop()
}

/** Puts the current answer on the status bar. */
function say(found) {
  status.text = label(found.database)
  status.tooltip = tooltip(found)
  status.show()
}

/**
 * Asks which database to answer about.
 *
 * The list is the `.zu1` files in the workspace, because a project with
 * one of them has one answer and a project with three should be asked
 * rather than guessed at. Nothing is on the list too, since a file
 * being edited before its database exists is normal and there has to be
 * a way back to it.
 */
async function choose(context) {
  const found = await vscode.workspace.findFiles('**/*.zu1', '**/node_modules/**', 64)
  const root = workspace()
  const items = found
    .map((uri) => ({
      label: root ? path.relative(root, uri.fsPath) : uri.fsPath,
      description: uri.fsPath,
    }))
    .sort((a, b) => a.label.localeCompare(b.label))
  items.push({
    label: 'No database',
    description: 'Check syntax only, and offer keywords rather than table names',
  })

  const picked = await vscode.window.showQuickPick(items, {
    placeHolder: 'Which database should zu answer about?',
  })
  if (!picked) return
  const value = picked.label === 'No database' ? '' : picked.label
  const target = root ? vscode.ConfigurationTarget.Workspace : vscode.ConfigurationTarget.Global
  await vscode.workspace.getConfiguration('zu').update('database', value, target)
  // The configuration listener starts the server again, so there is
  // nothing to do here but let it.
}

async function deactivate() {
  await stop()
}

module.exports = { activate, deactivate }
