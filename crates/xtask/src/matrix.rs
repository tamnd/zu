//! The rows of a workflow's `include:` matrix.
//!
//! Two tables in this repository are the source of a matrix in a
//! workflow, the platforms and the repositories, and a third will be
//! along. Each of them is held to its workflow in both directions,
//! which means each of them has to read the matrix, which is why this
//! is here once rather than beside each of them.
//!
//! Reading the block by its indentation rather than parsing YAML: what
//! is wanted is a list of `key: value` under one key, the files are
//! written here, and a YAML library to read six keys out of them would
//! be a dependency in the build tooling of thirteen repositories. A
//! structure this does not understand is an error with a line number
//! rather than a row silently dropped, which is the rule the TOML
//! reader beside it follows too.

/// One row of a matrix, as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub line: usize,
    pub pairs: Vec<(String, String)>,
}

impl Row {
    /// The value of a key, or none where the row leaves it out, which
    /// is what a workflow reads a missing key as.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// Every row of every `include:` block in `text`, in file order, with
/// `file` named in any complaint so a reader knows which workflow is
/// meant.
///
/// Every block rather than the first, because a workflow that runs one
/// job per platform has one and a workflow that dispatches to eight
/// repositories in two stages has two, and what the tables ask is which
/// rows exist and not which job holds them.
pub fn rows(text: &str, file: &str) -> Result<Vec<Row>, String> {
    let lines: Vec<&str> = text.lines().collect();
    let blocks: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.trim() == "include:" && line.starts_with(' '))
        .map(|(at, _)| at)
        .collect();
    if blocks.is_empty() {
        return Err(format!("{file} has no matrix `include:`"));
    }
    let mut out: Vec<Row> = Vec::new();
    for at in blocks {
        let outer = indent(lines[at]);
        let mut block: Vec<Row> = Vec::new();
        for (n, raw) in lines.iter().enumerate().skip(at + 1) {
            let line = n + 1;
            let content = raw.trim();
            if content.is_empty() || content.starts_with('#') {
                continue;
            }
            if indent(raw) <= outer {
                break;
            }
            let (content, opens) = match content.strip_prefix("- ") {
                Some(rest) => (rest.trim(), true),
                None => (content, false),
            };
            let (key, value) = content
                .split_once(':')
                .ok_or_else(|| format!("{file}:{line}: no `:` in a matrix row"))?;
            let key = key.trim().to_string();
            let value = unquote(value.trim()).to_string();
            if opens {
                block.push(Row {
                    line,
                    pairs: Vec::new(),
                });
            }
            let row = block
                .last_mut()
                .ok_or_else(|| format!("{file}:{line}: a matrix key before any `- `"))?;
            if row.pairs.iter().any(|(k, _)| *k == key) {
                return Err(format!("{file}:{line}: {key} is set twice in one row"));
            }
            row.pairs.push((key, value));
        }
        if block.is_empty() {
            return Err(format!("{file}:{}: an empty matrix", at + 1));
        }
        out.append(&mut block);
    }
    Ok(out)
}

/// The number of leading spaces, which is what YAML nests by.
fn indent(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// A scalar with its quotes off. YAML lets a value be bare, and the
/// values here are targets and repository names, so both spellings turn
/// up.
fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(rest) = value.strip_prefix(quote)
            && let Some(inner) = rest.strip_suffix(quote)
        {
            return inner;
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    const MATRIX: &str = concat!(
        "jobs:\n",
        "  build:\n",
        "    strategy:\n",
        "      matrix:\n",
        "        include:\n",
        "          # a comment inside the block belongs to nobody\n",
        "          - name: one\n",
        "            runner: ubuntu-latest\n",
        "          - name: \"two\"\n",
        "            runner: macos-latest\n",
        "    runs-on: ${{ matrix.runner }}\n",
    );

    #[test]
    fn a_block_reads_as_its_rows() {
        let rows = rows(MATRIX, "w.yml").expect("reads");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].get("name"), Some("one"));
        assert_eq!(rows[1].get("name"), Some("two"), "quotes come off");
        assert_eq!(rows[1].get("container"), None);
    }

    #[test]
    fn the_block_ends_where_its_indentation_does() {
        let rows = rows(MATRIX, "w.yml").expect("reads");
        assert!(rows.iter().all(|row| row.get("runs-on").is_none()));
    }

    #[test]
    fn a_key_before_any_row_is_refused() {
        let matrix = MATRIX.replace("          - name: one", "          name: one");
        let error = rows(&matrix, "w.yml").expect_err("a key with no row");
        assert!(error.contains("before any"), "{error}");
    }

    #[test]
    fn a_key_written_twice_in_one_row_is_refused() {
        let matrix = MATRIX.replace(
            "            runner: ubuntu-latest\n",
            "            runner: ubuntu-latest\n            runner: ubuntu-24.04\n",
        );
        let error = rows(&matrix, "w.yml").expect_err("two runners");
        assert!(error.contains("set twice"), "{error}");
    }

    #[test]
    fn a_second_block_is_read_as_more_rows() {
        let matrix = format!(
            "{MATRIX}  site:\n    strategy:\n      matrix:\n        include:\n          - name: \
             three\n"
        );
        let rows = rows(&matrix, "w.yml").expect("reads");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2].get("name"), Some("three"));
    }

    #[test]
    fn a_workflow_with_no_matrix_says_which_workflow() {
        let error = rows("jobs:\n  build:\n", "conductor.yml").expect_err("no include");
        assert!(error.contains("conductor.yml"), "{error}");
    }
}
