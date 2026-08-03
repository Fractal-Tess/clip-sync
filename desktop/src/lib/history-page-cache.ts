export type HistoryPageData<T> = { items: T[]; total: number };

type FetchPage<T> = (query: string, offset: number, limit: number) => Promise<HistoryPageData<T>>;

const PREFETCH_RADIUS = 2;

export function createHistoryPageCache<T>(fetchPage: FetchPage<T>) {
	let query = '';
	let pageSize = 1;
	let generation = 0;
	let desiredPages = new Set<number>();
	const pages = new Map<number, HistoryPageData<T>>();
	const inFlight = new Map<number, Promise<HistoryPageData<T> | null>>();

	function reset() {
		generation += 1;
		pages.clear();
		inFlight.clear();
		desiredPages.clear();
	}

	function configure(nextQuery: string, nextPageSize: number) {
		const boundedPageSize = Math.max(1, Math.trunc(nextPageSize));
		if (query === nextQuery && pageSize === boundedPageSize) return;
		query = nextQuery;
		pageSize = boundedPageSize;
		reset();
	}

	function prepare(centerPage: number, total: number) {
		const pageCount = Math.max(1, Math.ceil(Math.max(0, total) / pageSize));
		const boundedCenter = Math.min(Math.max(0, Math.trunc(centerPage)), pageCount - 1);
		const first = Math.max(0, boundedCenter - PREFETCH_RADIUS);
		const last = Math.min(pageCount - 1, boundedCenter + PREFETCH_RADIUS);
		desiredPages = new Set(Array.from({ length: last - first + 1 }, (_, index) => first + index));
		for (const page of pages.keys()) {
			if (!desiredPages.has(page)) pages.delete(page);
		}
		return boundedCenter;
	}

	function read(page: number) {
		return pages.get(page) ?? null;
	}

	async function load(page: number, force = false) {
		if (!desiredPages.has(page)) desiredPages.add(page);
		if (!force) {
			const cached = read(page);
			if (cached) return cached;
			const pending = inFlight.get(page);
			if (pending) return pending;
		} else {
			generation += 1;
			inFlight.clear();
			pages.delete(page);
		}

		const requestGeneration = generation;
		const request = fetchPage(query, page * pageSize, pageSize)
			.then((result) => {
				if (requestGeneration !== generation || !desiredPages.has(page)) return null;
				pages.set(page, result);
				return result;
			})
			.finally(() => {
				if (inFlight.get(page) === request) inFlight.delete(page);
			});
		inFlight.set(page, request);
		return request;
	}

	function warm(centerPage: number, total: number) {
		prepare(centerPage, total);
		return Promise.allSettled(
			[...desiredPages].filter((page) => !pages.has(page)).map((page) => load(page))
		);
	}

	return {
		configure,
		reset,
		prepare,
		read,
		load,
		warm,
		cachedPages: () => [...pages.keys()].sort((left, right) => left - right)
	};
}
