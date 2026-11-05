-- Rebanada 2: usuarios, roles y sesiones.
--
-- Decisiones que esta migración fija y que son caras de cambiar después:
--   * ids: uuid v7 generados en la app (ordenables por tiempo, no adivinables).
--   * timestamps: TIMESTAMPTZ en UTC. Nunca TIMESTAMP sin zona.
--   * email: CITEXT para que "Juan@PyME.com" y "juan@pyme.com" sean el mismo
--     usuario. Un índice unique sobre LOWER(email) no protege los INSERT que
--     no pasan por el normalizador; CITEXT protege el constraint mismo.
--   * contraseñas: solo el hash PHC de argon2id. Nunca el plaintext.
--   * sesiones: el token en claro NUNCA se guarda. Se guarda su SHA-256, así
--     una lectura de la tabla no permite suplantar a nadie.
--   * roles: enum de Postgres, no una tabla. Con dos roles donde ninguno
--     comparte permisos finos, una tabla roles/permissions seria andamiaje sin
--     uso. Cuando haga falta un permiso que cruce roles, se agrega en una
--     migración nueva (ver docs/auth.md).

CREATE EXTENSION IF NOT EXISTS citext;

CREATE TYPE user_role AS ENUM ('owner', 'encargado');

CREATE TABLE users (
    id            uuid        PRIMARY KEY,
    email         citext      NOT NULL,
    password_hash text        NOT NULL,
    role          user_role   NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT users_email_not_blank CHECK (btrim(email::text) <> ''),
    CONSTRAINT users_password_hash_is_phc CHECK (password_hash LIKE '$argon2%')
);

-- El email es la identidad de login: unique es una regla de negocio, no un
-- detalle de implementación.
CREATE UNIQUE INDEX users_email_unique ON users (email);

CREATE TABLE sessions (
    -- SHA-256 del token entregado al cliente. El token en claro solo existe
    -- en la cookie; si esto se filtra, no sirve para autenticarse.
    token_hash    bytea       PRIMARY KEY,
    user_id       uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- Se guarda para detectar robo de cookie (el "device fingerprint" de la
    -- sesión). Si cambia, la sesión se considera comprometida.
    user_agent    text,
    created_at    timestamptz NOT NULL DEFAULT now(),
    last_used_at  timestamptz NOT NULL DEFAULT now(),
    expires_at    timestamptz NOT NULL
);

-- Toda request autenticada actualiza last_used_at y filtra por expires_at.
CREATE INDEX sessions_user_id_idx ON sessions (user_id);
CREATE INDEX sessions_expires_at_idx ON sessions (expires_at);

-- Mantener updated_at coherente sin depender de la disciplina del código.
CREATE OR REPLACE FUNCTION set_updated_at() RETURNS trigger AS $$
BEGIN
    NEW.updated_at = now();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER users_set_updated_at
    BEFORE UPDATE ON users
    FOR EACH ROW
    EXECUTE FUNCTION set_updated_at();
