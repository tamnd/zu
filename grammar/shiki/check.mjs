// The Shiki grammar against the words it was written from.
//
// `gql.tmLanguage.json` is generated, so what is worth checking is not
// that it holds the words but that the patterns built out of them mean
// what they were meant to. TextMate is a list of regular expressions
// run in order, and the order is the whole design: a keyword inside a
// string is a string, a type name where a value stands is a variable,
// and both of those are decided by which pattern gets there first.
//
// So this loads the grammar the way the documentation site does and
// asks two things of it. A table of statements, each with a word and
// the scope that word has to come out with, which is where the traps
// are written down. And every statement in the conformance corpus,
// which is the check that no pattern quietly matches more than it was
// meant to: a word coloured as a keyword has to be a keyword.

import { execFileSync } from "node:child_process";
import { existsSync, readFileSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { createHighlighter } from "shiki";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..", "..");
const grammar = JSON.parse(readFileSync(join(here, "gql.tmLanguage.json"), "utf8"));
const THEME = "github-dark";

// What a word of each kind is painted with, and the word lists
// themselves, read back out of the generated grammar so that this
// checks what the site loads rather than what the generator meant.
const KINDS = [
  ["keyword", "keyword.control.gql"],
  ["literal", "constant.language.gql"],
  ["function", "support.function.gql"],
  ["algorithm", "support.function.graph.gql"],
  ["type", "support.type.gql"],
];
const WORDS = new Map(KINDS.map(([kind, scope]) => [scope, words(kind)]));

const highlighter = await createHighlighter({ langs: [grammar], themes: [THEME] });
const problems = [];

// The traps, one line each: the statement, the word in it, and the
// scope that word has to have. A scope with a `!` in front is one the
// word must not have.
const TRAPS = [
  ["MATCH (n:Person) RETURN n", "MATCH", "keyword.control.gql"],
  ["MATCH (n) WHERE n.age IS NOT NULL RETURN n", "IS", "keyword.control.gql"],
  ["MATCH (n) WHERE n.age IS NOT NULL RETURN n", "NULL", "constant.language.gql"],
  ["RETURN count(*) AS n", "count", "support.function.gql"],
  ["CALL pagerank('KNOWS') YIELD node RETURN node", "pagerank", "support.function.graph.gql"],
  ["RETURN CAST(1 AS INT64) AS n", "INT64", "support.type.gql"],
  ["RETURN DATE '2024-01-01' AS d", "DATE", "support.type.gql"],
  ["RETURN LOCAL TIME '12:34:00' AS t", "TIME", "support.type.gql"],
  ["RETURN $since AS x", "$since", "variable.parameter.gql"],
  ["RETURN 1 + 2 AS n", "1", "constant.numeric.gql"],
  ["// a note\nRETURN 1 AS n", "// a note", "comment.line.double-slash.gql"],
  ["MATCH (`odd name`) RETURN 1 AS n", "`odd name`", "variable.other.quoted.gql"],
  // A keyword inside a string is text, which is the one mistake every
  // hand-written highlighter makes.
  ["RETURN 'MATCH' AS x", "'MATCH'", "string.quoted.single.gql"],
  ["RETURN 'MATCH' AS x", "'MATCH'", "!keyword.control.gql"],
  // A type name is a name until something makes it a type. `date` here
  // is a variable an earlier clause bound.
  ["MATCH (date) RETURN date", "date", "!support.type.gql"],
  // And an escape inside a string is not the end of it.
  [String.raw`RETURN 'it\'s' AS x`, String.raw`\'`, "constant.character.escape.gql"],
];

for (const [code, word, want] of TRAPS) {
  const negated = want.startsWith("!");
  const scope = negated ? want.slice(1) : want;
  const found = scopesOf(code, word);
  if (found === null) {
    problems.push(`${JSON.stringify(code)} has no token ${JSON.stringify(word)}`);
  } else if (found.includes(scope) === negated) {
    problems.push(
      `${JSON.stringify(word)} in ${JSON.stringify(code)} is ${found.join(" ")}, ` +
        `and it ${negated ? "must not be" : "has to be"} ${scope}`,
    );
  }
}

// Every statement the engine answers, coloured, with every word that
// came out as one of the kinds checked against the list that kind was
// written from. A pattern that matches more than its words is a
// variable painted as a keyword on the website.
const queries = process.env.ZU_GRAMMAR_QUERIES ?? join(root, "target", "grammar-queries");
if (!existsSync(queries)) {
  execFileSync("cargo", ["run", "-q", "-p", "xtask", "--", "grammar", "--queries", queries], {
    cwd: root,
    stdio: ["ignore", "inherit", "inherit"],
  });
}
const statements = readdirSync(queries)
  .filter((name) => name.endsWith(".gql"))
  .map((name) => [name, readFileSync(join(queries, name), "utf8")]);

let coloured = 0;
for (const [name, code] of statements) {
  const lit = highlighter.codeToTokens(code, {
    lang: "gql",
    theme: THEME,
    includeExplanation: true,
  });
  // A token is what the theme paints in one colour, so two words that
  // came out the same colour are one token and the words are its
  // explanation. The words are what this is about.
  for (const line of lit.tokens) {
    for (const part of line.flatMap((token) => token.explanation ?? [])) {
      const text = part.content.trim();
      const painted = part.scopes.map((scope) => scope.scopeName);
      for (const [scope, list] of WORDS) {
        if (!painted.includes(scope) || list.has(text.toLowerCase())) {
          continue;
        }
        problems.push(`${name}: ${JSON.stringify(text)} is painted ${scope} and is not one`);
      }
      coloured += 1;
    }
  }
}

if (problems.length > 0) {
  const shown = problems.slice(0, 20);
  for (const problem of shown) {
    console.error(problem);
  }
  if (problems.length > shown.length) {
    console.error(`  and ${problems.length - shown.length} more`);
  }
  console.error(
    `\nshiki: ${problems.length} to fix. The grammar is generated, so the fix is in ` +
      `crates/xtask/src/grammar.rs or grammar/vocabulary.toml rather than in the JSON.`,
  );
  process.exit(1);
}

console.log(
  `${TRAPS.length} scopes are what they have to be, and ${coloured} tokens of ` +
    `${statements.length} corpus statements are coloured as the words they are`,
);

/**
 * The words of one kind, out of the pattern that was built from them.
 * The longest alternation in the pattern is the word list: the type
 * pattern also holds the two words that introduce a type, and that
 * group is three long where the list is fifty.
 */
function words(kind) {
  const pattern = grammar.repository[kind]?.match ?? "";
  const groups = [...pattern.matchAll(/\(([^()]*\|[^()]*)\)/g)].map((m) => m[1].split("|"));
  const list = groups.sort((a, b) => b.length - a.length)[0];
  if (!list) {
    fail(`the grammar has no ${kind} pattern to read the ${kind} words out of`);
  }
  return new Set(list.map((word) => word.replace(/\\b/g, "").toLowerCase()));
}

/**
 * The scopes covering `word` where it stands in `code`. Shiki runs
 * neighbouring text of the same scope together, so this asks by
 * position rather than by token text: a word that came out inside a
 * longer token is a word that was painted with its surroundings, which
 * is an answer and not a missing token.
 */
function scopesOf(code, word) {
  const at = code.indexOf(word);
  if (at < 0) {
    fail(`${JSON.stringify(word)} is not in ${JSON.stringify(code)}`);
  }
  const lit = highlighter.codeToTokens(code, {
    lang: "gql",
    theme: THEME,
    includeExplanation: true,
  });
  for (const line of lit.tokens) {
    for (const token of line) {
      if (token.offset <= at && at < token.offset + token.content.length) {
        return scopes(token);
      }
    }
  }
  return null;
}

function scopes(token) {
  return (token.explanation ?? []).flatMap((part) => part.scopes.map((s) => s.scopeName));
}

function fail(message) {
  console.error(`shiki: ${message}`);
  process.exit(1);
}
