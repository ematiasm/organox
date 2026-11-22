/**
 * Exact integer-cent helpers for UI previews (amount to charge at the till,
 * remaining credit limit, oldest-first receipt allocation). The server remains
 * the only validator and source of truth; figures returned by the API are
 * rendered verbatim.
 */

const MONEY_PATTERN = /^(-)?(\d+)(?:\.(\d{1,2}))?$/;

/** Parse a decimal string with at most two fractional digits into integer cents. */
export function parseCents(raw: string): bigint | null {
	const trimmed = raw.trim();
	if (!trimmed) return null;
	const match = MONEY_PATTERN.exec(trimmed);
	if (!match) return null;
	const negative = match[1] === '-';
	const whole = BigInt(match[2] ?? '0');
	const frac = (match[3] ?? '').padEnd(2, '0');
	const cents = whole * 100n + BigInt(frac);
	return negative ? -cents : cents;
}

/** Format integer cents as a fixed two-decimal string (e.g. `1250n` -> `"12.50"`). */
export function formatCents(cents: bigint): string {
	const negative = cents < 0n;
	const abs = negative ? -cents : cents;
	const whole = abs / 100n;
	const frac = (abs % 100n).toString().padStart(2, '0');
	return `${negative ? '-' : ''}${whole}.${frac}`;
}

export function minCents(a: bigint, b: bigint): bigint {
	return a < b ? a : b;
}

export function maxCents(a: bigint, b: bigint): bigint {
	return a > b ? a : b;
}

/**
 * Convenience oldest-first prefill for the receipt form (S6). The server never
 * invents allocations; this only populates the editable inputs in the browser.
 */
export function allocateOldestFirst(
	amountCents: bigint,
	sales: ReadonlyArray<{ sale_id: string; outstanding: string; completed_at: string }>
): Record<string, string> {
	const result: Record<string, string> = {};
	if (amountCents <= 0n) return result;

	const ordered = [...sales].sort((a, b) => a.completed_at.localeCompare(b.completed_at));
	let remaining = amountCents;

	for (const sale of ordered) {
		if (remaining <= 0n) break;
		const due = parseCents(sale.outstanding);
		if (due === null || due <= 0n) continue;
		const take = minCents(remaining, due);
		if (take > 0n) {
			result[sale.sale_id] = formatCents(take);
			remaining -= take;
		}
	}

	return result;
}
