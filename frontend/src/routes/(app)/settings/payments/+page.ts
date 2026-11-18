import { api } from '$lib/api/client';

/**
 * Settings includes inactive rows: without them an operator cannot bring a
 * wallet or method back, and the table would hide history still in the ledger.
 */
export async function load() {
	const [wallets, methods] = await Promise.all([
		api.listWallets(true),
		api.listPaymentMethods(true)
	]);

	return { wallets, methods };
}
