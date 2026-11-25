/*
 * UI verification for customer and supplier drawer layout and account actions:
 * no-tabs header details box, 2 KPI cards + inline action toolbar, and unified
 * balance ledger (sales/purchases with nested credit notes/returns, payments,
 * credit refunds, and manual adjustments).
 *
 * Covers:
 * - L1: Drawer scales on desktop (>= 600px), has zero tabs, and action bar fits
 *       cleanly on desktop (1440px) and mobile (390px).
 * - C1: Customer inline Manual adjustment records (+), surfaces 422 when
 *       crossing below zero, and settles (−).
 * - C2: Customer inline Refund credit shows refundable credit, rejects excess
 *       with 422 detail, records partial refund, and settles via "Use full credit".
 * - C3: Customer unified Balance ledger shows an open on-account sale with its
 *       nested credit note, plus adjustments and refunds persisted across reload.
 * - S1: Supplier inline Manual adjustment records (+), rejects crossing below
 *       zero (422), and settles (−).
 * - S2: Supplier inline Refund credit records partial and full-credit refunds.
 * - S3: Supplier unified Balance ledger shows an open on-account purchase with
 *       its nested purchase return, plus adjustments and refunds after reload.
 *
 * Usage:
 *   E2E_BASE=http://localhost:3000 E2E_EMAIL=owner@pyme.com \
 *     E2E_PASSWORD=... node e2e/account-actions.cjs
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
const WALLET = `Acct actions wallet ${stamp}`;
const METHOD = `Acct actions method ${stamp}`;
const CUSTOMER = `Acct actions customer ${stamp}`;
const SUPPLIER = `Acct actions supplier ${stamp}`;
const PRODUCT = `Acct actions product ${stamp}`;
const SKU = `AA-${stamp}`;

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
		check(rule, name, false, String(error).split('\n').slice(0, 8).join(' | '));
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

async function selectOption(page, triggerId, optionText) {
	await page.locator(`#${triggerId}`).click();
	await page.getByRole('option', { name: optionText }).first().click();
}

(async () => {
	const browser = await chromium.launch();
	const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 } });
	const page = await ctx.newPage();
	const errors = [];
	page.on('pageerror', (e) => errors.push(String(e)));
	page.on('console', (m) => {
		if (m.type() === 'error') {
			// Expected 422 probes log browser fetch errors; ignore HTTP 422 status logs.
			if (!m.text().includes('422')) errors.push(m.text());
		}
	});

	try {
		await login(page);

		// Cancel any stale open drafts so our test sales/purchases start clean.
		const staleSale = await api(page, '/api/v1/sales/draft', { method: 'POST' });
		if (staleSale.body?.id) {
			await api(page, `/api/v1/sales/${staleSale.body.id}/cancel`, { method: 'POST' });
		}
		const stalePurchase = await api(page, '/api/v1/purchases/draft', { method: 'POST' });
		if (stalePurchase.body?.id) {
			await api(page, `/api/v1/purchases/${stalePurchase.body.id}/cancel`, { method: 'POST' });
		}

		const wallet = await api(page, '/api/v1/wallets', {
			method: 'POST',
			body: { name: WALLET }
		});
		const method = await api(page, '/api/v1/payment-methods', {
			method: 'POST',
			body: { name: METHOD, wallet_id: wallet.body.id }
		});
		const product = await api(page, '/api/v1/products', {
			method: 'POST',
			body: { kind: 'product', name: PRODUCT, sku: SKU, price: '50.00' }
		});
		const customer = await api(page, '/api/v1/customers', {
			method: 'POST',
			body: { name: CUSTOMER }
		});
		const supplier = await api(page, '/api/v1/suppliers', {
			method: 'POST',
			body: { name: SUPPLIER }
		});

		const customerId = customer.body.id;
		const supplierId = supplier.body.id;
		const methodId = method.body.id;
		const productId = product.body.id;

		// --- Customer drawer: responsive no-tabs layout + manual adjustments ---
		await page.goto(`${BASE}/customers?id=${customerId}`, { waitUntil: 'networkidle' });
		const sheet = page.locator('[data-slot="sheet-content"]');
		await sheet.waitFor({ state: 'visible', timeout: 10000 });

		await attempt(
			'L1',
			'drawer scales on desktop, removes tabs, and fits action bar on desktop & mobile',
			async () => {
				const desktopMetrics = await sheet.evaluate((el) => {
					const tabs = el.querySelectorAll('[role="tablist"]').length;
					const bar = el.querySelector('[data-slot="account-actions"]');
					const sheetRect = el.getBoundingClientRect();
					const barRect = bar?.getBoundingClientRect();
					return {
						tabs,
						sheetWidth: Math.round(sheetRect.width),
						barFitsSheet: barRect
							? barRect.right <= sheetRect.right + 1 && barRect.left >= sheetRect.left - 1
							: false
					};
				});
				await page.setViewportSize({ width: 390, height: 844 });
				const mobileMetrics = await sheet.evaluate((el) => {
					const bar = el.querySelector('[data-slot="account-actions"]');
					const sheetRect = el.getBoundingClientRect();
					const barRect = bar?.getBoundingClientRect();
					return {
						sheetWidth: Math.round(sheetRect.width),
						barFitsSheet: barRect
							? barRect.right <= sheetRect.right + 1 && barRect.left >= sheetRect.left - 1
							: false
					};
				});
				await page.setViewportSize({ width: 1440, height: 900 });
				const ok =
					desktopMetrics.tabs === 0 &&
					desktopMetrics.sheetWidth >= 600 &&
					desktopMetrics.barFitsSheet &&
					mobileMetrics.barFitsSheet;
				return {
					ok,
					detail: JSON.stringify({ desktopMetrics, mobileMetrics })
				};
			}
		);

		await attempt('C1', 'customer adjustment (+), 422 guard, and (−) settle', async () => {
			await sheet.getByRole('button', { name: 'Manual adjustment' }).click();
			await page.locator('#customer-adjustment-amount').fill('100.00');
			await page.locator('#customer-adjustment-note').fill(`Opening debt ${stamp}`);
			await sheet.getByRole('button', { name: 'Record adjustment' }).click();

			const status = sheet.locator('[role="status"]');
			await status.waitFor({ state: 'visible', timeout: 10000 });
			const inText = await status.innerText();
			if (!inText.includes('Adjustment recorded.') || !inText.includes('100.00')) {
				return { ok: false, detail: `unexpected status after in: ${inText}` };
			}

			// Try decreasing beyond zero -> expect 422 alert.
			await selectOption(page, 'customer-adjustment-direction', 'Decrease debt (−)');
			await page.locator('#customer-adjustment-amount').fill('150.00');
			await page.locator('#customer-adjustment-note').fill(`Too large ${stamp}`);
			await sheet.getByRole('button', { name: 'Record adjustment' }).click();
			const alert = sheet.locator('[role="alert"]');
			await alert.waitFor({ state: 'visible', timeout: 10000 });
			const alertText = await alert.innerText();
			if (!alertText.trim()) {
				return { ok: false, detail: 'expected 422 alert text' };
			}

			// Settle exact 100.00.
			await page.locator('#customer-adjustment-amount').fill('100.00');
			await page.locator('#customer-adjustment-note').fill(`Write-off ${stamp}`);
			await sheet.getByRole('button', { name: 'Record adjustment' }).click();
			await status.waitFor({ state: 'visible', timeout: 10000 });
			const outText = await status.innerText();
			return {
				ok: outText.includes('Balance: 0.00'),
				detail: outText
			};
		});

		// Seed unapplied customer credit ($60.00) via API, reload drawer.
		await api(page, `/api/v1/customers/${customerId}/receipts`, {
			method: 'POST',
			body: {
				payment_method_id: methodId,
				amount: '60.00',
				allocations: [],
				note: `Advance receipt ${stamp}`
			}
		});
		await page.reload({ waitUntil: 'networkidle' });
		await sheet.waitFor({ state: 'visible', timeout: 10000 });

		await attempt('C2', 'customer credit refund (422 guard, partial, full credit)', async () => {
			await sheet.getByRole('button', { name: 'Refund credit' }).click();
			const tabText = await sheet.innerText();
			if (!tabText.includes('60.00')) {
				return { ok: false, detail: `missing refundable credit 60.00: ${tabText.slice(0, 300)}` };
			}

			// Attempt excess refund -> 422 alert.
			await selectOption(page, 'customer-refund-method', METHOD);
			await page.locator('#customer-refund-amount').fill('70.00');
			await page.locator('#customer-refund-note').fill(`Excess refund ${stamp}`);
			await sheet.getByRole('button', { name: 'Record credit refund' }).click();
			await sheet.locator('[role="alert"]').waitFor({ state: 'visible', timeout: 10000 });

			// Partial refund of 25.00.
			await page.locator('#customer-refund-amount').fill('25.00');
			await page.locator('#customer-refund-note').fill(`Partial customer refund ${stamp}`);
			await sheet.getByRole('button', { name: 'Record credit refund' }).click();
			const status = sheet.locator('[role="status"]');
			await status.waitFor({ state: 'visible', timeout: 10000 });
			const partialText = await status.innerText();
			if (!partialText.includes('-35.00') || !partialText.includes('35.00')) {
				return { ok: false, detail: `unexpected partial refund status: ${partialText}` };
			}

			// Full remaining credit via helper button.
			await sheet.getByRole('button', { name: 'Use full credit' }).click();
			const filled = await page.locator('#customer-refund-amount').inputValue();
			if (filled !== '35.00') {
				return { ok: false, detail: `expected 35.00 prefill, got ${filled}` };
			}
			await page.locator('#customer-refund-note').fill(`Final customer refund ${stamp}`);
			await sheet.getByRole('button', { name: 'Record credit refund' }).click();
			await page.waitForFunction(
				() => {
					const el = document.querySelector('[data-slot="sheet-content"] [role="status"]');
					return el && el.textContent && el.textContent.includes('Balance: 0.00');
				},
				null,
				{ timeout: 10000 }
			);
			return { ok: true };
		});

		// Seed stock via completed cash purchase, then an on-account sale + partial credit note.
		const stockDraft = await api(page, '/api/v1/purchases/draft', { method: 'POST' });
		await api(page, `/api/v1/purchases/${stockDraft.body.id}/supplier`, {
			method: 'PUT',
			body: { supplier_id: supplierId }
		});
		await api(page, `/api/v1/purchases/${stockDraft.body.id}/items`, {
			method: 'POST',
			body: { product_id: productId, quantity: '10', unit_cost: '20.00' }
		});
		await api(page, `/api/v1/purchases/${stockDraft.body.id}/complete`, {
			method: 'POST',
			body: { payments: [{ payment_method_id: methodId, amount: '200.00' }], on_account: '0.00' }
		});

		const saleDraft = await api(page, '/api/v1/sales/draft', { method: 'POST' });
		const saleId = saleDraft.body.id;
		await api(page, `/api/v1/sales/${saleId}/customer`, {
			method: 'PUT',
			body: { customer_id: customerId }
		});
		await api(page, `/api/v1/sales/${saleId}/items`, {
			method: 'POST',
			body: { product_id: productId, quantity: '4' }
		});
		await api(page, `/api/v1/sales/${saleId}/complete`, {
			method: 'POST',
			body: { payments: [], on_account: '200.00' }
		});
		await api(page, `/api/v1/sales/${saleId}/credit-notes`, {
			method: 'POST',
			body: {
				reason: `Damaged item ${stamp}`,
				items: [{ product_id: productId, quantity: '1' }]
			}
		});

		await attempt(
			'C3',
			'customer unified ledger shows open sale + nested credit note + adjustments + refunds',
			async () => {
				await page.reload({ waitUntil: 'networkidle' });
				await sheet.waitFor({ state: 'visible', timeout: 10000 });
				const ledgerText = await sheet.locator('[data-slot="balance-ledger"]').innerText();
				const ok =
					ledgerText.includes(`Sale #${saleId.slice(0, 8)}`) &&
					ledgerText.includes('Partially paid') &&
					ledgerText.includes('Credit note') &&
					ledgerText.includes('-50.00') &&
					ledgerText.includes(`Opening debt ${stamp}`) &&
					ledgerText.includes(`Write-off ${stamp}`) &&
					ledgerText.includes('Credit refund');
				return { ok, detail: ok ? '' : ledgerText.slice(0, 800) };
			}
		);

		await attempt(
			'C4',
			'clicking customer ledger items opens sale, credit note, receipt, and adjustment documents',
			async () => {
				const ledger = sheet.locator('[data-slot="balance-ledger"]');
				const dialog = page.locator('[data-slot="account-document-dialog"]');

				// 1. Open Sale document
				await ledger.locator('button', { hasText: `Sale #${saleId.slice(0, 8)}` }).click();
				await dialog.waitFor({ state: 'visible', timeout: 10000 });
				await page.waitForFunction(
					(prod) => {
						const el = document.querySelector('[data-slot="account-document-dialog"]');
						return el && el.textContent && el.textContent.includes(prod);
					},
					PRODUCT,
					{ timeout: 10000 }
				);
				const saleText = await dialog.innerText();
				await dialog.getByRole('button', { name: 'Close', exact: true }).last().click();
				await dialog.waitFor({ state: 'hidden', timeout: 5000 });

				// 2. Open nested Credit note document
				await ledger.locator('button', { hasText: 'Credit note' }).first().click();
				await dialog.waitFor({ state: 'visible', timeout: 10000 });
				await page.waitForFunction(
					(reason) => {
						const el = document.querySelector('[data-slot="account-document-dialog"]');
						return el && el.textContent && el.textContent.includes(reason);
					},
					`Damaged item ${stamp}`,
					{ timeout: 10000 }
				);
				const cnText = await dialog.innerText();
				await dialog.getByRole('button', { name: 'Close', exact: true }).last().click();
				await dialog.waitFor({ state: 'hidden', timeout: 5000 });

				// 3. Open Receipt document
				await ledger.locator('button', { hasText: 'Receipt' }).first().click();
				await dialog.waitFor({ state: 'visible', timeout: 10000 });
				await page.waitForFunction(
					(methodName) => {
						const el = document.querySelector('[data-slot="account-document-dialog"]');
						return el && el.textContent && el.textContent.includes(methodName);
					},
					METHOD,
					{ timeout: 10000 }
				);
				const receiptText = await dialog.innerText();
				await dialog.getByRole('button', { name: 'Close', exact: true }).last().click();
				await dialog.waitFor({ state: 'hidden', timeout: 5000 });

				// 4. Open Adjustment document
				await ledger.locator('button', { hasText: `Opening debt ${stamp}` }).first().click();
				await dialog.waitFor({ state: 'visible', timeout: 10000 });
				const adjText = await dialog.innerText();
				await dialog.getByRole('button', { name: 'Close', exact: true }).last().click();
				await dialog.waitFor({ state: 'hidden', timeout: 5000 });

				const ok =
					saleText.includes('200.00') &&
					saleText.includes(PRODUCT) &&
					cnText.includes(`Damaged item ${stamp}`) &&
					cnText.includes('50.00') &&
					receiptText.includes(METHOD) &&
					receiptText.includes(WALLET) &&
					adjText.includes(`Opening debt ${stamp}`);
				return { ok, detail: ok ? '' : `${saleText} | ${cnText} | ${receiptText} | ${adjText}` };
			}
		);

		// --- Supplier drawer: manual adjustments, credit refunds, unified ledger ---
		await page.goto(`${BASE}/suppliers?id=${supplierId}`, { waitUntil: 'networkidle' });
		await sheet.waitFor({ state: 'visible', timeout: 10000 });

		await attempt('S1', 'supplier adjustment (+), 422 guard, and (−) settle', async () => {
			await sheet.getByRole('button', { name: 'Manual adjustment' }).click();
			await page.locator('#supplier-adjustment-amount').fill('120.00');
			await page.locator('#supplier-adjustment-note').fill(`Opening payable ${stamp}`);
			await sheet.getByRole('button', { name: 'Record adjustment' }).click();

			const status = sheet.locator('[role="status"]');
			await status.waitFor({ state: 'visible', timeout: 10000 });
			const inText = await status.innerText();
			if (!inText.includes('Adjustment recorded.') || !inText.includes('120.00')) {
				return { ok: false, detail: `unexpected status after in: ${inText}` };
			}

			// Crossing below zero -> 422 alert.
			await selectOption(page, 'supplier-adjustment-direction', 'Decrease payable (−)');
			await page.locator('#supplier-adjustment-amount').fill('200.00');
			await page.locator('#supplier-adjustment-note').fill(`Excess payable adj ${stamp}`);
			await sheet.getByRole('button', { name: 'Record adjustment' }).click();
			await sheet.locator('[role="alert"]').waitFor({ state: 'visible', timeout: 10000 });

			// Settle exact 120.00.
			await page.locator('#supplier-adjustment-amount').fill('120.00');
			await page.locator('#supplier-adjustment-note').fill(`Payable correction ${stamp}`);
			await sheet.getByRole('button', { name: 'Record adjustment' }).click();
			await status.waitFor({ state: 'visible', timeout: 10000 });
			const outText = await status.innerText();
			return {
				ok: outText.includes('Balance: 0.00'),
				detail: outText
			};
		});

		// Seed unapplied supplier advance credit ($80.00) via API, reload drawer.
		await api(page, `/api/v1/suppliers/${supplierId}/payments`, {
			method: 'POST',
			body: {
				payment_method_id: methodId,
				amount: '80.00',
				allocations: [],
				note: `Advance payment ${stamp}`
			}
		});
		await page.reload({ waitUntil: 'networkidle' });
		await sheet.waitFor({ state: 'visible', timeout: 10000 });

		await attempt('S2', 'supplier credit refund (partial and full credit)', async () => {
			await sheet.getByRole('button', { name: 'Refund credit' }).click();
			const tabText = await sheet.innerText();
			if (!tabText.includes('80.00')) {
				return { ok: false, detail: `missing refundable credit 80.00: ${tabText.slice(0, 300)}` };
			}

			await selectOption(page, 'supplier-refund-method', METHOD);
			await page.locator('#supplier-refund-amount').fill('30.00');
			await page.locator('#supplier-refund-note').fill(`Partial supplier refund ${stamp}`);
			await sheet.getByRole('button', { name: 'Record credit refund' }).click();
			const status = sheet.locator('[role="status"]');
			await status.waitFor({ state: 'visible', timeout: 10000 });
			const partialText = await status.innerText();
			if (!partialText.includes('-50.00') || !partialText.includes('50.00')) {
				return { ok: false, detail: `unexpected partial refund status: ${partialText}` };
			}

			await sheet.getByRole('button', { name: 'Use full credit' }).click();
			const filled = await page.locator('#supplier-refund-amount').inputValue();
			if (filled !== '50.00') {
				return { ok: false, detail: `expected 50.00 prefill, got ${filled}` };
			}
			await page.locator('#supplier-refund-note').fill(`Final supplier refund ${stamp}`);
			await sheet.getByRole('button', { name: 'Record credit refund' }).click();
			await page.waitForFunction(
				() => {
					const el = document.querySelector('[data-slot="sheet-content"] [role="status"]');
					return el && el.textContent && el.textContent.includes('Balance: 0.00');
				},
				null,
				{ timeout: 10000 }
			);
			return { ok: true };
		});

		// Seed an on-account purchase + partial purchase return to verify nested return row.
		const creditPurchaseDraft = await api(page, '/api/v1/purchases/draft', { method: 'POST' });
		const creditPurchaseId = creditPurchaseDraft.body.id;
		await api(page, `/api/v1/purchases/${creditPurchaseId}/supplier`, {
			method: 'PUT',
			body: { supplier_id: supplierId }
		});
		await api(page, `/api/v1/purchases/${creditPurchaseId}/items`, {
			method: 'POST',
			body: { product_id: productId, quantity: '4', unit_cost: '25.00' }
		});
		await api(page, `/api/v1/purchases/${creditPurchaseId}/complete`, {
			method: 'POST',
			body: { payments: [], on_account: '100.00' }
		});
		await api(page, `/api/v1/purchases/${creditPurchaseId}/returns`, {
			method: 'POST',
			body: {
				reason: `Defective batch ${stamp}`,
				surplus_mode: 'supplier_credit',
				items: [{ product_id: productId, quantity: '1' }]
			}
		});

		await attempt(
			'S3',
			'supplier unified ledger shows open purchase + nested return + adjustments + refunds',
			async () => {
				await page.reload({ waitUntil: 'networkidle' });
				await sheet.waitFor({ state: 'visible', timeout: 10000 });
				const ledgerText = await sheet.locator('[data-slot="balance-ledger"]').innerText();
				const ok =
					ledgerText.includes(`Purchase #${creditPurchaseId.slice(0, 8)}`) &&
					ledgerText.includes('Partially paid') &&
					ledgerText.includes('Purchase return') &&
					ledgerText.includes('-25.00') &&
					ledgerText.includes(`Opening payable ${stamp}`) &&
					ledgerText.includes(`Payable correction ${stamp}`) &&
					ledgerText.includes(`Partial supplier refund ${stamp}`) &&
					ledgerText.includes(`Final supplier refund ${stamp}`);
				return { ok, detail: ok ? '' : ledgerText.slice(0, 800) };
			}
		);

		await attempt(
			'S4',
			'clicking supplier ledger items opens purchase, purchase return, and payment documents',
			async () => {
				const ledger = sheet.locator('[data-slot="balance-ledger"]');
				const dialog = page.locator('[data-slot="account-document-dialog"]');

				// 1. Open Purchase document
				await ledger
					.locator('button', { hasText: `Purchase #${creditPurchaseId.slice(0, 8)}` })
					.click();
				await dialog.waitFor({ state: 'visible', timeout: 10000 });
				await page.waitForFunction(
					(prod) => {
						const el = document.querySelector('[data-slot="account-document-dialog"]');
						return el && el.textContent && el.textContent.includes(prod);
					},
					PRODUCT,
					{ timeout: 10000 }
				);
				const purchaseText = await dialog.innerText();
				await dialog.getByRole('button', { name: 'Close', exact: true }).last().click();
				await dialog.waitFor({ state: 'hidden', timeout: 5000 });

				// 2. Open nested Purchase return document
				await ledger.locator('button', { hasText: 'Purchase return' }).first().click();
				await dialog.waitFor({ state: 'visible', timeout: 10000 });
				await page.waitForFunction(
					(reason) => {
						const el = document.querySelector('[data-slot="account-document-dialog"]');
						return el && el.textContent && el.textContent.includes(reason);
					},
					`Defective batch ${stamp}`,
					{ timeout: 10000 }
				);
				const returnText = await dialog.innerText();
				await dialog.getByRole('button', { name: 'Close', exact: true }).last().click();
				await dialog.waitFor({ state: 'hidden', timeout: 5000 });

				// 3. Open Payment document
				await ledger.locator('button', { hasText: 'Payment' }).first().click();
				await dialog.waitFor({ state: 'visible', timeout: 10000 });
				await page.waitForFunction(
					(methodName) => {
						const el = document.querySelector('[data-slot="account-document-dialog"]');
						return el && el.textContent && el.textContent.includes(methodName);
					},
					METHOD,
					{ timeout: 10000 }
				);
				const paymentText = await dialog.innerText();
				await dialog.getByRole('button', { name: 'Close', exact: true }).last().click();
				await dialog.waitFor({ state: 'hidden', timeout: 5000 });

				const ok =
					purchaseText.includes('100.00') &&
					purchaseText.includes(PRODUCT) &&
					returnText.includes(`Defective batch ${stamp}`) &&
					returnText.includes('25.00') &&
					paymentText.includes(METHOD) &&
					paymentText.includes(WALLET);
				return { ok, detail: ok ? '' : `${purchaseText} | ${returnText} | ${paymentText}` };
			}
		);

		check('E2E', 'no console or page errors', errors.length === 0, errors.join(' | '));
	} finally {
		await browser.close();
	}

	const passed = results.filter((r) => r.ok).length;
	console.log(`\nSummary: ${passed}/${results.length} checks passed`);
	process.exit(passed === results.length ? 0 : 1);
})();
