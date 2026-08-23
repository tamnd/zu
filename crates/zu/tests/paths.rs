//! Path values: what a matched path is, building one, and measuring it.
//!
//! A path variable has bound to something since the first variable
//! length pattern ran, but what it bound to was a list of the right
//! elements rather than a path, so `p IS TYPED PATH` was false about a
//! value that was one. What is new is the type: the same elements, held
//! as the value ISO names, which is what lets GE06 build one, GF04
//! measure one, and 22G0Z refuse a sequence that is shaped like one and
//! describes a walk nobody can take.

use zu::query::{Value, run};
use zu_zu1::file::Zu1File;
use zu_zu1::graph::bulk_load_as;

/// A chain, 0 to 1 to 2 to 3, so a path has hops to have and the two
/// ends of one are different nodes.
fn graph(dir: &std::path::Path) -> Zu1File {
    let mut zu = Zu1File::create(&dir.join("paths.zu1")).unwrap();
    bulk_load_as(&mut zu, "person", "knows", 4, &[(0, 1), (1, 2), (2, 3)]).unwrap();
    zu
}

fn one(db: &mut Zu1File, source: &str) -> Value {
    let result = run(source, db, &[]).unwrap_or_else(|e| panic!("{source}: {e}"));
    assert_eq!(result.rows.len(), 1, "{source} returned {:?}", result.rows);
    result.rows[0][0].clone()
}

fn yes(db: &mut Zu1File, source: &str) -> bool {
    match one(db, source) {
        Value::Bool(b) => b,
        other => panic!("{source} answered {other:?}"),
    }
}

fn code(db: &mut Zu1File, source: &str) -> String {
    let err = run(source, db, &[]).expect_err(source);
    err.gqlstatus()
        .unwrap_or_else(|| panic!("{source}: {err} carries no status"))
        .code()
        .to_string()
}

/// GV55. A path variable binds a path, and a path is not the list of
/// the same elements: the two answer differently to `IS TYPED`, which
/// is the whole reason for giving the value its own type rather than
/// leaving it as the list it is made of.
#[test]
fn a_matched_path_is_a_path_and_not_a_list_of_the_same_elements() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let matched = "MATCH p = (a:person)-[:knows]->(b:person) WHERE a.id = 0 RETURN";
    assert!(yes(&mut db, &format!("{matched} (p IS TYPED PATH) AS v")));
    assert!(!yes(
        &mut db,
        &format!("{matched} (p IS TYPED LIST<ANY>) AS v")
    ));
    assert!(!yes(&mut db, "RETURN (1 IS TYPED PATH) AS v"));
    assert!(!yes(&mut db, "RETURN ([1] IS TYPED PATH) AS v"));

    // The elements are the nodes and edges of the walk, alternating,
    // with a node at each end.
    assert_eq!(
        one(&mut db, &format!("{matched} p AS v")),
        Value::Path(vec![
            Value::Node {
                table: 0,
                offset: 0,
            },
            Value::Rel {
                table: 1,
                src: 0,
                dst: 1,
                // The fixture runs each pair once, so the first edge
                // of the load order is the one this walk takes.
                ord: 0,
            },
            Value::Node {
                table: 0,
                offset: 1,
            },
        ])
    );
}

/// GE06 and GA09 together, which is why the corpus case for GE06 needs
/// GA09: a path built out of the elements a pattern matched has to equal
/// the path the pattern matched, or the constructor built something else
/// and the equality is the only thing that says which.
#[test]
fn a_path_built_from_elements_equals_the_path_that_was_matched() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let source = "MATCH p = (a:person)-[e:knows]->(b:person) WHERE a.id = 0 \
                  RETURN (PATH [a, e, b] = p) AS v";
    assert!(yes(&mut db, source));

    // Two paths over the same walk are the same path, and two over
    // different walks are not.
    let source = "MATCH p = (a:person)-[:knows]->(b:person), q = (c:person)-[:knows]->(d:person) \
                  WHERE a.id = 0 AND c.id = 0 RETURN (p = q) AS v";
    assert!(yes(&mut db, source));
    let source = "MATCH p = (a:person)-[:knows]->(b:person), q = (c:person)-[:knows]->(d:person) \
                  WHERE a.id = 0 AND c.id = 1 RETURN (p = q) AS v";
    assert!(!yes(&mut db, source));

    // A path of one node is a path: a walk that goes nowhere is still
    // somewhere. It is also the shortest one there is, because there is
    // no empty path.
    let source = "MATCH (a:person) WHERE a.id = 0 RETURN (PATH [a] IS TYPED PATH) AS v";
    assert!(yes(&mut db, source));

    // An edge may be walked against its direction, so the two nodes of
    // a hop may be named either way round. A path value is a walk and
    // not a claim about which way the edges point.
    let source = "MATCH (a:person)-[e:knows]->(b:person) WHERE a.id = 0 \
                  RETURN (PATH [b, e, a] IS TYPED PATH) AS v";
    assert!(yes(&mut db, source));

    // A null element nulls the whole path, the way a null endpoint
    // nulls a matched one: there is no path with a hole in it.
    let source = "MATCH (a:person) WHERE a.id = 0 RETURN (PATH [a, null, a] IS NULL) AS v";
    assert!(yes(&mut db, source));
}

/// GE06 over more than one hop, which is the case a constructor that
/// only ever joined a single pair would pass every other case here
/// without: the join has to hold at each step and the elements have to
/// stay in the order the query wrote them.
#[test]
fn a_path_may_be_built_over_several_hops() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let two = "MATCH p = (a:person)-[e1:knows]->(b:person)-[e2:knows]->(c:person) \
               WHERE a.id = 0 RETURN";
    assert!(yes(
        &mut db,
        &format!("{two} (PATH [a, e1, b, e2, c] = p) AS v")
    ));
    assert_eq!(
        one(
            &mut db,
            &format!("{two} PATH_LENGTH(PATH [a, e1, b, e2, c]) AS v")
        ),
        Value::Int(2)
    );

    // The order is part of the value. Both of these are paths over the
    // same three elements and they are not the same path, which is what
    // makes a path a walk rather than a set of what it touched.
    let source = "MATCH p = (a:person)-[e:knows]->(b:person) WHERE a.id = 0 \
                  RETURN (PATH [b, e, a] = p) AS v";
    assert!(!yes(&mut db, source));

    // A walk may go back the way it came, so an element may be written
    // twice. Nothing here asks for a trail: the modes that do are on the
    // pattern, and a constructor is not matching anything.
    let source = "MATCH (a:person)-[e:knows]->(b:person) WHERE a.id = 0 \
                  RETURN PATH_LENGTH(PATH [a, e, b, e, a]) AS v";
    assert_eq!(one(&mut db, source), Value::Int(2));

    // A built path is a value like the matched one is, so everything
    // that takes a path takes this: the elements come back in the order
    // the walk took them, and a set function gathers the paths
    // themselves rather than what they are made of.
    let source = "MATCH (a:person)-[e:knows]->(b:person) WHERE a.id = 0 \
                  RETURN SIZE(ELEMENTS(PATH [a, e, b])) AS v";
    assert_eq!(one(&mut db, source), Value::Int(3));
    let source = "MATCH (a:person)-[e:knows]->(b:person) RETURN COLLECT(PATH [a, e, b]) AS v";
    let Value::List(built) = one(&mut db, source) else {
        panic!("{source} did not answer a list");
    };
    assert_eq!(built.len(), 3);
    assert!(built.iter().all(|v| matches!(v, Value::Path(_))));
}

/// 22G0Z. The elements have to be a walk somebody can take. An edge
/// between two nodes it does not touch is the interesting case, because
/// the sequence is the right shape and the right kinds, so nothing but
/// the graph can tell it is wrong.
#[test]
fn a_path_whose_edges_do_not_join_its_nodes_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let source = "MATCH (a:person)-[e:knows]->(b:person)-[:knows]->(c:person) WHERE a.id = 0 \
                  RETURN PATH [b, e, c] AS v";
    assert_eq!(code(&mut db, source), "22G0Z");

    // The shape is refused by the same condition: a path with an even
    // number of elements has no node at one end, and the empty list is
    // not the empty path, because there is no empty path.
    let source = "MATCH (a:person)-[e:knows]->(b:person) WHERE a.id = 0 RETURN PATH [a, e] AS v";
    assert_eq!(code(&mut db, source), "22G0Z");
    assert_eq!(code(&mut db, "RETURN PATH [] AS v"), "22G0Z");

    // An element of the wrong kind is a different fault: the query is
    // wrong about what it wrote rather than the graph disagreeing about
    // what it joins, so it does not depend on the data and is refused
    // before the statement runs. That covers an edge where a node goes
    // as well as a number where either goes.
    let source = "MATCH (a:person)-[e:knows]->(b:person) WHERE a.id = 0 RETURN PATH [e] AS v";
    assert_eq!(code(&mut db, source), "22G03");
    let source = "MATCH (a:person) WHERE a.id = 0 RETURN PATH [a, 1, a] AS v";
    assert_eq!(code(&mut db, source), "22G03");
}

/// G004. Two paths join with the same operator two strings join with,
/// and the join is a walk rather than a sequence: the first path has to
/// end where the second one starts, and the shared node is in the answer
/// once. A join that does not meet is the same fault as a path built
/// from elements that do not join, and carries the same condition.
#[test]
fn two_paths_that_meet_at_a_node_join_into_one_walk() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let source = "MATCH p = (a:person)-[:knows]->(b:person), q = (b2:person)-[:knows]->(c:person) \
                  WHERE a.id = 0 AND b.id = b2.id RETURN PATH_LENGTH(p || q) AS v";
    assert_eq!(one(&mut db, source), Value::Int(2));

    // The joined path is the path the same walk matches, elements and
    // all, which is what says the shared node was not counted twice.
    let source = "MATCH p = (a:person)-[:knows]->(b:person), q = (b2:person)-[:knows]->(c:person), \
                  w = (a2:person)-[:knows]->(:person)-[:knows]->(:person) \
                  WHERE a.id = 0 AND b.id = b2.id AND a2.id = 0 RETURN (p || q) = w AS v";
    assert!(yes(&mut db, source));

    // Two paths that do not meet describe no walk, so there is nothing
    // to answer with.
    let source = "MATCH p = (a:person)-[:knows]->(b:person), q = (c:person)-[:knows]->(d:person) \
                  WHERE a.id = 0 AND c.id = 2 RETURN p || q AS v";
    assert_eq!(code(&mut db, source), "22G0Z");

    // A path joined to the one node path at its end is itself, which is
    // the join having an identity rather than a special case for it.
    let source = "MATCH p = (a:person)-[:knows]->(b:person) WHERE a.id = 0 \
                  RETURN (p || PATH [b]) = p AS v";
    assert!(yes(&mut db, source));

    // A path is not a list, and joining one to a list is the mistake the
    // operator's own message is about rather than a walk that does not
    // meet.
    let source = "MATCH p = (a:person)-[:knows]->(b:person) WHERE a.id = 0 RETURN p || [b] AS v";
    assert_eq!(code(&mut db, source), "22G03");
    let source = "MATCH p = (a:person)-[:knows]->(b:person) WHERE a.id = 0 RETURN p || NULL AS v";
    assert_eq!(one(&mut db, source), Value::Null);
}

/// GF04. Length is edges. The distinction matters because the element
/// count is a different number and both are integers, so an engine that
/// answered with the element count would be believed.
#[test]
fn path_length_counts_the_edges_and_not_the_elements() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let source = "MATCH p = (a:person)-[:knows]->(b:person) WHERE a.id = 0 \
                  RETURN PATH_LENGTH(p) AS v";
    assert_eq!(one(&mut db, source), Value::Int(1));
    let source = "MATCH p = (a:person)-[:knows]->(b:person)-[:knows]->(c:person) \
                  WHERE a.id = 0 RETURN PATH_LENGTH(p) AS v";
    assert_eq!(one(&mut db, source), Value::Int(2));
    let source = "MATCH (a:person) WHERE a.id = 0 RETURN PATH_LENGTH(PATH [a]) AS v";
    assert_eq!(one(&mut db, source), Value::Int(0));
    assert_eq!(one(&mut db, "RETURN PATH_LENGTH(null) AS v"), Value::Null);

    // A list of the same elements is not a path, so it does not have a
    // path's length: answering anyway would let a query that lost the
    // path on the way in look like it still had one.
    assert_eq!(code(&mut db, "RETURN PATH_LENGTH([1, 2, 3]) AS v"), "22G03");

    // size() still answers the element count for a path, which is the
    // number a query written before the path had a type was getting.
    let source = "MATCH p = (a:person)-[:knows]->(b:person) WHERE a.id = 0 RETURN SIZE(p) AS v";
    assert_eq!(one(&mut db, source), Value::Int(3));
}

/// ISO 20.16. `ELEMENTS` is how a query reads what a path holds, since
/// a path is a value of its own and nothing indexes into one. The list
/// it answers is the walk in the order it was taken, a node at each end
/// and an edge between each pair of them.
#[test]
fn elements_answers_the_walk_as_a_list() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let source = "MATCH p = (a:person)-[:knows]->(b:person)-[:knows]->(c:person) \
                  WHERE a.id = 0 RETURN ELEMENTS(p) AS v";
    let Value::List(items) = one(&mut db, source) else {
        panic!("{source} answered something that is not a list");
    };
    assert_eq!(items.len(), 5);
    for (at, item) in items.iter().enumerate() {
        let node = matches!(item, Value::Node { .. });
        assert_eq!(node, at % 2 == 0, "element {at} is {item:?}");
    }

    // The elements are the ones the path holds and in the same places,
    // so the list the path answers is the list the query can write out.
    let source = "MATCH p = (a:person)-[e:knows]->(b:person) WHERE a.id = 0 \
                  RETURN (ELEMENTS(p) = [a, e, b]) AS v";
    assert!(yes(&mut db, source));

    // A one node path is one element, and a null is a null.
    let source = "MATCH (a:person) WHERE a.id = 0 RETURN SIZE(ELEMENTS(PATH [a])) AS v";
    assert_eq!(one(&mut db, source), Value::Int(1));
    assert_eq!(one(&mut db, "RETURN ELEMENTS(null) AS v"), Value::Null);

    // A list is already the list of its elements, so asking a list for
    // them is a query that has lost the path it meant to ask about.
    assert_eq!(code(&mut db, "RETURN ELEMENTS([1, 2, 3]) AS v"), "22G03");
}

/// A path is a value like any other, so DISTINCT and ORDER BY owe it an
/// answer. ISO orders two paths no more than it orders two records, and
/// the answer still has to be the same on two runs or a query returns
/// different rows each time.
#[test]
fn paths_sort_and_group_the_same_way_twice() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let source = "MATCH p = (a:person)-[:knows]->(b:person) \
                  RETURN DISTINCT PATH_LENGTH(p) AS n, COUNT(*) AS c ORDER BY n";
    let first = run(source, &mut db, &[]).expect("distinct over paths");
    assert_eq!(first.rows, vec![vec![Value::Int(1), Value::Int(3)]]);

    let source = "MATCH p = (a:person)-[:knows]->(b:person) RETURN p AS v ORDER BY p";
    let ordered = run(source, &mut db, &[]).expect("order by a path");
    assert_eq!(ordered.rows.len(), 3);
    let again = run(source, &mut db, &[]).expect("order by a path twice");
    assert_eq!(ordered.rows, again.rows);
    // Sorting by the path sorts by its elements, so the walks come out
    // in the order of the node they start from.
    let starts: Vec<Value> = ordered
        .rows
        .iter()
        .map(|row| match &row[0] {
            Value::Path(elements) => elements[0].clone(),
            other => panic!("expected a path, got {other:?}"),
        })
        .collect();
    let mut sorted = starts.clone();
    sorted.sort_by_key(|v| match v {
        Value::Node { offset, .. } => *offset,
        other => panic!("expected a node, got {other:?}"),
    });
    assert_eq!(starts, sorted);
}

/// PATH is two things, the type and the constructor, and which one it
/// is comes from where it stands. It is not a third thing: ISO 21.3
/// reserves the word, so a query names the walk it builds something
/// else.
#[test]
fn path_is_the_type_and_the_constructor_and_not_a_name() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let source = "MATCH (a:person) WHERE a.id = 0 WITH PATH [a] AS walk \
                  RETURN PATH_LENGTH(walk) AS v";
    assert_eq!(one(&mut db, source), Value::Int(0));

    let source = "UNWIND [1, 2] AS path RETURN sum(path) AS v";
    let err = run(source, &mut db, &[]).expect_err(source);
    assert!(err.to_string().contains("reserved word"), "{source}: {err}");
}
