# Autenticación y autorización

## Modelo

Single-tenant: no hay `tenant_id`. Un despliegue = un negocio.

| Concepto | Implementación |
|---|---|
| Identidad | `users.email` (`citext`, unique). Login case-insensitive. |
| Contraseña | argon2id, hash PHC. Nunca plaintext, nunca `bcrypt`. |
| Sesión | Fila en `sessions`, clave = SHA-256 del token. |
| Token | 32 bytes de CSPRNG, base64url. Viaja en cookie `HttpOnly`. |
| Actor | Extractor `AuthUser`. Única vía para conocer quién hace la request. |
| Autorización | `Role` + `Role::can_manage_users()`. Punto único. |

## Por qué sesión y no JWT

Un admin web necesita **logout real** y **revocación inmediata**. Con JWT, cerrar
sesión es una sugerencia al cliente y revocar exige una lista negra en la DB — o
sea, volver al estado que evita. Aquí el logout borra la fila y el token deja de
servir en el request siguiente.

## Por qué solo el SHA-256 del token en la DB

El token ya tiene 256 bits de entropía de un CSPRNG: no hay diccionario que
atacar. Guardar el hash significa que una lectura de la tabla `sessions` permite
saber que hay sesiones, no usarlas. SHA-256 (no argon2) porque se calcula en cada
request autenticada y la entropía ya está en el token.

## Por qué argon2id y no bcrypt

argon2id es el estándar actual (recomendado por OWASP) y sus parámetros de costo
están dentro del propio PHC string: subir el costo no invalida hashes viejos.
Coste medido en esta máquina: ~390 ms por hash. Eso es intencional, y es la razón
de que el hashing corra en `spawn_blocking` y no en el runtime de tokio.

## Prevención de enumeración de usuarios

`POST /auth/login` devuelve **el mismo 401 y el mismo cuerpo** para:

- email inexistente
- contraseña incorrecta
- email malformado

Además, cuando el email no existe se verifica la contraseña contra un hash
señuelo de costo equivalente, precalculado al arrancar el servidor. Sin ese
precalentamiento, el primer intento fallido pagaba el costo de generar el señuelo
y la latencia lo delataba como oráculo (medido: 773 ms vs 391 ms).

## Cookies

```
Set-Cookie: session=<token>; HttpOnly; Secure; SameSite=Strict; Path=/; Expires=...
```

- `HttpOnly`: JavaScript no puede leerla (mitiga XSS de robo de sesión).
- `SameSite=Strict`: no viaja en requests originadas por otro sitio. Corta CSRF
  sin necesitar token CSRF, siempre que el frontend se sirva desde el mismo
  origen.
- `Secure`: obligatorio en producción. `APP_COOKIE_SECURE=false` solo para
  desarrollo sobre `http://localhost`.

**Si el frontend queda en otro origen** (`app.pyme.com` + `api.pyme.com`),
`SameSite=Strict` deja de funcionar: hay que pasar a `Lax` con dominio padre
común, o a `None` + token CSRF explícito. Es la decisión de despliegue que
condiciona esto.

## Vida de la sesión

- Deslizante: cada request autenticada la extiende 7 días.
- Tope absoluto: 30 días desde su creación, aunque se use todos los días.
  Sin ese tope, una cookie robada es válida indefinidamente.

## Roles

Hoy: `owner` (todo) y `encargado` (todo menos gestionar usuarios).

**No hay tablas `roles`/`permissions`.** Con dos roles donde ningún permiso cruza
roles, una tabla de permisos es andamiaje sin uso. La regla vive en un solo lugar:

```rust
impl Role {
    pub fn can_manage_users(self) -> bool {
        matches!(self, Role::Owner)
    }
}
```

Cuando aparezca un permiso que cruce roles (por ejemplo "depósito carga stock
pero no ve precios"), el cambio es:

1. Migración nueva con `permissions` y `role_permissions`.
2. Cambiar **el cuerpo** de las funciones `can_*` en `src/auth/model.rs`.

Los handlers no se tocan, porque ya piden `user.require_user_management()` y no
inspeccionan el rol a mano.

## Alta del primer usuario

`create-user` es el único mecanismo de alta fuera de la API. **No hay seed
automático al arrancar**, y es deliberado:

| Seed por `.env` al bootear | `create-user` |
|---|---|
| La contraseña queda en plaintext en el `.env` para siempre | No persiste plaintext |
| Condición de carrera con 2 réplicas (SELECT+INSERT) | No |
| Resucita usuarios borrados | No |
| Pisa contraseñas cambiadas | No |
| Sirve para el usuario 2..N | Sí |

```bash
# interactivo (pide sin eco)
cargo run --bin create-user -- --email duenio@pyme.com --role owner

# en Docker / no interactivo
printf '%s' "$ADMIN_PW" | docker compose exec -T app create-user \
  --email duenio@pyme.com --role owner --password-stdin
```

La contraseña nunca va por argumento: quedaría visible en `ps aux` de cualquier
usuario del sistema.

## Lo que falta

- **Rate limiting en login.** Un solo endpoint de login sin límite es fuerza
  bruta distribuida. `tower_governor` por IP, más bloqueo por cuenta.
- **Cambio de contraseña** (`POST /auth/password`) y revocación de todas las
  sesiones del usuario.
- **Desbloqueo**: no hay bloqueo por intentos fallidos todavía.
