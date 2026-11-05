/*
 * UI verification for the single navigation system.
 * R1: Every private screen has exactly one sidebar with the same links.
 * R2: Start a sale from the rail, not a page link.
 * R3: The expanded rail shows icons and labels and persists across navigation.
 * R4: Sign out lives in the user menu and ends the session.
 * Usage: node e2e/nav-shell.cjs (requires `vite dev` on :3000 and E2E_EMAIL/E2E_PASSWORD)
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

/** Visible sidebar landmarks with links, excluding nested landmarks. */
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

	// `/pos` remains a redirect; the new path is the actual destination.
	const screens = ['/products', '/users', '/operations/pos', '/operations/movements'];
	const navSets = {};

	for (const path of screens) {
		await page.goto(`${BASE}${path}`, { waitUntil: 'networkidle' });
		await page.waitForTimeout(400);
		const navs = await lateralNavs(page);
		navSets[path] = navs;
		check('R1', `${path}: one sidebar`, navs.length === 1, `found ${navs.length}: ${JSON.stringify(navs)}`);
		if (navs.length === 1) {
			const links = navs[0].links.join(', ');
			const expected = 'Sale, Products, Movements, Users';
			check('R1', `${path}: canonical navigation`, links === expected, `"${links}" != "${expected}"`);
		}
	}

	// R2: start a sale from the rail
	await page.goto(`${BASE}/products`, { waitUntil: 'networkidle' });
	const startSale = page.locator('nav a[href="/operations/pos"], nav button[href="/operations/pos"], a[aria-label="Sale"], button[aria-label="Sale"]').first();
	check('R2', 'the rail has the start sale action', (await startSale.count()) > 0, 'no control in the rail');
	const pageLinksToPos = await page.locator('main a[href*="pos"], main button:has-text("sale")').count();
	check('R2', 'the page does not repeat the sale link', pageLinksToPos === 0, `${pageLinksToPos} sale link(s) outside the rail`);

	if ((await startSale.count()) > 0) {
		await startSale.click();
		await page.waitForURL(/\/operations\/pos/, { timeout: 10000 });
		check('R2', 'the rail action leads to the sale', page.url().includes('/operations/pos'), page.url());
	}

	// R4: Sign out is in the user menu
	const looseLogout = await page.locator('nav button:has-text("Sign out"), aside button:has-text("Sign out")').count();
	const userButton = page.locator('aside button[aria-haspopup="menu"]').first();
	check('R4', 'no standalone Sign out in the rail', looseLogout === 0, `${looseLogout} standalone button(s)`);
	check('R4', 'the rail has a user button', (await userButton.count()) > 0, 'no user button');
	if ((await userButton.count()) > 0) {
		await userButton.click();
		const logoutItem = page.getByRole('menuitem', { name: /sign out/i }).first();
		check('R4', 'the user menu offers Sign out', (await logoutItem.count()) > 0, 'no Sign out item');

		// R3: expanding the rail shows icons and labels
		await page.keyboard.press('Escape');
		const toggle = page.locator('aside button[aria-controls]').first();
		const collapsedWidth = await page.locator('aside').first().evaluate((el) => el.getBoundingClientRect().width);
		if ((await toggle.count()) > 0) {
			await toggle.click();
			await page.waitForTimeout(250);
			const expandedWidth = await page.locator('aside').first().evaluate((el) => el.getBoundingClientRect().width);
			check('R3', 'the rail expands', expandedWidth > collapsedWidth + 40, `${collapsedWidth} -> ${expandedWidth}`);
			const visibleLabels = await page.locator('aside nav a').evaluateAll((els) =>
				els.filter((el) => el.getBoundingClientRect().width > 40).map((el) => el.textContent.trim())
			);
			check('R3', 'the expanded rail shows labels', visibleLabels.includes('Products') && visibleLabels.includes('Users'), JSON.stringify(visibleLabels));

			// persistence across navigation
			await page.locator('aside nav a[href="/products"]').first().click();
			await page.waitForTimeout(500);
			const stillExpanded = await page.locator('aside').first().evaluate((el) => el.getBoundingClientRect().width);
			check('R3', 'expansion persists across navigation', stillExpanded > collapsedWidth + 40, `${stillExpanded}`);
		} else {
			check('R3', 'the rail has an expand control', false, 'no button with aria-controls');
		}

		// R4 actual sign out
		await page.goto(`${BASE}/products`, { waitUntil: 'networkidle' });
		await page.locator('aside button[aria-haspopup="menu"]').first().click();
		await page.getByRole('menuitem', { name: /sign out/i }).first().click();
		await page.waitForURL(/\/login/, { timeout: 10000 }).catch(() => {});
		check('R4', 'Sign out ends the session', page.url().includes('/login'), page.url());
	}

	check('R1', 'no page errors', errors.length === 0, errors.join(' | '));

	await browser.close();

	const failed = results.filter((r) => !r.ok);
	console.log(`\n${results.length - failed.length}/${results.length} checks OK`);
	process.exit(failed.length === 0 ? 0 : 1);
})().catch((e) => {
	console.error('ERROR', e);
	process.exit(3);
});
