import { describe, expect, it } from 'vitest';

import { historyNavigationAction } from './history-navigation';

const navigate = (key: string, index: number, currentPage = 0, pageCount = 3) =>
	historyNavigationAction(key, index, 6, 3, currentPage, pageCount);

describe('history keyboard navigation', () => {
	it.each([
		['ArrowLeft', 2, 1],
		['h', 2, 1],
		['ArrowRight', 1, 2],
		['l', 1, 2],
		['ArrowUp', 4, 1],
		['k', 4, 1],
		['ArrowDown', 1, 4],
		['j', 1, 4]
	])('maps %s from record %i to record %i', (key, index, expectedIndex) => {
		expect(navigate(key, index)).toEqual({ type: 'focus', index: expectedIndex });
	});

	it.each([
		['ArrowRight', 2, 0],
		['l', 5, 3]
	])('pages forward with %s from a last-column record', (key, index, focus) => {
		expect(navigate(key, index, 1)).toEqual({ type: 'page', page: 2, focus });
	});

	it.each([
		['ArrowLeft', 0, 2],
		['h', 3, 5]
	])('pages backward with %s from a first-column record', (key, index, focus) => {
		expect(navigate(key, index, 1)).toEqual({ type: 'page', page: 0, focus });
	});

	it('does not wrap rows at the first or final page boundary', () => {
		expect(navigate('ArrowLeft', 3, 0)).toEqual({ type: 'focus', index: 3 });
		expect(navigate('ArrowRight', 2, 2)).toEqual({ type: 'focus', index: 2 });
	});

	it('does not move vertically when there is no record in that direction', () => {
		expect(navigate('k', 1)).toEqual({ type: 'focus', index: 1 });
		expect(navigate('j', 4)).toEqual({ type: 'focus', index: 4 });
	});

	it('ignores unrelated keys', () => {
		expect(navigate('x', 2)).toBeNull();
	});
});
