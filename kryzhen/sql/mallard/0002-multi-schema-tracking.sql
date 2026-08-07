-- kryzhen's own extension on top of stock mallard (versions 0 and 1 above are mallard's
-- real self-migrations, verbatim). Adds the retrofitted UNIQUE(name) index that
-- `applied_migrations` upserts rely on, and a per-schema association table so the same
-- migration set can be tracked across many schemas (schema-per-customer / RAG). Fully
-- schema-qualified: unlike 0000/0001 above, this script does not rely on `search_path`.

CREATE UNIQUE INDEX applied_migrations_name_key ON mallard.applied_migrations (name);

CREATE TABLE mallard.applied_migration_schemas(
    id              uuid            NOT NULL DEFAULT gen_random_uuid(),
    migration_name  text            NOT NULL,
    schema          text            NOT NULL,
    applied_on      timestamptz     NOT NULL DEFAULT now(),

    PRIMARY KEY (id),
    UNIQUE (migration_name, schema)
);
