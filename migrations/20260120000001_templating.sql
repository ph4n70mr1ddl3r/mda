-- Templating (PLAN §5.19). Templates are metadata authored in Studio; the body
-- is a SANDBOXED template DSL (variable interpolation + a restricted subset of
-- the bounded expression engine, §5.2): no arbitrary code, no I/O, cannot emit
-- raw SQL. Variables are the render context (record fields, actor, params).
--
-- A template renders under the recipient's / running user's field-level
-- visibility (§5.11): the render context is AuthZ-filtered by construction, so a
-- template can never emit a field the recipient cannot read (same structural
-- rule as reports, §5.17).
--
-- kind: email | document | message. locale: best-match resolution (NULL = the
-- default/fallback). Record-data i18n remains deferred (U5).

CREATE TABLE IF NOT EXISTS meta.md_template (
    id           UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id    UUID        NOT NULL,
    name         TEXT        NOT NULL,
    kind         TEXT        NOT NULL DEFAULT 'message',  -- email | document | message
    body         TEXT        NOT NULL,
    content_type TEXT        NOT NULL DEFAULT 'text/plain',
    locale       TEXT,                                     -- e.g. en-US; NULL = default
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name, locale)
);

-- RLS — same tenant-isolation policy as the other meta.md_* tables. (The
-- generic meta-RLS pass ran before this table existed, so gate it explicitly.)
ALTER TABLE meta.md_template ENABLE ROW LEVEL SECURITY;
ALTER TABLE meta.md_template FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON meta.md_template;
CREATE POLICY tenant_isolation ON meta.md_template
    USING (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);
