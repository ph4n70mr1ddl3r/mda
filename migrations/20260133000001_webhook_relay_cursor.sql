-- Move the webhook relay's high-water mark into a real migration.
--
-- Root cause: webhooks::relay_once created `sys_webhook_relay_cursor` lazily
-- ("CREATE TABLE IF NOT EXISTS …") on the *app* pool at runtime. As the
-- non-superuser role `mda_app` (every release/staging deployment) that CREATE
-- fails with "permission denied for schema public" — the relay then warn-looped
-- every 3s and no webhook delivery was ever enqueued (§5.21 silently dead).
-- Dev hid it: the dev server runs as the owner, where the lazy CREATE worked
-- (and, via the `"$user"` search_path quirk, even landed the table in the
-- `mda` schema on older databases).
--
-- Creating it here as schema-owned DDL fixes both: the table is granted to
-- `mda_app` with the rest of public, and relay_once now only reads/writes it.
-- The create is schema-QUALIFIED — unqualified DDL as the owner role `mda`
-- lands in the `mda` schema (the `"$user"` search_path quirk that also bit
-- migrations 20260126 / 20260132). The cursor restarting at 0 on adoption is
-- safe: delivery is at-least-once by contract (§5.21 replays are expected)
-- and the row is immediately re-advanced to the current event-log mark.
CREATE TABLE IF NOT EXISTS public.sys_webhook_relay_cursor (
    id  int  PRIMARY KEY,
    seq bigint NOT NULL
);

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'mda_app') THEN
        GRANT SELECT, INSERT, UPDATE, DELETE ON public.sys_webhook_relay_cursor TO mda_app;
    END IF;
END
$$;
