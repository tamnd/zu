//! Hostile text against the zuQL frontend: any byte string, lossily
//! decoded, must lex and parse without panicking, overflowing the stack,
//! or hanging. When the parser accepts, the structural invariant the
//! planner relies on must hold: the query is non-empty and its last
//! clause is RETURN.
#![no_main]

use libfuzzer_sys::fuzz_target;
use zu_query::ast::Clause;
use zu_query::parser;

fuzz_target!(|data: &[u8]| {
    let source = String::from_utf8_lossy(data);
    if let Ok(query) = parser::parse(&source) {
        assert!(
            matches!(query.clauses.last(), Some(Clause::Return { .. })),
            "accepted query must end in RETURN"
        );
    }
});
