/*
 * UI verification for the customers screen (odd/tasks/customers.md, T3).
 *
 * N:  the rail links to Customers in the catalog group.
 * C1: "New customer" opens the empty drawer; a create lands on the new id and
 *     the row appears in the list.
 * C2: Edit saves one field (phone) and it survives a reload.
 * C3: the search box matches by name and by document.
 * C4: deactivation confirms in two steps, the row leaves the list, and comes
 *     back only under "Include deactivated".
 * C5: a duplicate document shows the server's `problem.detail`.
 *
 * Usage: E2E_BASE=http://localhost:3000 node e2e/customers.cjs
 * Requires a running app serving the API, and E2E_EMAIL/E2E_PASSWORD for an
 * owner or manager account.
 */
const { chromium } = require('playwright');

const BASE = process.env.E2E_BASE ?? 'http://localhost:3000';
const EMAIL = process.env.E2E_EMAIL;
const PASSWORD = process.env.E2E_PASSWORD;

if (!EMAIL || !PASSWORD) {
	console.error('missing E2E_EMAIL / E2E_PASSWORD');
	process.exit(2);
}

const stamp = Date.now().toString().slice(-6);
const NAME = `E2E customer ${stamp}`;
const DOC = `E2E-${stamp}`;
const PHONE = `555-01${stamp.slice(-2)}`;

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

/** Open the drawer for the row whose text contains `name`. */
async function openCustomer(page, name) {
	const row = page.locator('tbody tr', { hasText: name }).first();
	await row.getByRole('button').first().click();
	await page.locator('[data-slot="sheet-content"]').waitFor({ state: 'visible', timeout: 5000 });
}

(async () => {
	const browser = await chromium.launch();
	const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 } });
	const page = await ctx.newPage();
	const errors = [];
	page.on('pageerror', (e) => errors.push(String(e)));
	page.on('console', (m) => {
		// The browser logs every non-2xx fetch as a console error, and this probe
		// deliberately triggers a 409 (C5) to read the rendered detail; that
		// resource line is not a page error. Anything else still is.
		if (m.type() === 'error' && !/status of 409 \(Conflict\)/.test(m.text())) errors.push(m.text());
	});

	try {
		await login(page);

		// --- N: the rail links to Customers -----------------------------------
		await page.goto(`${BASE}/customers`, { waitUntil: 'networkidle' });
		const nav = page.locator('nav a', { hasText: 'Customers' });
		check(
			'N',
			'the rail has a Customers entry',
			(await nav.count()) === 1,
			`count=${await nav.count()}`
		);

		// --- C1: create from the drawer ----------------------------------------
		const sheet = page.locator('[data-slot="sheet-content"]');
		await sheet.waitFor({ state: 'hidden', timeout: 5000 });

		await page.getByRole('button', { name: 'New customer' }).click();
		try {
			await sheet.waitFor({ state: 'visible', timeout: 5000 });
			const title = await page.locator('[data-slot="sheet-title"]').first().textContent();
			check('C1', 'New customer opens the empty form', title === 'New customer', `title=${title}`);

			await page.fill('#name', NAME);
			await page.fill('#tax_id', DOC);
			await page.fill('#email', `e2e-${stamp}@example.com`);
			await page.fill('#phone', PHONE);
			await page.locator('button[type=submit]').first().click();

			await page.waitForURL((u) => u.searchParams.has('id'), { timeout: 8000 });
			await page.waitForTimeout(600);
			const title2 = await page.locator('[data-slot="sheet-title"]').first().textContent();
			check('C1', 'create lands on the new customer drawer', title2 === NAME, `title=${title2}`);
			check('C1', '?new was replaced by ?id', !page.url().includes('new=1'), `url=${page.url()}`);
		} catch (e) {
			check('C1', 'create flow', false, String(e).split('\n')[0]);
		}

		await page.goto(`${BASE}/customers`, { waitUntil: 'networkidle' });
		await page.waitForFunction(
			(name) => document.body.innerText.includes(name),
			NAME,
			{ timeout: 10000 }
		);
		const row = page.locator('tbody tr', { hasText: NAME }).first();
		const rowText = await row.innerText();
		check('C1', 'the new customer is in the list', rowText.includes(DOC), rowText.replace(/\n/g, ' '));

		// --- C2: edit one field, reload, still there ---------------------------
		try {
			await openCustomer(page, NAME);
			await page.getByRole('button', { name: 'Edit' }).click();
			await page.waitForSelector('#phone', { timeout: 5000 });
			const newPhone = `${PHONE}9`;
			await page.fill('#phone', newPhone);
			await page.getByRole('button', { name: 'Save changes' }).click();
			await page.waitForTimeout(900);

			// A reload proves persistence, not just local state. The URL still
			// carries ?id, so the drawer reopens reading fresh server data; clicking
			// the row again would fight the sheet overlay covering the table.
			await page.reload({ waitUntil: 'networkidle' });
			await page.locator('[data-slot="sheet-content"]').waitFor({ state: 'visible', timeout: 5000 });
			const drawerText = await page.locator('[data-slot="sheet-content"]').first().innerText();
			check(
				'C2',
				'the edited phone survives a reload',
				drawerText.includes(newPhone),
				`drawer does not show ${newPhone}`
			);
		} catch (e) {
			check('C2', 'edit flow', false, String(e).split('\n')[0]);
		}

		// --- C3: search by name and by document --------------------------------
		await page.goto(`${BASE}/customers?search=${encodeURIComponent(NAME)}`, {
			waitUntil: 'networkidle'
		});
		await page.waitForTimeout(400);
		check(
			'C3',
			'search by name finds the customer',
			(await page.locator('tbody tr', { hasText: NAME }).count()) === 1,
			`rows=${await page.locator('tbody tr', { hasText: NAME }).count()}`
		);

		await page.goto(`${BASE}/customers?search=${encodeURIComponent(DOC)}`, {
			waitUntil: 'networkidle'
		});
		await page.waitForTimeout(400);
		check(
			'C3',
			'search by document finds the customer',
			(await page.locator('tbody tr', { hasText: NAME }).count()) === 1,
			`rows=${await page.locator('tbody tr', { hasText: NAME }).count()}`
		);

		// --- C4: deactivate in two steps, list and toggle ------------------------
		try {
			await page.goto(`${BASE}/customers?search=${encodeURIComponent(NAME)}`, {
				waitUntil: 'networkidle'
			});
			await openCustomer(page, NAME);
			await page.getByRole('button', { name: 'Deactivate' }).click();
			const confirm = page.getByRole('button', { name: 'Confirm deactivation' });
			await confirm.waitFor({ timeout: 5000 });
			await confirm.click();
			await page.waitForTimeout(900);

			// Count rows, not text: the empty state renders the search term, so a
			// text check would always find the name.
			const activeRows = await page.locator('tbody tr', { hasText: NAME }).count();
			check(
				'C4',
				'the deactivated customer left the list',
				activeRows === 0,
				`rows=${activeRows}`
			);

			await page.locator('#include-inactive').check();
			await page.waitForFunction(
				(name) =>
					Array.from(document.querySelectorAll('tbody tr')).some(
						(tr) => tr.innerText.includes(name) && tr.innerText.includes('Inactive')
					),
				NAME,
				{ timeout: 10000 }
			);
			check(
				'C4',
				'Include deactivated brings the row back as Inactive',
				true,
				'row missing after toggle'
			);
			check(
				'C4',
				'the toggle is URL state',
				page.url().includes('include_inactive=1'),
				`url=${page.url()}`
			);
		} catch (e) {
			check('C4', 'deactivate flow', false, String(e).split('\n')[0]);
		}

		// --- C5: a duplicate document shows the server's detail -----------------
		try {
			await page.goto(`${BASE}/customers`, { waitUntil: 'networkidle' });
			await page.getByRole('button', { name: 'New customer' }).click();
			await page.locator('[data-slot="sheet-content"]').waitFor({ state: 'visible', timeout: 5000 });
			await page.fill('#name', `E2E duplicate ${stamp}`);
			await page.fill('#tax_id', DOC);
			await page.locator('button[type=submit]').first().click();
			await page.waitForFunction(
				() => document.body.innerText.includes('the document is already in use'),
				{ timeout: 10000 }
			);
			check(
				'C5',
				'a duplicate document shows the server detail',
				true,
				'detail not shown'
			);
			check(
				'C5',
				'the failed create stays on the form',
				page.url().includes('new=1'),
				`url=${page.url()}`
			);
		} catch (e) {
			check('C5', 'duplicate document flow', false, String(e).split('\n')[0]);
		}

		check('X', 'no page errors', errors.length === 0, errors.join(' | '));
	} finally {
		await browser.close();
	}

	const failed = results.filter((result) => !result.ok);
	console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
	process.exit(failed.length === 0 ? 0 : 1);
})();
