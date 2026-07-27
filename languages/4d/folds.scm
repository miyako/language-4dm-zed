; Structure folding for 4D. Closing keywords (End if, End case...) are the
; named `keyword` nodes the grammar aliases its regex tokens to — anchors and
; captures on named nodes are well-defined, unlike anonymous-token wildcards.
; @start on the header's last structural node plus scope.byLine keeps the
; header line visible.

((if_statement condition: (_) @start (keyword) @end)
 (#set! role block) (#set! scope.byLine))

((while_statement condition: (_) @start (keyword) @end)
 (#set! role block) (#set! scope.byLine))

((repeat_statement "Repeat" @start "Until" @end)
 (#set! role block) (#set! scope.byLine))

((for_statement ")" @start (keyword) @end)
 (#set! role block) (#set! scope.byLine))

((for_each_statement ")" @start (keyword) @end .)
 (#set! role block) (#set! scope.byLine))

((case_statement . (keyword) @start (keyword) @end .)
 (#set! role block) (#set! scope.byLine))

; Each case branch folds from its condition to after its last statement.
; `_ @end.after .` is safe here: anchors skip anonymous nodes, and the last
; NAMED child (the final body statement) is exactly the intended fold end.
((case_branch condition: (_) @start _ @end.after .)
 (#set! role block) (#set! scope.byLine))

((try_statement "Try" @start (keyword) @end)
 (#set! role block) (#set! scope.byLine))

((use_statement object: (_) @start (keyword) @end)
 (#set! role block) (#set! scope.byLine))

((sql_block . (keyword) @start (keyword) @end .)
 (#set! role block) (#set! scope.byLine))

; Function bodies: fold after the parameter list; the return clause stays on
; the header line, so byLine leaves the whole signature visible. Last NAMED
; child = last body statement, so @end.after lands correctly.
((function_declaration (parameter_list) @start _ @end.after .)
 (#set! role function) (#set! scope.byLine))
