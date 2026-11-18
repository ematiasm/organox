/*
 * UI verification for wallets and payment methods (issue #36).
 *
 * P1: a wallet can be created from the settings screen and starts at 0.00.
 * P2: a payment method is created pointing at that wallet, and its row names it.
 * P3: the POS refuses to charge until a method is chosen, and shows where it lands.
 * P4: charging credits the wallet named by the method, for the sale total.
 * P5: a manual expense lowers the balance and the movement list shows it negative.
 * P6: with no active method the POS says so and Charge stays disabled.
 *
 * Usage: E2E_BASE=http://localhost:5173 node e2e/payments.cjs
 * Requires a running app with at least one product in stock, and
 * E2E_EMAIL/E2E_PASSWORD for an owner or manager account.
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
const WALLET = `Probe cash ${stamp}`;
const METHOD = `Probe card ${stamp}`;

const results = [];
function check(rule, name, ok, detail) {
	results.push({ rule, name, ok, detail });
	console.log(`${ok ? 'PASS' : 'FAIL'} [${rule}] ${name}${ok ? '' : ` — ${detail}`}`);
}

/** Read the wallet list exactly as the browser's session sees it. */
async function wallets(page) {
	const body = await page.evaluate(() =>
		fetch('/api/v1/wallets?include_inactive=true').then((response) => response.json())
	);
	return body.find((wallet) => wallet.name === WALLET) ?? null;
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

		// --- P1: create the wallet ------------------------------------------
		await page.goto(`${BASE}/settings/payments`, { waitUntil: 'networkidle' });
		await page.locator('#new-wallet').fill(WALLET);
		await page.getByRole('button', { name: 'Create wallet' }).click();
		await page.waitForFunction(
			(name) => document.body.innerText.includes(name),
			WALLET,
			{ timeout: 10000 }
		);

		const created = await wallets(page);
		check('P1', 'wallet created from settings', created !== null, 'wallet missing');
		check('P1', 'a new wallet starts at 0.00', created?.balance === '0.00', `balance=${created?.balance}`);

		// --- P2: create the method ------------------------------------------
		await page.locator('#new-method').fill(METHOD);
		await selectOption(page, 'new-method-wallet', WALLET);
		await page.getByRole('button', { name: 'Create method' }).click();
		await page.waitForFunction(
			(name) => document.body.innerText.includes(name),
			METHOD,
			{ timeout: 10000 }
		);

		const methodRow = page.locator('tr', { hasText: METHOD }).first();
		check(
			'P2',
			'the method row names the wallet it pays into',
			(await methodRow.innerText()).includes(WALLET),
			'wallet not shown'
		);

		// --- P3: the POS will not charge without a method --------------------
		await page.goto(`${BASE}/operations/pos`, { waitUntil: 'networkidle' });
		await page.locator('button.tile:not([disabled])').first().click();
		await page.waitForFunction(
			() => document.querySelectorAll('.lines .line').length > 0,
			{ timeout: 10000 }
		);

		const charge = page.getByRole('button', { name: /^Charge$/ });
		check('P3', 'Charge is disabled before choosing a method', await charge.isDisabled(), 'enabled');

		await selectOption(page, 'payment-method', METHOD);
		check('P3', 'Charge is enabled after choosing a method', await charge.isEnabled(), 'still disabled');
		check(
			'P3',
			'the POS shows where the money lands',
			(await page.locator('.pm-hint').innerText()).includes(WALLET),
			'no destination'
		);

		const totalText = await page.locator('.trow.big').innerText();
		const total = (totalText.match(/-?\d+\.\d{2}/) ?? [''])[0];
		check('P3', 'the total is shown with 2 decimals', total !== '', `total=${totalText}`);

		// --- P4: charge, and the money lands in the wallet -------------------
		await charge.click();
		await page.locator('.done').waitFor({ state: 'visible', timeout: 15000 });
		const done = await page.locator('.done').innerText();
		check('P4', 'the receipt names the method', done.includes(METHOD), done);
		check('P4', 'the receipt names the wallet', done.includes(WALLET), done);

		const afterCharge = await wallets(page);
		check(
			'P4',
			'the wallet balance grew by the sale total',
			afterCharge !== null && Number(afterCharge.balance) === Number(total),
			`balance=${afterCharge?.balance} total=${total}`
		);

		// --- P5: an expense lowers the balance ------------------------------
		await page.goto(`${BASE}/settings/payments`, { waitUntil: 'networkidle' });
		const walletRow = page.locator('tr', { hasText: WALLET }).first();
		await walletRow.getByRole('button', { name: 'Movements' }).click();
		await page.locator(`#movement-amount-${created.id}`).fill('10.00');
		await page.getByRole('button', { name: 'Record movement' }).click();
		await page.waitForFunction(
			() => document.body.innerText.includes('-10.00'),
			{ timeout: 10000 }
		);

		const afterExpense = await wallets(page);
		check(
			'P5',
			'the expense lowered the balance',
			afterExpense !== null && Number(afterExpense.balance) === Number(total) - 10,
			`balance=${afterExpense?.balance} total=${total}`
		);

		// --- P6: no active method, no charge ---------------------------------
		const methodRowNow = page.locator('tr', { hasText: METHOD }).first();
		await methodRowNow.getByRole('button', { name: 'Deactivate' }).click();
		await methodRowNow.getByRole('button', { name: 'Confirm deactivation' }).click();
		await page.waitForFunction(
			(name) => {
				const row = Array.from(document.querySelectorAll('tr')).find((tr) =>
					tr.innerText.includes(name)
				);
				return row ? row.innerText.includes('Activate') : false;
			},
			METHOD,
			{ timeout: 10000 }
		);

		await page.goto(`${BASE}/operations/pos`, { waitUntil: 'networkidle' });
		await page.locator('button.tile:not([disabled])').first().click();
		const emptyMessage = await page.locator('.pm-empty').innerText();
		check(
			'P6',
			'the POS explains there is no active method',
			emptyMessage.includes('No active payment methods'),
			emptyMessage
		);
		check(
			'P6',
			'Charge stays disabled with no active method',
			await page.getByRole('button', { name: /^Charge$/ }).isDisabled(),
			'enabled'
		);

		check('X', 'no page errors', errors.length === 0, errors.join(' | '));
	} finally {
		await browser.close();
	}

	const failed = results.filter((result) => !result.ok);
	console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
	process.exit(failed.length === 0 ? 0 : 1);
})();
