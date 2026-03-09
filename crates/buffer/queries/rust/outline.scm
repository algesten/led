; Annotations
(attribute_item) @annotation
(line_comment) @annotation

; Functions
(function_item
  (visibility_modifier)? @context
  "fn" @context
  name: (identifier) @name) @item

; Structs
(struct_item
  (visibility_modifier)? @context
  "struct" @context
  name: (type_identifier) @name
  body: (field_declaration_list
    "{" @open
    "}" @close)?) @item

; Enums
(enum_item
  (visibility_modifier)? @context
  "enum" @context
  name: (type_identifier) @name
  body: (enum_variant_list
    "{" @open
    "}" @close)) @item

; Enum variants
(enum_variant
  name: (identifier) @name) @item

; Impl blocks
(impl_item
  "impl" @context
  type: (type_identifier) @name
  body: (declaration_list
    "{" @open
    "}" @close)) @item

; Trait definitions
(trait_item
  (visibility_modifier)? @context
  "trait" @context
  name: (type_identifier) @name
  body: (declaration_list
    "{" @open
    "}" @close)) @item

; Modules
(mod_item
  (visibility_modifier)? @context
  "mod" @context
  name: (identifier) @name) @item

; Type aliases
(type_item
  (visibility_modifier)? @context
  "type" @context
  name: (type_identifier) @name) @item

; Constants and statics
(const_item
  (visibility_modifier)? @context
  "const" @context
  name: (identifier) @name) @item

(static_item
  (visibility_modifier)? @context
  "static" @context
  name: (identifier) @name) @item

; Macros
(macro_definition
  name: (identifier) @name) @item
