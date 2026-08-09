CREATE SCHEMA IF NOT EXISTS app;

CREATE TABLE app.accounts (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    email text NOT NULL UNIQUE,
    display_name text,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE app.projects (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    owner_account_id bigint NOT NULL REFERENCES app.accounts(id),
    slug text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT projects_owner_slug_key UNIQUE (owner_account_id, slug)
);

CREATE INDEX projects_owner_account_id_idx
    ON app.projects (owner_account_id);
