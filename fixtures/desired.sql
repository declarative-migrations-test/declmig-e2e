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

CREATE INDEX projects_owner_slug_lower_idx
    ON app.projects (owner_account_id, lower(slug))
    WHERE slug <> '';

CREATE TABLE app.account_audit (
    audit_id bigint GENERATED ALWAYS AS IDENTITY,
    account_id bigint NOT NULL,
    old_display_name text,
    new_display_name text,
    CONSTRAINT account_audit_pkey PRIMARY KEY (audit_id),
    CONSTRAINT account_audit_account_id_fkey
        FOREIGN KEY (account_id) REFERENCES app.accounts(id)
);

CREATE FUNCTION app.audit_account_update() RETURNS trigger AS $$
BEGIN
    INSERT INTO app.account_audit(account_id, old_display_name, new_display_name)
    VALUES ((NEW).id, (OLD).display_name, (NEW).display_name);
    RETURN NEW;
END
$$ LANGUAGE PLpgSQL;

CREATE TRIGGER accounts_audit
    AFTER UPDATE ON app.accounts
    FOR EACH ROW EXECUTE FUNCTION app.audit_account_update();

CREATE PROCEDURE app.set_account_display_name(account_id bigint, new_display_name text)
    LANGUAGE PLpgSQL AS $$
BEGIN
    UPDATE app.accounts
       SET display_name = new_display_name
     WHERE accounts.id = account_id;
END
$$;

CREATE VIEW app.named_accounts AS
    SELECT id, email, display_name
      FROM app.accounts
     WHERE display_name IS NOT NULL;
