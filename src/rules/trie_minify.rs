use super::container::{ContainerCondition, ContainerName};
use super::document::MozDocumentRule;
use super::media::MediaRule;
use super::scope::ScopeRule;
use super::starting_style::StartingStyleRule;
use super::supports::SupportsRule;
use super::{ContainerRule, CssRule, CssRuleList, Location, MinifyContext, NestedDeclarationsRule, StyleRule};
use crate::declaration::DeclarationBlock;
use crate::error::MinifyError;
use crate::media_query::MediaList;
use crate::parser::DefaultAtRule;
use crate::properties::{Property, PropertyId};
use crate::selector::SelectorList;
use crate::targets::Targets;
use crate::vendor_prefix::VendorPrefix;
use indexmap::IndexMap;

type NodeId = usize;
type DeclId = usize;

pub(crate) fn merge_rule_list<'i, T: Clone>(
  rules: &mut CssRuleList<'i, T>,
  context: &mut MinifyContext<'_, 'i>,
) -> Result<(), MinifyError> {
  if context.css_modules {
    return Ok(());
  }

  if !should_merge_rule_list(rules, context)? {
    return Ok(());
  }

  let mut trie = RuleTrie::new(can_reuse_style_declarations(context));
  for rule in rules.0.drain(..) {
    trie.insert_rule(0, rule, context)?;
  }

  rules.0 = trie.emit_rule_list(0, false);
  Ok(())
}

struct RuleTrie<'i, T = DefaultAtRule> {
  nodes: Vec<TrieNode<'i, T>>,
  declarations: Vec<DeclarationEntry<'i>>,
  dedupe_declarations: bool,
}

struct TrieNode<'i, T = DefaultAtRule> {
  key: Option<RuleKey<'i>>,
  media_query: Option<MediaList<'i>>,
  loc: Location,
  children: Vec<(RuleKey<'i>, NodeId)>,
  items: Vec<NodeItem<'i, T>>,
}

enum NodeItem<'i, T = DefaultAtRule> {
  DeclarationSet(DeclarationSet<'i>),
  Child(NodeId),
  Barrier(CssRule<'i, T>),
}

#[derive(Clone)]
struct DeclarationSet<'i> {
  entries: Vec<DeclId>,
  normal: IndexMap<PropertyId<'i>, usize>,
  important: IndexMap<PropertyId<'i>, usize>,
}

struct DeclarationEntry<'i> {
  declaration: Property<'i>,
  important: bool,
}

impl<'i> DeclarationSet<'i> {
  fn new() -> Self {
    Self {
      entries: Vec::new(),
      normal: IndexMap::new(),
      important: IndexMap::new(),
    }
  }

  fn insert(
    &mut self,
    id: DeclId,
    key: PropertyId<'i>,
    important: bool,
    dedupe: bool,
    arena: &[DeclarationEntry<'i>],
  ) {
    let existing = {
      let map = if important { &self.important } else { &self.normal };
      map.get(&key).copied()
    };

    if let Some(position) = existing.filter(|_| dedupe) {
      if self.can_replace_at(position, &key, important, arena) {
        self.entries[position] = id;
        return;
      }
    }

    let position = self.entries.len();
    self.entries.push(id);
    let map = if important {
      &mut self.important
    } else {
      &mut self.normal
    };
    map.insert(key, position);
  }

  fn can_replace_at(
    &self,
    position: usize,
    property_id: &PropertyId<'i>,
    important: bool,
    arena: &[DeclarationEntry<'i>],
  ) -> bool {
    self.entries[position + 1..].iter().all(|id| {
      let entry = &arena[*id];
      entry.important != important || !properties_conflict(&entry.declaration.property_id(), property_id)
    })
  }
}

#[derive(Clone, PartialEq)]
enum RuleKey<'i> {
  Media(MediaList<'i>),
  Supports(SupportsConditionKey<'i>),
  Container {
    name: Option<ContainerName<'i>>,
    condition: Option<ContainerCondition<'i>>,
  },
  Scope {
    scope_start: Option<SelectorList<'i>>,
    scope_end: Option<SelectorList<'i>>,
  },
  StartingStyle,
  MozDocument,
  Selector(SelectorKey<'i>),
}

#[derive(Clone, PartialEq)]
struct SupportsConditionKey<'i>(super::supports::SupportsCondition<'i>);

#[derive(Clone, PartialEq)]
struct SelectorKey<'i> {
  selectors: SelectorList<'i>,
  vendor_prefix: VendorPrefix,
  source_index: u32,
}

impl<'i, T: Clone> RuleTrie<'i, T> {
  fn new(dedupe_declarations: bool) -> Self {
    Self {
      nodes: vec![TrieNode {
        key: None,
        media_query: None,
        loc: Location {
          source_index: 0,
          line: 0,
          column: 0,
        },
        children: Vec::new(),
        items: Vec::new(),
      }],
      declarations: Vec::new(),
      dedupe_declarations,
    }
  }

  fn insert_rule(
    &mut self,
    node_id: NodeId,
    rule: CssRule<'i, T>,
    context: &mut MinifyContext<'_, 'i>,
  ) -> Result<(), MinifyError> {
    match rule {
      CssRule::Media(media) => {
        let key = RuleKey::Media(normalize_media_key(&media, context)?);
        let child = self.get_or_insert_child(node_id, key, media.loc);
        if self.nodes[child].media_query.is_none() {
          self.nodes[child].media_query = Some(media.query.clone());
        }
        self.insert_rule_list(child, media.rules, context)?;
      }
      CssRule::Supports(supports) => {
        let key = RuleKey::Supports(SupportsConditionKey(supports.condition.clone()));
        let child = self.get_or_insert_child(node_id, key, supports.loc);
        self.insert_rule_list(child, supports.rules, context)?;
      }
      CssRule::Container(container) => {
        let key = RuleKey::Container {
          name: container.name.clone(),
          condition: container.condition.clone(),
        };
        let child = self.get_or_insert_child(node_id, key, container.loc);
        self.insert_rule_list(child, container.rules, context)?;
      }
      CssRule::Scope(scope) => {
        let key = RuleKey::Scope {
          scope_start: scope.scope_start.clone(),
          scope_end: scope.scope_end.clone(),
        };
        let child = self.get_or_insert_child(node_id, key, scope.loc);
        self.insert_rule_list(child, scope.rules, context)?;
      }
      CssRule::StartingStyle(rule) => {
        let child = self.get_or_insert_child(node_id, RuleKey::StartingStyle, rule.loc);
        self.insert_rule_list(child, rule.rules, context)?;
      }
      CssRule::MozDocument(document) => {
        let child = self.get_or_insert_child(node_id, RuleKey::MozDocument, document.loc);
        self.insert_rule_list(child, document.rules, context)?;
      }
      CssRule::Style(style) => {
        let has_declarations = style.declarations.len() > 0;
        let allow_declaration_reuse = can_reuse_style_declarations(context);
        let key = RuleKey::Selector(SelectorKey {
          selectors: style.selectors.clone(),
          vendor_prefix: style.vendor_prefix,
          source_index: style.loc.source_index,
        });
        let child =
          self.get_or_insert_style_child(node_id, key, style.loc, has_declarations, allow_declaration_reuse);
        self.insert_declarations(child, style.declarations);
        self.insert_rule_list(child, style.rules, context)?;
      }
      CssRule::NestedDeclarations(nested) => {
        self.insert_declarations(node_id, nested.declarations);
      }
      rule => {
        self.nodes[node_id].items.push(NodeItem::Barrier(rule));
      }
    }

    Ok(())
  }

  fn insert_rule_list(
    &mut self,
    node_id: NodeId,
    rules: CssRuleList<'i, T>,
    context: &mut MinifyContext<'_, 'i>,
  ) -> Result<(), MinifyError> {
    for rule in rules.0 {
      self.insert_rule(node_id, rule, context)?;
    }

    Ok(())
  }

  fn insert_declarations(&mut self, node_id: NodeId, declarations: DeclarationBlock<'i>) {
    if declarations.len() == 0 {
      return;
    }

    if matches!(self.nodes[node_id].items.last(), Some(NodeItem::DeclarationSet(_))) {
      let item = self.nodes[node_id].items.pop().unwrap();
      let NodeItem::DeclarationSet(mut set) = item else {
        unreachable!()
      };
      self.push_declarations_into_set(&mut set, declarations);
      self.nodes[node_id].items.push(NodeItem::DeclarationSet(set));
      return;
    }

    let mut set = DeclarationSet::new();
    self.push_declarations_into_set(&mut set, declarations);
    self.nodes[node_id].items.push(NodeItem::DeclarationSet(set));
  }

  fn push_declarations_into_set(&mut self, set: &mut DeclarationSet<'i>, mut declarations: DeclarationBlock<'i>) {
    for declaration in declarations.declarations.drain(..) {
      let id = self.declarations.len();
      let key = declaration.property_id();
      self.declarations.push(DeclarationEntry {
        declaration,
        important: false,
      });
      set.insert(id, key, false, self.dedupe_declarations, &self.declarations);
    }

    for declaration in declarations.important_declarations.drain(..) {
      let id = self.declarations.len();
      let key = declaration.property_id();
      self.declarations.push(DeclarationEntry {
        declaration,
        important: true,
      });
      set.insert(id, key, true, self.dedupe_declarations, &self.declarations);
    }
  }

  fn get_or_insert_child(&mut self, node_id: NodeId, key: RuleKey<'i>, loc: Location) -> NodeId {
    if let Some(NodeItem::Child(last_child)) = self.nodes[node_id].items.last() {
      if self.nodes[*last_child].key.as_ref() == Some(&key) {
        return *last_child;
      }
    }

    self.insert_child(node_id, key, loc)
  }

  fn get_or_insert_style_child(
    &mut self,
    node_id: NodeId,
    key: RuleKey<'i>,
    loc: Location,
    has_new_declarations: bool,
    allow_declaration_reuse: bool,
  ) -> NodeId {
    if let Some(NodeItem::Child(last_child)) = self.nodes[node_id].items.last() {
      if self.nodes[*last_child].key.as_ref() == Some(&key)
        && (!has_new_declarations || allow_declaration_reuse || !self.node_has_declarations(*last_child))
      {
        return *last_child;
      }
    }

    self.insert_child(node_id, key, loc)
  }

  fn insert_child(&mut self, node_id: NodeId, key: RuleKey<'i>, loc: Location) -> NodeId {
    let child = self.nodes.len();
    self.nodes.push(TrieNode {
      key: Some(key.clone()),
      media_query: None,
      loc,
      children: Vec::new(),
      items: Vec::new(),
    });
    self.nodes[node_id].children.push((key, child));
    self.nodes[node_id].items.push(NodeItem::Child(child));
    child
  }

  fn node_has_declarations(&self, node_id: NodeId) -> bool {
    self.nodes[node_id]
      .items
      .iter()
      .any(|item| matches!(item, NodeItem::DeclarationSet(declarations) if !declarations.entries.is_empty()))
  }

  fn emit_rule_list(&self, node_id: NodeId, in_style_context: bool) -> Vec<CssRule<'i, T>> {
    let mut rules = Vec::new();

    for item in &self.nodes[node_id].items {
      match item {
        NodeItem::DeclarationSet(set) => {
          if !set.entries.is_empty() {
            rules.push(CssRule::NestedDeclarations(NestedDeclarationsRule {
              declarations: self.materialize_declarations(&set.entries),
              loc: self.nodes[node_id].loc,
            }));
          }
        }
        NodeItem::Child(child) => {
          if let Some(rule) = self.emit_child(*child, in_style_context) {
            rules.push(rule);
          }
        }
        NodeItem::Barrier(rule) => rules.push(rule.clone()),
      }
    }

    rules
  }

  fn emit_child(&self, node_id: NodeId, in_style_context: bool) -> Option<CssRule<'i, T>> {
    let node = &self.nodes[node_id];
    match node.key.as_ref()? {
      RuleKey::Media(query) => {
        let rules = CssRuleList(self.emit_rule_list(node_id, in_style_context));
        if rules.0.is_empty() {
          return None;
        }

        Some(CssRule::Media(MediaRule {
          query: node.media_query.as_ref().unwrap_or(query).clone(),
          rules,
          loc: node.loc,
        }))
      }
      RuleKey::Supports(condition) => {
        let rules = CssRuleList(self.emit_rule_list(node_id, in_style_context));
        if rules.0.is_empty() {
          return None;
        }

        Some(CssRule::Supports(SupportsRule {
          condition: condition.0.clone(),
          rules,
          loc: node.loc,
        }))
      }
      RuleKey::Container { name, condition } => {
        let rules = CssRuleList(self.emit_rule_list(node_id, in_style_context));
        if rules.0.is_empty() {
          return None;
        }

        Some(CssRule::Container(ContainerRule {
          name: name.clone(),
          condition: condition.clone(),
          rules,
          loc: node.loc,
        }))
      }
      RuleKey::Scope { scope_start, scope_end } => {
        let rules = CssRuleList(self.emit_rule_list(node_id, false));
        if rules.0.is_empty() {
          return None;
        }

        Some(CssRule::Scope(ScopeRule {
          scope_start: scope_start.clone(),
          scope_end: scope_end.clone(),
          rules,
          loc: node.loc,
        }))
      }
      RuleKey::StartingStyle => {
        let rules = CssRuleList(self.emit_rule_list(node_id, in_style_context));
        if rules.0.is_empty() {
          return None;
        }

        Some(CssRule::StartingStyle(StartingStyleRule { rules, loc: node.loc }))
      }
      RuleKey::MozDocument => {
        let rules = CssRuleList(self.emit_rule_list(node_id, false));
        if rules.0.is_empty() {
          return None;
        }

        Some(CssRule::MozDocument(MozDocumentRule { rules, loc: node.loc }))
      }
      RuleKey::Selector(selector) => self.emit_style_rule(node_id, selector),
    }
  }

  fn emit_style_rule(&self, node_id: NodeId, selector: &SelectorKey<'i>) -> Option<CssRule<'i, T>> {
    let node = &self.nodes[node_id];
    let mut declarations = DeclarationBlock::new();
    let mut nested_rules = Vec::new();
    let mut emitted_child = false;

    for item in &node.items {
      match item {
        NodeItem::DeclarationSet(set) => {
          if set.entries.is_empty() {
            continue;
          }

          if emitted_child {
            nested_rules.push(CssRule::NestedDeclarations(NestedDeclarationsRule {
              declarations: self.materialize_declarations(&set.entries),
              loc: node.loc,
            }));
          } else {
            append_declarations(&mut declarations, self.materialize_declarations(&set.entries));
          }
        }
        NodeItem::Child(child) => {
          if let Some(rule) = self.emit_child(*child, true) {
            nested_rules.push(rule);
            emitted_child = true;
          }
        }
        NodeItem::Barrier(rule) => {
          nested_rules.push(rule.clone());
          emitted_child = true;
        }
      }
    }

    let style = StyleRule {
      selectors: selector.selectors.clone(),
      vendor_prefix: selector.vendor_prefix,
      declarations,
      rules: CssRuleList(nested_rules),
      loc: node.loc,
    };

    if style.is_empty() {
      None
    } else {
      Some(CssRule::Style(style))
    }
  }

  fn materialize_declarations(&self, ids: &[DeclId]) -> DeclarationBlock<'i> {
    let mut declarations = DeclarationBlock::new();
    for id in ids {
      let entry = &self.declarations[*id];
      if entry.important {
        declarations.important_declarations.push(entry.declaration.clone());
      } else {
        declarations.declarations.push(entry.declaration.clone());
      }
    }

    declarations
  }
}

fn normalize_media_key<'i, T>(
  media: &MediaRule<'i, T>,
  context: &MinifyContext<'_, 'i>,
) -> Result<MediaList<'i>, MinifyError> {
  let mut query = media.query.clone();
  if let Some(custom_media) = &context.custom_media {
    query.transform_custom_media(media.loc, custom_media)?;
  }

  query.transform_resolution(context.targets.current);
  Ok(query)
}

fn should_merge_rule_list<'i, T: Clone>(
  rules: &CssRuleList<'i, T>,
  context: &MinifyContext<'_, 'i>,
) -> Result<bool, MinifyError> {
  if rules.0.len() < 2 {
    return should_merge_nested_rule_list(rules, context);
  }

  for pair in rules.0.windows(2) {
    if adjacent_rules_can_merge(&pair[0], &pair[1], context)? {
      return Ok(true);
    }
  }

  should_merge_nested_rule_list(rules, context)
}

fn should_merge_nested_rule_list<'i, T: Clone>(
  rules: &CssRuleList<'i, T>,
  context: &MinifyContext<'_, 'i>,
) -> Result<bool, MinifyError> {
  for rule in &rules.0 {
    let nested = match rule {
      CssRule::Media(rule) => &rule.rules,
      CssRule::Supports(rule) => &rule.rules,
      CssRule::Container(rule) => &rule.rules,
      CssRule::Scope(rule) => &rule.rules,
      CssRule::StartingStyle(rule) => &rule.rules,
      CssRule::MozDocument(rule) => &rule.rules,
      CssRule::Style(rule) => &rule.rules,
      _ => continue,
    };

    if should_merge_rule_list(nested, context)? {
      return Ok(true);
    }
  }

  Ok(false)
}

fn adjacent_rules_can_merge<'i, T: Clone>(
  a: &CssRule<'i, T>,
  b: &CssRule<'i, T>,
  context: &MinifyContext<'_, 'i>,
) -> Result<bool, MinifyError> {
  Ok(match (a, b) {
    (CssRule::Media(a), CssRule::Media(b)) => normalize_media_key(a, context)? == normalize_media_key(b, context)?,
    (CssRule::Supports(a), CssRule::Supports(b)) => a.condition == b.condition,
    (CssRule::Container(a), CssRule::Container(b)) => a.name == b.name && a.condition == b.condition,
    (CssRule::Scope(a), CssRule::Scope(b)) => a.scope_start == b.scope_start && a.scope_end == b.scope_end,
    (CssRule::StartingStyle(_), CssRule::StartingStyle(_)) => true,
    (CssRule::MozDocument(_), CssRule::MozDocument(_)) => true,
    (CssRule::Style(a), CssRule::Style(b)) => {
      a.selectors == b.selectors
        && a.vendor_prefix == b.vendor_prefix
        && a.loc.source_index == b.loc.source_index
        && (can_reuse_style_declarations(context) || a.declarations.len() == 0 || b.declarations.len() == 0)
    }
    _ => false,
  })
}

fn append_declarations<'i>(to: &mut DeclarationBlock<'i>, mut from: DeclarationBlock<'i>) {
  to.declarations.append(&mut from.declarations);
  to.important_declarations.append(&mut from.important_declarations);
}

fn properties_conflict<'i>(a: &PropertyId<'i>, b: &PropertyId<'i>) -> bool {
  if same_property_id(a, b) || matches!(a, PropertyId::All) || matches!(b, PropertyId::All) {
    return true;
  }

  if longhands_include(a, b) || longhands_include(b, a) {
    return true;
  }

  let Some(a_longhands) = a.longhands() else {
    return false;
  };
  let Some(b_longhands) = b.longhands() else {
    return false;
  };

  a_longhands
    .iter()
    .any(|a_longhand| b_longhands.iter().any(|b_longhand| same_property_id(a_longhand, b_longhand)))
}

fn longhands_include<'i>(property_id: &PropertyId<'i>, longhand: &PropertyId<'i>) -> bool {
  property_id
    .longhands()
    .is_some_and(|longhands| longhands.iter().any(|id| same_property_id(id, longhand)))
}

fn same_property_id<'a, 'b>(a: &PropertyId<'a>, b: &PropertyId<'b>) -> bool {
  a.name() == b.name() && a.prefix() == b.prefix()
}

fn can_reuse_style_declarations(context: &MinifyContext<'_, '_>) -> bool {
  // Browser-target and CSS modules passes can emit extra rules from declarations.
  // Keep those declaration runs separate until the existing minifier has placed
  // the generated rules in source order.
  context.targets.current == Targets::default() && !context.css_modules
}
