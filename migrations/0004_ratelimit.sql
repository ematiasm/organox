-- Rebanada 4: protección contra fuerza bruta en el login.
--
-- Dos defensas distintas, porque atacan amenazas distintas:
--
--   1. Por CUENTA (esta tabla): cuántos intentos fallidos acumuló un email.
--      Frena el ataque vertical — un atacante contra UN usuario, desde muchas
--      IPs. Sin esto, un botnet con 1000 IPs puede probar 1000 contraseñas por
--      minuto contra el dueño del negocio.
--
--   2. Por IP (en memoria, ver src/ratelimit): cuántos intentos hace una IP.
--      Frena el ataque horizontal — una IP probando muchas cuentas con
--      contraseñas comunes. Es en memoria a propósito: ver docs/ratelimit.md.
--
-- Por qué en la DB y no en memoria para el caso por cuenta: el bloqueo debe
-- sobrevivir un reinicio. Si reiniciar el servidor limpia el contador, el
-- atacante solo necesita esperar un deploy.

CREATE TABLE login_attempts (
    -- El email normalizado (lowercase). No es FK a users a propósito: los
    -- intentos contra emails INEXISTENTES también se cuentan. Si solo
    -- contáramos usuarios reales, el atacante podría distinguir emails
    -- registrados por la diferencia entre "bloqueado" y "401".
    email         citext      PRIMARY KEY,

    -- Intentos fallidos dentro de la ventana actual.
    failed_count  integer     NOT NULL DEFAULT 0,

    -- Inicio de la ventana. Los intentos se cuentan desde acá; al pasar el
    -- tiempo de la ventana, el contador vuelve a cero.
    window_start  timestamptz NOT NULL DEFAULT now(),

    -- Hasta cuándo está bloqueada la cuenta. NULL = no bloqueada.
    -- Es un bloqueo SUAVE: dura minutos, no horas, y expira solo. Un bloqueo
    -- duro (permanente hasta intervención manual) permite que cualquiera deje
    -- al dueño fuera de su propio sistema escribiendo mal su contraseña.
    blocked_until timestamptz,

    updated_at    timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT login_attempts_failed_count_not_negative CHECK (failed_count >= 0)
);

-- Para limpiar filas viejas sin escanear la tabla completa.
CREATE INDEX login_attempts_updated_at_idx ON login_attempts (updated_at);
