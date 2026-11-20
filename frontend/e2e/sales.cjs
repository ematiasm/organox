/*
 * UI verification for charged sales and credit notes (T5, S9).
 *
 * S1: a product with stock, a wallet and a payment method are seeded through
 *     the API, and a sale of three units is charged.
 * S2: the sales list shows that sale with its server total and a "Credit note"
 *     action leading to the return screen.
 * S3: the return form shows the sale totals and the charged line with three
 *     units still returnable.
 * S4: the submit stays disabled until a reason and a quantity are entered, and
 *     the summary names units without inventing a money amount.
 * S5: issuing a note shows the server's total and refunds, refreshes what is
 *     left to return and resets the form.
 * S6: a reload keeps the note in the list.
 * S7: asking for more than is left shows the server's 409 detail and writes
 *     nothing.
 * S8: a sale paid twice with the same payment method refunds through both
 *     payments, and the created note renders two refund rows that share one
 *     wallet and method (the duplicate-key case).
 * S9: a line returned down to zero shows the fully-returned state with its
 *     input disabled.
 *
 * Usage: E2E_BASE=http://localhost:5173 E2E_EMAIL=owner@example.com \
 *   E2E_PASSWORD=... node e2e/sales.cjs
 * Requires a running app. Every name is stamped so repeated runs against the
 * development database create independent sales and notes.
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
const PRODUCT = `Credit note probe ${stamp}`;
const SKU = `CN-${stamp}`;
const WALLET = `Credit note wallet ${stamp}`;
const METHOD = `Credit note method ${stamp}`;
const REASON = `Returned one unit ${stamp}`;

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

/** Call the API with the browser's own session, as the payments probe does. */
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
	let paymentMethodId = null;
	// S7 deliberately elicits a 409. Chromium reports that fetch as a console
	// resource error; account for that one expected error without hiding others.
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

		// A previous interrupted run leaves an open draft for this dedicated e2e
		// user. Cancel it so the charged sale contains only the seeded product.
		const staleDraft = await api(page, '/api/v1/sales/draft', { method: 'POST' });
		if (staleDraft.body?.id) {
			await api(page, `/api/v1/sales/${staleDraft.body.id}/cancel`, { method: 'POST' });
		}

		// --- S1: seed an isolated sale and charge it ------------------------
		const product = await api(page, '/api/v1/products', {
			method: 'POST',
			body: { kind: 'product', name: PRODUCT, sku: SKU, price: '12.50' }
		});
		if (!(await attempt('S1', 'a product is seeded through the API', async () => ({
			ok: product.status === 201 && typeof product.body?.id === 'string',
			detail: JSON.stringify(product)
		})))) throw new Error('setup failed: see the checks above');
		const productId = product.body.id;

		const stock = await api(page, `/api/v1/products/${productId}/movements`, {
			method: 'POST',
			body: {
				reason: 'purchase',
				quantity: '10',
				unit_cost: '12.50',
				note: `Credit note probe stock ${stamp}`
			}
		});
		if (!(await attempt('S1', 'the product has stock', async () => ({
			ok: stock.status === 201,
			detail: JSON.stringify(stock)
		})))) throw new Error('setup failed: see the checks above');

		const wallet = await api(page, '/api/v1/wallets', {
			method: 'POST',
			body: { name: WALLET }
		});
		if (!(await attempt('S1', 'a wallet is seeded through the API', async () => ({
			ok: wallet.status === 201 && typeof wallet.body?.id === 'string',
			detail: JSON.stringify(wallet)
		})))) throw new Error('setup failed: see the checks above');

		const method = await api(page, '/api/v1/payment-methods', {
			method: 'POST',
			body: { name: METHOD, wallet_id: wallet.body.id }
		});
		paymentMethodId = method.body?.id ?? null;
		if (!(await attempt('S1', 'a payment method is seeded through the API', async () => ({
			ok: method.status === 201 && typeof method.body?.id === 'string',
			detail: JSON.stringify(method)
		})))) throw new Error('setup failed: see the checks above');

		const draft = await api(page, '/api/v1/sales/draft', { method: 'POST' });
		const saleId = draft.body?.id;
		if (!(await attempt('S1', 'a sale draft is opened', async () => ({
			ok: draft.status === 201 && typeof saleId === 'string',
			detail: JSON.stringify(draft)
		})))) throw new Error('setup failed: see the checks above');

		const added = await api(page, `/api/v1/sales/${saleId}/items`, {
			method: 'POST',
			body: { product_id: productId, quantity: '3' }
		});
		const charged = await api(page, `/api/v1/sales/${saleId}/complete`, {
			method: 'POST',
			body: { payments: [{ payment_method_id: method.body.id, amount: added.body?.total }] }
		});
		const total = charged.body?.total;
		if (!(await attempt('S1', 'the sale is charged for three units', async () => ({
			ok:
				added.status === 200 &&
				added.body?.item_count === 1 &&
				added.body?.items?.[0]?.quantity === '3' &&
				charged.status === 200 &&
				typeof total === 'string',
			detail: `add=${JSON.stringify(added)} complete=${JSON.stringify(charged)}`
		})))) throw new Error('setup failed: see the checks above');

		// --- S2: the list and its action -----------------------------------
		const shortId = saleId.slice(0, 8);
		const listed = await attempt('S2', 'the sales list shows the charged sale', async () => {
			await page.goto(`${BASE}/operations/sales`, { waitUntil: 'networkidle' });
			const row = page.locator('tbody tr', { hasText: shortId }).first();
			await row.waitFor({ state: 'visible', timeout: 15000 });
			const text = await row.innerText();
			return {
				ok: text.includes(shortId) && text.includes('3') && text.includes(total),
				detail: text
			};
		});
		if (listed) {
			await attempt('S2', 'the row links to the return screen', async () => {
				const link = page
					.locator('tbody tr', { hasText: shortId })
					.first()
					.getByRole('link', { name: 'Credit note' });
				const href = await link.getAttribute('href');
				await link.click();
				await page.waitForURL(
					(url) => url.pathname === `/operations/sales/${saleId}/credit-note`,
					{ timeout: 10000 }
				);
				return {
					ok: href === `/operations/sales/${saleId}/credit-note`,
					detail: `href=${href} url=${page.url()}`
				};
			});
		}

		// --- S3: the charged line and the server totals --------------------
		const onForm = await attempt('S3', 'the return form shows the charged line', async () => {
			await page.locator(`#return-line-${productId}`).waitFor({ state: 'visible', timeout: 15000 });
			const line = await page.locator(`#return-line-${productId}`).innerText();
			const quantity = page.locator(`#return-qty-${productId}`);
			const max = await quantity.getAttribute('max');
			const body = await page.locator('main').innerText();
			return {
				ok:
					line.includes(PRODUCT) &&
					line.includes('12.50') &&
					max === '3' &&
					body.includes(total) &&
					body.includes('0.00'),
				detail: `line=${line} max=${max}`
			};
		});

		if (onForm) {
			const quantity = page.locator(`#return-qty-${productId}`);
			const reason = page.locator('#return-reason');
			const submit = page.getByRole('button', { name: 'Issue credit note' });

			// --- S4: gating and the units-only summary ----------------------
			await attempt('S4', 'submit is disabled before a return is described', async () => ({
				ok:
					(await submit.isDisabled()) &&
					(await reason.inputValue()) === '' &&
					(await quantity.inputValue()) === '0',
				detail: `disabled=${await submit.isDisabled()}`
			}));
			await attempt('S4', 'reason and quantity enable submit', async () => {
				await quantity.fill('1');
				await reason.fill(REASON);
				const summary = await page.locator('#return-summary').innerText();
				const counter = await page.locator('#reason-counter').innerText();
				return {
					ok:
						(await submit.isEnabled()) &&
						summary.includes('You are returning 1 unit') &&
						summary.includes('the server will calculate the amount') &&
						!/\d+\.\d{2}/.test(summary) &&
						counter.includes(`${REASON.length}/200`),
					detail: `summary=${summary} counter=${counter}`
				};
			});

			// --- S5: the server's note -------------------------------------
			const issuedResponse = page.waitForResponse(
				(r) =>
					r.request().method() === 'POST' &&
					r.url().endsWith(`/sales/${saleId}/credit-notes`)
			);
			await submit.click();
			const response = await issuedResponse;
			const note = await response.json();
			if (!(await attempt('S5', 'the server created the note with 201', async () => ({
				ok: response.status() === 201 && note.total === '12.50',
				detail: `status=${response.status()} note=${JSON.stringify(note)}`
			})))) throw new Error('setup failed: see the checks above');

			await page.locator('.issued').waitFor({ state: 'visible', timeout: 15000 });
			await attempt('S5', 'the created note total is shown', async () => {
				const issued = await page.locator('.issued').innerText();
				return { ok: issued.includes(note.total), detail: issued };
			});
			await attempt('S5', 'the refund names its wallet and amount', async () => {
				const issued = await page.locator('.issued').innerText();
				const refund = note.refunds.find((r) => r.wallet_name === WALLET);
				return {
					ok: Boolean(refund) && issued.includes(WALLET) && issued.includes(refund.amount),
					detail: issued
				};
			});
			await attempt('S5', 'returnable quantity and credited quantity refresh', async () => {
				await page.waitForFunction(
					(id) => {
						const input = document.getElementById(`return-qty-${id}`);
						return input && input.max === '2' && input.value === '0';
					},
					productId,
					{ timeout: 15000 }
				);
				const line = await page.locator(`#return-line-${productId}`).innerText();
				return {
					ok: line.includes('2 left') && line.includes('1'),
					detail: line
				};
			});
			await attempt('S5', 'the form resets after the note', async () => ({
				ok:
					(await reason.inputValue()) === '' &&
					(await quantity.inputValue()) === '0' &&
					(await submit.isDisabled()),
				detail: `reason=${await reason.inputValue()} qty=${await quantity.inputValue()}`
			}));
			await attempt('S5', 'the notes list shows the reason, total and refund', async () => {
				const text = await page.locator('#credit-notes-table').innerText();
				return {
					ok: text.includes(REASON) && text.includes(note.total) && text.includes(WALLET),
					detail: text
				};
			});

			// --- S6: the note is a document, not a local state -------------
			await attempt('S6', 'a reload keeps the note listed', async () => {
				await page.reload({ waitUntil: 'networkidle' });
				const text = await page.locator('#credit-notes-table').innerText();
				return {
					ok: text.includes(REASON) && text.includes(note.total),
					detail: text
				};
			});

			// --- S7: the server rejects a return above what is left --------
			await attempt('S7', 'asking for more than is left shows the 409 detail', async () => {
				expectedConflicts += 1;
				await page.locator(`#return-qty-${productId}`).fill('3');
				await page.locator('#return-reason').fill(REASON);
				await page.getByRole('button', { name: 'Issue credit note' }).click();
				await page.waitForFunction(
					() => document.querySelector('[role="alert"]')?.textContent?.includes('only 2 left to return'),
					null,
					{ timeout: 15000 }
				);
				const detail = await page.locator('[role="alert"]').innerText();
				return { ok: detail.includes('only 2 left to return'), detail };
			});
			await attempt('S7', 'the rejected return wrote nothing', async () => {
				const after = await api(page, `/api/v1/sales/${saleId}/credit-notes`);
				return {
					ok:
						after.status === 200 &&
						after.body?.credit_notes?.length === 1 &&
						after.body?.items?.[0]?.returnable_quantity === '2',
					detail: JSON.stringify(after)
				};
			});
		}

		// --- S8: a sale paid twice with one method -------------------------
		//
		// Both payments land in the same wallet through the same method, so the
		// note's two refunds repeat wallet and method: the each key needs the
		// position to stay unique.
		const splitDraft = await api(page, '/api/v1/sales/draft', { method: 'POST' });
		const splitSaleId = splitDraft.body?.id;
		const splitAdded = await api(page, `/api/v1/sales/${splitSaleId}/items`, {
			method: 'POST',
			body: { product_id: productId, quantity: '2' }
		});
		const splitCharged = await api(page, `/api/v1/sales/${splitSaleId}/complete`, {
			method: 'POST',
			body: {
				payments: [
					{ payment_method_id: method.body.id, amount: '10.00' },
				{ payment_method_id: method.body.id, amount: '15.00' }
			]
			}
		});
		const splitOk = await attempt(
			'S8',
			'a sale paid twice with one method is charged',
			async () => ({
				ok:
					splitDraft.status === 201 &&
					typeof splitSaleId === 'string' &&
					splitAdded.status === 200 &&
					splitCharged.status === 200 &&
					splitCharged.body?.total === '25.00',
				detail: `draft=${JSON.stringify(splitDraft)} add=${JSON.stringify(splitAdded)} complete=${JSON.stringify(splitCharged)}`
			})
		);

		if (splitOk) {
			await page.goto(`${BASE}/operations/sales/${splitSaleId}/credit-note`, {
				waitUntil: 'networkidle'
			});
			await page.locator(`#return-qty-${productId}`).fill('1');
			await page.locator('#return-reason').fill(REASON);
			const splitPosted = page.waitForResponse(
				(r) =>
					r.request().method() === 'POST' &&
					r.url().endsWith(`/sales/${splitSaleId}/credit-notes`)
			);
			await page.getByRole('button', { name: 'Issue credit note' }).click();
			const splitResponse = await splitPosted;
			const splitNote = await splitResponse.json();
			await attempt('S8', 'the note refunds through both payments of the method', async () => ({
				ok:
					splitResponse.status() === 201 &&
					splitNote.refunds?.length === 2 &&
					splitNote.refunds.every((r) => r.wallet_name === WALLET) &&
					splitNote.refunds.some((r) => r.amount === '5.00') &&
					splitNote.refunds.some((r) => r.amount === '7.50'),
				detail: `status=${splitResponse.status()} note=${JSON.stringify(splitNote)}`
			}));

			await attempt('S8', 'the created note shows both refund rows of one method', async () => {
				await page.locator('.issued').waitFor({ state: 'visible', timeout: 15000 });
				const rows = page.locator('.issued ul li');
				const count = await rows.count();
				const text = await page.locator('.issued').innerText();
				return {
					ok: count === 2 && text.includes('5.00') && text.includes('7.50'),
					detail: `count=${count} text=${text}`
				};
			});
			await attempt('S8', 'the notes list shows both refund rows', async () => {
				const row = page.locator('#credit-notes-table tbody tr').first();
				await row.waitFor({ state: 'visible', timeout: 15000 });
				const refunds = row.locator('td').last().locator('li');
				const count = await refunds.count();
				const amounts = await refunds.allInnerTexts();
				return {
					ok:
						count === 2 &&
						amounts.some((t) => t.includes('5.00')) &&
						amounts.some((t) => t.includes('7.50')),
					detail: `count=${count} amounts=${JSON.stringify(amounts)}`
				};
			});

			// --- S9: the same sale returned down to zero -------------------
			const oneLeft = await attempt(
				'S9',
				'the first split note leaves one unit returnable',
				async () => {
					await page.waitForFunction(
						(id) => {
							const input = document.getElementById(`return-qty-${id}`);
							return input && input.max === '1' && input.value === '0';
						},
						productId,
						{ timeout: 15000 }
					);
					return { ok: true, detail: '' };
				}
			);

			if (oneLeft) {
				await page.locator(`#return-qty-${productId}`).fill('1');
				await page.locator('#return-reason').fill(REASON);
				const lastPosted = page.waitForResponse(
					(r) =>
						r.request().method() === 'POST' &&
						r.url().endsWith(`/sales/${splitSaleId}/credit-notes`)
				);
				await page.getByRole('button', { name: 'Issue credit note' }).click();
				const lastResponse = await lastPosted;
				const lastNote = await lastResponse.json();
				await attempt('S9', 'the second note keeps both refund rows distinct', async () => {
					await page.locator('.issued').waitFor({ state: 'visible', timeout: 15000 });
					const rows = page.locator('.issued ul li');
					const count = await rows.count();
					const text = await page.locator('.issued').innerText();
					return {
						ok:
							lastResponse.status() === 201 &&
							count === 2 &&
							text.includes('5.00') &&
							text.includes('7.50'),
							detail: `status=${lastResponse.status()} count=${count} text=${text}`
					};
				});
				await attempt('S9', 'the line shows the fully-returned state', async () => {
					await page.waitForFunction(
						(id) =>
							document
								.getElementById(`return-line-${id}`)
								?.textContent?.includes('Fully returned'),
						productId,
						{ timeout: 15000 }
					);
					const line = await page.locator(`#return-line-${productId}`).innerText();
					const input = page.locator(`#return-qty-${productId}`);
					return {
						ok:
							line.includes('Fully returned') &&
							!line.includes('left') &&
							(await input.isDisabled()),
						detail: `line=${line} disabled=${await input.isDisabled()}`
					};
				});
				await attempt('S9', 'the server reports nothing left to return', async () => {
					const after = await api(page, `/api/v1/sales/${splitSaleId}/credit-notes`);
					return {
						ok:
							after.status === 200 &&
							after.body?.credit_notes?.length === 2 &&
							Number(after.body?.items?.[0]?.returnable_quantity) === 0,
						detail: JSON.stringify(after)
					};
				});
			}
		}

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
