<script lang="ts">
	import { onMount, tick } from 'svelte';

	import {
		activateHistory,
		closeAppWindow,
		getHistory,
		getStatus,
		isTauri,
		updateHistory,
		type HistoryItem,
		type HistoryUpdate,
		type Status
	} from '$lib/bridge';
	import ControlNavigation from '$lib/components/control-navigation.svelte';
	import HistoryDeleteDialog from '$lib/components/history/history-delete-dialog.svelte';
	import HistoryFooter from '$lib/components/history/history-footer.svelte';
	import HistoryHeader from '$lib/components/history/history-header.svelte';
	import HistoryMessages from '$lib/components/history/history-messages.svelte';
	import HistoryRegister from '$lib/components/history/history-register.svelte';
	import HistorySearch from '$lib/components/history/history-search.svelte';
	import ControlCenter from '$lib/components/management/control-center.svelte';
	import { controlSections, type ControlSection } from '$lib/control-center';
	import { createHistoryImageCache } from '$lib/history-image-cache.svelte';
	import { createHistoryPageCache, type HistoryPageData } from '$lib/history-page-cache';
	import { historyNavigationAction, type HistoryPageFocus } from '$lib/history-navigation';
	import {
		DEFAULT_HISTORY_COLUMNS,
		DEFAULT_HISTORY_ROWS,
		HISTORY_COMPACT_ROW_HEIGHT,
		HISTORY_MIN_ROW_HEIGHT,
		historyGridCapacity,
		historyPageCount,
		historyPageForOffset,
		historyPageOffset
	} from '$lib/pagination';

	let section = $state<ControlSection>('history');
	let status = $state.raw<Status | null>(null);
	let history = $state.raw<HistoryItem[]>([]);
	let totalHistory = $state(0);
	let historyOffset = $state(0);
	let historyColumnCount = $state(DEFAULT_HISTORY_COLUMNS);
	let historyRowCount = $state(DEFAULT_HISTORY_ROWS);
	let query = $state('');
	let loading = $state(true);
	let statusLoading = $state(true);
	let refreshing = $state(false);
	let historyLoaded = $state(false);
	let error = $state<string | null>(null);
	let notice = $state<string | null>(null);
	let activating = $state<string | null>(null);
	let pendingDelete = $state.raw<{ item: HistoryItem; index: number } | null>(null);
	let selectedIndex = $state(0);
	let searchInput = $state<HTMLInputElement | null>(null);
	let historyGrid = $state<HTMLElement | null>(null);
	let historyWorkspace = $state<HTMLElement | null>(null);
	let refreshGeneration = 0;
	let active = true;
	let statusRequest: Promise<Status> | null = null;
	let resizeTimer: ReturnType<typeof setTimeout> | undefined;
	const connectedToTauri = isTauri();
	const imageCache = createHistoryImageCache();
	const pageCache = createHistoryPageCache<HistoryItem>(getHistory);
	const sectionTitle = $derived(
		controlSections.find((destination) => destination.id === section)?.label ?? 'History'
	);
	const pageSize = $derived(historyColumnCount * historyRowCount);
	const currentPage = $derived(historyPageForOffset(historyOffset, pageSize));
	const pageCount = $derived(historyPageCount(totalHistory, pageSize));
	const rangeStart = $derived(history.length === 0 ? 0 : historyOffset + 1);
	const rangeEnd = $derived(historyOffset + history.length);

	function errorMessage(cause: unknown) {
		return cause instanceof Error ? cause.message : String(cause);
	}

	function focusHistory(index = selectedIndex) {
		if (history.length === 0) return;
		const nextIndex = Math.min(Math.max(index, 0), history.length - 1);
		selectedIndex = nextIndex;
		const historyButton =
			historyGrid?.querySelectorAll<HTMLButtonElement>('.history-cell')[nextIndex];
		historyButton?.focus({ preventScroll: true });
		historyButton?.scrollIntoView({
			block: 'nearest',
			behavior: window.matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth'
		});
	}

	function requestStatus() {
		statusRequest ??= getStatus().finally(() => {
			statusRequest = null;
		});
		return statusRequest;
	}

	async function refreshStatus() {
		statusLoading = true;
		try {
			const nextStatus = await requestStatus();
			if (active) status = nextStatus;
		} catch {
			if (active) status = null;
		} finally {
			if (active) statusLoading = false;
		}
	}

	function applyHistoryPage(
		page: number,
		pageData: HistoryPageData<HistoryItem>,
		focus: HistoryPageFocus | null,
		generation: number
	) {
		history = pageData.items;
		totalHistory = pageData.total;
		historyOffset = page * pageSize;
		selectedIndex =
			history.length === 0
				? 0
				: typeof focus === 'number'
					? Math.min(Math.max(focus, 0), history.length - 1)
					: focus === 'last'
						? history.length - 1
						: focus === 'first'
							? 0
							: Math.min(selectedIndex, history.length - 1);
		historyLoaded = true;
		imageCache.beginPage(generation);
	}

	async function focusRequestedHistory(focus: HistoryPageFocus | null) {
		if (focus === null || history.length === 0) return;
		await tick();
		focusHistory(selectedIndex);
	}

	function prefetchCachedWindowImages(centerPage: number, generation: number) {
		if (generation !== refreshGeneration) return;
		const items = pageCache
			.cachedPages()
			.sort(
				(left, right) => Math.abs(left - centerPage) - Math.abs(right - centerPage) || left - right
			)
			.flatMap((page) => pageCache.read(page)?.items ?? []);
		imageCache.retain(items);
		imageCache.prefetch(items, generation);
	}

	function warmHistoryWindow(page: number, total: number, generation: number) {
		void pageCache.warm(page, total).then(() => prefetchCachedWindowImages(page, generation));
	}

	async function refresh({
		offset = historyOffset,
		includeStatus = true,
		focus = null,
		forcePage = true,
		resetPages = false
	}: {
		offset?: number;
		includeStatus?: boolean;
		focus?: HistoryPageFocus | null;
		forcePage?: boolean;
		resetPages?: boolean;
	} = {}) {
		const generation = ++refreshGeneration;
		const requestedPageSize = pageSize;
		const requestedPage = historyPageForOffset(offset, requestedPageSize);
		pageCache.configure(query, requestedPageSize);
		if (resetPages) pageCache.reset();
		pageCache.prepare(requestedPage, Math.max(totalHistory, offset + requestedPageSize));
		const requestedPageIsCached = pageCache.read(requestedPage) !== null;
		refreshing = true;
		loading = !requestedPageIsCached;
		error = null;
		if (includeStatus) statusLoading = true;
		const [statusResult, historyResult] = await Promise.allSettled([
			includeStatus ? requestStatus() : Promise.resolve(status),
			pageCache.load(requestedPage, forcePage)
		]);
		if (generation !== refreshGeneration) {
			if (includeStatus) statusLoading = false;
			return;
		}

		const failures: string[] = [];
		if (includeStatus) {
			statusLoading = false;
			if (statusResult.status === 'fulfilled') {
				status = statusResult.value;
			} else {
				status = null;
				failures.push(`Status: ${errorMessage(statusResult.reason)}`);
			}
		}
		if (historyResult.status === 'fulfilled' && historyResult.value) {
			const boundedOffset = historyPageOffset(
				requestedPage,
				historyResult.value.total,
				requestedPageSize
			);
			const boundedPage = historyPageForOffset(boundedOffset, requestedPageSize);
			if (boundedPage !== requestedPage) {
				void refresh({
					offset: boundedOffset,
					includeStatus: false,
					focus,
					forcePage: false
				});
				return;
			}

			applyHistoryPage(boundedPage, historyResult.value, focus, generation);
			prefetchCachedWindowImages(boundedPage, generation);
			warmHistoryWindow(boundedPage, historyResult.value.total, generation);
		} else {
			const reason =
				historyResult.status === 'rejected'
					? errorMessage(historyResult.reason)
					: 'The requested history page became stale';
			failures.push(`History: ${reason}`);
		}

		error = failures.length > 0 ? failures.join(' ') : null;
		loading = false;
		refreshing = false;
		await focusRequestedHistory(focus);
	}

	async function goToPage(page: number, focus: HistoryPageFocus | null = null) {
		if (totalHistory === 0) return;
		const offset = historyPageOffset(page, totalHistory, pageSize);
		if (offset === historyOffset) return;
		const boundedPage = historyPageForOffset(offset, pageSize);
		pageCache.prepare(boundedPage, totalHistory);
		const cached = pageCache.read(boundedPage);
		if (!cached) {
			await refresh({ offset, includeStatus: false, focus, forcePage: false });
			return;
		}

		const generation = ++refreshGeneration;
		loading = false;
		refreshing = false;
		error = null;
		applyHistoryPage(boundedPage, cached, focus, generation);
		prefetchCachedWindowImages(boundedPage, generation);
		await focusRequestedHistory(focus);
		warmHistoryWindow(boundedPage, cached.total, generation);
	}

	function updateGridCapacity(requestPage: boolean) {
		if (!historyWorkspace) return;
		const { width, height } = historyWorkspace.getBoundingClientRect();
		if (width <= 0 || height <= 0) return;
		const minRowHeight =
			window.innerHeight <= 360 ? HISTORY_COMPACT_ROW_HEIGHT : HISTORY_MIN_ROW_HEIGHT;
		const capacity = historyGridCapacity(width, height, minRowHeight);
		if (capacity.columns === historyColumnCount && capacity.rows === historyRowCount) return;

		const absoluteSelectedOffset = historyOffset + selectedIndex;
		const restoreGridFocus = historyGrid?.contains(document.activeElement) ?? false;
		historyColumnCount = capacity.columns;
		historyRowCount = capacity.rows;
		if (!requestPage) return;
		const offset = Math.floor(absoluteSelectedOffset / capacity.pageSize) * capacity.pageSize;
		const localSelectedIndex = absoluteSelectedOffset - offset;
		selectedIndex = localSelectedIndex;
		void refresh({
			offset,
			includeStatus: false,
			focus: restoreGridFocus ? localSelectedIndex : null
		});
	}

	async function mutateHistory(item: HistoryItem, index: number, action: HistoryUpdate) {
		activating = item.contentId;
		error = null;
		notice = null;
		try {
			const result = await updateHistory(item.contentId, action);
			if (!result.ok) throw new Error(result.message || 'History update failed');
			notice = result.message;
			pendingDelete = null;
			await refresh({
				includeStatus: false,
				focus: index,
				resetPages: true
			});
		} catch (cause) {
			error = errorMessage(cause);
			await tick();
			focusHistory(index);
		} finally {
			activating = null;
		}
	}

	function confirmPendingDelete() {
		if (!pendingDelete) return;
		void mutateHistory(pendingDelete.item, pendingDelete.index, 'delete');
	}

	function filterHistoryBySource(item: HistoryItem) {
		const source = item.sourceDevice || item.sourceNode;
		if (!source) return;
		const escapedSource = source.replaceAll('\\', '\\\\').replaceAll('"', '\\"');
		query = `device:"${escapedSource}"`;
		notice = null;
		void refresh({ offset: 0, resetPages: true });
	}

	async function activate(item: HistoryItem, index: number) {
		activating = item.contentId;
		error = null;
		notice = null;
		let windowClosed = false;
		try {
			const result = await activateHistory(item.contentId);
			if (!result.ok) throw new Error(result.message || 'Clipboard activation failed');
			notice = result.message;
			if (connectedToTauri) {
				await closeAppWindow();
				windowClosed = true;
			}
		} catch (cause) {
			error = errorMessage(cause);
		} finally {
			activating = null;
			if (!windowClosed) {
				await tick();
				focusHistory(index);
			}
		}
	}

	function handleHistoryNavigation(event: KeyboardEvent, index = selectedIndex) {
		const action = historyNavigationAction(
			event.key,
			index,
			history.length,
			historyColumnCount,
			currentPage,
			pageCount
		);
		if (!action) return false;

		event.preventDefault();
		if (action.type === 'page') {
			void goToPage(action.page, action.focus);
		} else {
			focusHistory(action.index);
		}
		return true;
	}

	function handleSearchKeydown(event: KeyboardEvent) {
		if (event.key === 'ArrowDown' && history.length > 0) {
			event.preventDefault();
			focusHistory();
		}
	}

	function handleGlobalKeydown(event: KeyboardEvent) {
		if (event.key === 'Escape') {
			event.preventDefault();
			void closeAppWindow();
			return;
		}
		if (event.defaultPrevented || event.altKey || event.ctrlKey || event.metaKey) return;
		const target = event.target;
		const isTyping =
			target instanceof HTMLInputElement ||
			target instanceof HTMLTextAreaElement ||
			(target instanceof HTMLElement && target.isContentEditable);
		if (isTyping) return;

		if (section !== 'history') return;

		if (event.key === '/') {
			event.preventDefault();
			searchInput?.focus();
			searchInput?.select();
		} else if (
			event.key === 'Enter' &&
			history[selectedIndex] &&
			!(target instanceof HTMLElement && target.closest('button, a'))
		) {
			event.preventDefault();
			void activate(history[selectedIndex], selectedIndex);
		} else if (history.length > 0 && handleHistoryNavigation(event)) {
			return;
		} else if (event.key === 'PageUp') {
			event.preventDefault();
			void goToPage(currentPage - 1, 'first');
		} else if (event.key === 'PageDown') {
			event.preventDefault();
			void goToPage(currentPage + 1, 'first');
		} else if (event.key.toLowerCase() === 'r') {
			event.preventDefault();
			notice = null;
			void refresh({ resetPages: true });
		}
	}

	onMount(() => {
		updateGridCapacity(false);
		const observer = new ResizeObserver(() => {
			if (resizeTimer) clearTimeout(resizeTimer);
			resizeTimer = setTimeout(() => updateGridCapacity(true), 120);
		});
		if (historyWorkspace) observer.observe(historyWorkspace);
		void refresh({ offset: 0 });
		const statusTimer = window.setInterval(() => {
			if (document.visibilityState === 'visible') void refreshStatus();
		}, 5_000);

		return () => {
			active = false;
			refreshGeneration += 1;
			observer.disconnect();
			window.clearInterval(statusTimer);
			if (resizeTimer) clearTimeout(resizeTimer);
		};
	});
</script>

<svelte:head>
	<title>ClipSync — {sectionTitle}</title>
	<meta
		name="description"
		content="Keyboard-first history for the ClipSync encrypted clipboard mesh"
	/>
</svelte:head>

<svelte:window onkeydowncapture={handleGlobalKeydown} />

<div class="app-shell">
	<HistoryHeader {status} loading={statusLoading} {connectedToTauri} />
	<ControlNavigation {section} onSelect={(destination) => (section = destination)} />
	<div class="section-view history-view" hidden={section !== 'history'}>
		<HistorySearch
			bind:query
			bind:searchInput
			{historyLoaded}
			total={totalHistory}
			{rangeStart}
			{rangeEnd}
			{refreshing}
			onSearch={() => {
				notice = null;
				void refresh({ offset: 0, resetPages: true });
			}}
			onKeydown={handleSearchKeydown}
		/>
		<HistoryMessages
			{connectedToTauri}
			{error}
			{notice}
			onRetry={() => void refresh({ resetPages: true })}
		/>
		<HistoryRegister
			bind:historyGrid
			bind:historyWorkspace
			bind:selectedIndex
			items={history}
			previews={imageCache.previews}
			{loading}
			{refreshing}
			{historyLoaded}
			total={totalHistory}
			{pageSize}
			columns={historyColumnCount}
			rows={historyRowCount}
			{query}
			{activating}
			onClearSearch={() => {
				query = '';
				notice = null;
				void refresh({ offset: 0 });
			}}
			onActivate={(item, index) => void activate(item, index)}
			onPin={(item, index) => void mutateHistory(item, index, item.pinned ? 'unpin' : 'pin')}
			onDelete={(item, index) => (pendingDelete = { item, index })}
			onFilterSource={filterHistoryBySource}
			onNavigate={handleHistoryNavigation}
			onRequestImage={imageCache.request}
		/>
	</div>
	{#if section !== 'history'}
		<div class="section-view management-view">
			{#key section}
				<ControlCenter {section} />
			{/key}
		</div>
	{/if}
	{#if pendingDelete}
		<HistoryDeleteDialog
			item={pendingDelete.item}
			busy={activating === pendingDelete.item.contentId}
			onCancel={() => (pendingDelete = null)}
			onConfirm={confirmPendingDelete}
		/>
	{/if}
	<HistoryFooter
		{currentPage}
		{pageCount}
		total={totalHistory}
		{status}
		{connectedToTauri}
		historyActive={section === 'history'}
		onPrevious={() => void goToPage(currentPage - 1, 'first')}
		onNext={() => void goToPage(currentPage + 1, 'first')}
	/>
</div>
