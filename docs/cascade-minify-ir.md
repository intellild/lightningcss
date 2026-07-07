# Cascade Minify IR

## Goal

This document describes an intermediate representation for declaration-level
minification across nested rules.

The trie minifier can merge equivalent rule paths and avoid repeated work, but
some optimizations require looking at the expanded cascade order rather than a
single declaration block. For example:

```css
.a {
  color: red;

  @media (min-width: 900px) {
    color: blue;
  }

  color: green;
}
```

The nested rule is a cascade participant at its source position. The CSS above
is equivalent to:

```css
.a { color: red; }

@media (min-width: 900px) {
  .a { color: blue; }
}

.a { color: green; }
```

`color: green` dominates both earlier `color` declarations, so both `red` and
`blue` can be removed. This cannot be proven from a single local
`DeclarationBlock`; it needs a source-ordered view of declarations across nested
rules.

The IR should make this kind of optimization possible while remaining
conservative whenever CSS cascade semantics are not fully modeled.

## Core Model

Nested rules are not isolated subtrees for cascade purposes. They are ordered
items in the parent rule body. A rule body is therefore modeled as an ordered
stream:

```text
declaration run
child rule
declaration run
child rule
barrier
...
```

Each declaration in that stream gets a stable id. A later declaration may delete
an earlier declaration only if it is proven to dominate it for every environment
where the earlier declaration applies.

The IR is about cascade analysis, not only structural sharing. It may be built
from the AST, from the rule trie, or during trie insertion. The important part is
that every declaration receives a stable entry with enough context to compare it
against declarations before and after nested children.

## Data Model

```rust
type DeclId = u32;
type ItemId = u32;

struct CascadeIr<'i, R> {
  declarations: Vec<DeclEntry<'i>>,
  items: Vec<CascadeItem>,
  chunks: Vec<CascadeChunk>,
  _phantom: PhantomData<R>,
}

struct CascadeChunk {
  items: Range<usize>,
}

enum CascadeItem {
  Declaration(DeclId),
  RuleStart(RuleFrameId),
  RuleEnd(RuleFrameId),
  Barrier(BarrierKind),
}

struct DeclEntry<'i> {
  property: Property<'i>,
  property_key: PropertyConflictKey<'i>,
  important: bool,
  source_order: u32,
  context: CascadeContext<'i>,
  origin: DeclOrigin,
  dead: bool,
}

struct DeclOrigin {
  item_id: ItemId,
  // Points back to the AST/trie declaration run so materialization can remove
  // the declaration without rebuilding unrelated rules.
  slot: DeclarationSlot,
}
```

The declaration arena is not required for a trie implementation, but it is useful
for this pass. It gives every declaration a stable id that can be referenced from
multiple structures, marked dead, and removed during materialization.

## Cascade Context

Each declaration must carry the context that determines its cascade rank and
applicability.

```rust
struct CascadeContext<'i> {
  selectors: ExpandedSelectorList<'i>,
  condition: ConditionStack<'i>,
  layer: LayerContext<'i>,
  scope: ScopeContext<'i>,
  source_index: u32,
}

struct ExpandedSelectorList<'i> {
  selectors: SelectorList<'i>,
  specificities: SmallVec<[Specificity; 1]>,
}

struct ConditionStack<'i> {
  media: SmallVec<[MediaList<'i>; 2]>,
  supports: SmallVec<[SupportsCondition<'i>; 2]>,
  container: SmallVec<[ContainerConditionKey<'i>; 2]>,
  starting_style_depth: u8,
}
```

The context must be based on expanded nested selectors. For example:

```css
.a {
  & { color: blue; }
}
```

and:

```css
.a {
  color: blue;
}
```

target the same selector. But:

```css
.a {
  &:hover { color: blue; }
  color: green;
}
```

cannot be simplified by exact-selector logic because `&:hover` has a different
selector and higher specificity.

## Rule Frames

When walking nested rules, the builder pushes and pops rule frames:

```rust
enum RuleFrame<'i> {
  Style {
    selectors: ExpandedSelectorList<'i>,
  },
  Media {
    query: MediaList<'i>,
  },
  Supports {
    condition: SupportsCondition<'i>,
  },
  Container {
    name: Option<ContainerName<'i>>,
    condition: Option<ContainerCondition<'i>>,
  },
  Layer {
    name: Option<LayerName<'i>>,
  },
  Scope {
    start: Option<SelectorList<'i>>,
    end: Option<SelectorList<'i>>,
  },
  StartingStyle,
}
```

Only frames whose semantics are modeled should participate in dominance
analysis. Unknown or unmodeled frames should become barriers.

## Dominance

A later declaration `B` dominates an earlier declaration `A` when all of these
are true:

- `B` appears later in source order;
- `B` applies whenever `A` applies;
- `B` targets all elements targeted by `A`;
- `B` has cascade priority greater than or equal to `A`;
- `B` writes the same property or a property that fully covers `A`;
- no unmodeled barrier sits between `A` and `B`.

For a first implementation, use a deliberately small safe subset:

- exact same expanded selector list;
- exact same specificity;
- exact same `!important` state;
- same source index when CSS modules are enabled;
- same layer context, or a computed layer rank known to be no weaker;
- exact same scope context, or no scope context on either declaration;
- later condition stack is known to cover the earlier condition stack;
- exact same longhand property key;
- no custom properties;
- no shorthand/longhand conflict handling;
- no target-generated fallback handling.

This first subset safely removes declarations like:

```css
.a {
  @media (min-width: 900px) {
    & { color: blue; }
  }

  color: green;
}
```

but keeps declarations like:

```css
.a {
  @media (min-width: 900px) {
    &:hover { color: blue; }
  }

  color: green;
}
```

## Condition Coverage

To delete `A` using later declaration `B`, `B` must apply in every environment
where `A` applies.

In logical terms:

```text
A.condition => B.condition
```

Examples:

```css
.a {
  @media (min-width: 900px) {
    color: blue;
  }

  color: green;
}
```

`green` is unconditional, so it covers the media-conditioned `blue`.

```css
.a {
  @media (min-width: 900px) {
    color: blue;
  }

  @media (min-width: 1200px) {
    color: green;
  }
}
```

`green` does not cover `blue`; between 900px and 1199px, `blue` can still win.

The first implementation should only prove simple coverage:

- identical condition stacks;
- later condition stack is a subset of the earlier stack by exact normalized
  keys;
- unconditional later declaration covers conditional earlier declaration.

More advanced implication, such as `(width >= 1200px) => (width >= 900px)`, can
be added later.

## Cascade Priority

Source order only decides between declarations after earlier cascade dimensions
are equal or no weaker for the later declaration.

The dominance check must account for:

- importance;
- cascade layer priority;
- selector specificity;
- scoping proximity;
- source order.

The implementation should not compare raw layer names directly. It should derive
a layer rank for the declaration's importance mode, because normal and important
declarations use different layer precedence rules.

The first implementation can avoid most risk by requiring identical layer and
scope context.

## Barriers

A barrier is any item that prevents dominance from being proven across it.

Initial barriers should include:

- unknown at-rules;
- custom at-rules whose cascade behavior is not modeled;
- anonymous layer blocks unless layer order is explicitly represented;
- scope boundaries unless scope proximity is represented;
- CSS modules source boundaries when cross-source merging would change behavior;
- any declaration transform that emits additional fallback rules outside the
  current modeled context;
- shorthand/longhand or custom property interactions that are not represented in
  the conflict key.

Barriers do not necessarily mean CSS itself has an isolation boundary. They mean
the optimizer does not have enough information to prove a deletion safely.

## Reverse Dominance Pass

The basic pass scans each safe chunk from back to front.

```text
later = DominanceMap::new()

for item in chunk.items.rev():
  if item is declaration:
    if later contains a declaration that dominates item:
      mark item dead
    else:
      insert item into later

  if item is barrier:
    later.clear()
```

If the IR keeps nested structure instead of a flat stream, the same logic can be
implemented recursively:

```text
scan_node(node, later):
  for item in node.items.rev():
    if item is declaration:
      test against later
      insert if live

    if item is child:
      scan_node(child, later)

    if item is barrier:
      later.clear()
```

The important detail is that child declarations are scanned in the same cascade
order as their expanded CSS. A child rule can override declarations before it,
and declarations after the child can override declarations inside it.

## Dominance Map

The first map should be keyed narrowly:

```rust
struct DominanceKey<'i> {
  property: LonghandId,
  important: bool,
  selector_key: ExpandedSelectorKey<'i>,
  layer_key: LayerKey<'i>,
  scope_key: ScopeKey<'i>,
  source_index: u32,
}
```

The value is a small list rather than a single declaration because condition
coverage may not be total:

```rust
struct DominanceMap<'i> {
  entries: IndexMap<DominanceKey<'i>, SmallVec<[DeclId; 2]>>,
}
```

When a declaration is inserted into the map, it may remove weaker entries that it
dominates. This keeps the candidate list short.

## Property Conflict Keys

The first implementation should only use exact longhand keys:

```rust
enum PropertyConflictKey<'i> {
  Longhand(LonghandId),
  Unsupported,
  CustomProperty(CustomPropertyName<'i>),
}
```

Only `Longhand` participates in deletion initially.

Unsupported in the first pass:

- shorthands overriding longhands;
- longhands overriding parts of shorthands;
- logical and physical property equivalence;
- vendor-prefixed property fallback groups;
- custom property dependency chains;
- declarations that emit target-dependent fallback rules.

Those can be added later with a richer property conflict graph.

## Materialization

After the dominance pass:

1. Remove dead declarations from their original declaration runs.
2. Remove empty nested declaration runs.
3. Remove empty rule wrappers only when doing so is already valid for that rule
   type.
4. Preserve source order for all live items.

Materialization should not move a later declaration into an earlier declaration
block unless the move has been proven safe. The pass can delete declarations
without changing the position of any live declaration.

## Relationship To The Rule Trie

The rule trie and the cascade IR solve different problems:

- the trie merges equivalent structural rule paths;
- the cascade IR removes declarations that are dead after considering nested
  cascade order.

They can share declaration ids. A practical implementation can build trie nodes
for at-rules and selectors, store declaration runs as node items, and assign each
declaration a `DeclId` in the cascade arena.

The trie must still preserve ordered node items:

```text
declaration run
child rule
declaration run
```

The cascade IR can then scan those items to remove dominated declarations across
child boundaries.

## Non-Goals For The First Pass

The first implementation should not attempt to solve:

- arbitrary selector containment;
- arbitrary media query implication;
- shorthand/longhand dominance;
- custom property dependency analysis;
- full scope proximity comparison;
- moving declarations across children;
- merging declaration runs across children.

Those optimizations can be layered on once the IR and safe exact-longhand pass
are in place.

## Test Cases

Safe deletion:

```css
.a {
  color: red;
  @media (min-width: 900px) {
    & { color: blue; }
  }
  color: green;
}
```

Expected live declarations:

```css
.a { color: green; }
```

Different selector, keep child:

```css
.a {
  @media (min-width: 900px) {
    &:hover { color: blue; }
  }
  color: green;
}
```

Different condition coverage, keep earlier conditional declaration:

```css
.a {
  @media (min-width: 900px) {
    color: blue;
  }
  @media (min-width: 1200px) {
    color: green;
  }
}
```

Different importance, keep important declaration:

```css
.a {
  @media (min-width: 900px) {
    color: blue !important;
  }
  color: green;
}
```

Nested selector expands to same selector, delete child declaration:

```css
.a {
  @media (min-width: 900px) {
    & { color: blue; }
  }
  & { color: green; }
}
```

## Implementation Order

1. Add declaration ids and an arena-backed `DeclEntry`.
2. Build cascade context while walking rule bodies.
3. Expand nested selectors before creating selector keys.
4. Emit a source-ordered declaration item stream.
5. Add exact-longhand reverse dominance within conservative chunks.
6. Materialize by dropping dead declaration ids.
7. Add tests for nested declarations before and after conditional child rules.
8. Extend condition and property conflict analysis incrementally.
