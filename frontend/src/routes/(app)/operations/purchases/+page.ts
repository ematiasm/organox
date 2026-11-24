import {
	api,
	ApiError,
	type PaymentMethod,
	type Product,
	type Purchase,
	type PurchaseStatus,
	type Supplier
} from '$lib/api/client';

const PAGE_SIZE = 25;

function parseStatus(raw: string | null): PurchaseStatus | undefined {
	if (raw === 'completed' || raw === 'cancelled' || raw === 'draft') return raw;
	if (raw === 'all') return undefined;
	return 'completed';
}

export async function load({ url }: { url: URL }) {
	const rawStatus = url.searchParams.get('status');
	const statusFilter = parseStatus(rawStatus);
	const statusTab = rawStatus === 'all' ? 'all' : (statusFilter ?? 'completed');
	const page = Math.max(1, Number(url.searchParams.get('page') ?? '1') || 1);

	const [draftRef, history, suppliersList, productsList, methods] = await Promise.all([
		api.openPurchaseDraft(),
		api.listPurchases({
			status: statusFilter,
			limit: PAGE_SIZE,
			offset: (page - 1) * PAGE_SIZE
		}),
		api.listSuppliers({ includeInactive: false, limit: 200, offset: 0 }),
		api.listProducts({ kind: 'product', includeInactive: false, limit: 200, offset: 0 }),
		api.listPaymentMethods(false)
	]);

	const draft = await api.getPurchase(draftRef.id);
	const total = history.total ?? 0;

	let selectedSupplier: Supplier | null = null;
	if (draft.supplier_id) {
		selectedSupplier = suppliersList.items.find((s) => s.id === draft.supplier_id) ?? null;
		if (!selectedSupplier) {
			try {
				selectedSupplier = await api.getSupplier(draft.supplier_id);
			} catch (error) {
				if (!(error instanceof ApiError && error.status === 404)) throw error;
			}
		}
	}

	return {
		draft,
		selectedSupplier,
		statusTab,
		page,
		historyItems: history.items ?? [],
		limit: history.limit ?? PAGE_SIZE,
		offset: history.offset ?? 0,
		total,
		lastPage: Math.max(1, Math.ceil(total / (history.limit ?? PAGE_SIZE))),
		suppliers: suppliersList.items ?? [],
		products: productsList.items ?? [],
		methods
	} satisfies {
		draft: Purchase;
		selectedSupplier: Supplier | null;
		statusTab: string;
		page: number;
		historyItems: Purchase[];
		limit: number;
		offset: number;
		total: number;
		lastPage: number;
		suppliers: Supplier[];
		products: Product[];
		methods: PaymentMethod[];
	};
}
