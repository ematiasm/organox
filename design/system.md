# Sistema de diseño — organox

Capturado de `frontend/src/app.css` (única fuente de tokens del proyecto) y de
los componentes de `frontend/src/lib/components/ui/` (shadcn-svelte). No inventa
valores: todo lo que sigue existe hoy en esos archivos.

Stack: **Tailwind v4** vía `@tailwindcss/vite`, preset **vega** de shadcn-svelte
con base color **olive**. Los tokens crudos viven en `:root` (claro) y `.dark`;
el bloque `@theme inline` los expone como utilidades (`bg-card`,
`text-muted-foreground`, `rounded-lg`…). Tipografía **Inter Variable**
autohospedada (`@fontsource-variable/inter`, importada en
`frontend/src/routes/+layout.svelte`).

Convención del proyecto: se usan los tokens de shadcn y **no** se inventan
variables propias. La única excepción es `--warn` (ver más abajo).

## Color

Valores literales de `frontend/src/app.css` (`oklch(...)`) con su equivalente
sRGB aproximado para comparar rápido.

| Token (utilidad) | Claro | Oscuro | Uso | Fuente |
|---|---|---|---|---|
| `--background` (`bg-background`) | `oklch(1 0 0)` ≈ `#ffffff` | `oklch(0.153 0.006 107.1)` ≈ `#0c0c09` | Fondo de la app | `app.css:18/58` |
| `--foreground` (`text-foreground`) | `oklch(0.153 0.006 107.1)` ≈ `#0c0c09` | `oklch(0.988 0.003 106.5)` ≈ `#fbfbf9` | Texto principal | `app.css` |
| `--card` (`bg-card`) | `oklch(1 0 0)` | `oklch(0.228 0.013 107.4)` ≈ `#1d1d16` | Tarjetas, tablas, carrito, rail | `app.css` |
| `--card-foreground` | `oklch(0.153 0.006 107.1)` | `oklch(0.988 0.003 106.5)` | Texto sobre tarjeta | `app.css` |
| `--popover` | `oklch(1 0 0)` | `oklch(0.228 0.013 107.4)` | Menús y diálogos | `app.css` |
| `--primary` (`bg-primary`) | `oklch(0.228 0.013 107.4)` ≈ `#1d1d16` | `oklch(0.93 0.007 106.5)` ≈ `#e8e8e3` | Acción primaria, ítem activo, logo | `app.css` |
| `--primary-foreground` | `oklch(0.988 0.003 106.5)` | `oklch(0.228 0.013 107.4)` | Texto sobre primario | `app.css` |
| `--secondary` | `oklch(0.966 0.005 106.5)` ≈ `#f4f4f0` | `oklch(0.286 0.016 107.4)` ≈ `#2b2b22` | Botón secundario | `app.css` |
| `--muted` | `oklch(0.966 0.005 106.5)` | `oklch(0.286 0.016 107.4)` | Fondos suaves, hover de menú | `app.css` |
| `--muted-foreground` (`text-muted-foreground`) | `oklch(0.58 0.031 107.3)` ≈ `#7c7c67` | `oklch(0.737 0.021 106.9)` ≈ `#abab9c` | Texto secundario, labels, SKU, iconos inactivos | `app.css` |
| `--accent` | `oklch(0.966 0.005 106.5)` | `oklch(0.286 0.016 107.4)` | Hover de ítems y fondos de foco | `app.css` |
| `--destructive` (`text-destructive`) | `oklch(0.577 0.245 27.325)` ≈ `#e7000b` | `oklch(0.704 0.191 22.216)` ≈ `#ff6467` | Error, salir, cantidad negativa | `app.css` |
| `--border` (`border-border`) | `oklch(0.93 0.007 106.5)` ≈ `#e8e8e3` | `oklch(1 0 0 / 10%)` | Bordes de 1px | `app.css` |
| `--input` | igual a `--border` | `oklch(1 0 0 / 15%)` | Borde de campos | `app.css` |
| `--ring` | `oklch(0.737 0.021 106.9)` | `oklch(0.58 0.031 107.3)` | Anillo de foco | `app.css` |
| `--warn` (`text-warn`) | `oklch(0.68 0.15 70)` ≈ `#d28500` | `oklch(0.78 0.14 75)` ≈ `#eba941` | Stock bajo (token propio del proyecto) | `app.css` |
| `--sidebar` | `oklch(0.988 0.003 106.5)` ≈ `#fbfbf9` | `oklch(0.228 0.013 107.4)` | Fondo del rail de navegación | `app.css` |
| `--sidebar-foreground` | `oklch(0.153 0.006 107.1)` | `oklch(0.988 0.003 106.5)` | Texto del rail | `app.css` |
| `--sidebar-accent` | `oklch(0.966 0.005 106.5)` | `oklch(0.286 0.016 107.4)` | Hover del rail | `app.css` |
| `--sidebar-accent-foreground` | `oklch(0.228 0.013 107.4)` | `oklch(0.988 0.003 106.5)` | Texto del rail en hover | `app.css` |
| `--sidebar-border` | `oklch(0.93 0.007 106.5)` | `oklch(1 0 0 / 10%)` | Borde derecho del rail | `app.css` |
| `--chart-1..--chart-5` | escala de grises (`0.87 → 0.269`) | idéntica | Solo gráficos; **no** sirve para alertas | `app.css` |

`--warn` es el único token propio: el preset vega no trae color de advertencia y
los `--chart-*` son escala de grises, así que no alcanzan para "stock bajo".

### Contraste verificado (WCAG AA)

Calculado sobre los valores sRGB de la tabla:

| Par | Claro | Oscuro |
|---|---|---|
| `--foreground` sobre `--background` | 19.6:1 | 18.9:1 |
| `--muted-foreground` sobre `--card` | 4.3:1 | 7.3:1 |
| `--primary` sobre `--background` | 16.9:1 | 15.9:1 |
| `--primary-foreground` sobre `--primary` | 16.4:1 | 13.8:1 |
| `--destructive` sobre `--background` | 4.8:1 | 6.8:1 |
| `--sidebar-foreground` sobre `--sidebar` | 18.9:1 | 16.4:1 |

Dos hallazgos del sync, **sin corregir en `app.css`** (el sync solo lee la
fuente; cambiarlos es una decisión aparte):

1. **`--warn` en claro falla AA para texto normal**: `oklch(0.68 0.15 70)` =
   `#d28500` da **2.96:1** sobre `--background` y **2.86:1** sobre `--sidebar`.
   Sirve como punto, borde o relleno, no como color de texto. Si se quiere usar
   como texto (la etiqueta "bajo" del catálogo), `oklch(0.55 0.15 70)` =
   `#a75d00` llega a **4.99:1** y conserva el tono ámbar.
2. **`--muted-foreground` en claro queda al borde**: `#7c7c67` sobre `--card`
   da **4.25:1**, apenas debajo del 4.5:1 de AA para texto normal. Es el preset
   vega de shadcn tal cual; afecta a SKU, labels y textos de ayuda en `text-sm`.
   Un paso más oscuro (`oklch(0.55 0.031 107.3)`) lo resuelve sin cambiar la
   familia de color.

## Tipografía

| Propiedad | Valor | Fuente |
|---|---|---|
| Familia | `'Inter Variable', ui-sans-serif, system-ui, sans-serif` (`--font-sans`, expuesta como `font-sans`) | `app.css:132` + `routes/+layout.svelte:3` |
| Base | `15px / 1.5`, color `--foreground` | `app.css:140-146` |
| `h1` de página | `text-xl` (`1.25rem`), `font-semibold`, `tracking-tight` | `routes/(app)/products/+page.svelte:47` |
| `h1` de login | `text-lg` (`1.125rem`), `font-semibold`, `tracking-tight` | `routes/login/+page.svelte:55` |
| Texto de tabla / UI | `text-sm` | componentes shadcn |
| Labels, ayudas, SKU | `text-xs` a `text-sm` con `text-muted-foreground` | `products/+page.svelte`, `app-shell.svelte` |
| Números | `tabular-nums` en precios, cantidades y totales | `routes/(app)/operations/pos/+page.svelte` |
| Monoespaciada | `font-mono` para SKU y atajos (`kbd`) | `products/+page.svelte`, POS |

No hay escala tipográfica declarada como token: se usan las utilidades de
Tailwind (`text-xs`, `text-sm`, `text-lg`, `text-xl`).

## Espaciado

No hay tokens de espaciado: se usa la escala de Tailwind v4 (`--spacing` base
`0.25rem`). Valores observados en las pantallas: `gap-0.5`, `gap-1.5`, `gap-2`,
`gap-4`, `p-4`, `p-8`, `p-10`, `px-2.5`, `px-3`, `py-1.5`, `py-2`.

## Radio

| Token | Valor | Uso | Fuente |
|---|---|---|---|
| `--radius` | `0.625rem` (`10px`) | Base del sistema | `app.css:38` |
| `rounded-sm` | `calc(--radius - 4px)` = `6px` | Ítems de menú | `app.css:127` |
| `rounded-md` | `calc(--radius - 2px)` = `8px` | Botones, inputs, alertas | `app.css:128` |
| `rounded-lg` | `--radius` = `10px` | Tarjetas, tablas, contenedores | `app.css:129` |
| `rounded-xl` | `calc(--radius + 4px)` = `14px` | Diálogos | `app.css:130` |
| `rounded-4xl` | píldora completa | `Badge` | `ui/badge/badge.svelte` |

## Superficies y elevación

La elevación se comunica con **borde + fondo**, no con sombras grandes:

- `--card` sobre `--background` (tarjetas, tablas, carrito).
- `shadow-xs` en botones `outline`/`secondary` e inputs (definido en el
  componente, no como token del proyecto).
- `shadow-md` + `ring-1 ring-foreground/10` en menús y popovers
  (`ui/dropdown-menu/dropdown-menu-content.svelte`).

## Componentes

Todos vienen de `frontend/src/lib/components/ui/` (shadcn-svelte) salvo el rail.

| Componente | Reglas | Fuente |
|---|---|---|
| Botón `default` | `bg-primary text-primary-foreground`, `h-9`, `px-2.5`, `rounded-md`, `text-sm font-medium` | `ui/button/button.svelte` |
| Botón `outline` | `border-border bg-background hover:bg-muted`, `shadow-xs` | `ui/button/button.svelte` |
| Botón `ghost` | `hover:bg-muted`, sin borde | `ui/button/button.svelte` |
| Botón `destructive` | `bg-destructive/10 text-destructive hover:bg-destructive/20` | `ui/button/button.svelte` |
| Tamaños de botón | `xs h-6`, `sm h-8`, `default h-9`, `lg h-10`, `icon size-9` | `ui/button/button.svelte` |
| Input | `h-9`, `rounded-md`, `border-input`, `bg-transparent`, `dark:bg-input/30`, foco `ring-3 ring-ring/50` | `ui/input/input.svelte` |
| Tabla | contenedor `rounded-lg border`, filas `border-b`, hover `bg-muted/50`, `text-sm` | `ui/table/*.svelte` |
| Celda numérica | `text-right tabular-nums` | `routes/(app)/products/+page.svelte` |
| Badge | `rounded-4xl border px-2 text-xs font-medium`; `outline` para tags | `ui/badge/badge.svelte` |
| Menú desplegable | `bg-popover text-popover-foreground rounded-md p-1 shadow-md ring-1 ring-foreground/10`, ítem `rounded-sm px-2 py-1.5 text-sm`, variante `destructive` con `text-destructive` | `ui/dropdown-menu/*.svelte` |
| Alerta de error | `rounded-md border border-destructive/40 bg-destructive/10 text-destructive text-sm` | `routes/login/+page.svelte:61` |
| Foco visible | Anillo de shadcn (`focus-visible:ring-3 ring-ring/50`) en componentes; el rail usa `outline: 2px solid var(--primary)` | componentes + `app-shell.svelte` |
| **Rail de navegación** | `--sidebar` de fondo, 64 px compacto / 236 px expandido, ítems `h-10 rounded-lg`, activo `color-mix(--primary 18%)` + `font-semibold`, grupos con label `0.7rem uppercase`, separador de 1px cuando está compacto | `lib/components/app-shell.svelte` |
| Menú de usuario | Botón con avatar + email + rol en el pie del rail; abre `DropdownMenu` hacia arriba con tema y Salir | `lib/components/app-shell.svelte` |

## Notas de uso

- Un solo rail para toda la app (venta, productos, movimientos, usuarios); ver
  `odd/tasks/ui-admin.md` S7.
- Nada de colores hex sueltos en pantallas: siempre utilidad de token
  (`bg-card`, `text-muted-foreground`, `border-border`).
- Los decimales de dinero y cantidad se muestran como string tal como llegan de
  la API, con `tabular-nums`.
