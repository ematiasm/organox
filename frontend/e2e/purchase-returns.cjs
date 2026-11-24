/*
 * UI verification for purchase returns (supplier returns).
 *
 * Covers:
 * - Seeding a supplier, product, wallet, payment method, and completed mixed
 *   purchase ($50 upfront cash + $100 on account = 6 units @ $25.00).
 * - Navigating from `/operations/purchases` via the Return action button to
 *   `/operations/purchases/{id}/return`.
 * - Verifying KPI cards (purchase total, already returned, open payable,
 *   wallet refund capacity) and returnable line state.
 * - Recording return #1 (5 units = $125.00) with `supplier_credit` selected:
 *   cancels $100.00 payable and grants $25.00 supplier credit.
 * - Recording return #2 (1 unit = $25.00) with `wallet_refund` selected:
 *   refunds $25.00 to the wallet and marks the line as Fully returned.
 * - Reloading to verify persistence of both issued returns and zero console errors.
 *
 * Usage:
 *   E2E_BASE=http://localhost:3000 E2E_EMAIL=owner@pyme.com \
 *     E2E_PASSWORD=... node e2e/purchase-returns.cjs
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
const SUPPLIER = `Return probe supplier ${stamp}`;
const PRODUCT = `Return probe item ${stamp}`;
const SKU = `PR-${stamp}`;
const WALLET = `Return probe wallet ${stamp}`;
const METHOD = `Return probe method ${stamp}`;
const REASON_CREDIT = `Damaged units keep credit ${stamp}`;
const REASON_WALLET = `Last unit refund cash ${stamp}`;

const results = [];
function check(rule, name, ok, detail = '') {
	results.push({ rule, name, ok, detail });
	console.log(`${ok ? 'PASS' : 'FAIL'} [${rule}] ${name}${ok ? '' : ` — ${detail}`}`);
}

async function attempt(rule, name, action) {
	try {
		const res = await action();
		check(rule, name, res.ok === true, res.detail ?? '');
		return res.ok === true;
	} catch (error) {
		check(rule, name, false, String(error).split('\n')[0]);
		return false;
	}
}

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

		// Cancel any stale open purchase draft for this operator.
		const stale = await api(page, '/api/v1/purchases/draft', { method: 'POST' });
		if (stale.body?.id) {
			await api(page, `/api/v1/purchases/${stale.body.id}/cancel`, { method: 'POST' });
		}

		// Seed supplier, product, wallet, payment method, and completed mixed purchase.
		const supplier = await api(page, '/api/v1/suppliers', {
			method: 'POST',
			body: { name: SUPPLIER }
		});
		const product = await api(page, '/api/v1/products', {
			method: 'POST',
			body: { kind: 'product', name: PRODUCT, sku: SKU, price: '40.00' }
		});
		const wallet = await api(page, '/api/v1/wallets', {
			method: 'POST',
			body: { name: WALLET }
		});
		const method = await api(page, '/api/v1/payment-methods', {
			method: 'POST',
			body: { name: METHOD, wallet_id: wallet.body.id }
		});
		const draft = await api(page, '/api/v1/purchases/draft', { method: 'POST' });
		const purchaseId = draft.body.id;

		await api(page, `/api/v1/purchases/${purchaseId}/supplier`, {
			method: 'PUT',
			body: { supplier_id: supplier.body.id }
		});
		await api(page, `/api/v1/purchases/${purchaseId}/items`, {
			method: 'POST',
			body: { product_id: product.body.id, quantity: '6', unit_cost: '25.00' }
		});
		const completed = await api(page, `/api/v1/purchases/${purchaseId}/complete`, {
			method: 'POST',
			body: {
				payments: [{ payment_method_id: method.body.id, amount: '50.00' }],
				on_account: '100.00'
			}
		});
		if (
			!(await attempt('S1', 'mixed purchase seeded (50 cash + 100 on account)', async () => ({
				ok: completed.status === 200,
				detail: JSON.stringify(completed)
			})))
		) {
			throw new Error('setup failed');
		}

		// Navigate to /operations/purchases and click the Return link for our purchase.
		await page.goto(`${BASE}/operations/purchases`, { waitUntil: 'networkidle' });
		const returnLink = page.locator(`a[href="/operations/purchases/${purchaseId}/return"]`).first();
		await attempt('S2', 'purchases history exposes Return link to return screen', async () => {
			await returnLink.waitFor({ state: 'visible', timeout: 10000 });
			await returnLink.click();
			await page.waitForURL(new RegExp(`/operations/purchases/${purchaseId}/return`), {
				timeout: 10000
			});
			return { ok: true };
		});

		// Verify KPI summary and returnable line.
		await attempt('S3', 'return screen displays totals, payable, wallet capacity and line', async () => {
			const text = await page.locator('main').innerText();
			const ok =
				text.includes('150.00') &&
				text.includes('100.00') &&
				text.includes('50.00') &&
				text.includes(PRODUCT) &&
				text.includes('6 left');
			return { ok, detail: ok ? '' : text.slice(0, 400) };
		});

		// Return #1: 5 units ($125.00) with supplier_credit -> cancels $100 debt + $25 supplier credit.
		await page.locator(`#return-qty-${product.body.id}`).fill('5');
		await page.locator('#surplus-mode-credit').check();
		await page.locator('#return-reason').fill(REASON_CREDIT);
		await page.locator('button[type=submit]').click();

		await attempt('S4', 'return with supplier_credit cancels debt first and grants credit', async () => {
			const banner = page.locator('.issued');
			await banner.waitFor({ state: 'visible', timeout: 10000 });
			const text = await banner.innerText();
			const ok =
				text.includes('125.00') &&
				text.includes('Payable cancelled: 100.00') &&
				text.includes('Supplier credit granted: 25.00');
			return { ok, detail: ok ? '' : text };
		});

		// Return #2: remaining 1 unit ($25.00) with wallet_refund -> refunds $25.00 to wallet.
		await page.locator(`#return-qty-${product.body.id}`).fill('1');
		await page.locator('#surplus-mode-wallet').check();
		await page.locator('#return-reason').fill(REASON_WALLET);
		await page.locator('button[type=submit]').click();

		await attempt('S5', 'return with wallet_refund refunds wallet and marks line fully returned', async () => {
			await page.waitForFunction(
				(walletName) => {
					const el = document.querySelector('.issued');
					return el && el.textContent && el.textContent.includes(walletName);
				},
				WALLET,
				{ timeout: 10000 }
			);
			const bannerText = await page.locator('.issued').innerText();
			const disabled = await page.locator(`#return-qty-${product.body.id}`).isDisabled();
			const rowText = await page.locator(`#return-line-${product.body.id}`).innerText();
			const ok =
				bannerText.includes('25.00') &&
				bannerText.includes(WALLET) &&
				disabled &&
				rowText.includes('Fully returned');
			return { ok, detail: ok ? '' : `${bannerText} | disabled=${disabled} | ${rowText}` };
		});

		// Reload and verify both issued returns appear in the history table.
		await page.reload({ waitUntil: 'networkidle' });
		await attempt('S6', 'reload preserves both issued returns in history table', async () => {
			const tableText = await page.locator('#purchase-returns-table').innerText();
			const ok = tableText.includes(REASON_CREDIT) && tableText.includes(REASON_WALLET);
			return { ok, detail: ok ? '' : tableText };
		});

		check('E2E', 'no console or page errors', errors.length === 0, errors.join(' | '));
	} finally {
		await browser.close();
	}

	const passed = results.filter((r) => r.ok).length;
	console.log(`\nSummary: ${passed}/${results.length} checks passed`);
	process.exit(passed === results.length ? 0 : 1);
})();
