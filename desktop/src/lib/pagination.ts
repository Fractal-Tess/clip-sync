export const DEFAULT_HISTORY_COLUMNS = 3;
export const DEFAULT_HISTORY_ROWS = 2;
export const MAX_HISTORY_COLUMNS = 8;
export const MAX_HISTORY_ROWS = 8;
export const HISTORY_GRID_RAIL_WIDTH = 17;
export const HISTORY_GRID_GAP = 1;
export const HISTORY_MIN_COLUMN_WIDTH = 220;
export const HISTORY_MIN_ROW_HEIGHT = 144;
export const HISTORY_COMPACT_ROW_HEIGHT = 92;

function positiveInteger(value: number, fallback: number) {
	return Number.isFinite(value) && value > 0 ? Math.max(1, Math.floor(value)) : fallback;
}

export function historyGridCapacity(
	width: number,
	height: number,
	minRowHeight = HISTORY_MIN_ROW_HEIGHT
) {
	const usableWidth = Math.max(0, width - HISTORY_GRID_RAIL_WIDTH);
	const columns = Math.min(
		MAX_HISTORY_COLUMNS,
		Math.max(
			1,
			Math.floor((usableWidth + HISTORY_GRID_GAP) / (HISTORY_MIN_COLUMN_WIDTH + HISTORY_GRID_GAP))
		)
	);
	const rows = Math.min(
		MAX_HISTORY_ROWS,
		Math.max(
			1,
			Math.floor((Math.max(0, height) + HISTORY_GRID_GAP) / (minRowHeight + HISTORY_GRID_GAP))
		)
	);
	return { columns, rows, pageSize: columns * rows };
}

export function historyPageCount(total: number, pageSize: number) {
	return Math.max(1, Math.ceil(Math.max(0, total) / positiveInteger(pageSize, 1)));
}

export function clampHistoryPage(page: number, total: number, pageSize: number) {
	return Math.min(
		Math.max(Number.isFinite(page) ? Math.trunc(page) : 0, 0),
		historyPageCount(total, pageSize) - 1
	);
}

export function historyPageOffset(page: number, total: number, pageSize: number) {
	return clampHistoryPage(page, total, pageSize) * positiveInteger(pageSize, 1);
}

export function historyPageForOffset(offset: number, pageSize: number) {
	return Math.floor(Math.max(0, offset) / positiveInteger(pageSize, 1));
}
