import { describe, expect, it } from 'vitest';

import {
	clampHistoryPage,
	HISTORY_COMPACT_ROW_HEIGHT,
	historyGridCapacity,
	historyPageCount,
	historyPageForOffset,
	historyPageOffset
} from './pagination';

describe('responsive history pagination', () => {
	it('derives columns and rows from the available history viewport', () => {
		expect(historyGridCapacity(760, 350)).toEqual({ columns: 3, rows: 2, pageSize: 6 });
		expect(historyGridCapacity(480, 160, HISTORY_COMPACT_ROW_HEIGHT)).toEqual({
			columns: 2,
			rows: 1,
			pageSize: 2
		});
		expect(historyGridCapacity(1_200, 720)).toEqual({ columns: 5, rows: 4, pageSize: 20 });
	});

	it('clamps extreme viewport sizes to safe request bounds', () => {
		expect(historyGridCapacity(0, 0)).toEqual({ columns: 1, rows: 1, pageSize: 1 });
		expect(historyGridCapacity(10_000, 10_000)).toEqual({ columns: 8, rows: 8, pageSize: 64 });
	});

	it('computes page counts and aligned backend offsets for a dynamic page size', () => {
		expect(historyPageCount(0, 6)).toBe(1);
		expect(historyPageCount(13, 6)).toBe(3);
		expect(clampHistoryPage(-1, 13, 6)).toBe(0);
		expect(clampHistoryPage(99, 13, 6)).toBe(2);
		expect(historyPageOffset(2, 13, 6)).toBe(12);
		expect(historyPageForOffset(12, 6)).toBe(2);
	});
});
