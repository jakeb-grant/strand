; Indentation for .strand (nvim-treesitter's capture names): one level
; inside every block and every bracketed list.

[
  (tree_block)
  (stmt_block)
  (token_block)
  (field_block)
  (service_block)
  (keyframes_block)
  (variant_list)
  (match_arms)
  (parameters)
  (arguments)
  (array)
] @indent.begin

[ "}" "]" ")" ] @indent.end @indent.branch

(comment) @indent.auto
