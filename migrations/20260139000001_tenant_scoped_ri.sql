-- Tenant-scoped referential integrity + composite uniques on EXISTING biz
-- tables, matching the DDL engine's new output (ddl.rs):
--   * UNIQUE (tenant_id, id) on every biz table — the target key for
--     tenant-scoped FKs, and a §5.1-compliant composite.
--   * Every biz→biz FK re-emitted as (tenant_id, col) → (tenant_id, id), so
--     Postgres RI (which bypasses RLS) can no longer accept another tenant's
--     record id as a reference target, nor let a foreign delete cascade reach
--     this tenant's rows.
--   * Old single-column UNIQUE constraints (which spanned every tenant
--     sharing the physical table) replaced by UNIQUE (tenant_id, col).
-- Fresh databases have no biz tables at migration time; this runs as a no-op
-- there and upgrades in-place databases.

DO $$
DECLARE
  t record;
  fk record;
  u record;
  col text;
  ondel text;
BEGIN
  -- 1. UNIQUE (tenant_id, id) on every biz table.
  FOR t IN SELECT tablename FROM pg_tables WHERE schemaname = 'biz' LOOP
    -- (the loop body's regclass cast is only reached when the table exists)
    IF NOT EXISTS (
      SELECT 1 FROM pg_constraint c
       WHERE c.contype = 'u' AND c.conrelid = format('biz.%I', t.tablename)::regclass
         AND (SELECT array_agg(a.attname ORDER BY a.attnum)
                FROM unnest(c.conkey) k
                JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k)
             = ARRAY['tenant_id', 'id']
    ) THEN
      EXECUTE format('ALTER TABLE biz.%I ADD CONSTRAINT %I_tenant_id_uk UNIQUE (tenant_id, id)',
                     t.tablename, t.tablename);
    END IF;
  END LOOP;

  -- 2. Replace single-column UNIQUE constraints with (tenant_id, col) composites.
  FOR u IN
    SELECT c.oid, c.conname, c.conrelid::regclass AS src,
           (SELECT a.attname FROM unnest(c.conkey) k
              JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k) AS col
      FROM pg_constraint c
      JOIN pg_namespace n ON n.oid = c.connamespace AND n.nspname = 'biz'
     WHERE c.contype = 'u'
       AND array_length(c.conkey, 1) = 1
       AND (SELECT a.attname FROM unnest(c.conkey) k
              JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k) <> 'id'
  LOOP
    EXECUTE format('ALTER TABLE %s DROP CONSTRAINT %I', u.src, u.conname);
    EXECUTE format('ALTER TABLE %s ADD CONSTRAINT %I UNIQUE (tenant_id, %I)',
                   u.src, format('%s_%s_uk', u.src, u.col), u.col);
  END LOOP;

  -- 3. Re-scope every single-column biz→biz FK to (tenant_id, col) → (tenant_id, id),
  --    preserving ON DELETE behavior and deferrability.
  FOR fk IN
    SELECT c.conname, c.conrelid::regclass AS src, c.confrelid::regclass AS tgt,
           (SELECT a.attname FROM unnest(c.conkey) k
              JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k) AS col,
           c.confdeltype, c.condeferrable
      FROM pg_constraint c
      JOIN pg_namespace n ON n.oid = c.connamespace AND n.nspname = 'biz'
     WHERE c.contype = 'f'
       AND c.confrelid::regclass::text LIKE 'biz.%'
       AND array_length(c.conkey, 1) = 1
  LOOP
    ondel := CASE fk.confdeltype
               WHEN 'c' THEN 'CASCADE'
               WHEN 'n' THEN 'SET NULL'
               WHEN 'd' THEN 'SET DEFAULT'
               WHEN 'a' THEN 'NO ACTION'
               ELSE 'RESTRICT'
             END;
    EXECUTE format('ALTER TABLE %s DROP CONSTRAINT %I', fk.src, fk.conname);
    EXECUTE format(
      'ALTER TABLE %s ADD CONSTRAINT %I FOREIGN KEY (tenant_id, %I) REFERENCES %s (tenant_id, id) ON DELETE %s%s',
      fk.src, fk.conname, fk.col, fk.tgt, ondel,
      CASE WHEN fk.condeferrable THEN ' DEFERRABLE INITIALLY DEFERRED' ELSE '' END);
  END LOOP;
END $$;
