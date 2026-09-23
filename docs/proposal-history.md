# Proposal modification history

Proposal `updated` values use the prepared repository's path history, including
its committed dirty materialization and sibling merge. An uncommitted edit does
not itself create a history date. A path without history uses the prepared file's
filesystem modification time at the original processing point. Section indexes,
already-generated pages, and ignored missing live paths do not request history.

History is scoped to a single Markdown preprocessing call. Small requests (fewer
than 128 admitted paths) use the direct `git log -1 --pretty=format:%ct -- <name>`
query. Large requests share one history walk and tree comparisons. The threshold
avoids imposing whole-history startup cost on small live edits. There is no
persistent cache, invalidation protocol, CLI switch, or alternate timestamp policy.

The batched walk must preserve **per-file** history simplification. A merge can
match one parent for the whole subtree but another, earlier parent for one file.
The batch asks Git for topological full history with parent rewriting, retaining
all merges. Ordinary commits unchanged for the common requested subtree may be
elided because every requested path is equal across those edges. For each retained
commit/parent pair, one tree diff identifies paths with changed blob IDs or modes.
Each unresolved path follows its **first equal parent**, or takes the current
committer timestamp if different from all parents. A root compares with absence.
This is not a greatest-timestamp search, and does not use a name-only first-hit map.
Git supplies effective parents and trees, including shallow and replacement history.

Large batches check Git's effective `log.follow` and `log.showSignature` settings
once. Enabled settings retain the exact direct query so Git still owns those
semantics; the batch does not implement rename-following or signature verification.
Normal full batches run one configuration query and one history command.

Regression tests compare both the forced batch and the cost-selected execution to
the original Git query, including branch/merge graphs, backdated commits, changes
of mode, deletion/re-addition, flat/bundled renames, shallow boundaries, replacement
history, new invocation HEADs, filesystem fallback, and lazy non-proposal inputs.
Opt-in test-only probes support identical baseline/candidate Markdown processing
and real-corpus before/after timing; no measurement entry point is compiled into
the production executable. Full benchmark/equivalence receipts are maintained in
the coordination workspace's Phase 7 evidence, including rejected slower attempts.
