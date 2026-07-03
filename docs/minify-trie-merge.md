# Minify Trie Merge Design

## Background

`CssRuleList::minify` currently performs merging, deduplication, and AST mutation while it walks the rule list. The existing implementation already has some important optimizations. For example, `StyleRuleKey` uses `rules + index + precomputed hash` to avoid copying selectors and declarations, and a `HashMap` detects style rules that are fully shadowed by later rules.

There are still several issues with the current model:

- Merging and deduplication are spread through the traversal, and mutating rules can trigger repeated minification.
- Removed rules are represented as `CssRule::Ignored` tombstones, and the final AST is not compacted.
- Adjacent `@media`, `@supports`, and `@container` merges re-run minification on their child rule lists.
- Rules with the same semantic path, or a shared path prefix, are only optimized locally.

The goal is to introduce a side-effect-aware trie merge model. During minification, rules are recorded by their CSS semantic path. Shared prefixes can be merged when it is safe, and the AST is reconstructed afterward.

## Core Idea

A CSS rule path is a semantic context, not a source-text path. For example:

```css
@media (width > 600px) {
  @supports (display: grid) {
    .a { color: red }
  }
}
```

can be represented as:

```text
Root
  Media(width > 600px)
    Supports(display: grid)
      Style(.a)
```

A later rule with the same prefix can use the same trie branch:

```css
@media (width > 600px) {
  @supports (display: grid) {
    .b { color: blue }
  }
}
```

When the trie is flushed, the reconstructed AST can share the outer `@media` and `@supports` wrappers.

However, CSS cascade order has side effects. The trie cannot globally merge all matching paths. For example:

```css
@media (width > 600px) { .a { color: red } }
.a { color: blue }
@media (width > 600px) { .a { color: green } }
```

Moving the second `@media` into the first `@media` position would change cascade behavior. The trie must therefore be built per side-effect-safe chunk, and flushed whenever a rule cannot be crossed safely.

## Terminology

- Path segment: A CSS semantic context node, such as an `@media` query, `@supports` condition, named `@layer`, `@container` name/condition, `@scope` condition, or nested selector.
- Path key: The sequence of path segments from the root to the current rule.
- Barrier: A semantic boundary that prevents rules from being merged across it.
- Chunk: A range of rules between two barriers. Trie merging only happens within a chunk.
- Leaf value: A rule reference or intermediate merge state stored at a trie leaf. It should reference original rule indexes where possible to avoid copying AST nodes.

## Data Structure Sketch

```rust
struct RuleTrie<'i, T> {
  root: RuleTrieNode<'i, T>,
}

struct RuleTrieNode<'i, T> {
  children: IndexMap<PathSegment<'i>, RuleTrieNode<'i, T>, PathBuildHasher>,
  meta: Option<PathNodeMeta>,
  entries: Vec<RuleEntry<'i, T>>,
}

enum PathSegment<'i> {
  Media(MediaList<'i>),
  Supports(SupportsCondition<'i>),
  Container {
    name: Option<ContainerName<'i>>,
    condition: Option<ContainerCondition<'i>>,
  },
  Layer(LayerName<'i>),
  Scope(ScopePathKey<'i>),
  StartingStyle,
  Nesting(SelectorPathKey<'i>),
}

struct PathNodeMeta {
  first_order: usize,
  loc: Location,
}

struct RuleEntry<'i, T> {
  order: usize,
  source_index: u32,
  kind: RuleEntryKind<'i, T>,
}

enum RuleEntryKind<'i, T> {
  Style(StyleRuleRef),
  Rule(CssRuleRef),
  Owned(CssRule<'i, T>),
}
```

`PathSegment` should reuse existing AST fields that already represent CSS semantics, such as `MediaList`, `SupportsCondition`, `ContainerName`, `ContainerCondition`, and `LayerName`. It should not use complete wrapper rules as keys. `MediaRule`, `SupportsRule`, `ContainerRule`, and `LayerBlockRule` also contain `rules` and `loc`, and those are not part of path identity.

`IndexMap` is still needed. Its job is not to decide shadowing or replacement. It records the first-seen order of path segments at the same trie level. Later occurrences of the same segment reuse the existing node, and AST reconstruction emits wrappers in first-seen order.

The main crate already depends on `indexmap`. The hashing strategy has two options:

- If the reused AST semantic fields can implement `Hash + Eq`, use `PathSegment` directly as a structured key, optionally with `rustc-hash` and `FxHasher`.
- If some semantic fields cannot provide stable `Hash + Eq`, introduce focused path key/newtype wrappers that store normalized semantic fields or a precomputed hash.

The current `StyleRuleKey` approach remains useful. It can be used as a value or inside a leaf bucket to avoid copying selectors and declarations. The trie key only describes the semantic path and does not perform style rule content deduplication itself.

## Path Design

A path segment must contain only the data that affects the semantic context of nested rules.

Initial support should include:

- `@media` query: reuse `MediaList<'i>`.
- `@supports` condition: reuse `SupportsCondition<'i>`.
- `@container` name and condition: reuse `Option<ContainerName<'i>>` and `Option<ContainerCondition<'i>>`.
- Named `@layer`: reuse `LayerName<'i>`.
- Nested style selector path: reuse selector semantic fields, or introduce a dedicated nested selector path key.

Later support can include:

- `@scope`.
- `@starting-style`.
- Other at-rules that can be proven safe.

Do not include these at first:

- Unknown at-rules.
- Anonymous layers.
- Source-order-sensitive rules, or rules whose commutativity has not been proven.

These should initially be treated as barriers.

The path key must not include:

- `rules`: child rules are represented by trie children and leaf entries.
- `loc`: source map information belongs in `PathNodeMeta` and does not participate in `Eq` or `Hash`.
- Temporary minification state, such as handler context or target stack state.

## Barrier Rules

When a barrier is encountered:

1. Flush the current trie, reconstructing its rules in order.
2. Append the barrier rule directly.
3. Start a new trie chunk.

The initial barrier set should be conservative:

- Ordinary top-level style rules when they may compete with wrapped style rules before or after them.
- `@import`, because import and layer declaration ordering is special.
- `@namespace`.
- Unknown at-rules.
- Anonymous `@layer`.
- CSS modules source boundaries when cross-source merging would change class/hash semantics.
- Any custom at-rule that is not yet classified.

Named `@layer` is a special case. The current implementation already allows non-adjacent named layer blocks to merge because layer order is determined by first declaration. The trie implementation can preserve this exception, but it must track layer declaration order separately.

## Merge Strategy

Trie merging has two layers:

1. Path-level merge: identical path segments share ancestor nodes.
2. Leaf-level merge: rules under the same leaf continue to use the existing style rule merge and shadowing logic.

Leaf-level merging can reuse current behavior:

- Same adjacent selector: merge declarations.
- Same declarations: merge selectors.
- Same selector plus same property id sequence: later rule shadows the earlier entry.

Details to fix or make explicit:

- The duplicate key should include the `!important` bit. A normal declaration and an important declaration cannot be compared by property id alone.
- CSS modules must still prevent cross-file style shadowing when `source_index` differs.
- Rules with nested rules should not enter leaf-level style shadowing unless the nested path is also identical and cascade-safe.

## AST Reconstruction

When the trie is flushed, reconstruct rules by doing DFS in the first-seen order recorded by `IndexMap`:

```text
emit(node):
  emit live entries attached to node
  for child in insertion_order(children):
    child_rules = emit(child)
    wrap child_rules by child PathSegment
```

Implementation notes:

- The output should not contain `CssRule::Ignored` tombstones.
- Empty child nodes should not generate wrapper rules.
- Repeated occurrences of the same path segment reuse the first trie node; later content is inserted into that node's leaf bucket or children.
- Shadowing is decided by the leaf-level bucket. A later equivalent style entry may replace an earlier entry. `IndexMap` only controls wrapper order.
- Named layer statements must preserve first declaration order.
- Source map location should come from `PathNodeMeta.loc`, usually the location of the first wrapper that created the path segment.

## Test Plan

Tests should cover three groups.

Behavior-preserving tests:

- Adjacent `@media`, `@supports`, and `@container` rules still merge.
- Duplicate style rules are still shadowed by later rules.
- Non-adjacent named `@layer` merging remains unchanged.
- CSS modules do not merge style shadowing across source files.

Barrier tests:

- Wrapped rules do not cross ordinary style rules in a way that changes cascade.
- Layer blocks do not merge across `@import`.
- Unknown at-rules are barriers.
- Anonymous layers are barriers.

New optimization tests:

- Matching `@media -> @supports` prefixes within the same chunk share wrappers.
- Multi-level common prefixes reconstruct in first-seen path segment order.
- Flushing the trie does not output `CssRule::Ignored`.

## Risks

- The largest risk is changing cascade order. Barriers should be conservative at first and relaxed only with targeted tests.
- Path key `Eq`/`Hash` must match CSS semantics. When reusing AST semantic fields, confirm they are not mutated after insertion into the trie.
- Wrapper location affects source maps and needs a clear policy.
- Recursively expanding nested rules may interact with the current ordering of `PropertyHandlerContext` additional rules. Preserve existing order first, then move logic gradually.
