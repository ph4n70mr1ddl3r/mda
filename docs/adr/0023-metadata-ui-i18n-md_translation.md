# ADR-0023: Metadata/UI internationalization (`md_translation`)

- **Status:** Accepted
- **Date:** 2026-01-28
- **Resolves:** PLAN §9 / Phase 11 "i18n (`sys_translation`)" (metadata/UI
  strings); narrows the U5 deferral (record-data i18n stays deferred)
- **Detail:** PLAN §9 / §5.19; migration `migrations/20260128000001_translations.sql`;
  implementation `crates/mda-api/src/i18n.rs` + the §5.19 render path

## Context

Phase 11 listed "i18n (`sys_translation`), theming" as hardening work, and §9
explicitly defers **data-level** i18n (U5: translatable enum/reference data,
record-level multi-language fields) until a real multi-locale tenant needs it.
What is needed now is **metadata/UI string** translation: every label, message,
and template a tenant authors should be localizable so a single tenant can
serve users in multiple locales without forking the model. Templating (§5.19)
already renders under a locale; it had no string catalog to draw from.

## Decision

Ship `meta.md_translation` — one value per `(locale, namespace, msg_key)`, with
`locale = ''` as the default/fallback bundle. A request locale resolves
**best-match**: exact (`en-US`) → language prefix (`en`) → default (`''`), so a
partial translation falls back gracefully to the complete default bundle.
`namespace` segments the keyspace (`ui`, `email`, a template/module name) so a
UI bootstrap, an email template, and a Studio module don't collide.

API (`mda-api::i18n`):
- `POST /api/translations` — upsert by natural key (idempotent).
- `GET /api/translations` — raw management list (`?namespace=`).
- `GET /api/i18n/:locale` / `GET /api/translations/:locale` — the **resolved**
  best-match bundle (`{ locale, namespace, translations: { ns.key: value } }`),
  ready for a UI bootstrap (`?namespace=` scopes it).
- `DELETE /api/translations/:locale[?namespace=]` and
  `DELETE /api/translations/:locale/:namespace/:key`.

**Template integration (§5.19).** The render path resolves the bundle for the
render locale and injects a nested `i18n[namespace][key]` object into the
context, so a template localizes with `{{ i18n.email.subject }}`. These are
pure strings — AuthZ-by-construction is preserved (a translation can never
carry a record field value).

**Tenant backup (§14).** `md_translation` is part of the tenant configuration
export/import snapshot (ADR-0018's tenant-config export), restored by natural
key — a tenant's localization ships with its config bundle.

## Consequences

- Closes metadata/UI i18n; U5 (record-data i18n) remains explicitly deferred
  and is unchanged — `md_translation` is strings only, never record values.
- Best-match fallback means a tenant can ship a complete default + partial
  overrides without a key ever resolving to nothing.
- The template locale + the translation locale share one resolution rule
  (exact → prefix → default), so a localized template and its localized
  strings agree on which locale is in effect.
