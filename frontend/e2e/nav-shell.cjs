/*
 * Verificación de UI para "sistema de navegación único".
 *
 * Reglas observables que comprueba (no detalles internos):
 *   R1  Toda pantalla privada muestra EXACTAMENTE UNA navegación lateral, y es
 *       la misma (mismo contenido de enlaces) en venta, productos y usuarios.
 *   R2  El permiso de venta se inicia desde un botón del rail y no desde un
 *       enlace de la página, así el rail es la única navegación.
 *   R3  El rail se expande y muestra icono + nombre; el estado se mantiene al
 *       navegar.
 *   R4  "Salir" vive en el menú del usuario, no suelto en el rail, y cierra la
 *       sesión.
 *
 * Uso: node e2e/nav-shell.cjs  (requiere `vite dev` en :3000 y E2E_EMAIL/E2E_PASSWORD)
 */
const { chromium } = require('playwright');

const BASE = process.env.E2E_BASE ?? 'http://localhost:3000';
const EMAIL = process.env.E2E_EMAIL;
const PASSWORD = process.env.E2E_PASSWORD;

if (!EMAIL || !PASSWORD) {
	console.error('faltan E2E_EMAIL / E2E_PASSWORD');
	process.exit(2);
}

const results = [];
function check(rule, name, ok, detail) {
	results.push({ rule, name, ok, detail });
	console.log(`${ok ? 'PASS' : 'FAIL'} [${rule}] ${name}${ok ? '' : ` — ${detail}`}`);
}

/**
 * Navegaciones laterales visibles: landmarks con enlaces, sin contar un
 * landmark anidado dentro de otro (un `nav` dentro de un `aside` es la misma
 * navegación, no dos).
 */
async function lateralNavs(page) {
	return page.evaluate(() => {
		const candidates = [...document.querySelectorAll('nav, aside[aria-label], aside')];
		const top = candidates.filter((el) => !candidates.some((other) => other !== el && other.contains(el)));
		return top
			.map((el) => {
				const box = el.getBoundingClientRect();
				const links = [...el.querySelectorAll('a[href]')].map((a) => a.textContent.trim());
				return {
					visible: box.width > 0 && box.height > 0,
					tag: el.tagName.toLowerCase(),
					links,
					label: (el.getAttribute('aria-label') ?? '').trim()
				};
			})
			.filter((n) => n.visible && n.links.length > 0);
	});
}

async function login(page) {
	await page.goto(`${BASE}/login`, { waitUntil: 'networkidle' });
	const inputs = page.locator('input');
	await inputs.nth(0).fill(EMAIL);
	await inputs.nth(1).fill(PASSWORD);
	await page.locator('button[type=submit]').first().click();
	await page.waitForURL(/\/(products|operations)/, { timeout: 15000 });
}

(async () => {
	const browser = await chromium.launch();
	const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 } });
	const page = await ctx.newPage();
	const errors = [];
	page.on('pageerror', (e) => errors.push(String(e)));

	await login(page);

	// `/pos` queda como redirección: el destino real es el nuevo path.
	const screens = ['/products', '/users', '/operations/pos', '/operations/movements'];
	const navSets = {};

	for (const path of screens) {
		await page.goto(`${BASE}${path}`, { waitUntil: 'networkidle' });
		await page.waitForTimeout(400);
		const navs = await lateralNavs(page);
		navSets[path] = navs;
		check('R1', `${path}: una sola navegación lateral`, navs.length === 1, `encontradas ${navs.length}: ${JSON.stringify(navs)}`);
		if (navs.length === 1) {
			const links = navs[0].links.join(', ');
			const expected = 'Venta, Productos, Movimientos, Usuarios';
			check('R1', `${path}: navegación canónica`, links === expected, `"${links}" != "${expected}"`);
		}
	}

	// R2: iniciar venta desde el rail
	await page.goto(`${BASE}/products`, { waitUntil: 'networkidle' });
	const startSale = page.locator('nav a[href="/operations/pos"], nav button[href="/operations/pos"], a[aria-label="Venta"], button[aria-label="Venta"]').first();
	check('R2', 'el rail tiene la acción de iniciar venta', (await startSale.count()) > 0, 'no existe el control en el rail');
	const pageLinksToPos = await page.locator('main a[href*="pos"], main button:has-text("venta")').count();
	check('R2', 'la página no repite el acceso a venta', pageLinksToPos === 0, `hay ${pageLinksToPos} enlace(s) a venta fuera del rail`);

	if ((await startSale.count()) > 0) {
		await startSale.click();
		await page.waitForURL(/\/operations\/pos/, { timeout: 10000 });
		check('R2', 'la acción del rail lleva a la venta', page.url().includes('/operations/pos'), page.url());
	}

	// R4: Salir está en el menú del usuario
	const looseLogout = await page.locator('nav button:has-text("Salir"), aside button:has-text("Salir")').count();
	const userButton = page.locator('aside button[aria-haspopup="menu"]').first();
	check('R4', 'no hay "Salir" suelto en el rail', looseLogout === 0, `${looseLogout} botón(es) sueltos`);
	check('R4', 'el rail tiene el botón de usuario', (await userButton.count()) > 0, 'no hay botón de usuario');
	if ((await userButton.count()) > 0) {
		await userButton.click();
		const logoutItem = page.getByRole('menuitem', { name: /salir/i }).first();
		check('R4', 'el menú del usuario ofrece Salir', (await logoutItem.count()) > 0, 'no hay ítem Salir');

		// R3: expandir el rail muestra icono + nombre
		await page.keyboard.press('Escape');
		const toggle = page.locator('aside button[aria-controls]').first();
		const collapsedWidth = await page.locator('aside').first().evaluate((el) => el.getBoundingClientRect().width);
		if ((await toggle.count()) > 0) {
			await toggle.click();
			await page.waitForTimeout(250);
			const expandedWidth = await page.locator('aside').first().evaluate((el) => el.getBoundingClientRect().width);
			check('R3', 'el rail se expande', expandedWidth > collapsedWidth + 40, `${collapsedWidth} -> ${expandedWidth}`);
			const visibleLabels = await page.locator('aside nav a').evaluateAll((els) =>
				els.filter((el) => el.getBoundingClientRect().width > 40).map((el) => el.textContent.trim())
			);
			check('R3', 'el rail expandido muestra nombres', visibleLabels.includes('Productos') && visibleLabels.includes('Usuarios'), JSON.stringify(visibleLabels));

			// persistencia al navegar
			await page.locator('aside nav a[href="/products"]').first().click();
			await page.waitForTimeout(500);
			const stillExpanded = await page.locator('aside').first().evaluate((el) => el.getBoundingClientRect().width);
			check('R3', 'la expansión se mantiene al navegar', stillExpanded > collapsedWidth + 40, `${stillExpanded}`);
		} else {
			check('R3', 'el rail tiene control de expandir', false, 'no hay botón con aria-controls');
		}

		// R4 cierre de sesión real
		await page.goto(`${BASE}/products`, { waitUntil: 'networkidle' });
		await page.locator('aside button[aria-haspopup="menu"]').first().click();
		await page.getByRole('menuitem', { name: /salir/i }).first().click();
		await page.waitForURL(/\/login/, { timeout: 10000 }).catch(() => {});
		check('R4', 'Salir cierra la sesión', page.url().includes('/login'), page.url());
	}

	check('R1', 'sin errores de página', errors.length === 0, errors.join(' | '));

	await browser.close();

	const failed = results.filter((r) => !r.ok);
	console.log(`\n${results.length - failed.length}/${results.length} checks OK`);
	process.exit(failed.length === 0 ? 0 : 1);
})().catch((e) => {
	console.error('ERROR', e);
	process.exit(3);
});
