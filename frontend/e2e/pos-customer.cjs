/*
 * UI verification for the customer at the point of sale and in the sales
 * history (odd/tasks/customers.md, T4).
 *
 * S4: the POS attaches a customer to the draft it opens on load and clears it
 *     again through PUT /sales/{id}/customer; a customer deactivated in
 *     another tab is refused with the server's detail, and the draft keeps
 *     its previous customer.
 * S5: the history shows the name frozen at checkout: renaming the customer
 *     afterwards must not rewrite the completed row.
 * S10: the selected customer is visible next to the ticket total, and the
 *     sales history shows the customer name, or a dash for an anonymous sale.
 *
 * Usage: E2E_BASE=http://localhost:3000 E2E_EMAIL=owner@example.com \
 *   E2E_PASSWORD=... node e2e/pos-customer.cjs
 * Requires a running app. Every name is stamped so repeated runs against the
 * development database create independent sales and customers.
 */
const { chromium } = require('playwright');
const { randomBytes } = require('node:crypto');

const BASE = process.env.E2E_BASE ?? 'http://localhost:3000';
const EMAIL = process.env.E2E_EMAIL;
const PASSWORD = process.env.E2E_PASSWORD;

if (!EMAIL || !PASSWORD) {
	console.error('missing E2E_EMAIL / E2E_PASSWORD');
	process.exit(2);
}

const stamp = Date.now().toString().slice(-6) + randomBytes(2).toString('hex');
const PRODUCT = `POS customer probe ${stamp}`;
const SKU = `PC-${stamp}`;
const WALLET = `POS customer wallet ${stamp}`;
const METHOD = `POS customer method ${stamp}`;
const ANA = `POS customer Ana ${stamp}`;
const ANA_DOC = `DOC-ANA-${stamp}`;
const BOB = `POS customer Bob ${stamp}`;
// S5 renames Bob after checkout: the completed row must keep the old name.
const BOB_RENAMED = `POS customer Bob renamed ${stamp}`;

const results = [];
function check(rule, name, ok, detail) {
	results.push({ rule, name, ok, detail });
	console.log(`${ok ? 'PASS' : 'FAIL'} [${rule}] ${name}${ok ? '' : ` — ${detail}`}`);
}

/** Fail one rule without abandoning the checks that follow it. */
async function attempt(rule, name, action) {
	try {
		const result = await action();
		check(rule, name, result.ok === true, result.detail ?? '');
		return result.ok === true;
	} catch (error) {
		check(rule, name, false, String(error).split('\n')[0]);
		return false;
	}
}

/** Call the API with the browser's own session, as the sales probe does. */
async function api(page, path, { method = 'GET', body } = {}) {
	return page.evaluate(
		async ({ path, method, body }) => {
			const response = await fetch(path, {
				method,
				credentials: 'same-origin',
				headers: body === undefined ? undefined : { 'content-type': 'application/json' },
				body: body === undefined ? undefined : JSON.stringify(body)
			});
			const text = await response.text();
			let payload = null;
			try {
				payload = text ? JSON.parse(text) : null;
			} catch {
				payload = text;
			}
			return { status: response.status, body: payload };
		},
		{ path, method, body }
	);
}

async function login(page) {
	await page.goto(`${BASE}/login`, { waitUntil: 'networkidle' });
	const inputs = page.locator('input');
	await inputs.nth(0).fill(EMAIL);
	await inputs.nth(1).fill(PASSWORD);
	await page.locator('button[type=submit]').first().click();
	await page.waitForURL(/\/(products|operations)/, { timeout: 15000 });
}

/** Pick an option from the shadcn Select whose trigger is `triggerId`. */
async function selectOption(page, triggerId, name) {
	await page.locator(`#${triggerId}`).click();
	await page.getByRole('option', { name: new RegExp(name) }).first().click();
}

/** Wait until the customer trigger shows exactly `name`. */
async function customerShows(page, name) {
	await page.waitForFunction(
		(value) => document.querySelector('.cust-trigger')?.textContent?.trim() === value,
		name,
		{ timeout: 15000 }
	);
}

(async () => {
	const browser = await chromium.launch();
	const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 } });
	const page = await ctx.newPage();
	const errors = [];
	let paymentMethodId = null;
	// The refused attach (a customer deactivated behind the UI's back) elicits
	// a 409. Chromium reports that fetch as a console resource error; account
	// for that one expected error without hiding others.
	let expectedConflicts = 0;
	page.on('pageerror', (e) => errors.push(String(e)));
	page.on('console', (m) => {
		if (m.type() !== 'error') return;
		if (expectedConflicts > 0 && m.text().includes('409 (Conflict)')) {
			expectedConflicts -= 1;
			return;
		}
		errors.push(m.text());
	});

	try {
		await login(page);

		// A previous interrupted run leaves an open draft for this dedicated
		// e2e user. Cancel it so the POS opens a fresh anonymous draft.
		const staleDraft = await api(page, '/api/v1/sales/draft', { method: 'POST' });
		if (staleDraft.body?.id) {
			await api(page, `/api/v1/sales/${staleDraft.body.id}/cancel`, { method: 'POST' });
		}

		// --- Setup: one product with stock, one method, two customers ------
		const product = await api(page, '/api/v1/products', {
			method: 'POST',
			body: { kind: 'product', name: PRODUCT, sku: SKU, price: '12.50' }
		});
		if (
			!(await attempt('S4', 'a product is seeded through the API', async () => ({
				ok: product.status === 201 && typeof product.body?.id === 'string',
				detail: JSON.stringify(product)
			})))
		)
			throw new Error('setup failed: see the checks above');
		const productId = product.body.id;

		const stock = await api(page, `/api/v1/products/${productId}/movements`, {
			method: 'POST',
			body: {
				reason: 'purchase',
				quantity: '10',
				unit_cost: '12.50',
				note: `POS customer probe stock ${stamp}`
			}
		});
		if (
			!(await attempt('S4', 'the product has stock', async () => ({
				ok: stock.status === 201,
				detail: JSON.stringify(stock)
			})))
		)
			throw new Error('setup failed: see the checks above');

		const wallet = await api(page, '/api/v1/wallets', {
			method: 'POST',
			body: { name: WALLET }
		});
		if (
			!(await attempt('S4', 'a wallet is seeded through the API', async () => ({
				ok: wallet.status === 201 && typeof wallet.body?.id === 'string',
				detail: JSON.stringify(wallet)
			})))
		)
			throw new Error('setup failed: see the checks above');

		const method = await api(page, '/api/v1/payment-methods', {
			method: 'POST',
			body: { name: METHOD, wallet_id: wallet.body.id }
		});
		paymentMethodId = method.body?.id ?? null;
		if (
			!(await attempt('S4', 'a payment method is seeded through the API', async () => ({
				ok: method.status === 201 && typeof method.body?.id === 'string',
				detail: JSON.stringify(method)
			})))
		)
			throw new Error('setup failed: see the checks above');

		const ana = await api(page, '/api/v1/customers', {
			method: 'POST',
			body: { name: ANA, tax_id: ANA_DOC }
		});
		const bob = await api(page, '/api/v1/customers', {
			method: 'POST',
			body: { name: BOB }
		});
		if (
			!(await attempt('S4', 'two customers are seeded through the API', async () => ({
				ok:
					ana.status === 201 &&
					typeof ana.body?.id === 'string' &&
					bob.status === 201 &&
					typeof bob.body?.id === 'string',
				detail: `ana=${JSON.stringify(ana)} bob=${JSON.stringify(bob)}`
			})))
		)
			throw new Error('setup failed: see the checks above');
		const anaId = ana.body.id;
		const bobId = bob.body.id;

		// --- S10: the picker on the draft the POS opens --------------------
		await page.goto(`${BASE}/operations/pos`, { waitUntil: 'networkidle' });
		await attempt('S10', 'the draft opens with no customer', async () => {
			const text = await page.locator('.cust-trigger').innerText();
			return { ok: text.trim() === 'No customer', detail: text };
		});

		// The POS opened the draft on load; asking the API again returns that
		// same draft, so its id is the one the picker writes to.
		const draft = await api(page, '/api/v1/sales/draft', { method: 'POST' });
		const saleId = draft.body?.id;
		await attempt('S4', 'the POS draft is reachable through the API', async () => ({
			// The POS already opened the draft on load, so this POST reuses it
			// (200) instead of creating one (201).
			ok:
				typeof saleId === 'string' &&
				(draft.status === 201 || (draft.status === 200 && draft.body?.reused === true)),
			detail: JSON.stringify(draft)
		}));

		await attempt('S10', 'search by name finds the customer', async () => {
			await page.locator('.cust-trigger').click();
			await page.locator('.cust-panel input').fill(ANA);
			const row = page.locator('.cust-panel .cust-option', { hasText: ANA }).first();
			await row.waitFor({ state: 'visible', timeout: 15000 });
			return { ok: true, detail: '' };
		});

		await attempt('S10', 'search by document finds the customer', async () => {
			await page.locator('.cust-panel input').fill(ANA_DOC);
			const row = page.locator('.cust-panel .cust-option', { hasText: ANA }).first();
			await row.waitFor({ state: 'visible', timeout: 15000 });
			return { ok: true, detail: '' };
		});

		await attempt('S10', 'attaching shows the customer next to the ticket total', async () => {
			await page.locator('.cust-panel .cust-option', { hasText: ANA }).first().click();
			await customerShows(page, ANA);
			const totals = await page.locator('.totals').innerText();
			return { ok: totals.includes(ANA) && totals.includes('Total'), detail: totals };
		});

		await attempt('S4', 'the draft carries the customer through the API', async () => {
			const sale = await api(page, `/api/v1/sales/${saleId}`);
			return {
				ok:
					sale.status === 200 &&
					sale.body?.customer_id === anaId &&
					sale.body?.customer_name === ANA,
				detail: JSON.stringify(sale.body)
			};
		});

		await attempt('S10', 'the "No customer" option clears the selection', async () => {
			await page.locator('.cust-trigger').click();
			await page.locator('.cust-panel .cust-none').click();
			await customerShows(page, 'No customer');
			const sale = await api(page, `/api/v1/sales/${saleId}`);
			return {
				ok: sale.status === 200 && sale.body?.customer_id === null,
				detail: JSON.stringify(sale.body)
			};
		});

		await attempt('S10', 'a second customer replaces the first', async () => {
			await page.locator('.cust-trigger').click();
			await page.locator('.cust-panel input').fill(BOB);
			const row = page.locator('.cust-panel .cust-option', { hasText: BOB }).first();
			await row.waitFor({ state: 'visible', timeout: 15000 });
			await row.click();
			await customerShows(page, BOB);
			return { ok: true, detail: '' };
		});

		// --- S4: a deactivated customer is refused, the draft keeps Bob ----
		await attempt('S4', 'a customer deactivated in another tab is refused', async () => {
			// The picker keeps the row it already loaded, so the cashier can
			// still click it after another tab deactivated the customer.
			await page.locator('.cust-trigger').click();
			await page.locator('.cust-panel input').fill(ANA);
			const row = page.locator('.cust-panel .cust-option', { hasText: ANA }).first();
			await row.waitFor({ state: 'visible', timeout: 15000 });
			const off = await api(page, `/api/v1/customers/${anaId}/deactivate`, {
				method: 'POST'
			});
			if (off.status !== 204) return { ok: false, detail: JSON.stringify(off) };
			expectedConflicts += 1;
			await row.click();
			await page.waitForFunction(
				() => document.querySelector('.error')?.textContent?.includes('deactivated'),
				{ timeout: 15000 }
			);
			const detail = await page.locator('.error').innerText();
			return {
				ok: detail.includes('the customer is deactivated and cannot be attached to a sale'),
				detail
			};
		});

		await attempt('S4', 'the refused attach leaves the previous customer intact', async () => {
			const sale = await api(page, `/api/v1/sales/${saleId}`);
			const trigger = (await page.locator('.cust-trigger').innerText()).trim();
			// Dismiss the picker the way a cashier would before charging.
			await page.locator('.cust-trigger').click();
			return {
				ok:
					sale.status === 200 &&
					sale.body?.customer_id === bobId &&
					sale.body?.customer_name === BOB &&
					trigger === BOB,
				detail: `api=${JSON.stringify(sale.body)} trigger=${trigger}`
			};
		});

		// --- S10: charge, and read the frozen name in the history ----------
		await attempt('S10', 'the sale charges with the customer attached', async () => {
			await page.locator('button.tile', { hasText: PRODUCT }).first().click();
			await page.locator('.lines .line').first().waitFor({ state: 'visible', timeout: 15000 });
			await selectOption(page, 'payment-method', METHOD);
			await page.getByRole('button', { name: /^Charge$/ }).click();
			await page.locator('.done').waitFor({ state: 'visible', timeout: 15000 });
			return { ok: true, detail: '' };
		});

		const shortId = saleId.slice(0, 8);
		await attempt('S10', 'the history shows the customer of the charged sale', async () => {
			await page.goto(`${BASE}/operations/sales`, { waitUntil: 'networkidle' });
			const row = page.locator('tbody tr', { hasText: shortId }).first();
			await row.waitFor({ state: 'visible', timeout: 15000 });
			const cell = (await row.locator('td').nth(2).innerText()).trim();
			return { ok: cell === BOB, detail: cell };
		});

		// The row grew a cell; the header has to grow with it, or every label
		// sits over the wrong column and no screenshot shows it.
		await attempt('S10', 'the history header labels every column', async () => {
			const heads = (await page.locator('thead th').allInnerTexts()).map((t) => t.trim());
			const cells = await page.locator('tbody tr').first().locator('td').count();
			return {
				ok: heads.length === cells && heads[2] === 'Customer',
				detail: `${heads.join('|')} vs ${cells} cells`
			};
		});

		await attempt('S10', 'the completed row keeps its Credit note action', async () => {
			const row = page.locator('tbody tr', { hasText: shortId }).first();
			const href = await row
				.getByRole('link', { name: 'Credit note' })
				.getAttribute('href');
			return {
				ok: href === `/operations/sales/${saleId}/credit-note`,
				detail: String(href)
			};
		});

		await attempt('S5', 'renaming the customer does not rewrite the completed row', async () => {
			const renamed = await api(page, `/api/v1/customers/${bobId}`, {
				method: 'PATCH',
				body: { name: BOB_RENAMED }
			});
			if (renamed.status !== 204) return { ok: false, detail: JSON.stringify(renamed) };
			await page.reload({ waitUntil: 'networkidle' });
			const row = page.locator('tbody tr', { hasText: shortId }).first();
			const cell = (await row.locator('td').nth(2).innerText()).trim();
			return { ok: cell === BOB, detail: cell };
		});

		await attempt('S10', 'a purchase with no customer shows a dash', async () => {
			await page.goto(`${BASE}/operations/pos`, { waitUntil: 'networkidle' });
			const trigger = (await page.locator('.cust-trigger').innerText()).trim();
			if (trigger !== 'No customer') return { ok: false, detail: `trigger=${trigger}` };
			const fresh = await api(page, '/api/v1/sales/draft', { method: 'POST' });
			const anonymousId = fresh.body?.id;
			await page.locator('button.tile', { hasText: PRODUCT }).first().click();
			await page.locator('.lines .line').first().waitFor({ state: 'visible', timeout: 15000 });
			await selectOption(page, 'payment-method', METHOD);
			await page.getByRole('button', { name: /^Charge$/ }).click();
			await page.locator('.done').waitFor({ state: 'visible', timeout: 15000 });
			await page.goto(`${BASE}/operations/sales`, { waitUntil: 'networkidle' });
			const row = page
				.locator('tbody tr', { hasText: anonymousId.slice(0, 8) })
				.first();
			await row.waitFor({ state: 'visible', timeout: 15000 });
			const cell = (await row.locator('td').nth(2).innerText()).trim();
			return { ok: cell === '—', detail: cell };
		});

		check('X', 'no page errors', errors.length === 0, errors.join(' | '));
	} finally {
		// Leave no extra active method behind: the payments probe asserts that
		// the POS has none once it deactivates its own.
		try {
			if (paymentMethodId) {
				const disabled = await api(page, `/api/v1/payment-methods/${paymentMethodId}`, {
					method: 'PATCH',
					body: { is_active: false }
				});
				check(
					'cleanup',
					'the probe deactivates its payment method',
					disabled.status === 204,
					JSON.stringify(disabled)
				);
			}
		} catch (error) {
			check(
				'cleanup',
				'the probe deactivates its payment method',
				false,
				String(error).split('\n')[0]
			);
		}
		await browser.close();
	}

	finish();
})().catch((error) => {
	console.error('ERROR', error);
	process.exit(3);
});

/** Report the tally and exit once, whether the probe ended early or not. */
function finish() {
	const failed = results.filter((result) => !result.ok);
	console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
	process.exit(failed.length === 0 ? 0 : 1);
}
