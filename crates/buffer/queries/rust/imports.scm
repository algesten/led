(use_declaration) @import

(use_declaration
  argument: (scoped_identifier
    path: (_) @namespace
    name: (_) @name))

(use_declaration
  argument: (use_as_clause
    path: (_) @namespace
    alias: (identifier) @alias))

(use_declaration
  argument: (scoped_use_list
    path: (_) @namespace
    list: (use_list) @list))

(use_declaration
  argument: (use_wildcard
    (_) @namespace) @wildcard)

(use_declaration
  argument: (identifier) @name)
