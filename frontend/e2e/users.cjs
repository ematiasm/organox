/*
 * UI verification for the users screen and password rotation (T8, S11).
 *
 * P1: after login the Users screen shows the signed-in owner's email and role,
 *     and the count text matches the number of table rows.
 * P2: a second user created through the API appears in the list after a reload.
 * P3: that user signs in in a second browser context and reaches a private screen.
 * P4: the owner resets that user's password from the row action and the dialog
 *     states that all of the user's sessions were ended.
 * P5: the target's context is now signed out: a private screen lands on /login,
 *     the old password no longer signs in and the new one does.
 * P6: a wrong current password in the account menu shows the error, stays on the
 *     page and leaves the session working.
 * P7: a successful self change signs out an owner context opened earlier while
 *     the context that performed it still works.
 *
 * Usage: E2E_BASE=http://localhost:5173 E2E_EMAIL=owner@example.com \
 *   E2E_PASSWORD=... node e2e/users.cjs
 * Requires a running API and dev server. The probe creates users and has no way
 * to delete them: neither create-user nor the API offers a delete operation.
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
const TARGET_EMAIL = `e2e-users-${stamp}@example.test`;
const generated = (label) => `E2e${label}${stamp}${randomBytes(4).toString('hex')}!`;
const TARGET_INITIAL = generated('Initial');
const TARGET_RESET = generated('Reset');
const OWNER_NEW = generated('Owner');
const WRONG_CURRENT = generated('Wrong');

const results = [];
function check(rule, name, ok, detail) {
	results.push({ rule, name, ok, detail });
	console.log(`${ok ? 'PASS' : 'FAIL'} [${rule}] ${name}${ok ? '' : ` — ${detail}`}`);
}

/** Fail a single rule without abandoning the checks that follow it. */
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

/** Watch a page for unexpected JavaScript errors. */
function watch(page, errors) {
	page.on('pageerror', (error) => errors.push(String(error)));
	page.on('console', (message) => {
		if (message.type() === 'error') errors.push(message.text());
	});
}

/** Submit the login form and report where it landed and what it said. */
async function loginOutcome(page, email, password) {
	await page.goto(`${BASE}/login`, { waitUntil: 'networkidle' });
	await page.locator('#email').fill(email);
	await page.locator('#password').fill(password);
	await page.locator('button[type=submit]').first().click();
	await page.waitForFunction(
		() => location.pathname !== '/login' || document.querySelector('[role="alert"]') !== null,
		null,
		{ timeout: 15000 }
	);
	const error = (await page.locator('[role="alert"]').allTextContents()).map((text) => text.trim());
	return { path: new URL(page.url()).pathname, error };
}

async function login(page, email, password) {
	const outcome = await loginOutcome(page, email, password);
	if (outcome.path === '/login') {
		throw new Error(`login did not leave /login: ${outcome.error.join(' | ') || 'no message'}`);
	}
}

/** The session as the browser's own cookie sees it. */
async function session(page) {
	return page.evaluate(async () => {
		const response = await fetch('/api/v1/auth/me', { credentials: 'same-origin' });
		const body = await response.json().catch(() => null);
		return { status: response.status, email: body?.email ?? null };
	});
}

async function openChangePassword(page) {
	const dialog = page.locator('[data-slot="dialog-content"]');
	// A rejected attempt leaves the dialog open with its error message.
	const cancel = page.getByRole('button', { name: 'Cancel' });
	if (await cancel.isVisible().catch(() => false)) {
		await cancel.click();
	} else {
		await page.keyboard.press('Escape');
	}
	await dialog.first().waitFor({ state: 'hidden', timeout: 5000 }).catch(() => {});

	await page.locator('button.user').click();
	const item = page.locator('[data-slot="dropdown-menu-item"]', { hasText: 'Change password' });
	await item.waitFor({ state: 'visible', timeout: 5000 });
	await item.click();
	await dialog.waitFor({ state: 'visible', timeout: 5000 });
	return dialog;
}

(async () => {
	const browser = await chromium.launch();
	const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 } });
	const page = await ctx.newPage();
	const errors = [];
	watch(page, errors);

	let targetPage = null;
	let otherOwnerPage = null;
	let otherOwnerSetupError = null;

	try {
		await login(page, EMAIL, PASSWORD);

		// The second owner context must exist before any password changes.
		try {
			const otherCtx = await browser.newContext({ viewport: { width: 1440, height: 900 } });
			otherOwnerPage = await otherCtx.newPage();
			watch(otherOwnerPage, errors);
			await login(otherOwnerPage, EMAIL, PASSWORD);
			const ownerSession = await session(otherOwnerPage);
			if (ownerSession.status !== 200 || ownerSession.email !== EMAIL) {
				throw new Error(`me=${ownerSession.status} email=${ownerSession.email}`);
			}
		} catch (error) {
			otherOwnerSetupError = String(error).split('\n')[0];
		}

		// --- P1: the owner's own row and the count ---------------------------
		await attempt('P1', "owner's email and role are in the list", async () => {
			await page.goto(`${BASE}/users`, { waitUntil: 'networkidle' });
			const row = page.locator('tbody tr', { hasText: EMAIL }).first();
			await row.waitFor({ state: 'visible', timeout: 10000 });
			const text = (await row.innerText()).replace(/\s+/g, ' ').trim();
			return {
				ok: text.includes(EMAIL) && /Owner/i.test(text),
				detail: `row=${text}`
			};
		});
		await attempt('P1', 'count text matches the number of rows', async () => {
			const rows = await page.locator('tbody tr').count();
			const counter = await page.evaluate(() => {
				const el = Array.from(document.querySelectorAll('main *')).find(
					(node) =>
						node.children.length === 0 && /^\d+ users?$/.test(node.textContent?.trim() ?? '')
				);
				return el?.textContent?.trim() ?? '';
			});
			const count = Number(counter.match(/^(\d+)/)?.[1] ?? -1);
			return {
				ok: count === rows,
				detail: `count=${counter} rows=${rows}`
			};
		});

		// --- P2: create through the API, then reload the screen --------------
		const rowsBefore = await page.locator('tbody tr').count();
		await attempt('P2', 'API creates the second user', async () => {
			const created = await page.evaluate(
				async ({ email, password }) => {
					const response = await fetch('/api/v1/users', {
						method: 'POST',
						credentials: 'same-origin',
						headers: { 'content-type': 'application/json' },
						body: JSON.stringify({ email, password, role: 'manager' })
					});
					return { status: response.status, body: await response.json().catch(() => null) };
				},
				{ email: TARGET_EMAIL, password: TARGET_INITIAL }
			);
			return {
				ok: created.status === 201 && typeof created.body?.id === 'string',
				detail: `status=${created.status} body=${JSON.stringify(created.body)}`
			};
		});
		await attempt('P2', 'the new user appears after a reload', async () => {
			await page.reload({ waitUntil: 'networkidle' });
			await page
				.locator('tbody tr', { hasText: TARGET_EMAIL })
				.first()
				.waitFor({ state: 'visible', timeout: 10000 });
			const rows = await page.locator('tbody tr').count();
			return {
				ok: rows === rowsBefore + 1,
				detail: `before=${rowsBefore} rows=${rows}`
			};
		});

		// --- P3: separate cookies, real private screen ----------------------
		await attempt('P3', 'the second user signs in in a separate context', async () => {
			const targetCtx = await browser.newContext({ viewport: { width: 1440, height: 900 } });
			targetPage = await targetCtx.newPage();
			watch(targetPage, errors);
			await login(targetPage, TARGET_EMAIL, TARGET_INITIAL);
			const me = await session(targetPage);
			return {
				ok: me.status === 200 && me.email === TARGET_EMAIL,
				detail: `path=${new URL(targetPage.url()).pathname} me=${me.status} email=${me.email}`
			};
		});
		await attempt('P3', 'the second context reaches a private screen', async () => {
			await targetPage.goto(`${BASE}/products`, { waitUntil: 'networkidle' });
			const path = new URL(targetPage.url()).pathname;
			return { ok: path === '/products', detail: `path=${path}` };
		});

		// --- P4: owner reset from the row action ----------------------------
		await attempt('P4', 'row action resets the password and reports revocation', async () => {
			const row = page.locator('tbody tr', { hasText: TARGET_EMAIL }).first();
			const button = row.getByRole('button', { name: `Reset password for ${TARGET_EMAIL}` });
			await button.waitFor({ state: 'visible', timeout: 5000 });
			await button.click();
			const dialog = page.locator('[data-slot="dialog-content"]');
			await dialog.waitFor({ state: 'visible', timeout: 5000 });
			await dialog.locator('#reset-password').fill(TARGET_RESET);
			await dialog.getByRole('button', { name: 'Reset password' }).click();
			const status = dialog.getByRole('status');
			await status.waitFor({ state: 'visible', timeout: 10000 });
			const text = (await status.innerText()).replace(/\s+/g, ' ').trim();
			return {
				ok: text.includes('All sessions of that user were ended'),
				detail: `status=${text}`
			};
		});

		// --- P5: the reset really ended the target's sessions ---------------
		await attempt('P5', 'a private screen sends the target context to /login', async () => {
			await targetPage.goto(`${BASE}/products`, { waitUntil: 'networkidle' });
			await targetPage.waitForURL('**/login', { timeout: 10000 });
			return { ok: new URL(targetPage.url()).pathname === '/login', detail: `url=${targetPage.url()}` };
		});
		await attempt('P5', 'the old password no longer signs in', async () => {
			const outcome = await loginOutcome(targetPage, TARGET_EMAIL, TARGET_INITIAL);
			return {
				ok:
					outcome.path === '/login' &&
					outcome.error.some((text) => /invalid credentials/i.test(text)),
				detail: `path=${outcome.path} error=${outcome.error.join(' | ')}`
			};
		});
		await attempt('P5', 'the new password signs in', async () => {
			const outcome = await loginOutcome(targetPage, TARGET_EMAIL, TARGET_RESET);
			return {
				ok: outcome.path === '/products' && outcome.error.length === 0,
				detail: `path=${outcome.path} error=${outcome.error.join(' | ')}`
			};
		});

		// --- P6: a wrong current password is only a form error --------------
		await attempt('P6', 'the account menu reports a wrong current password', async () => {
			await page.goto(`${BASE}/users`, { waitUntil: 'networkidle' });
			const dialog = await openChangePassword(page);
			await dialog.locator('#current-password').fill(WRONG_CURRENT);
			await dialog.locator('#new-password').fill(generated('Unused'));
			await dialog.getByRole('button', { name: 'Change password' }).click();
			const alert = dialog.getByRole('alert');
			await alert.waitFor({ state: 'visible', timeout: 10000 });
			const text = (await alert.innerText()).trim();
			return {
				ok: text === 'The current password is incorrect.',
				detail: `error=${text}`
			};
		});
		await attempt('P6', 'the rejected change stays on the same page', async () => {
			const path = new URL(page.url()).pathname;
			return { ok: path === '/users', detail: `path=${path}` };
		});
		await attempt('P6', 'the session still works after the rejection', async () => {
			const me = await session(page);
			return {
				ok: me.status === 200 && me.email === EMAIL,
				detail: `me=${me.status} email=${me.email}`
			};
		});

		// --- P7: self change keeps this session, kills the other one --------
		await attempt('P7', 'self change succeeds and reports the other sessions ended', async () => {
			const dialog = await openChangePassword(page);
			await dialog.locator('#current-password').fill(PASSWORD);
			await dialog.locator('#new-password').fill(OWNER_NEW);
			await dialog.getByRole('button', { name: 'Change password' }).click();
			const status = dialog.getByRole('status');
			await status.waitFor({ state: 'visible', timeout: 10000 });
			const text = (await status.innerText()).replace(/\s+/g, ' ').trim();
			return {
				ok: /other sessions were ended/i.test(text),
				detail: `status=${text}`
			};
		});
		await attempt('P7', 'the earlier owner context is signed out', async () => {
			if (!otherOwnerPage) throw new Error(otherOwnerSetupError ?? 'no second owner context');
			await otherOwnerPage.goto(`${BASE}/products`, { waitUntil: 'networkidle' });
			await otherOwnerPage.waitForURL('**/login', { timeout: 10000 });
			return {
				ok: new URL(otherOwnerPage.url()).pathname === '/login',
				detail: `url=${otherOwnerPage.url()}`
			};
		});
		await attempt('P7', 'the acting owner context still works', async () => {
			const me = await session(page);
			await page.goto(`${BASE}/users`, { waitUntil: 'networkidle' });
			const path = new URL(page.url()).pathname;
			return {
				ok: me.status === 200 && me.email === EMAIL && path === '/users',
				detail: `me=${me.status} email=${me.email} path=${path}`
			};
		});

		// Revoked sessions make the browser log expected 401s, so console errors
		// are printed as context, not turned into an extra verdict.
		console.log('\nbrowser errors:');
		if (errors.length === 0) {
			console.log('  none');
		} else {
			for (const error of errors) console.log('  ' + error);
		}
	} finally {
		await browser.close();
	}

	const failed = results.filter((result) => !result.ok);
	console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
	process.exit(failed.length === 0 ? 0 : 1);
})();
