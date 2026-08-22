# The JSONL session protocol, version 1

`zu shell --format jsonl` is a zu session over a pipe: one JSON object per line in, one JSON object per line out, on a process that stays alive so the catalog, the statistics, the plan cache and the decoded block caches are paid for once instead of once per query. It is how an editor, a notebook kernel, a test harness or an agent drives zu without linking against anything, and it is specified here rather than described in a help page because a wire that programs depend on is an interface and not an output format (`dx/12` §5).

The same loop serves a pipe with no flag at all. `zu shell` asks whether standard input is a terminal, gives a person the editor and gives a program the frames, and `--format jsonl` is for the caller that is a program while holding a terminal open, which is what a harness driving zu under a pty is.

## Versioning

The first line of every session is the greeting, written before any line has been read:

```json
{"protocol":1,"zu":"0.0.1","c_abi":"0.5","format_version":1,"features":["arrow"],"file":"social.zu1"}
```

`protocol` is the version of this document. It moves when a frame or a reply changes meaning, and it does not move when one is added, so a client that reads the fields it knows and ignores the rest keeps working across a release. A client should read the greeting first, check `protocol` against what it was written for, and refuse a number it does not know rather than guess; everything else in the object is a build fact, spelled the way `zu version --format json` spells it. `file` is the path as it was given on the command line, for the client that got its session through a wrapper script and does not know which graph it is holding.

`{"op":"hello"}` answers with the same object at any time, which is what a client that attached to a session somebody else started asks first.

## Framing

One line in, one line out, always in that order and always one for one. Every reply ends in exactly one newline and contains none, because the reader on the other side counts lines rather than braces. A line that starts with `{` is a frame. Any other non-empty line is a bare statement, run with no parameters, with `\n`, `\t` and `\\` unfolded first so that a multi-line statement travels on one line; a backslash pair that means nothing here is left alone, so a statement that was never folded still runs. Empty lines are skipped.

Nothing is asynchronous. A frame is answered before the next one is read, so a client needs no request identifiers and no ordering rules, and a statement that takes a minute takes the session with it for a minute. Cancelling one needs a second channel this protocol does not have, which is the C ABI's `zu_conn_interrupt` and the shell's `Ctrl-C`.

## The frames

`{"op":"query","q":"...","params":{...}}` runs a statement and answers with a result. `params` is optional.

`{"op":"prepare","q":"..."}` compiles a statement and answers `{"stmt":1,"params":["src"]}`: the handle to run it with and the parameter names it wants, in the order the statement first names them. `{"op":"execute","stmt":1,"params":{...}}` runs a prepared statement and answers with a result. `{"op":"close_stmt","stmt":1}` answers `{"closed":true}`, or `{"closed":false}` for a handle that was already closed or never existed, which makes closing twice safe rather than an error.

`{"op":"explain","q":"..."}` compiles and renders the plan and runs nothing, and answers `{"text":"..."}`. `{"op":"explain_analyze","q":"...","params":{...}}` runs the statement and answers with the same shape carrying what each operator actually did. The two are deliberately separate frames: `explain` is the one a caller can afford beside a latency it measured separately, and the only one that is safe to ask about a statement that writes.

`{"op":"hello"}` answers with the greeting. `{"op":"quit"}` answers `{"bye":true}` and ends the session; closing the pipe does the same thing without the courtesy.

## Results

```json
{"gqlstatus":"00000","columns":["n"],"rows":[[3]]}
```

`gqlstatus` is the completion condition: `00000` for a successful statement, `00001` for one that had no projection to return. A statement that raised something it survived grows a `notices` array of diagnostic records. Rows are JSON arrays in column order, and the values are the query engine's own JSON spelling: null, booleans, numbers, strings, arrays for lists, objects for records, an object naming a table and an offset for a node, and an object naming a table, a source and a destination for a rel. A float is written with its point kept, so `3.0` does not arrive as an integer.

## Failures

A statement that failed answers with `error` and a `failure` object:

```json
{"error":"unexpected end of query, expected ')'","failure":{"gqlstatus":"42001","condition":"syntax error or access rule violation, invalid syntax","severity":"exception","message":"unexpected end of query, expected ')'"}}
```

`gqlstatus` is the standard's condition code, `condition` is the standard's own text for it, `severity` is one of `success`, `no data`, `warning`, `informational`, `exception`, and `message` is zu's sentence, which names the position and keeps naming it, so printing the message alone is still a complete report.

The rest of what ISO 39075 subclause 23.2 asks a diagnostic record to carry is written beside those where the record carries it, and left out where it does not, because a field holding null for what the condition is about reads as a condition about nothing rather than as a record with no opinion. `subject` is the thing the statement named that the condition is about, spelled the way the statement spelled it, and `subject_kind` is one lower-case word out of `graph`, `schema`, `label`, `property`, `variable`, `type` and `function`, so a client asking whether this is about a label compares one string against one word instead of parsing a phrase; the two are written together or not at all. `graph` and `schema` are where the statement was running. `line`, `column` and `offset` are the place, counted the three ways, and `excerpt` is the whole line that place falls on, quoted at the moment the condition was raised, for the client that has the failure and no longer has the statement. A condition raised while the statement ran, a division by zero say, has no token to point at and so carries none of the four. Adding a member does not move the protocol version, so a client that reads only the code keeps working.

A failure the protocol raised, rather than the engine, has `error` and no `failure`:

```json
{"error":"unknown op \"sing\""}
```

A malformed frame, an unknown op and a `params` that is not an object are faults of this protocol and not conditions the standard defines, so they carry no code rather than a plausible one. That is the difference a client tests: `failure` is present exactly when the engine raised a condition. Neither kind ends the session. The loop reads the next line, and the caches, the prepared statements and the open file are all still there, which is the whole reason a harness keeps one process rather than spawning one per case.

## Parameters

`params` is an object from name to value, without the `$`. The five JSON scalars are the five obvious values. A JSON array is a list and a JSON object is a record, nested to whatever depth is written, which is the mapping the data model already implies: a record is named fields and an object is named members. A record's fields are sorted by name on the way in, so two clients that wrote the same fields in different orders sent the same value.

A temporal has no spelling here, because JSON has no date and a string that looks like one is a string. A statement that wants one takes a string parameter and calls the constructor for the type it means, `date($when)` or `duration($span)`, which says which calendar type was meant instead of leaving the wire to guess it from the characters.

The two references do have a spelling, because a graph and a binding table are values a statement can be handed and neither has a JSON shape of its own. Each is an object with one member whose name begins with a dollar sign, which no field of a record can, so nothing a client could otherwise write loses its meaning to this.

`{"$graph": "/social"}` is a graph reference, written the way a statement writes one. That is the path the graph is at, where the last segment names the graph and what stands in front of it names the schema, or one of the four words that name a graph without naming it, `CURRENT_GRAPH`, `CURRENT_PROPERTY_GRAPH`, `HOME_GRAPH` and `HOME_PROPERTY_GRAPH`. The words are what a client that does not know the paths of the engine it is talking to can write, and they mean here what they mean in a statement. A path that names no graph is `42002`, the same condition a `USE` of it raises, and it arrives as a failure with a code rather than a fault of the protocol, because naming a graph that is not there is a question the standard has an answer for.

`{"$table": {"columns": ["id", "name"], "rows": [[1, "a"]]}}` is a binding table, written out. Every row is as long as the column list and every cell is a parameter value in its own right, so a table may hold lists and records. What it cannot hold is an element: a node is an offset in a snapshot, and nothing a client is holding names one.

```
{"op":"query","q":"USE GRAPH $g MATCH (n) RETURN count(n) AS n","params":{"g":{"$graph":"CURRENT_PROPERTY_GRAPH"}}}
{"op":"query","q":"RETURN BINDING TABLE $t IS TYPED BINDING TABLE AS t","params":{"t":{"$table":{"columns":["id"],"rows":[[1],[2]]}}}}
```

## A session

```
$ zu shell social.zu1 --format jsonl
{"protocol":1,"zu":"0.0.1","c_abi":"0.5","format_version":1,"features":[],"file":"social.zu1"}
{"op":"prepare","q":"MATCH (p:person {id: $id})-[:follows]->(f) RETURN f.id AS id"}
{"stmt":1,"params":["id"]}
{"op":"execute","stmt":1,"params":{"id":3}}
{"gqlstatus":"00000","columns":["id"],"rows":[[7],[11]]}
{"op":"query","q":"MATCH (p:person) WHERE p.id IN $ids RETURN count(p) AS n","params":{"ids":[1,2,3]}}
{"gqlstatus":"00000","columns":["n"],"rows":[[3]]}
{"op":"close_stmt","stmt":1}
{"closed":true}
{"op":"quit"}
{"bye":true}
```

## What version 1 does not do

A result arrives whole, on one line, so a client that wants the first row of a large answer pays for all of them; streaming needs a reply that can be continued and that is a change of meaning rather than an addition. There is no way to interrupt a running statement over this pipe, for the reason given above. There is no binary or Arrow framing, so a column of a million values travels as a million JSON numbers. Each of those is a version 2 question, and none of them is a reason to hold version 1 back from the callers it already serves.
