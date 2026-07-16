//! The draft model — the JSONB document a Studio draft holds (`md_draft.model`),
//! the shape JSON bundles use for export/import, and the **pure** diff/validate
//! logic for the additive-only Phase-1 publish (PLAN §5.8).
//!
//! Phase 1 supports **additive ops only**: a publish may add new modules,
//! entities, fields, and relationships, but may not remove, rename, or retype
//! anything already active (those are transforms/destructive ops — Phase 2).

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The known Phase-2 field-type registry (PLAN §5.6 / §9).
pub const KNOWN_FIELD_TYPES: &[&str] = &[
    "string",
    "text",
    "integer",
    "decimal",
    "money",
    "bool",
    "date",
    "datetime",
    "enum",
    "reference",
    "json",
    "auto_number",
];

/// The whole draft model — the unit a draft stores and a bundle transports.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DraftModel {
    #[serde(default)]
    pub modules: Vec<DraftModule>,
    #[serde(default)]
    pub entities: Vec<DraftEntity>,
}

impl DraftModel {
    pub fn empty() -> Self {
        Self::default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DraftModule {
    pub id: Uuid,
    pub name: String,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DraftEntity {
    pub id: Uuid,
    #[serde(default)]
    pub module_id: Option<Uuid>,
    pub name: String,
    pub table_name: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub fields: Vec<DraftField>,
    #[serde(default)]
    pub relationships: Vec<DraftRelationship>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DraftField {
    pub id: Uuid,
    pub name: String,
    #[serde(default)]
    pub label: Option<String>,
    pub field_type: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub is_unique: bool,
    #[serde(default)]
    pub is_indexed: bool,
    #[serde(default)]
    pub default_expr: Option<serde_json::Value>,
    #[serde(default = "default_object")]
    pub config: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DraftRelationship {
    pub id: Uuid,
    pub source_field_name: String,
    pub target_entity_id: Uuid,
    pub cardinality: String,
    pub strength: String,
    #[serde(default)]
    pub on_delete: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub reference_qualifier: Option<serde_json::Value>,
    #[serde(default)]
    pub rollup_summary: Option<serde_json::Value>,
}

fn default_object() -> serde_json::Value {
    serde_json::json!({})
}

// ===== Diff / validate (pure; additive-only for Phase 1) =====

/// Result of diffing an active model against a draft. `valid` is true only when
/// the draft is publishable under Phase-1 (additive-only) rules.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DiffReport {
    pub valid: bool,
    /// Phase-2 operations present (removals / transforms) — rejected in Phase 1.
    pub violations: Vec<String>,
    /// Hard validation errors in the additions themselves.
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub additions: AdditionSummary,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct AdditionSummary {
    pub modules: usize,
    pub entities: usize,
    pub fields: usize,
    pub relationships: usize,
}

/// Compare `active` against `draft` under additive-only rules.
///
/// Every artifact present in `active` must still be present in `draft`
/// *unchanged*; anything in `draft` not in `active` is an addition and must be
/// self-consistent (unique names/ids, known types, resolved references).
pub fn diff(active: &DraftModel, draft: &DraftModel) -> DiffReport {
    let mut report = DiffReport {
        valid: true,
        ..Default::default()
    };

    // Index the active model by id.
    let active_mods: HashMap<Uuid, &DraftModule> =
        active.modules.iter().map(|m| (m.id, m)).collect();
    let active_ents: HashMap<Uuid, &DraftEntity> =
        active.entities.iter().map(|e| (e.id, e)).collect();

    // Index the draft the same way, and detect duplicate ids / names.
    let (draft_mods, dup_mod) = index_modules(draft);
    let (draft_ents, dup_ent) = index_entities(draft);
    let (draft_fields, dup_field) = index_fields(draft);
    let (draft_rels, dup_rel) = index_relationships(draft);

    for msg in dup_mod
        .into_iter()
        .chain(dup_ent)
        .chain(dup_field)
        .chain(dup_rel)
    {
        report.errors.push(msg);
    }

    // 1) Everything active must be present in the draft unchanged (else transform
    //    or removal — both Phase 2). Count unchanged as "not an addition".
    for m in &active.modules {
        match draft_mods.get(&m.id) {
            None => {
                report.violations.push(format!(
                    "module {} was removed (destructive — Phase 2)",
                    m.name
                ));
            }
            Some(d) if canon_module(d) != canon_module(m) => {
                report.violations.push(format!(
                    "module {} was modified (transform — Phase 2)",
                    m.name
                ));
            }
            _ => {}
        }
    }
    for e in &active.entities {
        match draft_ents.get(&e.id) {
            None => report.violations.push(format!(
                "entity {} was removed (destructive — Phase 2)",
                e.name
            )),
            Some((parent, d)) if *parent != e.module_id || canon_entity(d) != canon_entity(e) => {
                report.violations.push(format!(
                    "entity {} was modified (transform — Phase 2)",
                    e.name
                ));
            }
            _ => {}
        }
        for f in &e.fields {
            match draft_fields.get(&f.id) {
                None => report.violations.push(format!(
                    "field {}.{} was removed (destructive — Phase 2)",
                    e.name, f.name
                )),
                Some((parent, d)) if *parent != e.id || canon_field(d) != canon_field(f) => {
                    report.violations.push(format!(
                        "field {}.{} was modified (transform — Phase 2)",
                        e.name, f.name
                    ));
                }
                _ => {}
            }
        }
        for r in &e.relationships {
            match draft_rels.get(&r.id) {
                None => report.violations.push(format!(
                    "relationship {} on {} was removed (destructive — Phase 2)",
                    r.source_field_name, e.name
                )),
                Some((parent, d)) if *parent != e.id || canon_rel(d) != canon_rel(r) => {
                    report.violations.push(format!(
                        "relationship {} on {} was modified (transform — Phase 2)",
                        r.source_field_name, e.name
                    ));
                }
                _ => {}
            }
        }
    }

    // 2) Validate the additions (draft artifacts not in active).
    let known: HashSet<&str> = KNOWN_FIELD_TYPES.iter().copied().collect();
    let mut mod_names: HashSet<&str> = active.modules.iter().map(|m| m.name.as_str()).collect();
    let mut ent_names: HashSet<&str> = active.entities.iter().map(|e| e.name.as_str()).collect();
    let mut table_names: HashSet<&str> = active
        .entities
        .iter()
        .map(|e| e.table_name.as_str())
        .collect();
    let all_entity_ids: HashSet<Uuid> = active
        .entities
        .iter()
        .chain(draft.entities.iter())
        .map(|e| e.id)
        .collect();

    // new modules
    for m in &draft.modules {
        if active_mods.contains_key(&m.id) {
            continue;
        }
        report.additions.modules += 1;
        if m.name.trim().is_empty() {
            report.errors.push("a new module has an empty name".into());
        }
        if !mod_names.insert(m.name.as_str()) {
            report
                .errors
                .push(format!("duplicate module name {}", m.name));
        }
    }
    // new entities (+ their fields/relationships)
    for e in &draft.entities {
        if active_ents.contains_key(&e.id) {
            continue;
        }
        report.additions.entities += 1;
        if e.name.trim().is_empty() {
            report.errors.push("a new entity has an empty name".into());
        }
        if e.table_name.trim().is_empty() {
            report
                .errors
                .push(format!("entity {} has an empty table_name", e.name));
        } else if !is_valid_table_name(&e.table_name) {
            report.errors.push(format!(
                "entity {} has an invalid table_name `{}`",
                e.name, e.table_name
            ));
        }
        if !ent_names.insert(e.name.as_str()) {
            report
                .errors
                .push(format!("duplicate entity name {}", e.name));
        }
        if !table_names.insert(e.table_name.as_str()) {
            report
                .errors
                .push(format!("duplicate table_name {}", e.table_name));
        }

        let mut field_names: HashSet<&str> = HashSet::new();
        let mut rel_names: HashSet<&str> = HashSet::new();
        for f in &e.fields {
            report.additions.fields += 1;
            if !known.contains(f.field_type.as_str()) {
                report.errors.push(format!(
                    "field {}.{} has unknown type {}",
                    e.name, f.name, f.field_type
                ));
            }
            if f.name.trim().is_empty() {
                report
                    .errors
                    .push(format!("entity {} has a field with an empty name", e.name));
            }
            if !field_names.insert(f.name.as_str()) {
                report
                    .errors
                    .push(format!("duplicate field name {}.{}", e.name, f.name));
            }
        }
        for r in &e.relationships {
            report.additions.relationships += 1;
            if !all_entity_ids.contains(&r.target_entity_id) {
                report.errors.push(format!(
                    "relationship {} on {} targets unknown entity {}",
                    r.source_field_name, e.name, r.target_entity_id
                ));
            }
            if !is_valid_cardinality(&r.cardinality) {
                report.errors.push(format!(
                    "relationship {} on {} has invalid cardinality {}",
                    r.source_field_name, e.name, r.cardinality
                ));
            }
            if !is_valid_strength(&r.strength) {
                report.errors.push(format!(
                    "relationship {} on {} has invalid strength {}",
                    r.source_field_name, e.name, r.strength
                ));
            }
            if !rel_names.insert(r.source_field_name.as_str()) {
                report.errors.push(format!(
                    "duplicate relationship column {} on {}",
                    r.source_field_name, e.name
                ));
            }
        }
    }

    if !report.violations.is_empty() || !report.errors.is_empty() {
        report.valid = false;
    }
    report
}

// ---- canonicalization (only user-facing fields; excludes id) ----

fn canon_module(m: &DraftModule) -> serde_json::Value {
    serde_json::json!({ "name": m.name, "label": m.label })
}

fn canon_entity(e: &DraftEntity) -> serde_json::Value {
    serde_json::json!({
        "module_id": e.module_id,
        "name": e.name,
        "table_name": e.table_name,
        "label": e.label,
        "description": e.description,
    })
}

fn canon_field(f: &DraftField) -> serde_json::Value {
    serde_json::json!({
        "name": f.name,
        "label": f.label,
        "field_type": f.field_type,
        "required": f.required,
        "is_unique": f.is_unique,
        "is_indexed": f.is_indexed,
        "default_expr": f.default_expr,
        "config": f.config,
    })
}

fn canon_rel(r: &DraftRelationship) -> serde_json::Value {
    serde_json::json!({
        "source_field_name": r.source_field_name,
        "target_entity_id": r.target_entity_id,
        "cardinality": r.cardinality,
        "strength": r.strength,
        "on_delete": r.on_delete,
        "required": r.required,
    })
}

// ---- index helpers (id → data, plus duplicate detection) ----

fn index_modules(draft: &DraftModel) -> (HashMap<Uuid, &DraftModule>, Vec<String>) {
    let mut map = HashMap::new();
    let mut dups = Vec::new();
    let mut seen = HashSet::new();
    for m in &draft.modules {
        if !seen.insert(m.id) {
            dups.push(format!("duplicate module id {}", m.id));
        }
        map.insert(m.id, m);
    }
    (map, dups)
}

#[allow(clippy::type_complexity)]
fn index_entities(
    draft: &DraftModel,
) -> (HashMap<Uuid, (Option<Uuid>, &DraftEntity)>, Vec<String>) {
    let mut map = HashMap::new();
    let mut dups = Vec::new();
    let mut seen = HashSet::new();
    for e in &draft.entities {
        if !seen.insert(e.id) {
            dups.push(format!("duplicate entity id {}", e.id));
        }
        map.insert(e.id, (e.module_id, e));
    }
    (map, dups)
}

fn index_fields(draft: &DraftModel) -> (HashMap<Uuid, (Uuid, &DraftField)>, Vec<String>) {
    let mut map = HashMap::new();
    let mut dups = Vec::new();
    let mut seen = HashSet::new();
    for e in &draft.entities {
        for f in &e.fields {
            if !seen.insert(f.id) {
                dups.push(format!("duplicate field id {}", f.id));
            }
            map.insert(f.id, (e.id, f));
        }
    }
    (map, dups)
}

fn index_relationships(
    draft: &DraftModel,
) -> (HashMap<Uuid, (Uuid, &DraftRelationship)>, Vec<String>) {
    let mut map = HashMap::new();
    let mut dups = Vec::new();
    let mut seen = HashSet::new();
    for e in &draft.entities {
        for r in &e.relationships {
            if !seen.insert(r.id) {
                dups.push(format!("duplicate relationship id {}", r.id));
            }
            map.insert(r.id, (e.id, r));
        }
    }
    (map, dups)
}

fn is_valid_table_name(name: &str) -> bool {
    // conservative: lowercase ident, [a-z0-9_], starts with a letter, reasonable length
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && name.len() <= 63
}

fn is_valid_cardinality(c: &str) -> bool {
    matches!(c, "one_to_many" | "many_to_one" | "many_to_many")
}

fn is_valid_strength(s: &str) -> bool {
    matches!(s, "master_detail" | "lookup")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ent(id: u128, name: &str) -> DraftEntity {
        DraftEntity {
            id: Uuid::from_u128(id),
            module_id: None,
            name: name.into(),
            table_name: name.to_lowercase(),
            label: None,
            description: None,
            fields: vec![],
            relationships: vec![],
        }
    }

    #[test]
    fn empty_adds_are_valid() {
        let r = diff(&DraftModel::empty(), &DraftModel::empty());
        assert!(r.valid);
    }

    #[test]
    fn adding_entity_is_valid() {
        let mut draft = DraftModel::empty();
        draft.entities.push(ent(1, "Customer"));
        let r = diff(&DraftModel::empty(), &draft);
        assert!(r.valid, "{:?}", r.errors);
        assert_eq!(r.additions.entities, 1);
    }

    #[test]
    fn removing_active_is_violation() {
        let active = DraftModel {
            modules: vec![],
            entities: vec![ent(1, "Customer")],
        };
        let r = diff(&active, &DraftModel::empty());
        assert!(!r.valid);
        assert!(r.violations.iter().any(|v| v.contains("removed")));
    }

    #[test]
    fn renaming_active_is_transform() {
        let mut active = DraftModel::empty();
        active.entities.push(ent(1, "Customer"));
        let mut draft = DraftModel::empty();
        let mut e = ent(1, "Customer2"); // same id, different name
        e.table_name = "customer".into();
        draft.entities.push(e);
        let r = diff(&active, &draft);
        assert!(!r.valid);
        assert!(r.violations.iter().any(|v| v.contains("modified")));
    }

    #[test]
    fn unchanged_active_is_fine_alongside_additions() {
        let mut active = DraftModel::empty();
        active.entities.push(ent(1, "Customer"));
        let mut draft = active.clone();
        draft.entities.push(ent(2, "Invoice")); // addition
        let r = diff(&active, &draft);
        assert!(r.valid, "{:?}", r);
        assert_eq!(r.additions.entities, 1);
    }

    #[test]
    fn rejects_unknown_field_type_and_bad_table_name() {
        let mut draft = DraftModel::empty();
        let mut e = ent(1, "X");
        e.table_name = "Bad Name".into();
        e.fields.push(DraftField {
            id: Uuid::from_u128(9),
            name: "f".into(),
            label: None,
            field_type: "blob".into(),
            required: false,
            is_unique: false,
            is_indexed: false,
            default_expr: None,
            config: serde_json::json!({}),
        });
        draft.entities.push(e);
        let r = diff(&DraftModel::empty(), &draft);
        assert!(!r.valid);
        assert!(r.errors.iter().any(|e| e.contains("unknown type blob")));
        assert!(r.errors.iter().any(|e| e.contains("invalid table_name")));
    }

    #[test]
    fn rejects_dangling_relationship_target() {
        let mut draft = DraftModel::empty();
        let mut e = ent(1, "Invoice");
        e.relationships.push(DraftRelationship {
            id: Uuid::from_u128(7),
            source_field_name: "ref_customer_id".into(),
            target_entity_id: Uuid::from_u128(999), // nonexistent
            cardinality: "many_to_one".into(),
            strength: "lookup".into(),
            on_delete: None,
            required: false,
            reference_qualifier: None,
            rollup_summary: None,
        });
        draft.entities.push(e);
        let r = diff(&DraftModel::empty(), &draft);
        assert!(!r.valid);
        assert!(r
            .errors
            .iter()
            .any(|e| e.contains("targets unknown entity")));
    }
}
