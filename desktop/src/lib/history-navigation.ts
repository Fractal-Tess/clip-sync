export type HistoryPageFocus = 'first' | 'last' | number;

export type HistoryNavigationAction =
	{ type: 'focus'; index: number } | { type: 'page'; page: number; focus: HistoryPageFocus };

export function historyNavigationAction(
	key: string,
	index: number,
	itemCount: number,
	columns: number,
	currentPage: number,
	pageCount: number
): HistoryNavigationAction | null {
	const normalizedKey = key.length === 1 ? key.toLowerCase() : key;
	const safeColumns = Math.max(1, Math.trunc(columns));

	switch (normalizedKey) {
		case 'ArrowLeft':
		case 'h': {
			const row = Math.floor(index / safeColumns);
			if (index % safeColumns !== 0) return { type: 'focus', index: index - 1 };
			return currentPage > 0
				? {
						type: 'page',
						page: currentPage - 1,
						focus: row * safeColumns + safeColumns - 1
					}
				: { type: 'focus', index };
		}
		case 'ArrowRight':
		case 'l': {
			const row = Math.floor(index / safeColumns);
			const isLastColumn = (index + 1) % safeColumns === 0;
			if (!isLastColumn && index + 1 < itemCount) {
				return { type: 'focus', index: index + 1 };
			}
			return currentPage + 1 < pageCount
				? { type: 'page', page: currentPage + 1, focus: row * safeColumns }
				: { type: 'focus', index };
		}
		case 'ArrowUp':
		case 'k':
			return {
				type: 'focus',
				index: index - safeColumns >= 0 ? index - safeColumns : index
			};
		case 'ArrowDown':
		case 'j':
			return {
				type: 'focus',
				index: index + safeColumns < itemCount ? index + safeColumns : index
			};
		case 'Home':
			return { type: 'focus', index: 0 };
		case 'End':
			return { type: 'focus', index: Math.max(0, itemCount - 1) };
		default:
			return null;
	}
}
