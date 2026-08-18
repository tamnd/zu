; Written by `cargo xtask grammar` from grammar/vocabulary.toml. Do not edit.
;
; The scopes are the ones every tree-sitter editor already has a colour for,
; so a theme nobody wrote for this language still paints it.

; keywords
[
  "USE"
  "OPTIONAL"
  "MATCH"
  "UNWIND"
  "WITH"
  "WHERE"
  "RETURN"
  "YIELD"
  "DISTINCT"
  "AS"
  "GROUP"
  "ORDER"
  "BY"
  "ASC"
  "ASCENDING"
  "DESC"
  "DESCENDING"
  "NULLS"
  "FIRST"
  "LAST"
  "SKIP"
  "LIMIT"
  "ALL"
  "ANY"
  "SHORTEST"
  "PATHS"
  "GROUPS"
  "KEEP"
  "WALK"
  "TRAIL"
  "ACYCLIC"
  "SIMPLE"
  "AND"
  "OR"
  "XOR"
  "NOT"
  "IS"
  "IN"
  "STARTS"
  "ENDS"
  "CONTAINS"
  "LIKE"
  "EXISTS"
  "TYPED"
  "CAST"
  "CREATE"
  "DROP"
  "SCHEMA"
  "GRAPH"
  "PROPERTY"
  "TYPE"
  "IF"
  "REPLACE"
  "COPY"
  "OF"
  "NODE"
  "EDGE"
  "RELATIONSHIP"
  "PROPERTIES"
  "NO"
  "CURRENT_GRAPH"
  "CURRENT_PROPERTY_GRAPH"
  "LIST"
  "ARRAY"
  "RECORD"
  "VALUE"
  "INSERT"
  "SET"
  "REMOVE"
  "DELETE"
  "DETACH"
  "NODETACH"
  "CALL"
] @keyword

; literals
[
  "NULL"
  "TRUE"
  "FALSE"
] @constant.builtin

; The rest is the shape of the tree rather than a list of words. A name is a
; type where a type stands and a variable where a value does, which is the
; whole reason an editor wants a grammar and not a word list.
(comment) @comment
(string) @string
(integer) @number
(float) @number
(parameter) @variable.parameter
(label) @type
(type_name) @type
(function_call name: (identifier) @function)
(call_clause name: (identifier) @function.builtin)
(path_constructor name: (identifier) @function.builtin)
(exists_block name: (identifier) @keyword)
(value_block name: (identifier) @keyword)
(variable) @variable
(property_access property: (identifier) @property)
(property_map key: (identifier) @property)
(projection_item alias: (identifier) @variable)
[
  "+"
  "-"
  "*"
  "/"
  "%"
  "="
  "<>"
  "<"
  "<="
  ">"
  ">="
  "&"
  "|"
  "!"
] @operator
[
  "("
  ")"
  "["
  "]"
  "{"
  "}"
] @punctuation.bracket
[
  ","
  "."
  ":"
  ";"
] @punctuation.delimiter
