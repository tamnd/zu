# The grammars of zuQL

A statement is read in four places and run in one. The engine's parser
is the one that runs it, in `crates/zu-query/src/parser.rs`, and it is
the definition of the language. The other three look at it: the shell
colours a statement as it is typed, an editor parses one that is
finished, and the documentation site colours one printed on a page.
This directory holds the word list all four agree on, and the grammar
the site needs.

    vocabulary.toml            every word of zuQL, and what kind of word it is
    shiki/gql.tmLanguage.json  the TextMate grammar, for the website
    shiki/check.mjs            the TextMate grammar against the words

The editor's grammar is not here. It is
[tamnd/tree-sitter-gql](https://github.com/tamnd/tree-sitter-gql),
which left this repository in August 2026 with its history: a
tree-sitter grammar commits its generated parser so an editor can build
it without the CLI, that file is 2.2 MB, and every clone of the engine
was paying for each rewrite of it.

Neither grammar is a second parser. Nothing in either one decides what
a statement means, and a statement one of them and the engine disagree
about is a bug in the grammar rather than a dialect. What they are for
is the answer a parser that reports errors cannot give: an editor needs
a tree for text that is being typed and is not a statement yet.

## One word list

`vocabulary.toml` is the list, grouped by what a word is: a keyword the
parser gives meaning to by position, a literal, a built-in function, a
graph algorithm, a value type name. Three consumers read it and
`cargo xtask grammar` puts it where each one needs it.

    cargo xtask grammar          write the generated files, check the rest
    cargo xtask grammar --check  write nothing, and fail on any drift
    cargo xtask grammar --list   print `kind<TAB>word` for every word

`shiki/gql.tmLanguage.json` is written from the list and committed,
because the website consumes the file rather than the generator, and it
is checked, so a word added to the list and not regenerated fails.

The second consumer is the shell, whose table in
`crates/zu-cli/src/highlight.rs` is checked rather than written. It is
sorted for a binary search and read by a scanner on the keystroke path,
so it stays hand-written and the check holds it to the list.

The third is the tree-sitter grammar, and it is somewhere else, so
`cargo xtask grammar` audits it when there is a checkout of it to
audit. It looks at `$ZU_TREE_SITTER` and then at a sibling directory
called `tree-sitter-gql`, and it says which halves it did. With a
checkout it writes that repository's `queries/highlights.scm` and
checks that every keyword its `grammar.js` spells is a word this list
knows, because a word the grammar parses and nothing colours comes out
plain in an editor. Without one it does the shell and the site and says
so.

That check is a gate in the grammar's CI, which checks this repository
out and runs the same command. So a word added here and not carried
across turns the grammar's build red rather than this one, which is
where the work of carrying it across is.

The reverse direction is not required anywhere. The list holds the
words the parser refuses by name, `MERGE`, `FILTER`, `LET` and the
rest, and those are coloured everywhere and parsed nowhere: an error is
easier to believe when the word looked like a word.

## What holds the grammar to the engine

The conformance corpus, which is a thousand statements the engine
answers and the one body of zuQL written without these grammars in
mind.

    npm install
    npm test

`shiki/check.mjs` loads the TextMate grammar the way the site does,
checks a table of statements where the traps are (a keyword inside a
string is a string, a type name where a value stands is a variable) and
then checks that no word in the corpus is painted as a kind of word it
is not.

The tree-sitter grammar gets the same corpus put to it, in its own
repository, by a check that runs `cargo xtask grammar --queries` here
and parses everything that comes out. Nothing in either place reads a
YAML file: the reader in `crates/zu-corpus` is the one the conformance
runner uses, and a second reader of the corpus is a second thing to
keep right.

The TextMate grammar is a list of regular expressions rather than a
grammar, so it cannot tell a type from a variable of the same name and
does not try. What it has is the word lists and the shapes that are
unambiguous on their own, which is what a code block on a page needs.
An editor that wants to be right uses the tree-sitter grammar.
