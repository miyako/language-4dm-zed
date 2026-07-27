; 4D syntax highlighting for Nova — captures are Nova THEME SELECTORS.

; ---- Comments & header ------------------------------------------------------
(line_comment) @comment
(block_comment) @comment
(attributes_header) @processing

; ---- Literals ---------------------------------------------------------------
(string) @string
(number) @value.number
(time_literal) @value.number
(date_literal) @value.number

; ---- Builtins (tokenized :Cnnn / :Knnn:n and scanner-recognized names) ------
(command) @identifier.core.function
(command_name) @identifier.core.function
(constant) @identifier.constant
(constant_name) @identifier.constant
(system_variable) @identifier.core.global

; Untokenized multi-word plugin/component commands: VP SET CELL STYLE(...)
(multiword_name) @identifier.function

; ---- Variables --------------------------------------------------------------
(local_variable) @identifier.variable
(interprocess_variable) @identifier.global
(parameter_indirection) @identifier.argument

; ---- Database references ----------------------------------------------------
(field_reference) @identifier.property
(table_reference) @identifier.type

; ---- Members & calls --------------------------------------------------------
(postfix_expression member: (identifier) @identifier.property)
(postfix_expression member: (local_variable) @identifier.property)

; ---- Declarations -----------------------------------------------------------
(modifier) @keyword
(function_declaration accessor: _ @keyword)
(function_declaration name: (identifier) @definition.method)
(var_declaration name: (identifier) @identifier.variable)
(property_declaration name: (identifier) @definition.property)
(parameter name: (local_variable) @identifier.argument)
(var_declaration type: (identifier) @identifier.type)
(property_declaration type: (identifier) @identifier.type)
(parameter type: (identifier) @identifier.type)
(function_declaration return_type: (identifier) @identifier.type)

; ---- Keywords that are plain string tokens ----------------------------------
[
  "Use" "If" "Else" "While" "Repeat" "Until" "For" "Try" "Catch"
  "var" "property" "Function" "function"
  "return" "break" "continue" "throw" "defer"
  "#DECLARE"
] @keyword

; ---- Keywords that are regex tokens (End if, Case of, For each, ...) -------
; These are aliased to a named `keyword` node in the grammar precisely so
; queries can target them: tree-sitter anchors ignore anonymous nodes, so the
; earlier anchored-wildcard approach (`_ @keyword .`) bound to the last NAMED
; child instead — painting conditions and body statements, never the closer.
(keyword) @keyword

; ---- Operators & punctuation ------------------------------------------------
[
  ":=" "+=" "-=" "*=" "/="
  "=" "#" "<" ">" "<=" ">="
  "+" "-" "*" "/" "%" "\\" "^"
  "&" "|" "&&" "||" "^|" "<<" ">>"
  "??" "?+" "?-" "->" "?" "..."
] @operator

["(" ")" "[" "]" "{" "}"] @bracket
(char_ref_open) @bracket
(char_ref_close) @bracket
