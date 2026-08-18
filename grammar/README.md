# The grammars of zuQL

A statement is read in four places and run in one. The engine's parser
is the one that runs it, in `crates/zu-query/src/parser.rs`, and it is
the definition of the language. The other three look at it: the shell
colours a statement as it is typed, an editor parses one that is
finished, and the documentation site colours one printed on a page.
This directory holds the two grammars those three need and the word
list all four agree on.

    vocabulary.toml            every word of zuQL, and what kind of word it is
    tree-sitter-gql/           the tree-sitter grammar, for editors
    shiki/gql.tmLanguage.json  the TextMate grammar, for the website
    corpus.mjs                 the grammar against the conformance corpus
    shiki/check.mjs            the TextMate grammar against the words

None of these is a second parser. Nothing here decides what a statement
means, and a statement one of them and the engine disagree about is a
bug here rather than a dialect. What they are for is the answer a
parser that reports errors cannot give: an editor needs a tree for text
that is being typed and is not a statement yet.

## One word list

`vocabulary.toml` is the list, grouped by what a word is: a keyword the
parser gives meaning to by position, a literal, a built-in function, a
graph algorithm, a value type name. Three consumers read it and
`cargo xtask grammar` puts it where each one needs it.

    cargo xtask grammar          write the generated files, check the rest
    cargo xtask grammar --check  write nothing, and fail on any drift
    cargo xtask grammar --list   print `kind<TAB>word` for every word

Two files are written from the list:
`tree-sitter-gql/queries/highlights.scm` and
`shiki/gql.tmLanguage.json`. Both are committed, because an editor and
the website consume the files rather than the generator, and both are
checked, so a word added to the list and not regenerated fails.

The third consumer is the shell, whose table in
`crates/zu-cli/src/highlight.rs` is checked rather than written. It is
sorted for a binary search and read by a scanner on the keystroke path,
so it stays hand-written and the check holds it to the list.

The check also runs the other way for the tree-sitter grammar: every
keyword the grammar spells has to be a word the list knows, because a
word the grammar parses and nothing colours comes out plain in an
editor. The reverse is not required. The list holds the words the
parser refuses by name, `MERGE`, `FILTER`, `LET` and the rest, and
those are coloured everywhere and parsed nowhere: an error is easier to
believe when the word looked like a word.

## What holds the grammars to the engine

The conformance corpus, which is 945 statements the engine answers and
the one body of zuQL written without these grammars in mind.

    npm install
    npm test

`npm test` regenerates the parser, runs the tree-sitter test corpus in
`tree-sitter-gql/test`, then runs the two checks. `corpus.mjs` parses
every statement the engine accepts and fails on an error node in any of
them, parses every statement the engine answers with a syntax error and
fails on one that parsed, and runs the highlight query over all of them
so a renamed node fails here rather than in an editor. `shiki/check.mjs`
loads the TextMate grammar the way the site does, checks a table of
statements where the traps are (a keyword inside a string is a string,
a type name where a value stands is a variable) and then checks that no
word in the corpus is painted as a kind of word it is not.

The statements come out of `cargo xtask grammar --queries`, which reads
the corpus with the engine's own reader. Nothing in this directory
reads a YAML file.

Two statements the engine refuses parse here, and both are decisions
written down in `corpus.mjs`. `CAST(1 AS NOPE)` names a type nobody
has, which is a table the binder holds and not a shape. And an empty
file is what an editor opens with.

## Using the grammars

`tree-sitter-gql/src` is committed, so the grammar builds without the
tree-sitter CLI, which is what an editor package or a Neovim install
needs. It is generated from `grammar.js`, and CI regenerates it and
fails if the committed files move, so the C is derived rather than
maintained.

The TextMate grammar is a list of regular expressions rather than a
grammar, so it cannot tell a type from a variable of the same name and
does not try. What it has is the word lists and the shapes that are
unambiguous on their own, which is what a code block on a page needs.
An editor that wants to be right uses the tree-sitter grammar.
