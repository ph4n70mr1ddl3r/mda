//! Effective identity + permission checks (PLAN §5.11).

use std::collections::{HashMap, HashSet};

use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    None,
    Read,
    Write,
}

impl Access {
    pub fn parse(s: &str) -> Self {
        match s {
            "none" => Access::None,
            "read" => Access::Read,
            _ => Access::Write,
        }
    }
}

/// Org-wide default for an entity (record grain baseline).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owd {
    Private,
    Team,
    PublicRead,
    PublicReadWrite,
}

impl Owd {
    pub fn parse(s: &str) -> Self {
        match s {
            "team" => Owd::Team,
            "public_read" => Owd::PublicRead,
            "public_read_write" => Owd::PublicReadWrite,
            _ => Owd::Private,
        }
    }

    /// Anyone in the tenant may read (for the record-visibility predicate).
    pub fn allows_read_for_all(self) -> bool {
        matches!(self, Owd::PublicRead | Owd::PublicReadWrite)
    }

    pub fn allows_write_for_all(self) -> bool {
        matches!(self, Owd::PublicReadWrite)
    }
}

/// The resolved effective context for an authenticated user.
#[derive(Debug, Clone)]
pub struct Identity {
    pub user_id: Uuid,
    pub tenant_id: Uuid,
    pub team_id: Option<Uuid>,
    pub is_superuser: bool,
    /// (entity, verb) — verb/entity may be `*`.
    object_perms: HashSet<(String, String)>,
    /// (entity, field) -> most-permissive access across the user's roles.
    field_perms: HashMap<(String, String), Access>,
}

impl Identity {
    pub(crate) fn new(
        user_id: Uuid,
        tenant_id: Uuid,
        team_id: Option<Uuid>,
        object_perms: HashSet<(String, String)>,
        field_perms: HashMap<(String, String), Access>,
    ) -> Self {
        let is_superuser = object_perms.contains(&("*".to_string(), "*".to_string()));
        Self {
            user_id,
            tenant_id,
            team_id,
            is_superuser,
            object_perms,
            field_perms,
        }
    }

    /// Object-level: may the user perform `verb` on `entity`?
    pub fn can(&self, entity: &str, verb: &str) -> bool {
        self.is_superuser
            || self
                .object_perms
                .contains(&(entity.to_string(), verb.to_string()))
            || self
                .object_perms
                .contains(&(entity.to_string(), "*".to_string()))
            || self
                .object_perms
                .contains(&("*".to_string(), verb.to_string()))
    }

    /// Field-level access. No rule => full (FLS is opt-in restriction in Phase 3).
    pub fn field_access(&self, entity: &str, field: &str) -> Access {
        if self.is_superuser {
            return Access::Write;
        }
        self.field_perms
            .get(&(entity.to_string(), field.to_string()))
            .copied()
            .unwrap_or(Access::Write)
    }
}
