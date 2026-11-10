; Validated for airblast-dev/tree-sitter-gdshader v0.4.0
; commit 68268631c8b6dc093985f1246b099f81b30ea7d1.
(function_definition
  block: (compound_statement) @function.inside) @function.around

(struct_definition
  fields: (struct_fields) @class.inside) @class.around
