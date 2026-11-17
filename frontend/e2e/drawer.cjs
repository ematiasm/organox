/*
 * UI verification for the product drawer (issue #33).
 * D1: clicking a product row opens the drawer with that product's data and
 *     movements, and the URL carries `?id=`.
 * D2: closing (Escape or Back) hides the drawer.
 * D3: "New product" opens the empty form; a create lands on the new id.
 * D4: Edit saves a PATCH and the drawer reflects the change.
 * D5: Deactivate confirms in two steps and the row leaves the list.
 *
 * Usage: E2E_BASE=http://localhost:5173 node e2e/drawer.cjs
 * Requires a running dev server (or built app) and E2E_EMAIL/E2E_PASSWORD.
 */
const { chromium } = require('playwright');

const BASE = process.env.E2E_BASE ?? 'http://localhost:3000';
const EMAIL = process.env.E2E_EMAIL;
const PASSWORD = process.env.E2E_PASSWORD;

if (!EMAIL || !PASSWORD) {
	console.error('missing E2E_EMAIL / E2E_PASSWORD');
	process.exit(2);
}

const results = [];
function check(rule, name, ok, detail) {
	results.push({ rule, name, ok, detail });
	console.log(`${ok ? 'PASS' : 'FAIL'} [${rule}] ${name}${ok ? '' : ` — ${detail}`}`);
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
	page.on('console', (m) => {
		if (m.type() === 'error') errors.push(m.text());
	});

	try {
		await login(page);
		await page.goto(`${BASE}/products`, { waitUntil: 'networkidle' });

		const sheet = page.locator('[data-slot="sheet-content"]');
		await sheet.waitFor({ state: 'hidden', timeout: 5000 });

		// --- D1: row click opens the drawer ---------------------------------
		const firstRowButton = page.locator('tbody tr button').first();
		const firstRowName = await firstRowButton.textContent();
		await firstRowButton.click();
		try {
			await sheet.waitFor({ state: 'visible', timeout: 5000 });
			const title = await page.locator('[data-slot="sheet-title"]').first().textContent();
			check('D1', 'row click opens the drawer', title === firstRowName, `title=${title}`);
			check(
				'D1',
				'drawer shows the movements section',
				(await page.locator('text=Stock movements').count()) > 0,
				'no movements heading'
			);
			check(
				'D1',
				'URL carries ?id=',
				page.url().includes('id='),
				`url=${page.url()}`
			);
			// The sheet animates in (200ms, slide-from-right). Playwright calls an
			// element visible as soon as it has a box, so a screenshot taken right
			// after the assertions catches the panel mid-translate, off-frame. Wait
			// until its right edge is actually inside the viewport before capturing,
			// or the screenshot contradicts the passing checks.
			await page.waitForFunction(
				() => {
					const el = document.querySelector('[data-slot="sheet-content"]');
					if (!el) return false;
					const box = el.getBoundingClientRect();
					return box.right <= window.innerWidth + 1 && box.width > 0;
				},
				{ timeout: 5000 }
			);
		} catch (e) {
			check('D1', 'row click opens the drawer', false, String(e).split('\n')[0]);
		}
		await page.screenshot({ path: '/tmp/e2e-drawer-view.png' });

		// --- D2: Escape closes ----------------------------------------------
		await page.keyboard.press('Escape');
		await page.waitForTimeout(300);
		check(
			'D2',
			'Escape closes the drawer',
			(await sheet.count()) === 0 || !(await sheet.first().isVisible()),
			'sheet still visible'
		);

		// --- D3: New product -> create lands on the new id --------------------
		await page.goto(`${BASE}/products`, { waitUntil: 'networkidle' });
		await page.locator('button', { hasText: 'New product' }).first().click();
		try {
			await sheet.waitFor({ state: 'visible', timeout: 5000 });
			const title = await page.locator('[data-slot="sheet-title"]').first().textContent();
			check('D3', 'New product opens the empty form', title === 'New product', `title=${title}`);

			const stamp = Date.now().toString().slice(-6);
			const name = `E2E drawer ${stamp}`;
			await page.fill('#name', name);
			await page.fill('#sku', `E2E-${stamp}`);
			await page.fill('#price', '42.50');
			await page.locator('button[type=submit]').first().click();

			await page.waitForURL((u) => u.searchParams.has('id'), { timeout: 8000 });
			await page.waitForTimeout(600);
			const title2 = await page.locator('[data-slot="sheet-title"]').first().textContent();
			check(
				'D3',
				'create lands on the new product drawer',
				title2 === name,
				`title=${title2}`
			);
			check(
				'D3',
				'?new was replaced by ?id',
				!page.url().includes('new=1'),
				`url=${page.url()}`
			);
		} catch (e) {
			check('D3', 'create flow', false, String(e).split('\n')[0]);
		}
		await page.screenshot({ path: '/tmp/e2e-drawer-create.png' });

		// --- D4: Edit saves and the drawer reflects it -------------------------
		try {
			await page.locator('button', { hasText: 'Edit' }).first().click();
			await page.waitForSelector('#name', { timeout: 5000 });
			const edited = `${await page.inputValue('#name')} v2`;
			await page.fill('#name', edited);
			await page.locator('button[type=submit]').first().click();
			await page.waitForTimeout(900);
			const title = await page.locator('[data-slot="sheet-title"]').first().textContent();
			check('D4', 'edit saves and the drawer reflects it', title === edited, `title=${title}`);
		} catch (e) {
			check('D4', 'edit flow', false, String(e).split('\n')[0]);
		}
		await page.screenshot({ path: '/tmp/e2e-drawer-edit.png' });

		// --- D5: two-step deactivate -------------------------------------------
		try {
			const editedName = await page.locator('[data-slot="sheet-title"]').first().textContent();
			await page.locator('button', { hasText: 'Deactivate' }).first().click();
			const confirm = page.locator('button', { hasText: 'Confirm deactivation' });
			await confirm.waitFor({ timeout: 5000 });
			await confirm.click();
			await page.waitForTimeout(900);
			check(
				'D5',
				'deactivation closes the drawer',
				(await sheet.count()) === 0 || !(await sheet.first().isVisible()),
				'sheet still visible'
			);
			await page.goto(`${BASE}/products?search=${encodeURIComponent(editedName)}`, {
				waitUntil: 'networkidle'
			});
			await page.waitForTimeout(400);
			// Count rows, not text: the empty state renders the search term inside
			// "No products match …", so a text check would always find the name.
			const rows = await page.locator('tbody tr').count();
			check('D5', 'the deactivated row left the list', rows === 0, `rows=${rows}`);
		} catch (e) {
			check('D5', 'deactivate flow', false, String(e).split('\n')[0]);
		}

		if (errors.length > 0) {
			console.log('\nbrowser errors:');
			for (const e of errors) console.log('  ' + e);
		} else {
			console.log('\nbrowser errors: none');
		}
		const failed = results.filter((r) => !r.ok).length;
		console.log(`\n${results.length - failed}/${results.length} checks passed`);
		process.exit(failed > 0 || errors.length > 0 ? 1 : 0);
	} finally {
		await browser.close();
	}
})();
