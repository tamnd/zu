// The tree-sitter grammar against the conformance corpus.
//
// A grammar checked by its own test corpus is a grammar that agrees
// with its author. The corpus is 945 statements the engine answers, so
// it is the one body of zuQL that was written without this grammar in
// mind, and every statement the engine accepts has to parse here with
// no error node in the tree. The statements the engine answers with a
// syntax error are the other half of the same question: a grammar that
// accepts everything is not a grammar.
//
// The statements come out of `cargo xtask grammar --queries`, which
// reads the corpus with the engine's own reader. Nothing here reads a
// YAML file: a fourth reader of the corpus is a fourth thing to keep
// right, and the one in `crates/zu-corpus` is the one the runner uses.

import { execFileSync, spawnSync } from "node:child_process";
import { mkdirSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { basename, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..");
const queries = process.env.ZU_GRAMMAR_QUERIES ?? join(root, "target", "grammar-queries");

// Two statements the engine refuses and this grammar cannot, each for a
// reason that is a decision rather than a gap. Written out by name so
// that a third one has to be argued for in a diff.
const REFUSED_ANYWAY = new Map([
  [
    "error--a-cast-to-a-type-nobody-has",
    "which words name a type is the binder's table, and a grammar that " +
      "spelled it would refuse the cast as a shape rather than as the error it is",
  ],
  [
    "error--nothing-to-parse",
    "an empty file is what an editor opens with, and a grammar that refused " +
      "it would paint every new buffer red",
  ],
]);

if (!process.env.ZU_GRAMMAR_QUERIES) {
  execFileSync(
    "cargo",
    ["run", "-q", "-p", "xtask", "--", "grammar", "--queries", queries],
    { cwd: root, stdio: ["ignore", "inherit", "inherit"] },
  );
}

const accepted = readdirSync(queries).filter((name) => name.endsWith(".gql"));
const refused = readdirSync(join(queries, "refused")).filter((name) => name.endsWith(".gql"));
if (accepted.length === 0 || refused.length === 0) {
  fail(`${queries} holds ${accepted.length} statements and ${refused.length} refused ones`);
}

// The paths go through a file rather than the command line because
// there are a thousand of them.
const pathsFile = join(root, "target", "grammar-paths.txt");
mkdirSync(dirname(pathsFile), { recursive: true });
writeFileSync(
  pathsFile,
  `${[
    ...accepted.map((name) => join(queries, name)),
    ...refused.map((name) => join(queries, "refused", name)),
  ].join("\n")}\n`,
);

const bad = errors(pathsFile);

const problems = [];
for (const name of accepted) {
  if (bad.has(name)) {
    problems.push(`${name} is a statement the engine answers and this grammar cannot parse:
    ${readFileSync(join(queries, name), "utf8").trim()}`);
  }
}
for (const name of refused) {
  const stem = name.replace(/\.gql$/, "");
  if (bad.has(name) === REFUSED_ANYWAY.has(stem)) {
    problems.push(
      REFUSED_ANYWAY.has(stem)
        ? `${name} is refused by the grammar now, and it is on the list of statements ` +
          `the grammar is not expected to refuse. Take it off the list.`
        : `${name} is a syntax error to the engine and this grammar parses it:
    ${readFileSync(join(queries, "refused", name), "utf8").trim()}`,
    );
  }
}

if (problems.length > 0) {
  for (const problem of problems) {
    console.error(problem);
  }
  fail(
    `${problems.length} of ${accepted.length + refused.length} statements disagree with the engine`,
  );
}

// The highlight query names nodes and fields of the grammar, so a
// grammar that renamed one leaves a query that either matches nothing
// or does not compile. Running it over every statement answers both,
// and it is the editor's own path: a query that fails to compile takes
// the highlighting down rather than the check.
const queried = run("query", ["queries/highlights.scm", "--paths", pathsFile], { keep: false });
if (queried.status !== 0) {
  console.error(queried.stderr.trim());
  fail("the highlight query does not run against the grammar it was written for");
}

console.log(
  `${accepted.length} statements parse and ${refused.length - REFUSED_ANYWAY.size} of the ` +
    `${refused.length} the engine refuses do not parse, ` +
    `the other ${REFUSED_ANYWAY.size} by decision`,
);

/**
 * The names of the files whose tree holds an error node. `--quiet`
 * prints a line for a file that did not parse and nothing for a file
 * that did, so the output is the answer.
 */
function errors(list) {
  const ran = run("parse", ["--quiet", "--paths", list]);
  return new Set(
    ran.stdout
      .split("\n")
      .filter((line) => line.trim() !== "")
      .map((line) => basename(line.split(/\s/)[0])),
  );
}

/**
 * One tree-sitter command, run from the grammar's own directory, which
 * is what makes the parser it uses the one in this repository.
 */
function run(command, args, { keep = true } = {}) {
  const bin = join(here, "node_modules", ".bin");
  const ran = spawnSync("tree-sitter", [command, ...args], {
    cwd: join(here, "tree-sitter-gql"),
    encoding: "utf8",
    env: { ...process.env, PATH: `${bin}:${process.env.PATH}` },
    // A query over a thousand statements answers with every capture in
    // all of them, which is tens of megabytes and a buffer this does
    // not need: what is wanted from that run is whether it ran.
    stdio: keep ? "pipe" : ["ignore", "ignore", "pipe"],
  });
  if (ran.error) {
    fail(`tree-sitter ${command} did not run: ${ran.error.message}. Run npm install first.`);
  }
  return ran;
}

function fail(message) {
  console.error(`corpus: ${message}`);
  process.exit(1);
}
