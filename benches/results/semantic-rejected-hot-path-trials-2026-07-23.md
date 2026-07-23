# Rejected semantic hot-path trials (2026-07-23 to 2026-07-24)

These measurements preserve four implementation trials that did not provide
enough end-to-end evidence to ship. Lower paired delta is better. Every default
run used four alternating A/B pairs, six warmups, and 30 retained samples per
scenario. The lazy-line-table confirmation used eight pairs, 12 warmups, and 30
retained samples.

Every candidate commit is preserved by the pushed `benchmark/...` tag recorded
as `source.b_ref` in its manifest. The exact-snapshot trial also records a
pushed baseline tag as `source.a_ref`; the other trials use the preserved
`origin/main` commit recorded in their manifests.

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

## Collection-local capture role table

Candidate `0b8626b40950074f6d935a00bed7b0bd05ec2ecb` resolved capture roles
once per host or injection collection rather than once per matched capture.

Markdown typing delta improved by a median -9.67%, but
`rust_sparse_32k_minus/typing_delta` regressed in all four pairs with a +8.80%
median. Other primary scenarios changed sign across pairs. The mixed result is
not safe to ship as a general hot-path optimization.

Evidence: `semantic-capture-role-table-2026-07-23/`

## Collection-local pattern priority table

Candidate `5681ae5fff701b9f5570fe4a169aca76ebd8b47a` parsed query priorities
once per host or injection collection rather than once per match.

Only `rust_sparse_32k_exact/typing_delta` improved in all four pairs (-1.70%
median). Markdown typing delta and burst had +0.73% and +0.49% medians,
respectively, while Rust large delta was effectively unchanged (+0.15%).
Collection-local precomputation therefore adds fixed work without demonstrating a
general end-to-end benefit.

Evidence: `semantic-pattern-priority-table-2026-07-23/`

## Exact-snapshot cache before language preparation

Candidate `0dad66b49356483ae42a91bdde8632d219e7b3c9` moved the existing
full/delta exact-snapshot cache lookup before language loading and highlight
query preparation.

The change did not establish an end-to-end cache-hit win. Paired medians were
-0.69% for the Rust full cache hit, +3.97% for the Markdown injection full
cache hit, -2.88% for the Unicode full cache hit, and +0.72% for the Rust
no-op delta. Every target scenario changed sign across the four pairs. Primary
typing controls stayed near neutral (Rust +0.29%, Markdown -1.44%), so there
was no compensating throughput benefit. The earlier lookup is rejected because
it did not demonstrate a repeatable end-to-end improvement.

Evidence: `semantic-snapshot-cache-first-2026-07-24/`

## Consequence

Do not transplant these collection-local forms. These measurements do not rule
out a table shared across all host and injection collections in one request. If
capture roles or pattern properties are revisited, prefer constructing an
immutable query plan when a query or settings generation is loaded, then
measure the complete LSP path again. This keeps immutable work out of repeated
requests without weakening semantic-token freshness.

Also keep the exact-snapshot lookup after language preparation. Revisit that
ordering only if attribution shows language/query preparation has become
material on cache-hit requests.
