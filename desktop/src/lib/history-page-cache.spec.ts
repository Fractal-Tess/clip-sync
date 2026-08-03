import { describe, expect, it } from 'vitest';

import { createHistoryPageCache } from './history-page-cache';

describe('history page cache', () => {
	it('keeps two pages on either side and advances the window incrementally', async () => {
		const offsets: number[] = [];
		const cache = createHistoryPageCache<string>(async (_query, offset, limit) => {
			offsets.push(offset);
			return { items: [`page-${offset / limit}`], total: 100 };
		});
		cache.configure('', 10);
		cache.prepare(4, 100);
		await cache.load(4);
		await cache.warm(4, 100);

		expect(cache.cachedPages()).toEqual([2, 3, 4, 5, 6]);
		expect(offsets.sort((left, right) => left - right)).toEqual([20, 30, 40, 50, 60]);

		await cache.warm(5, 100);

		expect(cache.cachedPages()).toEqual([3, 4, 5, 6, 7]);
		expect(offsets.filter((offset) => offset === 70)).toHaveLength(1);
	});

	it('bounds prefetching at the beginning and end of history', async () => {
		const cache = createHistoryPageCache(async (_query, offset) => ({
			items: [offset],
			total: 25
		}));
		cache.configure('', 10);

		await cache.warm(0, 25);
		expect(cache.cachedPages()).toEqual([0, 1, 2]);

		await cache.warm(2, 25);
		expect(cache.cachedPages()).toEqual([0, 1, 2]);
	});

	it('drops stale responses when the query or page size changes', async () => {
		let resolveRequest: ((value: { items: string[]; total: number }) => void) | undefined;
		const cache = createHistoryPageCache<string>(
			() =>
				new Promise((resolve) => {
					resolveRequest = resolve;
				})
		);
		cache.configure('first', 10);
		cache.prepare(0, 20);
		const stale = cache.load(0);

		cache.configure('second', 5);
		resolveRequest?.({ items: ['stale'], total: 20 });
		await stale;

		expect(cache.read(0)).toBeNull();
		expect(cache.cachedPages()).toEqual([]);
	});
});
