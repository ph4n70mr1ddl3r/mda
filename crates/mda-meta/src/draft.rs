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
    "attachment",
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
    pub retirements: RetirementSummary,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct AdditionSummary {
    pub modules: usize,
    pub entities: usize,
    pub fields: usize,
    pub relationships: usize,
}

/// Artifacts the draft retires (Phase 2 two-phase destructive: retire now,
/// purge after grace). Retirements are *allowed* — the data is kept until purge.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RetirementSummary {
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
                report.retirements.modules += 1;
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
            None => report.retirements.entities += 1,
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
                None => report.retirements.fields += 1,
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
                None => report.retirements.relationships += 1,
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
    // Field/relationship additions are identified by id, not by their entity:
    // a new field on an *existing* entity is an addition too (and reaches the
    // same DDL + runtime SQL interpolation), so it must pass the same gate.
    let active_field_ids: HashSet<Uuid> = active
        .entities
        .iter()
        .flat_map(|e| e.fields.iter().map(|f| f.id))
        .collect();
    let active_rel_ids: HashSet<Uuid> = active
        .entities
        .iter()
        .flat_map(|e| e.relationships.iter().map(|r| r.id))
        .collect();

    // new modules
    for m in &draft.modules {
        if active_mods.contains_key(&m.id) {
            continue;
        }
        report.additions.modules += 1;
        if m.name.trim().is_empty() {
            report.errors.push("a new module has an empty name".into());
        } else if !is_valid_identifier(&m.name) {
            report.errors.push(format!(
                "module {} has an invalid name (lowercase [a-z][a-z0-9_]*, ≤63 chars, not reserved)",
                m.name
            ));
        }
        if !mod_names.insert(m.name.as_str()) {
            report
                .errors
                .push(format!("duplicate module name {}", m.name));
        }
    }
    // new entities (entity-level checks) + all field/relationship additions
    // (on new *and* existing entities — ids already active are "unchanged",
    // verified in section 1 above).
    for e in &draft.entities {
        let active_ent = active_ents.get(&e.id);
        if active_ent.is_none() {
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
        }

        // Name-uniqueness within the entity is checked against the active
        // entity's names too — a new field shadowing an active one (different
        // id, same name) would collide with the hoisted column / attribute key.
        let mut field_names: HashSet<&str> = active_ent
            .map(|a| a.fields.iter().map(|f| f.name.as_str()).collect())
            .unwrap_or_default();
        let mut rel_names: HashSet<&str> = active_ent
            .map(|a| {
                a.relationships
                    .iter()
                    .map(|r| r.source_field_name.as_str())
                    .collect()
            })
            .unwrap_or_default();
        for f in &e.fields {
            if active_field_ids.contains(&f.id) {
                continue;
            }
            report.additions.fields += 1;
            if !known.contains(f.field_type.as_str()) {
                report.errors.push(format!(
                    "field {}.{} has unknown type {}",
                    e.name, f.name, f.field_type
                ));
            }
            // `reference` is in the §5.6 type registry but is not a legal
            // *field* type: a reference is modeled as a relationship (which
            // hoists a real FK column, §5.7). A `reference`-typed field would
            // publish fine but be unwritable at runtime (`coerce` rejects it) —
            // catch it at the modeling gate instead.
            if f.field_type == "reference" {
                report.errors.push(format!(
                    "field {}.{}: a reference is modeled as a relationship (hoisted FK column), not a field type",
                    e.name, f.name
                ));
            }
            if f.name.trim().is_empty() {
                report
                    .errors
                    .push(format!("entity {} has a field with an empty name", e.name));
            } else if !is_valid_identifier(&f.name) {
                report.errors.push(format!(
                    "field {}.{} has an invalid name (lowercase [a-z][a-z0-9_]*, ≤63 chars, not reserved)",
                    e.name, f.name
                ));
            }
            if !field_names.insert(f.name.as_str()) {
                report
                    .errors
                    .push(format!("duplicate field name {}.{}", e.name, f.name));
            }
        }
        for r in &e.relationships {
            if active_rel_ids.contains(&r.id) {
                continue;
            }
            report.additions.relationships += 1;
            if !is_valid_identifier(&r.source_field_name) {
                report.errors.push(format!(
                    "relationship column {} on {} is not a valid SQL identifier",
                    r.source_field_name, e.name
                ));
            }
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
    is_valid_identifier(name)
}

/// A safe SQL identifier for both DDL interpolation and JSONB-attribute keys:
/// lowercase `[a-z][a-z0-9_]*`, ≤ 63 chars (PG `NAMEDATALEN-1`), and neither a
/// SQL reserved word nor one of MDA's reserved core column names. This single
/// gate is what lets us interpolate entity/field/relationship names into the
/// generated `biz.*` SQL (PLAN §5.16 — metadata is untrusted). Re-used by the
/// DDL layer as a defense-in-depth assert before interpolation.
pub fn is_valid_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && name.len() <= 63
        && !is_reserved(name)
}

/// Reject SQL reserved words (unquoted use in DDL/queries would fail or parse
/// ambiguously) and MDA core columns (a field shadowing these would collide
/// with a real `biz.<table>` column / the JSONB payload key in `reconstruct`).
fn is_reserved(name: &str) -> bool {
    matches!(
        name,
        // SQL reserved words (unquoted use in DDL/queries would fail or parse
        // ambiguously) — a focused subset of PostgreSQL's RESERVED list,
        // biased toward names a business modeler might plausibly pick.
        "all"
            | "and"
            | "any"
            | "as"
            | "asc"
            | "between"
            | "by"
            | "case"
            | "cast"
            | "check"
            | "create"
            | "cross"
            | "default"
            | "delete"
            | "desc"
            | "distinct"
            | "else"
            | "end"
            | "except"
            | "false"
            | "for"
            | "from"
            | "full"
            | "grant"
            | "group"
            | "having"
            | "ilike"
            | "in"
            | "inner"
            | "insert"
            | "intersect"
            | "into"
            | "is"
            | "join"
            | "key"
            | "left"
            | "like"
            | "limit"
            | "not"
            | "null"
            | "offset"
            | "on"
            | "or"
            | "order"
            | "outer"
            | "primary"
            | "references"
            | "returning"
            | "right"
            | "select"
            | "set"
            | "table"
            | "then"
            | "to"
            | "true"
            | "union"
            | "unique"
            | "update"
            | "user"
            | "using"
            | "when"
            | "where"
            | "with"
        // MDA core columns — a field shadowing these would collide with a
        // real `biz.<table>` column / the JSONB payload key in `reconstruct`.
        | "id"
            | "tenant_id"
            | "owner_id"
            | "state"
            | "version"
            | "created_at"
            | "updated_at"
            | "attributes"
    )
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
    fn removing_active_is_retirement() {
        let active = DraftModel {
            modules: vec![],
            entities: vec![ent(1, "Customer")],
        };
        let r = diff(&active, &DraftModel::empty());
        assert!(r.valid, "retirements are allowed: {r:?}");
        assert_eq!(r.retirements.entities, 1);
        assert!(r.violations.is_empty());
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

    #[test]
    fn rejects_invalid_field_name() {
        let mut draft = DraftModel::empty();
        let mut e = ent(1, "Customer");
        e.fields.push(DraftField {
            id: Uuid::from_u128(2),
            name: "shady'; DROP TABLE; --".into(),
            label: None,
            field_type: "string".into(),
            required: false,
            is_unique: false,
            is_indexed: false,
            default_expr: None,
            config: serde_json::json!({}),
        });
        draft.entities.push(e);
        let r = diff(&DraftModel::empty(), &draft);
        assert!(!r.valid);
        assert!(r.errors.iter().any(|m| m.contains("invalid name")));
    }

    // Regression (Phase-11 review): a NEW field on an EXISTING entity used to
    // skip the identifier gate entirely (diff only validated fields of brand-new
    // entities) — a malicious name reached the DDL and runtime SQL interpolation.
    #[test]
    fn rejects_invalid_field_name_on_existing_entity() {
        let mut active = DraftModel::empty();
        let mut a = ent(1, "Customer");
        a.fields.push(DraftField {
            id: Uuid::from_u128(11),
            name: "name".into(),
            label: None,
            field_type: "string".into(),
            required: false,
            is_unique: false,
            is_indexed: false,
            default_expr: None,
            config: serde_json::json!({}),
        });
        active.entities.push(a);

        let mut draft = active.clone();
        draft.entities[0].fields.push(DraftField {
            id: Uuid::from_u128(12), // new id → an addition on an existing entity
            name: "x' || (SELECT password FROM sec.sec_user) || 'y".into(),
            label: None,
            field_type: "string".into(),
            required: false,
            is_unique: false,
            is_indexed: true, // forces a hoisted generated column (DDL sink)
            default_expr: None,
            config: serde_json::json!({}),
        });
        let r = diff(&active, &draft);
        assert!(
            !r.valid,
            "malicious field on existing entity must be rejected"
        );
        assert!(r.errors.iter().any(|m| m.contains("invalid name")));
        // the same bug class for relationships
        let mut draft2 = active.clone();
        draft2.entities[0].relationships.push(DraftRelationship {
            id: Uuid::from_u128(13),
            source_field_name: "evil col; --".into(),
            target_entity_id: Uuid::from_u128(1),
            cardinality: "many_to_one".into(),
            strength: "lookup".into(),
            on_delete: None,
            required: false,
            reference_qualifier: None,
            rollup_summary: None,
        });
        let r2 = diff(&active, &draft2);
        assert!(
            !r2.valid,
            "malicious relationship on existing entity must be rejected"
        );
        assert!(r2
            .errors
            .iter()
            .any(|m| m.contains("not a valid SQL identifier")));
    }

    // Regression (Phase-11 review): a new field whose NAME collides with an
    // active field on the same entity (different id) would collide with the
    // hoisted column / attribute key at publish time.
    #[test]
    fn rejects_new_field_shadowing_active_field_name() {
        let mut active = DraftModel::empty();
        let mut a = ent(1, "Customer");
        a.fields.push(DraftField {
            id: Uuid::from_u128(11),
            name: "name".into(),
            label: None,
            field_type: "string".into(),
            required: false,
            is_unique: false,
            is_indexed: false,
            default_expr: None,
            config: serde_json::json!({}),
        });
        active.entities.push(a);

        let mut draft = active.clone();
        draft.entities[0].fields.push(DraftField {
            id: Uuid::from_u128(12),
            name: "name".into(), // duplicate of the active field's name
            label: None,
            field_type: "text".into(),
            required: false,
            is_unique: false,
            is_indexed: false,
            default_expr: None,
            config: serde_json::json!({}),
        });
        let r = diff(&active, &draft);
        assert!(!r.valid);
        assert!(r.errors.iter().any(|m| m.contains("duplicate field name")));
    }

    // A well-formed new field on an existing entity IS a valid addition.
    #[test]
    fn accepts_valid_field_on_existing_entity() {
        let mut active = DraftModel::empty();
        let mut a = ent(1, "Customer");
        a.fields.push(DraftField {
            id: Uuid::from_u128(11),
            name: "name".into(),
            label: None,
            field_type: "string".into(),
            required: false,
            is_unique: false,
            is_indexed: false,
            default_expr: None,
            config: serde_json::json!({}),
        });
        active.entities.push(a);

        let mut draft = active.clone();
        draft.entities[0].fields.push(DraftField {
            id: Uuid::from_u128(12),
            name: "tier".into(),
            label: None,
            field_type: "string".into(),
            required: false,
            is_unique: false,
            is_indexed: false,
            default_expr: None,
            config: serde_json::json!({}),
        });
        let r = diff(&active, &draft);
        assert!(r.valid, "{:?}", r.errors);
        assert_eq!(r.additions.fields, 1);
    }

    #[test]
    fn rejects_reserved_and_core_field_names() {
        let bad_names = ["order", "attributes", "select", "id", "owner_id"];
        let mut draft = DraftModel::empty();
        let mut e = ent(1, "Customer");
        for (idx, bad) in bad_names.iter().enumerate() {
            e.fields.push(DraftField {
                id: Uuid::from_u128(10 + idx as u128),
                name: (*bad).into(),
                label: None,
                field_type: "string".into(),
                required: false,
                is_unique: false,
                is_indexed: false,
                default_expr: None,
                config: serde_json::json!({}),
            });
        }
        draft.entities.push(e);
        let r = diff(&DraftModel::empty(), &draft);
        assert!(!r.valid);
        for bad in bad_names {
            assert!(
                r.errors.iter().any(|m| m.contains(bad)),
                "expected rejection of {bad}: {r:?}"
            );
        }
    }

    #[test]
    fn rejects_reference_typed_field_directs_to_relationships() {
        // `reference` is a known registry type but must be expressed as a
        // relationship — a field of that type would be unwritable at runtime.
        let mut draft = DraftModel::empty();
        let mut e = ent(1, "Customer");
        e.fields.push(DraftField {
            id: Uuid::from_u128(21),
            name: "parent".into(),
            label: None,
            field_type: "reference".into(),
            required: false,
            is_unique: false,
            is_indexed: false,
            default_expr: None,
            config: serde_json::json!({}),
        });
        draft.entities.push(e);
        let r = diff(&DraftModel::empty(), &draft);
        assert!(!r.valid);
        assert!(r
            .errors
            .iter()
            .any(|m| m.contains("modeled as a relationship")));
    }

    #[test]
    fn rejects_invalid_relationship_column() {
        let mut draft = DraftModel::empty();
        let customer = ent(2, "Customer");
        let mut invoice = ent(1, "Invoice");
        invoice.relationships.push(DraftRelationship {
            id: Uuid::from_u128(3),
            source_field_name: "evil column".into(),
            target_entity_id: customer.id,
            cardinality: "many_to_one".into(),
            strength: "lookup".into(),
            on_delete: None,
            required: false,
            reference_qualifier: None,
            rollup_summary: None,
        });
        draft.entities.push(customer);
        draft.entities.push(invoice);
        let r = diff(&DraftModel::empty(), &draft);
        assert!(!r.valid);
        assert!(r
            .errors
            .iter()
            .any(|m| m.contains("not a valid SQL identifier")));
    }
}
