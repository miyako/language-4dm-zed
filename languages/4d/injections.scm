; Begin SQL ... End SQL blocks reparse as SQL.
((sql_block body: (sql_content) @injection.content)
 (#set! injection.language sql))
