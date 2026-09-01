\set ON_ERROR_STOP on

CREATE ROLE app_owner LOGIN;
CREATE EXTENSION pgcrypto;
CREATE SCHEMA app AUTHORIZATION app_owner;
SET ROLE app_owner;

CREATE TABLE app.items (
    id integer PRIMARY KEY,
    label text NOT NULL,
    payload bytea NOT NULL,
    created_at timestamptz NOT NULL
);

CREATE TABLE app.large_objects (
    id integer PRIMARY KEY,
    object_id oid NOT NULL
);

CREATE FUNCTION app.normalize_label()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    NEW.label := upper(NEW.label);
    RETURN NEW;
END;
$$;

CREATE TRIGGER items_normalize_label
BEFORE INSERT OR UPDATE ON app.items
FOR EACH ROW EXECUTE FUNCTION app.normalize_label();

CREATE FUNCTION app.item_label(item_id integer)
RETURNS text
LANGUAGE sql
STABLE
AS $$
    SELECT label FROM app.items WHERE id = item_id;
$$;

CREATE VIEW app.item_summary AS
SELECT id, label, encode(payload, 'hex') AS payload_hex
FROM app.items;

INSERT INTO app.items(id, label, payload, created_at) VALUES
    (1, '서울', decode('000102ff', 'hex'), '2026-01-02T03:04:05Z'),
    (2, 'restore-proof', decode('deadbeef', 'hex'), '2026-02-03T04:05:06Z');

INSERT INTO app.large_objects(id, object_id)
VALUES (1, lo_from_bytea(0, decode('00112233445566778899aabbccddeeff', 'hex')));

RESET ROLE;
GRANT USAGE ON SCHEMA app TO PUBLIC;
