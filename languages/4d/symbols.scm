; Symbolication: class functions, constructors-by-params, and properties.

((function_declaration
   name: [(identifier) (multiword_name)] @name
   (parameter_list)? @arguments.target) @subtree
 (#set! role method)
 (#set! arguments.query "arguments.scm"))

((property_declaration name: (identifier) @name) @subtree
 (#set! role property))

((extends_clause super: (_) @name) @subtree
 (#set! role class))
