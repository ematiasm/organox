-- Rebanada 1: no hay dominio todavía.
--
-- Esta migración existe para probar el ciclo completo (sqlx migrate run,
-- _sqlx_migrations, tests con DB). La primera tabla real llega en la Rebanada 2
-- (users) y NO debe editarse esta migración: se agrega una nueva.
--
-- Regla del proyecto: una migración ya aplicada es inmutable. Se corrige con
-- otra migración, nunca editando la anterior.

SELECT 1;
