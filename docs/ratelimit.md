# Rate limiting en login

## Qué es y qué no es

**No prohíbe el abuso: lo acota.** No impide que alguien intente adivinar una
contraseña; hace que intentarlo sea lento, finito y observable.

La distinción no es semántica, es de diseño. Cualquier sistema que prometa
*prohibir* el abuso termina en uno de dos lugares:

1. **Miente**: el atacante igual intenta, solo que más despacio.
2. **Niega servicio a usuarios legítimos**: es lo que pasa cuando el bloqueo es
   duro y permanente.

El objetivo real es otro: **hacer que el ataque sea económicamente inviable y
dejar rastro**. 5 intentos cada 5 minutos son 1440 por día. Contra un espacio de
~10¹² combinaciones razonables, es irrelevante. Y cada intento queda logueado.

## Tres amenazas, tres defensas

| Amenaza | Qué es | Defensa |
|---|---|---|
| **Fuerza bruta vertical** | Muchas IPs contra **una** cuenta (el dueño) | Límite por cuenta, en Postgres |
| **Fuerza bruta horizontal** | Una IP contra **muchas** cuentas con contraseñas comunes | Límite por IP, en memoria |
| **Amplificación de CPU** | Inundar `/auth/login` sabiendo que cada request gasta 400 ms de argon2 | Límite por IP + cortar **antes** de hashear |

Las dos primeras capas son necesarias y no se sustituyen:

- **Solo por IP**: un botnet con 1000 IPs prueba 1000 contraseñas por minuto sin
  activar ningún límite.
- **Solo por cuenta**: el atacante prueba 1 contraseña en cada una de 100 000
  cuentas. Ninguna cuenta se bloquea, y las contraseñas comunes caen igual.

## Capa 1 — Por cuenta (Postgres)

Tabla `login_attempts`, una fila por email normalizado.

**Por qué Postgres y no memoria**: el bloqueo debe sobrevivir un reinicio. Si
reiniciar el proceso limpia el contador, el atacante solo necesita esperar un
deploy — o tirar el proceso él mismo.

**Por qué se cuentan los emails inexistentes**: si solo contaran los usuarios
reales, la diferencia entre "429 bloqueado" y "401 credenciales inválidas"
permitiría **enumerar qué emails están registrados**. Es el mismo oráculo que se
cierra con el hash señuelo, por otra vía.

### Semántica exacta

Con `APP_LOGIN_MAX_FAILURES=3`:

| Intento | Contraseña | Respuesta |
|---|---|---|
| 1 | incorrecta | `401` |
| 2 | incorrecta | `401` |
| 3 | incorrecta | `401` ← **dispara el bloqueo, pero informa la verdad** |
| 4 | cualquiera | `429` + `Retry-After` ← ni se evalúa |

El tercer intento devuelve **401, no 429**, porque su contraseña *era*
efectivamente incorrecta. Decirle "demasiados intentos" en el mismo intento que
lo bloqueó es mentirle al usuario. El bloqueo rige desde el siguiente.

Esto se descubrió con un test que falló (`account_is_locked_after_max_failures`):
el código devolvía 429 en el tercero, y la semántica documentada era otra. Se
corrigió el código, no el test.

### Por qué el bloqueo es suave

Dura `APP_LOGIN_BLOCK_MINUTES` (5 por defecto) y expira solo.

Un bloqueo duro (hasta intervención manual) convierte esta defensa en un **vector
de denegación de servicio**: cualquiera que conozca el email del dueño lo deja
fuera de su propio sistema escribiendo mal la contraseña. En un sistema
administrativo de una PyME, eso es peor que el ataque que intenta evitar.

Contrapartida asumida: un atacante puede esperar 5 minutos y volver a probar 5
contraseñas. El límite no impide el ataque, lo hace **irrelevante en volumen**.

### Un login correcto limpia el contador

Sin esto, alguien que se equivocó 3 veces y después recordó su contraseña queda
bloqueado igual. Un login exitoso es prueba suficiente de que no es un atacante.

### Ventana y limpieza

Los intentos se cuentan dentro de una ventana de 15 minutos: fuera de ella, el
contador vuelve a empezar. `AccountLimiter::purge_stale` borra filas sin
actividad (aún no está enganchado a un job; ver "Lo que falta").

## Capa 2 — Por IP (memoria)

`IpLimiter`, ventana fija, `APP_LOGIN_IP_MAX_PER_MINUTE` + un burst.

**Por qué en memoria**: es la barrera que corta **antes** de tocar la base y
antes de gastar un hash. Un atacante que hace 10 000 requests por segundo no debe
provocar 10 000 queries ni 10 000 argon2: eso ya sería un ataque exitoso sin
adivinar una sola contraseña. Se verificó: una vez bloqueado, la respuesta llega
en **0 ms** en vez de 400 ms.

**Costo asumido**: el estado vive en el proceso.
- No se comparte entre réplicas: con N réplicas el límite efectivo es N×.
- Se pierde al reiniciar.

Es aceptable porque esta capa es una **primera barrera**, no la defensa
principal. La que sobrevive reinicios y réplicas es la de por cuenta. Si algún
día hace falta un límite por IP distribuido, se mueve a Redis sin cambiar la
interfaz de `IpLimiter`.

### `X-Forwarded-For` se ignora, a propósito

El middleware lee la IP de `ConnectInfo` (la conexión TCP), **no** del header
`X-Forwarded-For`.

Ese header lo controla el cliente. Honrarlo sin un proxy confiable delante
permite a un atacante elegir una IP distinta en cada request y **anular el límite
por completo**. Es la forma más común de romper esta defensa sin darse cuenta.

Si algún día hay un reverse proxy (nginx, Caddy, un balanceador), hay que:

1. configurar en el proxy que **sobrescriba** `X-Forwarded-For` (no que lo
   agregue), y
2. agregar a la app una lista de proxies confiables, y solo entonces leer el
   header desde ellos.

## `Retry-After` es lo que hace el 429 accionable

Toda respuesta 429 trae `Retry-After` en segundos. Sin él, el cliente no sabe si
esperar 2 segundos o 2 días, y su opción por defecto es martillar — que es
exactamente lo que el límite intenta evitar.

## Configuración

| Variable | Default | Qué controla |
|---|---|---|
| `APP_LOGIN_MAX_FAILURES` | `5` | Intentos fallidos por cuenta antes de bloquear |
| `APP_LOGIN_BLOCK_MINUTES` | `5` | Duración del bloqueo |
| `APP_LOGIN_IP_MAX_PER_MINUTE` | `30` | Logins por minuto por IP (+ burst de 10) |

Un valor inválido **impide arrancar**, con el nombre exacto de la variable:

```
error fatal: error de configuración: APP_LOGIN_MAX_FAILURES debe ser al menos 1
error fatal: error de configuración: APP_LOGIN_MAX_FAILURES tiene un valor inválido ("cinco"): invalid digit found in string
```

## Qué NO hace

- **No protege contra credenciales filtradas.** Si `duenio@pyme.com` aparece en
  un dump de otra empresa y usa la misma contraseña, entra en el primer intento.
  Eso se ataca con 2FA y con verificación contra listas de contraseñas filtradas.
- **No protege contra phishing.** El dueño entrega su contraseña en una web
  falsa; el límite no se activa porque no hay intentos fallidos. También 2FA.
- **No distribuye el estado entre réplicas** (ver arriba).
- **No bloquea IPs de forma permanente**, ni debería: lista negra de IPs es un
  juego perdido contra NAT compartido y movilidad móvil.

## Lo que falta

- **Job de limpieza** de `login_attempts` y `sessions` vencidas. Las funciones
  existen (`purge_stale`, `purge_expired_sessions`) pero no hay scheduler. Una
  consulta cada hora en el arranque alcanza para una PyME.
- **2FA (TOTP)** para `owner`. Es la defensa que sí cierra el hueco de
  credenciales filtradas, que el rate limiting no toca.
- **Notificación** al dueño cuando su cuenta se bloquea repetidamente: el log ya
  lo registra (`WARN login bloqueado por intentos fallidos`), falta el aviso.
- **Lista de contraseñas comprometidas** (Have I Been Pwned, k-anonymity).
