# Trie Minify Design

## Goal

The minifier should build a semantic rule trie first, and only materialize the
final AST after equivalent rule paths have been merged.

The important shift is:

- at-rules and selector rules create trie nodes;
- declaration lines do not create trie nodes;
- declarations are stored in an arena and referenced by index;
- declarations attached to the same trie node are merged with last-write-wins
  semantics where CSS allows it;
- nested rules continue insertion from the current trie node, not from the root.

This avoids the current failure mode where small nested rule lists are minified
first, later merged, and then minified again as a larger block.

## Target Shape

For input like:

```css
@media (min-width: 900px) {
  .card {
    color: red;

    @supports (display: grid) {
      display: grid;
    }
  }
}

@media (width >= 900px) {
  .card {
    color: blue;
  }
}
```

the trie should look like:

```text
root
  @media (width >= 900px)
    .card
      declarations -> decl_set_0
      @supports (display: grid)
        declarations -> decl_set_1
```

`color: red` and `color: blue` land in the same declaration set for the same
semantic path. The later `color: blue` declaration replaces the earlier one.

## Core Data Model

```rust
struct RuleTrie<'i, T> {
  nodes: Vec<TrieNode<'i, T>>,
  declarations: DeclarationArena<'i>,
}

struct TrieNode<'i, T> {
  key: Option<RuleKey<'i>>,
  loc: Location,
  children: IndexMap<RuleKey<'i>, NodeId>,
  items: Vec<NodeItem>,
}

enum NodeItem {
  DeclarationSet(DeclSetId),
  Child(NodeId),
  Barrier(CssRuleId),
}

enum RuleKey<'i> {
  AtRule(AtRuleKey<'i>),
  Selector(SelectorKey<'i>),
}

enum AtRuleKey<'i> {
  Media(MediaList<'i>),
  Supports(SupportsCondition<'i>),
  Container {
    name: Option<ContainerName<'i>>,
    condition: Option<ContainerCondition<'i>>,
  },
  Scope {
    start: Option<SelectorList<'i>>,
    end: Option<SelectorList<'i>>,
  },
  StartingStyle,
  MozDocument,
  Layer(LayerName<'i>),
}

struct SelectorKey<'i> {
  selectors: SelectorList<'i>,
  vendor_prefix: VendorPrefix,
  source_index: u32,
}
```

`IndexMap` is used for child lookup because output order must follow first
insertion order. Reusing an existing child node must not move it later in the
output.

## Declaration Storage

Declarations are payload, not trie structure.

```rust
struct DeclarationArena<'i> {
  entries: Vec<DeclarationEntry<'i>>,
}

struct DeclarationSet {
  normal: IndexMap<DeclarationKey, DeclId>,
  important: IndexMap<DeclarationKey, DeclId>,
  order: usize,
}

struct DeclarationEntry<'i> {
  declaration: Property<'i>,
  important: bool,
  source_order: usize,
}
```

When a declaration line is inserted:

1. Push the declaration into `DeclarationArena`.
2. Compute a `DeclarationKey`.
3. Insert the arena index into the current node's declaration set.
4. If the key already exists, replace the old index with the new one.

This gives cheap last-write-wins behavior for exact declaration keys.

Important declarations must live in a separate map from normal declarations. A
normal declaration and an important declaration are not interchangeable.

## Declaration Keys

The first implementation should be conservative:

- exact longhand/property identity can use direct replacement;
- custom properties can use their custom property name;
- vendor-prefix-sensitive declarations must include prefix state in the key;
- shorthand/longhand groups should initially fall back to existing
  `DeclarationBlock::minify` behavior unless the conflict relation is proven.

In other words, the trie should first aggregate declarations by semantic rule
path. Exact duplicate declaration replacement is safe and cheap. More advanced
shorthand/longhand compaction can happen after materialization or as a later
declaration-map improvement.

Examples that require care:

```css
margin-left: 1px;
margin: 2px;
margin-left: 3px;
```

and:

```css
border-color: red;
border-left-color: blue;
```

These cannot be treated as simple independent exact keys without understanding
the shorthand group.

## Ordered Node Items

The trie still needs an ordered item stream inside each node.

Declarations can be merged inside a declaration set, but declaration sets must
stay in the correct relative position with nested child rules and barriers.

For example:

```css
.a {
  @media (min-width: 900px) {
    color: red;
  }

  color: blue;
}
```

must not be flattened as if all declarations appeared before or after the nested
rule. The node should store an ordered stream like:

```text
.a
  child @media(...)
  declaration_set
```

When a new declaration arrives at the same current output position, it is merged
into the active declaration set. When a nested rule or barrier is inserted, a
later declaration may need a new declaration set item.

## Insertion Algorithm

Insertion walks the input AST once in source order.

```text
insert_rule(current_node, rule):
  if rule is supported at-rule:
    key = normalize_at_rule_key(rule)
    child = get_or_insert_child(current_node, key)
    insert_rule_list(child, rule.children)

  else if rule is style selector:
    key = normalize_selector_key(rule)
    child = get_or_insert_child(current_node, key)
    insert_declarations(child, rule.declarations)
    insert_rule_list(child, rule.nested_rules)

  else if rule is declaration:
    decl_id = arena.push(rule)
    decl_set = get_or_create_active_declaration_set(current_node)
    decl_set.insert(declaration_key(rule), decl_id)

  else:
    flush current chunk or insert as barrier
```

The important part is that declaration lines never create trie nodes. They attach
to the current prefix node.

## Prefix Cursor Optimization

Nested rule insertion should keep a cursor to the current trie node.

Without this, every nested declaration would repeatedly walk:

```text
root -> @media -> .selector -> @supports -> ...
```

Instead, insertion should pass `NodeId` down recursively:

```text
insert_rule_list(node_id, rules):
  for rule in rules:
    insert_rule(node_id, rule)
```

When many adjacent rules share a prefix, cache the last successful child lookup:

```rust
struct InsertCursor {
  node: NodeId,
  last_child_key: Option<RuleKey>,
  last_child_node: Option<NodeId>,
}
```

This is especially useful for generated CSS and nested output where many rules
repeat the same prefix.

## Normalization Before Trie Keys

Trie keys must be semantic keys, not raw source text.

Examples:

```css
@media (min-width: 900px) {}
@media (width >= 900px) {}
```

These should produce the same `AtRuleKey::Media` when targets allow the same
canonical representation.

Normalization should be limited to the key itself. It should not recursively
minify child rule lists before insertion into the trie. That is the repeated
work this design is meant to remove.

## Barriers And Chunks

The trie cannot merge across arbitrary source-order boundaries.

This is unsafe:

```css
@media (min-width: 900px) { .a { color: red } }
.a { color: blue }
@media (min-width: 900px) { .a { color: green } }
```

If both `@media` blocks were merged before `.a { color: blue }`, cascade order
would change.

Therefore the trie operates within a side-effect-safe chunk. A barrier flushes
the current chunk and starts a new one.

Initial barriers should be conservative:

- ordinary rules that would change cascade if crossed;
- `@import`;
- `@namespace`;
- unknown at-rules;
- anonymous layers;
- CSS modules source boundaries when cross-source merging would change behavior;
- any rule whose commutativity has not been proven.

Named layers are a special case because their cascade order is determined by
layer declaration order rather than physical block adjacency. They can keep a
dedicated layer-order path, but should be handled explicitly.

## Materialization

After insertion, the trie is materialized back into `CssRuleList`.

```text
emit(node):
  for item in node.items:
    if item is DeclarationSet:
      emit declaration block from arena indexes

    if item is Child:
      child_rules = emit(child)
      wrap child_rules according to child.key

    if item is Barrier:
      emit barrier rule
```

For a selector node, materialization creates a `StyleRule` with:

- declarations from declaration-set items;
- nested rules from child at-rule/selector items;
- source location from the first rule that created the node.

For an at-rule node, materialization creates the corresponding wrapper rule with
the emitted child rules.

Empty declaration sets and empty wrapper nodes should not produce output.

## Interaction With Existing Minify Logic

The trie should replace rule-list structural merging, but not all declaration
semantics in the first step.

Keep using existing declaration minification for:

- shorthand/longhand merging;
- logical property fallback generation;
- vendor-prefix handling;
- custom property edge cases;
- target-dependent declaration transforms.

The trie should make sure those declaration transforms run once on the final
declaration block for a semantic rule path, not once per fragment and again after
merge.

## Difference From PR 1263

PR 1263 is a narrow pre-pass:

- only adjacent `@media`, `@supports`, and `@container`;
- only same top-level wrapper type and exactly equal pre-minify key;
- no selector nodes;
- no declaration arena;
- no shared nested prefix trie;
- no support for normalized-equivalent keys after key canonicalization.

The target design here is broader:

- at-rules and selector rules both participate in the trie;
- declarations are payload stored by arena index;
- nested rules continue from the current node;
- common prefixes can be reused at any supported depth within a safe chunk;
- key normalization happens before lookup, without minifying child lists first.

## Test Plan

Behavior tests:

- adjacent `@media`, `@supports`, and `@container` still merge;
- selector-equivalent rules merge declarations under the same node;
- duplicate exact declarations keep the later declaration;
- `!important` and normal declarations do not overwrite each other;
- CSS modules do not merge across source boundaries when that would change
  semantics;
- named layer behavior remains unchanged.

Barrier tests:

- repeated `@media` separated by an ordinary style rule does not merge across the
  style rule;
- unknown at-rules flush the active chunk;
- `@import` and `@namespace` preserve source-order constraints;
- anonymous layers do not merge through unrelated rules.

Optimization tests:

- `@media -> @supports -> selector` repeated paths share one prefix;
- nested syntax and explicit wrapper syntax materialize to the same optimized
  AST when they are semantically equivalent;
- normalized media/container keys reuse the same trie node;
- deep repeated prefixes avoid repeated root lookup through cursor reuse;
- pure declaration runs under a selector create declaration arena entries, not
  trie nodes.

Benchmark cases:

- long adjacent wrapper runs;
- alternating `ABCABC` nested wrapper patterns;
- deep repeated prefixes with different suffixes;
- mixed nested syntax and explicit at-rule syntax;
- large declaration blocks with repeated exact properties.

## Implementation Order

1. Introduce `RuleTrie`, `TrieNode`, `RuleKey`, and declaration arena types.
2. Add insertion for at-rule nodes only, preserving current behavior.
3. Add selector nodes.
4. Move declaration blocks into node-local declaration sets.
5. Materialize final AST from the trie.
6. Run existing declaration minification only on materialized final blocks.
7. Add exact declaration overwrite via `IndexMap`.
8. Add prefix cursor caching for nested insertion.
9. Expand declaration-key handling for shorthand groups only after targeted tests.

This sequence keeps the risky semantic changes small. The first useful win is
eliminating repeated minification of child rule lists. The later wins come from
declaration payload deduplication and prefix cursor reuse.
