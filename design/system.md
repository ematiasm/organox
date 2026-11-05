# Design system — organox

Captured from `frontend/src/app.css` (the project's only source of tokens) and
components in `frontend/src/lib/components/ui/` (shadcn-svelte). No values are
invented: everything below currently exists in those files.

Stack: **Tailwind v4** via `@tailwindcss/vite`, shadcn-svelte **vega** preset
with **olive** base color. Raw tokens live in `:root` (light) and `.dark`;
the `@theme inline` block exposes them as utilities (`bg-card`,
`text-muted-foreground`, `rounded-lg`…). **Inter Variable** is self-hosted
(`@fontsource-variable/inter`, imported in
`frontend/src/routes/+layout.svelte`).

Project convention: use shadcn tokens and do **not** invent custom variables.
The only exception is `--warn` (see below).

## Color

Literal values from `frontend/src/app.css` (`oklch(...)`), with approximate
sRGB equivalents for quick comparison.

| Token (utility) | Light | Dark | Use | Source |
|---|---|---|---|---|
| `--background` (`bg-background`) | `oklch(1 0 0)` ≈ `#ffffff` | `oklch(0.153 0.006 107.1)` ≈ `#0c0c09` | App background | `app.css:18/58` |
| `--foreground` (`text-foreground`) | `oklch(0.153 0.006 107.1)` ≈ `#0c0c09` | `oklch(0.988 0.003 106.5)` ≈ `#fbfbf9` | Main text | `app.css` |
| `--card` (`bg-card`) | `oklch(1 0 0)` | `oklch(0.228 0.013 107.4)` ≈ `#1d1d16` | Cards, tables, cart, rail | `app.css` |
| `--card-foreground` | `oklch(0.153 0.006 107.1)` | `oklch(0.988 0.003 106.5)` | Text on cards | `app.css` |
| `--popover` | `oklch(1 0 0)` | `oklch(0.228 0.013 107.4)` | Menus and dialogs | `app.css` |
| `--primary` (`bg-primary`) | `oklch(0.228 0.013 107.4)` ≈ `#1d1d16` | `oklch(0.93 0.007 106.5)` ≈ `#e8e8e3` | Primary action, active item, logo | `app.css` |
| `--primary-foreground` | `oklch(0.988 0.003 106.5)` | `oklch(0.228 0.013 107.4)` | Text on primary | `app.css` |
| `--secondary` | `oklch(0.966 0.005 106.5)` ≈ `#f4f4f0` | `oklch(0.286 0.016 107.4)` ≈ `#2b2b22` | Secondary button | `app.css` |
| `--muted` | `oklch(0.966 0.005 106.5)` | `oklch(0.286 0.016 107.4)` | Soft backgrounds, menu hover | `app.css` |
| `--muted-foreground` (`text-muted-foreground`) | `oklch(0.58 0.031 107.3)` ≈ `#7c7c67` | `oklch(0.737 0.021 106.9)` ≈ `#abab9c` | Secondary text, labels, SKU, inactive icons | `app.css` |
| `--accent` | `oklch(0.966 0.005 106.5)` | `oklch(0.286 0.016 107.4)` | Item hover and focus backgrounds | `app.css` |
| `--destructive` (`text-destructive`) | `oklch(0.577 0.245 27.325)` ≈ `#e7000b` | `oklch(0.704 0.191 22.216)` ≈ `#ff6467` | Error, sign out, negative quantity | `app.css` |
| `--border` (`border-border`) | `oklch(0.93 0.007 106.5)` ≈ `#e8e8e3` | `oklch(1 0 0 / 10%)` | 1px borders | `app.css` |
| `--input` | same as `--border` | `oklch(1 0 0 / 15%)` | Field border | `app.css` |
| `--ring` | `oklch(0.737 0.021 106.9)` | `oklch(0.58 0.031 107.3)` | Focus ring | `app.css` |
| `--warn` (`text-warn`) | `oklch(0.68 0.15 70)` ≈ `#d28500` | `oklch(0.78 0.14 75)` ≈ `#eba941` | Low stock (project token) | `app.css` |
| `--sidebar` | `oklch(0.988 0.003 106.5)` ≈ `#fbfbf9` | `oklch(0.228 0.013 107.4)` | Navigation rail background | `app.css` |
| `--sidebar-foreground` | `oklch(0.153 0.006 107.1)` | `oklch(0.988 0.003 106.5)` | Rail text | `app.css` |
| `--sidebar-accent` | `oklch(0.966 0.005 106.5)` | `oklch(0.286 0.016 107.4)` | Rail hover | `app.css` |
| `--sidebar-accent-foreground` | `oklch(0.228 0.013 107.4)` | `oklch(0.988 0.003 106.5)` | Rail text on hover | `app.css` |
| `--sidebar-border` | `oklch(0.93 0.007 106.5)` | `oklch(1 0 0 / 10%)` | Rail right border | `app.css` |
| `--chart-1..--chart-5` | grayscale (`0.87 → 0.269`) | identical | Charts only; **not** suitable for alerts | `app.css` |

`--warn` is the only custom token: the vega preset has no warning color and
`--chart-*` is grayscale, so it is not enough for "low stock".

### Verified contrast (WCAG AA)

Calculated from the sRGB values in the table:

| Pair | Light | Dark |
|---|---|---|
| `--foreground` on `--background` | 19.6:1 | 18.9:1 |
| `--muted-foreground` on `--card` | 4.3:1 | 7.3:1 |
| `--primary` on `--background` | 16.9:1 | 15.9:1 |
| `--primary-foreground` on `--primary` | 16.4:1 | 13.8:1 |
| `--destructive` on `--background` | 4.8:1 | 6.8:1 |
| `--sidebar-foreground` on `--sidebar` | 18.9:1 | 16.4:1 |

Two sync findings, **not corrected in `app.css`** (the sync only reads the
source; changing them is a separate decision):

1. **`--warn` in light mode fails AA for normal text**: `oklch(0.68 0.15 70)` =
   `#d28500` yields **2.96:1** on `--background` and **2.86:1** on `--sidebar`.
   It works for a dot, border or fill, not as a text color. To use it for text
   (the catalog's "low" label), `oklch(0.55 0.15 70)` = `#a75d00` reaches
   **4.99:1** and keeps the amber tone.
2. **`--muted-foreground` in light mode is borderline**: `#7c7c67` on `--card`
   yields **4.25:1**, just below AA's 4.5:1 for normal text. This is the
   unmodified shadcn vega preset; it affects SKU, labels and help text in
   `text-sm`. One darker step (`oklch(0.55 0.031 107.3)`) fixes it without
   changing the color family.

## Typography

| Property | Value | Source |
|---|---|---|
| Family | `'Inter Variable', ui-sans-serif, system-ui, sans-serif` (`--font-sans`, exposed as `font-sans`) | `app.css:132` + `routes/+layout.svelte:3` |
| Base | `15px / 1.5`, color `--foreground` | `app.css:140-146` |
| Page `h1` | `text-xl` (`1.25rem`), `font-semibold`, `tracking-tight` | `routes/(app)/products/+page.svelte:47` |
| Login `h1` | `text-lg` (`1.125rem`), `font-semibold`, `tracking-tight` | `routes/login/+page.svelte:55` |
| Table / UI text | `text-sm` | shadcn components |
| Labels, help, SKU | `text-xs` to `text-sm` with `text-muted-foreground` | `products/+page.svelte`, `app-shell.svelte` |
| Numbers | `tabular-nums` for prices, quantities and totals | `routes/(app)/operations/pos/+page.svelte` |
| Monospace | `font-mono` for SKU and shortcuts (`kbd`) | `products/+page.svelte`, POS |

No typographic scale is declared as a token: Tailwind utilities are used
(`text-xs`, `text-sm`, `text-lg`, `text-xl`).

## Spacing

There are no spacing tokens: the Tailwind v4 scale is used (`--spacing` base
`0.25rem`). Values observed on screens: `gap-0.5`, `gap-1.5`, `gap-2`,
`gap-4`, `p-4`, `p-8`, `p-10`, `px-2.5`, `px-3`, `py-1.5`, `py-2`.

## Radius

| Token | Value | Use | Source |
|---|---|---|---|
| `--radius` | `0.625rem` (`10px`) | System base | `app.css:38` |
| `rounded-sm` | `calc(--radius - 4px)` = `6px` | Menu items | `app.css:127` |
| `rounded-md` | `calc(--radius - 2px)` = `8px` | Buttons, inputs, alerts | `app.css:128` |
| `rounded-lg` | `--radius` = `10px` | Cards, tables, containers | `app.css:129` |
| `rounded-xl` | `calc(--radius + 4px)` = `14px` | Dialogs | `app.css:130` |
| `rounded-4xl` | full pill | `Badge` | `ui/badge/badge.svelte` |

## Surfaces and elevation

Elevation is communicated with **border + background**, not large shadows:

- `--card` on `--background` (cards, tables, cart).
- `shadow-xs` on `outline`/`secondary` buttons and inputs (defined in the
  component, not as a project token).
- `shadow-md` + `ring-1 ring-foreground/10` on menus and popovers
  (`ui/dropdown-menu/dropdown-menu-content.svelte`).

## Components

All come from `frontend/src/lib/components/ui/` (shadcn-svelte) except the rail.

| Component | Rules | Source |
|---|---|---|
| `default` button | `bg-primary text-primary-foreground`, `h-9`, `px-2.5`, `rounded-md`, `text-sm font-medium` | `ui/button/button.svelte` |
| `outline` button | `border-border bg-background hover:bg-muted`, `shadow-xs` | `ui/button/button.svelte` |
| `ghost` button | `hover:bg-muted`, no border | `ui/button/button.svelte` |
| `destructive` button | `bg-destructive/10 text-destructive hover:bg-destructive/20` | `ui/button/button.svelte` |
| Button sizes | `xs h-6`, `sm h-8`, `default h-9`, `lg h-10`, `icon size-9` | `ui/button/button.svelte` |
| Input | `h-9`, `rounded-md`, `border-input`, `bg-transparent`, `dark:bg-input/30`, focus `ring-3 ring-ring/50` | `ui/input/input.svelte` |
| Table | `rounded-lg border` container, `border-b` rows, `bg-muted/50` hover, `text-sm` | `ui/table/*.svelte` |
| Numeric cell | `text-right tabular-nums` | `routes/(app)/products/+page.svelte` |
| Badge | `rounded-4xl border px-2 text-xs font-medium`; `outline` for tags | `ui/badge/badge.svelte` |
| Dropdown menu | `bg-popover text-popover-foreground rounded-md p-1 shadow-md ring-1 ring-foreground/10`, item `rounded-sm px-2 py-1.5 text-sm`, `destructive` variant with `text-destructive` | `ui/dropdown-menu/*.svelte` |
| Error alert | `rounded-md border border-destructive/40 bg-destructive/10 text-destructive text-sm` | `routes/login/+page.svelte:61` |
| Visible focus | Shadcn ring (`focus-visible:ring-3 ring-ring/50`) on components; rail uses `outline: 2px solid var(--primary)` | components + `app-shell.svelte` |
| **Navigation rail** | `--sidebar` background, 64 px compact / 236 px expanded, `h-10 rounded-lg` items, active `color-mix(--primary 18%)` + `font-semibold`, groups with `0.7rem uppercase` label, 1px separator when compact | `lib/components/app-shell.svelte` |
| User menu | Button with avatar + email + role in the rail footer; opens `DropdownMenu` upward with theme and Sign out | `lib/components/app-shell.svelte` |

## Usage notes

- One rail for the whole app (sale, products, movements, users); see
  `odd/tasks/ui-admin.md` S7.
- No standalone hex colors on screens: always use a token utility
  (`bg-card`, `text-muted-foreground`, `border-border`).
- Money and quantity decimals are displayed as strings exactly as received from
  the API, with `tabular-nums`.
