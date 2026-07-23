# Rejected semantic hot-path trials (2026-07-23)

These measurements preserve three implementation trials that did not provide
enough end-to-end evidence to ship. Lower paired delta is better. Every default
run used four alternating A/B pairs, six warmups, and 30 retained samples per
scenario. The lazy-line-table confirmation used eight pairs, 12 warmups, and 30
retained samples.

## Lazy content line table

Candidate `5e2375c08b58dafae961a24b92f112bcb4f84a03` deferred collecting
`text.lines()` until the first multiline capture.

The default run was inconclusive. The confirmation still crossed zero in every
scenario: Rust sparse medians ranged from -4.01% to +0.95%, Rust large delta was
+1.41%, and Markdown delta was +5.57%. The change is rejected because it did
not produce a repeatable sparse-document win and retained a Markdown delta
regression signal.

Evidence:

- `semantic-lazy-content-lines-2026-07-23/`
- `semantic-lazy-content-lines-confirmation-2026-07-23/`

## Request-local capture role table

Candidate `0b8626b40950074f6d935a00bed7b0bd05ec2ecb` resolved capture roles
once per request rather than once per matched capture.

Markdown typing delta improved by a median -9.67%, but
`rust_sparse_32k_minus/typing_delta` regressed in all four pairs with a +8.80%
median. Other primary scenarios changed sign across pairs. The mixed result is
not safe to ship as a general hot-path optimization.

Evidence: `semantic-capture-role-table-2026-07-23/`

## Request-local pattern priority table

Candidate `5681ae5fff701b9f5570fe4a169aca76ebd8b47a` parsed query priorities
once per request rather than once per match.

Only `rust_sparse_32k_exact/typing_delta` improved in all four pairs (-1.70%
median). Markdown typing delta and burst had +0.73% and +0.49% medians,
respectively, while Rust large delta was effectively unchanged (+0.15%).
Request-local precomputation therefore adds fixed work without demonstrating a
general end-to-end benefit.

Evidence: `semantic-pattern-priority-table-2026-07-23/`

## Consequence

Do not transplant these request-local forms. If capture roles or pattern
properties are revisited, construct an immutable query plan when a query or
settings generation is loaded, then measure the complete LSP path again. This
keeps immutable work out of repeated requests without weakening semantic-token
freshness.
