-- Internationalization — metadata/UI string translations (PLAN §9 / Phase 11
-- deferral). `sys_translation`/`md_translation` covers **metadata/UI strings
-- only** for v1 (labels, messages, template strings); record-data i18n is the
-- explicitly-deferred U5 (translatable enum/reference data, multi-language
-- record fields) and stays out until a real multi-locale tenant needs it.
--
-- Model: one row per (locale, namespace, msg_key) → value. `locale = ''` is the
-- default/fallback bundle (every key resolves to *something*). A request locale
-- resolves best-match: exact (en-US) → language prefix (en) → default ('').
-- `namespace` segments the keyspace (e.g. 'ui', 'email', a template name) so a
-- UI bootstrap, an email template, and a Studio module don't collide.
--
-- RLS: same tenant-isolation policy as the other meta.md_* tables.

CREATE TABLE IF NOT EXISTS meta.md_translation (
    id          UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   UUID        NOT NULL,
    locale      TEXT        NOT NULL DEFAULT '',        -- '' = default/fallback
    namespace   TEXT        NOT NULL DEFAULT 'ui',
    msg_key     TEXT        NOT NULL,
    value       TEXT        NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, locale, namespace, msg_key)
);

CREATE INDEX IF NOT EXISTS md_translation_lookup_idx
    ON meta.md_translation (tenant_id, namespace, locale);

ALTER TABLE meta.md_translation ENABLE ROW LEVEL SECURITY;
ALTER TABLE meta.md_translation FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON meta.md_translation;
CREATE POLICY tenant_isolation ON meta.md_translation
    USING (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);
